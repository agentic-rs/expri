//! Additive launch requests share the dashboard's transactional save invariants.
use expri_core::experiment_assignment::{
  ExperimentSelection, RunExperimentAssignment, RunExperimentIntent,
};

use super::*;

enum ResolvedSelection {
  Existing(Experiment),
  New {
    name: String,
    anchor: Option<RunScope>,
  },
}

fn anchor(
  db: &Connection,
  project_id: &str,
  run_id: &str,
  origin: Option<&str>,
) -> ApiResult<RunScope> {
  if let Some(origin) = origin {
    let scope = RunScope {
      project_id: project_id.into(),
      origin: origin.into(),
      run_id: run_id.into(),
    };
    if available(db, &scope)? {
      return Ok(scope);
    }
    return Err(ApiError::new(404, "cannot join a missing or expired run"));
  }
  let mut query = db
    .prepare(
      "SELECT DISTINCT json_extract(target,'$.scope.origin') AS origin
    FROM (SELECT target FROM files UNION ALL SELECT target FROM streams)
    WHERE json_extract(target,'$.kind')='run'
      AND json_extract(target,'$.scope.project_id')=?1
      AND json_extract(target,'$.scope.run_id')=?2
      AND NOT EXISTS(SELECT 1 FROM run_retention r WHERE r.project_id=?1
        AND r.origin=json_extract(target,'$.scope.origin') AND r.run_id=?2
        AND (r.status<>'archived' OR r.delete_after<=?3)) LIMIT 2",
    )
    .map_err(database)?;
  let origins = query
    .query_map(params![project_id, run_id, run_retention::now()], |row| {
      row.get::<_, String>(0)
    })
    .map_err(database)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(database)?;
  match origins.as_slice() {
    [origin] => Ok(RunScope {
      project_id: project_id.into(),
      origin: origin.clone(),
      run_id: run_id.into(),
    }),
    [] => Err(ApiError::new(404, "cannot join a missing or expired run")),
    _ => Err(ApiError::new(
      409,
      "run ID exists on multiple origins; specify the run's origin",
    )),
  }
}

fn membership(db: &Connection, scope: &RunScope) -> ApiResult<Option<String>> {
  db.query_row(
    "SELECT experiment_id FROM experiment_members WHERE project_id=?1 AND origin=?2 AND run_id=?3",
    params![scope.project_id, scope.origin, scope.run_id],
    |row| row.get(0),
  )
  .optional()
  .map_err(database)
}

fn existing(db: &Connection, project_id: &str, id: &str) -> ApiResult<Experiment> {
  load(db, project_id, id)?.ok_or_else(|| ApiError::new(404, "experiment is missing"))
}

fn resolve(
  db: &Connection,
  project_id: &str,
  selection: &ExperimentSelection,
) -> ApiResult<ResolvedSelection> {
  validate_component(project_id).map_err(bad)?;
  selection.validate().map_err(bad)?;
  projects::ensure_active(db, project_id)?;
  match selection {
    ExperimentSelection::Existing { experiment_id } => Ok(ResolvedSelection::Existing(existing(
      db,
      project_id,
      experiment_id,
    )?)),
    ExperimentSelection::New { name } => Ok(ResolvedSelection::New {
      name: name.trim().into(),
      anchor: None,
    }),
    ExperimentSelection::JoinRun { run_id, origin } => {
      let anchor = anchor(db, project_id, run_id, origin.as_deref())?;
      if let Some(id) = membership(db, &anchor)? {
        Ok(ResolvedSelection::Existing(existing(db, project_id, &id)?))
      } else {
        Ok(ResolvedSelection::New {
          name: format!("Experiment {}", anchor.run_id),
          anchor: Some(anchor),
        })
      }
    }
  }
}

pub(in crate::service::store) fn validate_selection(
  db: &Connection,
  project_id: &str,
  selection: &ExperimentSelection,
) -> ApiResult<()> {
  validate_capacity(db, project_id, resolve(db, project_id, selection)?)
}

