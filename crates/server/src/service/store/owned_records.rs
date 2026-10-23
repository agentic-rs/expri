//! Rows owned by stored run/input bytes. Both deletion paths share this inventory.
//! Callers first persist external cleanup work and retain their deletion fences,
//! experiment lineage, receipts, and shared-object references separately.

use super::*;

const TARGET_TABLES: &[&str] = &[
  "chunks",
  "streams",
  "dashboard_overviews",
  "tracking_metric_rows",
  "tracking_metric_summaries",
  "tracking_metric_state",
  "tracking_asset_uses",
  "tracking_system_blocks",
  "tracking_system_packs",
  "tracking_versions",
  "files",
  "uploads",
];

const SCOPE_TABLES: &[&str] = &[
  "tracking_run_seals",
  "tracking_run_epochs",
  "tracking_attempt_history",
  "tracking_attempt_aborts",
  "result_archives",
  "run_control_bindings",
  "run_control_commands",
];

pub(super) enum Owner<'a> {
  Run(&'a RunScope),
  Project(&'a str),
}

pub(super) fn delete(db: &rusqlite::Transaction<'_>, owner: Owner<'_>) -> ApiResult<()> {
  let (target_filter, scope_filter, target_values, scope_values) = match owner {
    Owner::Run(scope) => (
      "json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3",
      "scope=?1",
      vec![
        scope.project_id.clone(),
        scope.origin.clone(),
        scope.run_id.clone(),
      ],
      vec![
        serde_json::to_string(scope).map_err(|_| ApiError::new(500, "cannot encode run scope"))?,
      ],
    ),
    Owner::Project(project_id) => (
      "COALESCE(json_extract(target,'$.scope.project_id'),json_extract(target,'$.project_id'))=?1",
      "json_extract(scope,'$.project_id')=?1",
      vec![project_id.into()],
      vec![project_id.into()],
    ),
  };
  db.execute(
    &format!(
      "DELETE FROM parts WHERE upload_id IN (SELECT upload_id FROM uploads WHERE {target_filter})"
    ),
    rusqlite::params_from_iter(&target_values),
  )
  .map_err(database)?;
  for table in TARGET_TABLES {
    db.execute(
      &format!("DELETE FROM {table} WHERE {target_filter}"),
      rusqlite::params_from_iter(&target_values),
    )
    .map_err(database)?;
  }
  for table in SCOPE_TABLES {
    db.execute(
      &format!("DELETE FROM {table} WHERE {scope_filter}"),
      rusqlite::params_from_iter(&scope_values),
    )
    .map_err(database)?;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::service::store::tests::MockStorage;

  #[test]
  fn both_owner_scopes_remove_projections_and_commands_without_crossing_boundaries() {
    for whole_project in [false, true] {
      let directory = tempfile::tempdir().unwrap();
      let store = Store::open(directory.path(), MockStorage::default()).unwrap();
      let selected = RunScope {
        project_id: "project".into(),
        origin: "worker".into(),
        run_id: "run".into(),
      };
      let scopes = [
        selected.clone(),
        RunScope {
          origin: "other-worker".into(),
          ..selected.clone()
        },
        RunScope {
          run_id: "other-run".into(),
          ..selected.clone()
        },
        RunScope {
          project_id: "other-project".into(),
          ..selected.clone()
        },
      ];
      for scope in &scopes {
        store
          .execute(Request::AppendTracking {
            scope: scope.clone(),
            path: "outputs/metrics.jsonl".into(),
            offset: 0,
            data_base64: base64::engine::general_purpose::STANDARD
              .encode(b"{\"step\":0,\"metrics\":{\"loss\":1}}\n"),
          })
          .unwrap();
        let target = target_json(&FileTarget::Run {
          scope: scope.clone(),
          path: "outputs/system-profile.jsonl".into(),
        })
        .unwrap();
        let encoded = serde_json::to_string(scope).unwrap();
        let db = store.db().unwrap();
        db.execute("INSERT INTO tracking_system_blocks(target,raw_offset,raw_size,packed_offset,packed_size,sha256) VALUES(?1,0,100,0,20,'digest')", [&target]).unwrap();
        db.execute(
          "INSERT INTO tracking_system_packs(target,raw_size,packed_size) VALUES(?1,100,20)",
          [&target],
        )
        .unwrap();
        db.execute("INSERT INTO tracking_attempt_history(scope,request_id,epoch,fingerprint,snapshot) VALUES(?1,'continue',1,'fingerprint','{}')", [&encoded]).unwrap();
        db.execute(
          "INSERT INTO run_control_bindings(scope,facts,last_seen_at_ms) VALUES(?1,'{}',0)",
          [&encoded],
        )
        .unwrap();
        db.execute("INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after,status) VALUES(?1,?2,?3,0,1,'deleted')", params![scope.project_id,scope.origin,scope.run_id]).unwrap();
        db.execute("INSERT INTO experiment_members(project_id,origin,run_id,experiment_id) VALUES(?1,?2,?3,'exp-history')", params![scope.project_id,scope.origin,scope.run_id]).unwrap();
      }
      let mut db = store.db().unwrap();
      let tx = db.transaction().unwrap();
      delete(
        &tx,
        if whole_project {
          Owner::Project(&selected.project_id)
        } else {
          Owner::Run(&selected)
        },
      )
      .unwrap();
      tx.commit().unwrap();
      for scope in &scopes {
        let removed =
          scope == &selected || (whole_project && scope.project_id == selected.project_id);
        let metrics = target_json(&FileTarget::Run {
          scope: scope.clone(),
          path: "outputs/metrics.jsonl".into(),
        })
        .unwrap();
        let system = target_json(&FileTarget::Run {
          scope: scope.clone(),
          path: "outputs/system-profile.jsonl".into(),
        })
        .unwrap();
        for table in [
          "files",
          "tracking_versions",
          "tracking_metric_rows",
          "tracking_metric_summaries",
          "tracking_metric_state",
        ] {
          let count: usize = db
            .query_row(
              &format!("SELECT COUNT(*) FROM {table} WHERE target=?1"),
              [&metrics],
              |row| row.get(0),
            )
            .unwrap();
          assert_eq!(
            count,
            usize::from(!removed),
            "{table}: {scope:?}, project={whole_project}"
          );
        }
        for table in ["tracking_system_blocks", "tracking_system_packs"] {
          let count: usize = db
            .query_row(
              &format!("SELECT COUNT(*) FROM {table} WHERE target=?1"),
              [&system],
              |row| row.get(0),
            )
            .unwrap();
          assert_eq!(count, usize::from(!removed), "{table}: {scope:?}");
        }
        for table in ["tracking_attempt_history", "run_control_bindings"] {
          let count: usize = db
            .query_row(
              &format!("SELECT COUNT(*) FROM {table} WHERE scope=?1"),
              [serde_json::to_string(scope).unwrap()],
              |row| row.get(0),
            )
            .unwrap();
          assert_eq!(count, usize::from(!removed), "{table}: {scope:?}");
        }
      }
      for table in ["run_retention", "experiment_members"] {
        let count: usize = db
          .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
          })
          .unwrap();
        assert_eq!(
          count,
          scopes.len(),
          "{table} is managed by the deletion caller"
        );
      }
    }
  }
}
