//! Service-owned grouping and lineage never alter worker-owned run history.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::TryLockError;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;

mod assignment;
pub(super) use assignment::validate_selection as validate_assignment_selection;

pub(in crate::service) const EXPERIMENT_LIMIT: usize = 128;
const MEMBER_LIMIT: usize = 256;
const LINEAGE_LIMIT: usize = 512;
const PROJECT_BYTE_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::service) struct Lineage {
  pub parent: RunScope,
  pub child: RunScope,
  pub kind: LineageKind,
  pub checkpoint: Option<String>,
  /// Decimal text preserves all u64 steps when the browser decodes JSON.
  pub step: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::service) enum LineageKind {
  Continued,
  Restarted,
  Split,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(in crate::service) struct Experiment {
  pub experiment_id: String,
  pub name: String,
  pub description: String,
  pub revision: String,
  pub members: Vec<RunScope>,
  pub lineage: Vec<Lineage>,
  pub created_at: String,
  pub updated_at: String,
  pub unavailable_runs: Vec<RunScope>,
  pub membership_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::service) struct ExperimentSave {
  pub project_id: String,
  pub experiment_id: Option<String>,
  pub expected_revision: Option<String>,
  pub create_request_id: Option<String>,
  pub name: String,
  pub description: String,
  pub members: Vec<RunScope>,
  pub lineage: Vec<Lineage>,
}

pub(super) fn initialize(db: &Connection) -> crate::error::Result<()> {
  db.execute_batch(
    "CREATE TABLE IF NOT EXISTS experiments (
      project_id TEXT NOT NULL, experiment_id TEXT NOT NULL, record TEXT NOT NULL,
      PRIMARY KEY(project_id,experiment_id));
    CREATE TABLE IF NOT EXISTS experiment_members (
      project_id TEXT NOT NULL, origin TEXT NOT NULL, run_id TEXT NOT NULL,
      experiment_id TEXT NOT NULL, PRIMARY KEY(project_id,origin,run_id));
    CREATE INDEX IF NOT EXISTS experiment_members_group ON experiment_members(project_id,experiment_id);
    CREATE TABLE IF NOT EXISTS experiment_revisions (
      project_id TEXT PRIMARY KEY, revision INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS experiment_creates (
      project_id TEXT NOT NULL, create_request_id TEXT NOT NULL,
      experiment_id TEXT NOT NULL, fingerprint TEXT NOT NULL,
      PRIMARY KEY(project_id,create_request_id));
    CREATE TABLE IF NOT EXISTS experiment_assignment_intents (
      project_id TEXT NOT NULL, request_id TEXT NOT NULL, fingerprint TEXT NOT NULL,
      experiment_id TEXT NOT NULL, PRIMARY KEY(project_id,request_id));
    CREATE TABLE IF NOT EXISTS experiment_assignment_receipts (
      project_id TEXT NOT NULL, request_id TEXT NOT NULL, origin TEXT NOT NULL,
      run_id TEXT NOT NULL, receipt TEXT NOT NULL,
      PRIMARY KEY(project_id,request_id,origin,run_id));",
  )
  .map_err(|_| crate::error::ExpriError::Message("cannot initialize experiments".into()))
}

fn key(scope: &RunScope) -> (&str, &str) {
  (&scope.origin, &scope.run_id)
}

fn text(
  value: &str,
  field: &str,
  characters: usize,
  bytes: usize,
  multiline: bool,
) -> ApiResult<String> {
  let value = value.trim();
  if value.len() > bytes
    || value.chars().count() > characters
    || value
      .chars()
      .any(|ch| ch.is_control() && !(multiline && matches!(ch, '\n' | '\t')))
  {
    return Err(ApiError::new(400, format!("invalid experiment {field}")));
  }
  Ok(value.into())
}

fn normalize(mut selection: ExperimentSave) -> ApiResult<ExperimentSave> {
  validate_component(&selection.project_id).map_err(bad)?;
  match (&selection.experiment_id, &selection.expected_revision) {
    (None, None) => {
      let request_id = selection
        .create_request_id
        .as_deref()
        .ok_or_else(|| ApiError::new(400, "creating an experiment requires create_request_id"))?;
      validate_component(request_id)
        .map_err(|_| ApiError::new(400, "invalid experiment create_request_id"))?;
    }
    (Some(id), Some(revision))
      if !revision.is_empty() && revision.len() <= 64 && selection.create_request_id.is_none() =>
    {
      validate_component(id).map_err(bad)?;
    }
    _ => {
      return Err(ApiError::new(
        400,
        "experiment edits require an ID and revision; create uses null for both",
      ));
    }
  }
  selection.name = text(&selection.name, "name", 128, 512, false)?;
  if selection.name.is_empty() {
    return Err(ApiError::new(400, "experiment name is required"));
  }
  selection.description = text(&selection.description, "description", 2048, 8192, true)?;
  if selection.members.len() > MEMBER_LIMIT || selection.lineage.len() > LINEAGE_LIMIT {
    return Err(ApiError::new(
      400,
      "experiment supports at most 256 runs and 512 lineage relationships",
    ));
  }
  let mut members = BTreeSet::new();
  for scope in &selection.members {
    validate_scope(scope).map_err(bad)?;
    if scope.project_id != selection.project_id || !members.insert(key(scope)) {
      return Err(ApiError::new(
        400,
        "experiment members must be distinct runs from this project",
      ));
    }
  }
  let mut pairs = BTreeSet::new();
  let mut children = BTreeMap::<_, Vec<_>>::new();
  let mut degrees: BTreeMap<_, usize> = members.iter().map(|scope| (*scope, 0)).collect();
  for edge in &mut selection.lineage {
    validate_scope(&edge.parent).map_err(bad)?;
    validate_scope(&edge.child).map_err(bad)?;
    let parent = key(&edge.parent);
    let child = key(&edge.child);
    if edge.parent.project_id != selection.project_id
      || edge.child.project_id != selection.project_id
      || !members.contains(&parent)
      || !members.contains(&child)
      || parent == child
      || !pairs.insert((parent, child))
    {
      return Err(ApiError::new(
        400,
        "lineage requires distinct parent and child members, with one relationship per pair",
      ));
    }
    if let Some(checkpoint) = &edge.checkpoint {
      let checkpoint = text(checkpoint, "checkpoint reference", 1024, 4096, false)?;
      if checkpoint.is_empty() {
        return Err(ApiError::new(400, "checkpoint reference must not be empty"));
      }
      edge.checkpoint = Some(checkpoint);
    }
    if let Some(step) = &edge.step {
      if step.is_empty() || !step.bytes().all(|byte| byte.is_ascii_digit()) || step.len() > 20 {
        return Err(ApiError::new(400, "lineage step must be decimal u64 text"));
      }
      edge.step = Some(
        step
          .parse::<u64>()
          .map_err(|_| ApiError::new(400, "lineage step exceeds u64"))?
          .to_string(),
      );
    }
    children.entry(parent).or_default().push(child);
    *degrees.get_mut(&child).expect("validated member") += 1;
  }
  let mut ready: Vec<_> = degrees
    .iter()
    .filter_map(|(scope, degree)| (*degree == 0).then_some(*scope))
    .collect();
  let mut visited = 0;
  while let Some(parent) = ready.pop() {
    visited += 1;
    for child in children.get(&parent).into_iter().flatten() {
      let degree = degrees.get_mut(child).expect("validated child");
      *degree -= 1;
      if *degree == 0 {
        ready.push(*child);
      }
    }
  }
  if visited != members.len() {
    return Err(ApiError::new(
      400,
      "experiment lineage must not contain a cycle",
    ));
  }
  Ok(selection)
}

fn load(db: &Connection, project_id: &str, experiment_id: &str) -> ApiResult<Option<Experiment>> {
  let record: Option<String> = db
    .query_row(
      "SELECT record FROM experiments WHERE project_id=?1 AND experiment_id=?2",
      params![project_id, experiment_id],
      |row| row.get(0),
    )
    .optional()
    .map_err(database)?;
  record
    .map(|record| {
      serde_json::from_str(&record).map_err(|_| ApiError::new(500, "invalid stored experiment"))
    })
    .transpose()
}

fn available(db: &Connection, scope: &RunScope) -> ApiResult<bool> {
  let at = run_retention::now();
  db.query_row(
    "SELECT NOT EXISTS(SELECT 1 FROM project_deletions WHERE project_id=?1)
      AND NOT EXISTS(SELECT 1 FROM run_retention WHERE project_id=?1 AND origin=?2 AND run_id=?3 AND (status<>'archived' OR delete_after<=?4))
      AND EXISTS(SELECT 1 FROM files WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3
      UNION ALL SELECT 1 FROM streams WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3)",
    params![scope.project_id, scope.origin, scope.run_id, at], |row| row.get(0),
  ).map_err(database)
}

fn decorate(db: &Connection, mut experiment: Experiment) -> ApiResult<Experiment> {
  experiment.unavailable_runs.clear();
  for scope in &experiment.members {
    if !available(db, scope)? {
      experiment.unavailable_runs.push(scope.clone());
    }
  }
  experiment.membership_count = experiment.members.len();
  Ok(experiment)
}

fn project_revision(db: &Connection, project_id: &str) -> ApiResult<String> {
  db.query_row(
    "SELECT revision FROM experiment_revisions WHERE project_id=?1",
    [project_id],
    |row| row.get::<_, u64>(0),
  )
  .optional()
  .map_err(database)
  .map(|revision| revision.unwrap_or(0).to_string())
}

fn snapshot(db: &Connection, project_id: &str) -> ApiResult<Value> {
  let mut statement = db
    .prepare("SELECT record FROM experiments WHERE project_id=?1 ORDER BY experiment_id")
    .map_err(database)?;
  let records = statement
    .query_map([project_id], |row| row.get::<_, String>(0))
    .map_err(database)?;
  let mut experiments = Vec::new();
  for record in records {
    let experiment = serde_json::from_str(&record.map_err(database)?)
      .map_err(|_| ApiError::new(500, "invalid stored experiment"))?;
    experiments.push(decorate(db, experiment)?);
  }
  Ok(
    json!({"project_id":project_id,"revision":project_revision(db,project_id)?,"experiments":experiments,
    "limits":{"experiments":EXPERIMENT_LIMIT,"members":MEMBER_LIMIT,"lineage":LINEAGE_LIMIT,"project_bytes":PROJECT_BYTE_LIMIT}}),
  )
}

fn changed(db: &Connection, project_id: &str, scopes: &[RunScope]) -> ApiResult<()> {
  db.execute("INSERT INTO experiment_revisions(project_id,revision) VALUES(?1,1) ON CONFLICT(project_id) DO UPDATE SET revision=revision+1", [project_id]).map_err(database)?;
  db.execute("INSERT INTO project_change_revisions(project_id,revision) VALUES(?1,1) ON CONFLICT(project_id) DO UPDATE SET revision=revision+1", [project_id]).map_err(database)?;
  db.execute(
    "UPDATE dashboard_catalog_revision SET revision=revision+1 WHERE id=1",
    [],
  )
  .map_err(database)?;
  for scope in scopes {
    if available(db, scope)? {
      record_run_activity(db, scope)?;
    }
  }
  Ok(())
}

pub(super) fn revision(db: &Connection, scope: &RunScope) -> ApiResult<Option<String>> {
  db.query_row("SELECT 'experiment:'||m.experiment_id||':'||json_extract(e.record,'$.revision') FROM experiment_members m JOIN experiments e USING(project_id,experiment_id) WHERE m.project_id=?1 AND m.origin=?2 AND m.run_id=?3",
    params![scope.project_id,scope.origin,scope.run_id],|row|row.get(0)).optional().map_err(database)
}

impl<S: ObjectStorage> Store<S> {
  pub(in crate::service) fn experiments(&self, project_id: &str) -> ApiResult<Value> {
    validate_component(project_id).map_err(bad)?;
    let db = self.db()?;
    projects::ensure_active(&db, project_id)?;
    snapshot(&db, project_id)
  }

  pub(in crate::service) fn dashboard_experiment_projects(
    &self,
    limit: usize,
  ) -> ApiResult<Vec<String>> {
    dashboard_page_bounds(limit, 0)?;
    self.db()?.prepare("SELECT DISTINCT project_id FROM experiments e WHERE NOT EXISTS(SELECT 1 FROM project_deletions d WHERE d.project_id=e.project_id) ORDER BY project_id LIMIT ?1")
      .map_err(database)?.query_map([limit], |row|row.get(0)).map_err(database)?.collect::<rusqlite::Result<Vec<_>>>().map_err(database)
  }

  pub(in crate::service) fn experiment_members(
    &self,
    project_id: &str,
    experiment_id: &str,
  ) -> ApiResult<Vec<RunScope>> {
    validate_component(project_id).map_err(bad)?;
    validate_component(experiment_id).map_err(bad)?;
    let db = self.db()?;
    projects::ensure_active(&db, project_id)?;
    let experiment = load(&db, project_id, experiment_id)?
      .ok_or_else(|| ApiError::new(404, "experiment is missing"))?;
    experiment
      .members
      .into_iter()
      .filter_map(|scope| match available(&db, &scope) {
        Ok(true) => Some(Ok(scope)),
        Ok(false) => None,
        Err(error) => Some(Err(error)),
      })
      .collect()
  }

  pub(in crate::service) fn dashboard_run_experiment(&self, scope: &RunScope) -> ApiResult<Value> {
    validate_scope(scope).map_err(bad)?;
    let db = self.db()?;
    // A run row needs only the group label. Avoid decoding a potentially large
    // membership/lineage document again for every visible run.
    let summary: Option<(String, String, String)> = db
      .query_row(
        "SELECT e.experiment_id,json_extract(e.record,'$.name'),json_extract(e.record,'$.revision')
      FROM experiment_members m JOIN experiments e USING(project_id,experiment_id)
      WHERE m.project_id=?1 AND m.origin=?2 AND m.run_id=?3",
        params![scope.project_id, scope.origin, scope.run_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
      )
      .optional()
      .map_err(database)?;
    match summary {
      Some((experiment_id, name, revision)) => {
        Ok(json!({"experiment_id":experiment_id,"name":name,"revision":revision}))
      }
      None => Ok(Value::Null),
    }
  }

  pub(in crate::service) fn save_experiment(
    &self,
    selection: ExperimentSave,
  ) -> ApiResult<Experiment> {
    let selection = normalize(selection)?;
    let gate = self.project_gate(&selection.project_id)?;
    let _operation = match gate.try_read() {
      Ok(operation) => operation,
      Err(TryLockError::WouldBlock) => {
        return Err(ApiError::new(
          409,
          "project operation is busy; retry saving experiment",
        ));
      }
      Err(_) => return Err(ApiError::new(503, "project operation unavailable")),
    };
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    let result = save_normalized(&tx, selection)?;
    tx.commit().map_err(database)?;
    Ok(result)
  }

  pub(in crate::service) fn delete_experiment(
    &self,
    project_id: &str,
    experiment_id: &str,
    expected_revision: &str,
  ) -> ApiResult<Value> {
    validate_component(project_id).map_err(bad)?;
    validate_component(experiment_id).map_err(bad)?;
    let gate = self.project_gate(project_id)?;
    let _operation = match gate.try_read() {
      Ok(operation) => operation,
      Err(TryLockError::WouldBlock) => {
        return Err(ApiError::new(
          409,
          "project operation is busy; retry deleting experiment",
        ));
      }
      Err(_) => return Err(ApiError::new(503, "project operation unavailable")),
    };
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    projects::ensure_active(&tx, project_id)?;
    let experiment = load(&tx, project_id, experiment_id)?
      .ok_or_else(|| ApiError::new(404, "experiment is missing"))?;
    if experiment.revision != expected_revision {
      return Err(ApiError::new(
        409,
        "experiment changed; refresh before deleting",
      ));
    }
    tx.execute(
      "DELETE FROM experiment_members WHERE project_id=?1 AND experiment_id=?2",
      params![project_id, experiment_id],
    )
    .map_err(database)?;
    tx.execute(
      "DELETE FROM experiments WHERE project_id=?1 AND experiment_id=?2",
      params![project_id, experiment_id],
    )
    .map_err(database)?;
    changed(&tx, project_id, &experiment.members)?;
    let revision = project_revision(&tx, project_id)?;
    tx.commit().map_err(database)?;
    Ok(
      json!({"project_id":project_id,"experiment_id":experiment_id,"deleted":true,"revision":revision}),
    )
  }
}

/// Shared transactional save; callers have normalized and validated the selection.
fn save_normalized(db: &Connection, selection: ExperimentSave) -> ApiResult<Experiment> {
  projects::ensure_active(db, &selection.project_id)?;
  let fingerprint = hex_digest(
    &serde_json::to_vec(&selection)
      .map_err(|_| ApiError::new(500, "cannot encode experiment request"))?,
  );
  if let Some(request_id) = &selection.create_request_id {
    let existing: Option<(String,String)> = db.query_row("SELECT experiment_id,fingerprint FROM experiment_creates WHERE project_id=?1 AND create_request_id=?2",params![selection.project_id,request_id],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(database)?;
    if let Some((id, previous)) = existing {
      if previous != fingerprint {
        return Err(ApiError::new(
          409,
          "experiment create_request_id is already assigned to different values",
        ));
      }
      let experiment = load(db, &selection.project_id, &id)?.ok_or_else(|| {
        ApiError::new(
          410,
          "this experiment creation was already completed and the experiment was deleted",
        )
      })?;
      return decorate(db, experiment);
    }
  }
  let old = selection
    .experiment_id
    .as_ref()
    .map(|id| load(db, &selection.project_id, id))
    .transpose()?
    .flatten();
  if selection.experiment_id.is_some() && old.is_none() {
    return Err(ApiError::new(404, "experiment is missing"));
  }
  if old.as_ref().map(|experiment| &experiment.revision) != selection.expected_revision.as_ref() {
    return Err(ApiError::new(
      409,
      "experiment changed; refresh before saving",
    ));
  }
  let id = match &old {
    Some(experiment) => experiment.experiment_id.clone(),
    None => {
      let count: usize = db
        .query_row(
          "SELECT COUNT(*) FROM experiments WHERE project_id=?1",
          [&selection.project_id],
          |row| row.get(0),
        )
        .map_err(database)?;
      if count >= EXPERIMENT_LIMIT {
        return Err(ApiError::new(
          409,
          "project supports at most 128 experiments",
        ));
      }
      db.query_row("SELECT 'exp-'||lower(hex(randomblob(12)))", [], |row| {
        row.get(0)
      })
      .map_err(database)?
    }
  };
  for scope in &selection.members {
    let retained = old
      .as_ref()
      .is_some_and(|experiment| experiment.members.contains(scope));
    if !retained && !available(db, scope)? {
      return Err(ApiError::new(
        404,
        "cannot add a missing or expired run to an experiment",
      ));
    }
    let assigned: Option<String> = db.query_row("SELECT experiment_id FROM experiment_members WHERE project_id=?1 AND origin=?2 AND run_id=?3",params![scope.project_id,scope.origin,scope.run_id],|row|row.get(0)).optional().map_err(database)?;
    if assigned.is_some_and(|assigned| assigned != id) {
      return Err(ApiError::new(
        409,
        "run belongs to another experiment; remove it there before assigning it",
      ));
    }
  }
  let now = chrono::Utc::now().to_rfc3339();
  let mut experiment = Experiment {
    experiment_id: id.clone(),
    name: selection.name,
    description: selection.description,
    revision: old
      .as_ref()
      .map_or(Ok(1), |old| {
        old
          .revision
          .parse::<u64>()
          .ok()
          .and_then(|revision| revision.checked_add(1))
          .ok_or_else(|| ApiError::new(500, "invalid stored experiment revision"))
      })?
      .to_string(),
    members: selection.members,
    lineage: selection.lineage,
    created_at: old
      .as_ref()
      .map_or_else(|| now.clone(), |old| old.created_at.clone()),
    updated_at: now,
    unavailable_runs: Vec::new(),
    membership_count: 0,
  };
  experiment.membership_count = experiment.members.len();
  if let Some(old) = &old
    && old.name == experiment.name
    && old.description == experiment.description
    && old.members == experiment.members
    && old.lineage == experiment.lineage
  {
    return decorate(db, old.clone());
  }
  let encoded = serde_json::to_string(&experiment)
    .map_err(|_| ApiError::new(500, "cannot encode experiment"))?;
  db.execute("INSERT INTO experiments(project_id,experiment_id,record) VALUES(?1,?2,?3) ON CONFLICT(project_id,experiment_id) DO UPDATE SET record=excluded.record",params![selection.project_id,id,encoded]).map_err(database)?;
  if let Some(request_id) = &selection.create_request_id {
    db.execute("INSERT INTO experiment_creates(project_id,create_request_id,experiment_id,fingerprint) VALUES(?1,?2,?3,?4)",params![selection.project_id,request_id,id,fingerprint]).map_err(database)?;
  }
  db.execute(
    "DELETE FROM experiment_members WHERE project_id=?1 AND experiment_id=?2",
    params![selection.project_id, id],
  )
  .map_err(database)?;
  for scope in &experiment.members {
    db.execute(
      "INSERT INTO experiment_members(project_id,origin,run_id,experiment_id) VALUES(?1,?2,?3,?4)",
      params![scope.project_id, scope.origin, scope.run_id, id],
    )
    .map_err(database)?;
  }
  // Reserve enough space for every retained run to become unavailable later.
  let mut budget = snapshot(db, &selection.project_id)?;
  for record in budget["experiments"]
    .as_array_mut()
    .expect("snapshot experiments")
  {
    record["unavailable_runs"] = record["members"].clone();
  }
  if serde_json::to_vec(&budget)
    .map_err(|_| ApiError::new(500, "cannot encode experiments"))?
    .len()
    > PROJECT_BYTE_LIMIT - 128
  {
    return Err(ApiError::new(
      409,
      "project experiment metadata exceeds the 1 MiB limit",
    ));
  }
  let mut scopes = old.map_or_else(Vec::new, |old| old.members);
  for scope in &experiment.members {
    if !scopes.contains(scope) {
      scopes.push(scope.clone());
    }
  }
  changed(db, &selection.project_id, &scopes)?;
  let result = decorate(db, experiment)?;
  Ok(result)
}

fn hex_digest(bytes: &[u8]) -> String {
  Sha256::digest(bytes)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

#[cfg(test)]
mod tests;