fn validate_capacity(
  db: &Connection,
  project_id: &str,
  resolved: ResolvedSelection,
) -> ApiResult<()> {
  match resolved {
    ResolvedSelection::Existing(experiment) if experiment.members.len() >= MEMBER_LIMIT => {
      Err(ApiError::new(409, "experiment supports at most 256 runs"))
    }
    ResolvedSelection::New { .. } => {
      let count: usize = db
        .query_row(
          "SELECT COUNT(*) FROM experiments WHERE project_id=?1",
          [project_id],
          |row| row.get(0),
        )
        .map_err(database)?;
      if count >= EXPERIMENT_LIMIT {
        return Err(ApiError::new(
          409,
          "project supports at most 128 experiments",
        ));
      }
      Ok(())
    }
    _ => Ok(()),
  }
}

impl<S: ObjectStorage> Store<S> {
  pub(in crate::service) fn validate_experiment_intent(
    &self,
    project_id: &str,
    intent: &RunExperimentIntent,
  ) -> ApiResult<()> {
    validate_component(project_id).map_err(bad)?;
    intent.validate().map_err(bad)?;
    let db = self.db()?;
    projects::ensure_active(&db, project_id)?;
    let previous: Option<(String, String)> = db.query_row(
      "SELECT fingerprint,experiment_id FROM experiment_assignment_intents WHERE project_id=?1 AND request_id=?2",
      params![project_id,intent.request_id], |row|Ok((row.get(0)?,row.get(1)?))
    ).optional().map_err(database)?;
    if let Some((fingerprint, experiment_id)) = previous {
      if fingerprint
        != hex_digest(
          &serde_json::to_vec(&intent.selection)
            .map_err(|_| ApiError::new(500, "cannot encode experiment selection"))?,
        )
      {
        return Err(ApiError::new(
          409,
          "experiment request_id is already assigned to a different selection",
        ));
      }
      let experiment = load(&db, project_id, &experiment_id)?
        .ok_or_else(|| ApiError::new(410, "the experiment selected by this request was deleted"))?;
      return validate_capacity(&db, project_id, ResolvedSelection::Existing(experiment));
    }
    validate_selection(&db, project_id, &intent.selection)
  }

  #[cfg(test)]
  fn validate_experiment_selection(
    &self,
    project_id: &str,
    selection: &ExperimentSelection,
  ) -> ApiResult<()> {
    let db = self.db()?;
    validate_selection(&db, project_id, selection)
  }

  pub(in crate::service) fn assign_run_experiment(
    &self,
    scope: &RunScope,
    intent: &RunExperimentIntent,
  ) -> ApiResult<RunExperimentAssignment> {
    validate_scope(scope).map_err(bad)?;
    intent.validate().map_err(bad)?;
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = gate.try_read().map_err(|error| match error {
      TryLockError::WouldBlock => ApiError::new(
        409,
        "project operation is busy; retry experiment assignment",
      ),
      TryLockError::Poisoned(_) => ApiError::new(503, "project operation unavailable"),
    })?;
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    projects::ensure_active(&tx, &scope.project_id)?;
    let fingerprint = hex_digest(
      &serde_json::to_vec(&intent.selection)
        .map_err(|_| ApiError::new(500, "cannot encode experiment selection"))?,
    );
    let previous: Option<(String, String)> = tx.query_row(
      "SELECT fingerprint,experiment_id FROM experiment_assignment_intents WHERE project_id=?1 AND request_id=?2",
      params![scope.project_id,intent.request_id], |row|Ok((row.get(0)?,row.get(1)?))
    ).optional().map_err(database)?;
    if previous
      .as_ref()
      .is_some_and(|(saved, _)| saved != &fingerprint)
    {
      return Err(ApiError::new(
        409,
        "experiment request_id is already assigned to a different selection",
      ));
    }
    let receipt: Option<String> = tx.query_row(
      "SELECT receipt FROM experiment_assignment_receipts WHERE project_id=?1 AND request_id=?2 AND origin=?3 AND run_id=?4",
      params![scope.project_id,intent.request_id,scope.origin,scope.run_id], |row|row.get(0)
    ).optional().map_err(database)?;
    if let Some(receipt) = receipt {
      // Manual membership edits and deletion take precedence over background retries.
      return serde_json::from_str(&receipt)
        .map_err(|_| ApiError::new(500, "invalid experiment assignment receipt"));
    }
    if !available(&tx, scope)? {
      return Err(ApiError::new(
        404,
        "publish the run before assigning its experiment",
      ));
    }
    let resolved =
      if let Some((_, id)) = &previous {
        ResolvedSelection::Existing(load(&tx, &scope.project_id, id)?.ok_or_else(|| {
          ApiError::new(410, "the experiment selected by this request was deleted")
        })?)
      } else {
        resolve(&tx, &scope.project_id, &intent.selection)?
      };
    let mut selection = match resolved {
      ResolvedSelection::Existing(experiment) => ExperimentSave {
        project_id: scope.project_id.clone(),
        experiment_id: Some(experiment.experiment_id),
        expected_revision: Some(experiment.revision),
        create_request_id: None,
        name: experiment.name,
        description: experiment.description,
        members: experiment.members,
        lineage: experiment.lineage,
      },
      ResolvedSelection::New { name, anchor } => {
        if anchor.as_ref() == Some(scope) {
          return Err(ApiError::new(400, "a new run cannot join itself"));
        }
        // The assignment ledger is authoritative; this private key only reuses
        // the dashboard's validated creation path inside the same transaction.
        let create_request_id = tx
          .query_row(
            "SELECT 'assignment-'||lower(hex(randomblob(16)))",
            [],
            |row| row.get(0),
          )
          .map_err(database)?;
        ExperimentSave {
          project_id: scope.project_id.clone(),
          experiment_id: None,
          expected_revision: None,
          create_request_id: Some(create_request_id),
          name,
          description: String::new(),
          members: anchor.into_iter().collect(),
          lineage: Vec::new(),
        }
      }
    };
    if !selection.members.contains(scope) {
      selection.members.push(scope.clone());
    }
    if let Some(parent) = run_control::experiment_restart_parent(&tx, scope, intent)? {
      // An explicit different experiment never moves the source run. Preserve
      // manually recorded relationships when this pair is already present.
      if selection.members.contains(&parent)
        && !selection
          .lineage
          .iter()
          .any(|edge| edge.parent == parent && edge.child == *scope)
      {
        selection.lineage.push(Lineage {
          parent,
          child: scope.clone(),
          kind: LineageKind::Restarted,
          checkpoint: None,
          step: None,
        });
      }
    }
    let experiment = save_normalized(&tx, normalize(selection)?)?;
    let receipt = RunExperimentAssignment {
      scope: scope.clone(),
      experiment_id: experiment.experiment_id,
      name: experiment.name,
    };
    tx.execute("INSERT OR IGNORE INTO experiment_assignment_intents(project_id,request_id,fingerprint,experiment_id) VALUES(?1,?2,?3,?4)",
      params![scope.project_id,intent.request_id,fingerprint,receipt.experiment_id]).map_err(database)?;
    let encoded = serde_json::to_string(&receipt)
      .map_err(|_| ApiError::new(500, "cannot encode experiment assignment"))?;
    tx.execute("INSERT INTO experiment_assignment_receipts(project_id,request_id,origin,run_id,receipt) VALUES(?1,?2,?3,?4,?5)",
      params![scope.project_id,intent.request_id,scope.origin,scope.run_id,encoded]).map_err(database)?;
    tx.commit().map_err(database)?;
    Ok(receipt)
  }
}

#[cfg(test)]
mod tests;
