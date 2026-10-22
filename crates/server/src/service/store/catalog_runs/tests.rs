use super::*;
use crate::service::store::tests::MockStorage;

fn query() -> RunListQuery {
  RunListQuery {
    project_id: None,
    origin: None,
    archival: RunArchiveFilter::Active,
    limit: 100,
    cursor: None,
  }
}

fn scope(project: &str, origin: &str, run: &str) -> RunScope {
  RunScope {
    project_id: project.into(),
    origin: origin.into(),
    run_id: run.into(),
  }
}

fn seed_stream(db: &Connection, scope: &RunScope) {
  let target = FileTarget::Run {
    scope: scope.clone(),
    path: "logs/stdout.log".into(),
  };
  db.execute(
    "INSERT INTO streams(target,size) VALUES(?1,0)",
    [target_json(&target).unwrap()],
  )
  .unwrap();
  record_run_activity(db, scope).unwrap();
}

fn state<S: ObjectStorage>(store: &Store<S>, scope: &RunScope, value: serde_json::Value) {
  let bytes = serde_json::to_vec(&value).unwrap();
  store
    .execute(Request::PutDocument {
      scope: scope.clone(),
      path: "run-state.json".into(),
      revision: 1,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

#[test]
fn negotiated_catalog_and_dashboard_share_observations_without_changing_legacy_history() {
  use expri_core::run_control::{ControlAction, ControlLifecycle, RunControlRequest};
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let scope = scope("project", "worker", "run-observed");
  let started_at = "2026-10-19T00:00:00Z";
  state(
    &store,
    &scope,
    serde_json::json!({
      "schema_version":1,"run_id":scope.run_id,"task":"train","status":"running",
      "started_at":started_at,"finished_at":null,"exit_code":null,
      "supervisor":{"generation":1},
    }),
  );
  store
    .execute(Request::RunControl {
      request: RunControlRequest::Poll {
        scope: scope.clone(),
        binding_id: "binding".into(),
        attempt: 1,
        generation: 2,
        status: "paused".into(),
        capabilities: vec![ControlAction::Resume, ControlAction::Stop],
        command: vec!["train".into()],
        checkpoints: vec![],
        lifecycle: Some(ControlLifecycle {
          started_at: started_at.into(),
          finished_at: None,
          exit_code: None,
        }),
        requirements: None,
        resource_status: None,
        manual_paused: Some(true),
        experiment_assignment_supported: None,
        result: None,
      },
    })
    .unwrap();
  let legacy = store.browse_runs(&query()).unwrap().runs.remove(0);
  assert_eq!(legacy.status, "running");
  assert!(
    serde_json::to_value(&legacy)
      .unwrap()
      .get("observation")
      .is_none()
  );
  let effective = store.browse_run_summaries(&query()).unwrap().runs.remove(0);
  assert_eq!(effective.status, "paused");
  let observation = effective.observation.unwrap();
  assert_eq!(observation.recorded_status, "running");
  assert!(observation.online);
  assert_eq!(
    observation.available_actions,
    vec![ControlAction::Resume, ControlAction::Stop]
  );
  let mut dashboard =
    serde_json::json!({"status":"running","started_at":started_at,"current_attempt":1});
  store
    .decorate_run_control(&mut dashboard, &scope, 1)
    .unwrap();
  assert_eq!(dashboard["status"], effective.status);
  assert_eq!(
    dashboard["observation"],
    serde_json::to_value(observation).unwrap()
  );
  store
    .db()
    .unwrap()
    .execute("UPDATE run_control_bindings SET last_seen_at_ms=1", [])
    .unwrap();
  let offline = store.browse_run_summaries(&query()).unwrap().runs.remove(0);
  assert_eq!(
    offline.status, "paused",
    "disconnect does not invent an execution outcome"
  );
  assert!(!offline.observation.unwrap().online);
  assert_eq!(
    store.browse_runs(&query()).unwrap().runs[0].status,
    "running"
  );
}

#[test]
fn paged_catalog_covers_more_than_500_runs_and_keeps_scope_collisions_distinct() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let mut expected = Vec::new();
  {
    let mut db = store.db().unwrap();
    let tx = db.transaction().unwrap();
    for id in 0..605 {
      let scope = scope("project", "worker", &format!("run-{id:04}"));
      seed_stream(&tx, &scope);
      expected.push(scope);
    }
    for scope in [
      scope("a-project", "worker", "run-0604"),
      scope("project", "another", "run-0604"),
    ] {
      seed_stream(&tx, &scope);
      expected.push(scope);
    }
    let input = FileRecord {
      target: FileTarget::Input {
        project_id: "input-only".into(),
        input_id: "private-input".into(),
      },
      size: 1,
      sha256: Some("a".repeat(64)),
      storage: FileStorage::Object,
      provenance: None,
    };
    tx.execute(
      "INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,'input',1)",
      params![
        target_json(&input.target).unwrap(),
        serde_json::to_string(&input).unwrap()
      ],
    )
    .unwrap();
    tx.commit().unwrap();
  }
  expected.sort_by(expri_core::service_catalog::compare_run_scopes);
  let mut selected = query();
  selected.limit = 37;
  let mut actual = Vec::new();
  loop {
    let page = store.browse_runs(&selected).unwrap();
    assert!(page.runs.len() <= 37);
    assert!(
      page
        .runs
        .iter()
        .all(|run| run.status == "unknown" && !run.warnings.is_empty())
    );
    actual.extend(page.runs.into_iter().map(|run| run.scope));
    selected.cursor = page.next_cursor;
    if selected.cursor.is_none() {
      break;
    }
  }
  assert_eq!(actual, expected);
  assert_eq!(
    store
      .execute(Request::ListRuns {
        project_id: "project".into(),
        origin: "worker".into()
      })
      .unwrap_err()
      .status,
    413
  );
}

#[test]
fn filters_and_retention_fences_are_applied_before_pagination() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let now = run_retention::now();
  {
    let db = store.db().unwrap();
    for scope in [
      scope("project", "a", "active"),
      scope("project", "b", "archived"),
      scope("project", "a", "expired"),
      scope("project", "a", "cleaning"),
      scope("deleted-project", "a", "deleted"),
      scope("other-project", "a", "other"),
    ] {
      seed_stream(&db, &scope);
    }
    for (id, origin, status, deadline) in [
      ("archived", "b", "archived", now + 3600),
      ("expired", "a", "archived", now - 1),
      ("cleaning", "a", "needs_attention", now - 1),
    ] {
      db.execute("INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after,status) VALUES('project',?1,?2,?3,?4,?5)", params![origin, id, now-100, deadline, status]).unwrap();
    }
    db.execute(
      "INSERT INTO project_deletions(project_id,revision) VALUES('deleted-project','revision')",
      [],
    )
    .unwrap();
  }
  let mut selected = query();
  selected.project_id = Some("project".into());
  let active = store.browse_runs(&selected).unwrap();
  assert_eq!(active.runs.len(), 1);
  assert_eq!(active.runs[0].scope.run_id, "active");
  selected.archival = RunArchiveFilter::Archived;
  let archived = store.browse_runs(&selected).unwrap();
  assert_eq!(archived.runs.len(), 1);
  assert!(archived.runs[0].archived);
  selected.archival = RunArchiveFilter::All;
  assert_eq!(store.browse_runs(&selected).unwrap().runs.len(), 2);
  selected.project_id = None;
  selected.origin = Some("a".into());
  let page = store.browse_runs(&selected).unwrap();
  assert_eq!(page.runs.len(), 2);
  assert!(
    page
      .runs
      .iter()
      .all(|run| run.scope.origin == "a" && run.scope.project_id != "deleted-project")
  );
  selected.project_id = Some("deleted-project".into());
  assert_eq!(store.browse_runs(&selected).unwrap_err().status, 410);
}

#[test]
fn cursor_and_limit_validation_rejects_invalid_or_foreign_positions() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  for limit in [0, 101] {
    let mut selected = query();
    selected.limit = limit;
    assert_eq!(store.browse_runs(&selected).unwrap_err().status, 400);
  }
  let mut selected = query();
  selected.cursor = Some("v2:project:worker:run".into());
  assert_eq!(store.browse_runs(&selected).unwrap_err().status, 400);
  selected.cursor = Some(encode_run_cursor(&scope("other", "worker", "run")).unwrap());
  selected.project_id = Some("project".into());
  assert_eq!(store.browse_runs(&selected).unwrap_err().status, 400);
}

#[test]
fn catalog_queries_seek_scoped_ordering_and_complete_file_scope_indexes() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let db = store.db().unwrap();
  for (project_id, origin, cursor) in [
    (None, None, None),
    (Some("project"), None, None),
    (None, Some("worker"), None),
    (Some("project"), Some("worker"), None),
    (None, None, Some(scope("project", "worker", "run-middle"))),
  ] {
    let selected = RunListQuery {
      project_id: project_id.map(str::to_string),
      origin: origin.map(str::to_string),
      cursor: cursor.as_ref().map(encode_run_cursor).transpose().unwrap(),
      ..query()
    };
    let plan = db
      .prepare(&format!("EXPLAIN QUERY PLAN {}", selection_sql(&selected)))
      .unwrap()
      .query_map(
        params![
          project_id,
          origin,
          false,
          cursor.as_ref().map(|scope| scope.run_id.as_str()),
          cursor.as_ref().map(|scope| scope.project_id.as_str()),
          cursor.as_ref().map(|scope| scope.origin.as_str()),
          run_retention::now(),
          101
        ],
        |row| row.get::<_, String>(3),
      )
      .unwrap()
      .collect::<rusqlite::Result<Vec<_>>>()
      .unwrap();
    for (table, index) in [
      ("f", "dashboard_files_scope"),
      ("s", "dashboard_streams_scope"),
    ] {
      let detail = plan
        .iter()
        .find(|detail| detail.starts_with(&format!("SEARCH {table} ")))
        .unwrap();
      assert!(detail.contains(index), "{detail}");
      assert_eq!(detail.matches("<expr>=?").count(), 4, "{detail}");
    }
    if project_id.is_some() || origin.is_some() || cursor.is_some() {
      assert!(
        plan
          .iter()
          .any(|detail| detail.starts_with("SEARCH scopes ")),
        "{plan:?}"
      );
    }
    assert!(
      !plan.iter().any(|detail| detail.contains("USE TEMP B-TREE")),
      "{plan:?}"
    );
  }
}

#[test]
fn final_page_fence_rejects_lifecycle_changes_without_breaking_cursor_completeness() {
  for change in [
    "archive", "restore", "expire", "cleanup", "missing", "project",
  ] {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let selected_scope = scope("project", "worker", "run-z");
    let was_archived = ["restore", "expire", "cleanup"].contains(&change);
    let at = run_retention::now();
    {
      let db = store.db().unwrap();
      seed_stream(&db, &selected_scope);
      seed_stream(&db, &scope("project", "worker", "run-a"));
      if was_archived {
        db.execute("INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after,status) VALUES('project','worker','run-z',?1,?2,'archived')", params![at, at+3600]).unwrap();
      }
    }
    let selected = RunListQuery {
      archival: RunArchiveFilter::All,
      limit: 1,
      ..query()
    };
    let page = store.browse_runs(&selected).unwrap();
    assert_eq!(page.runs[0].scope, selected_scope);
    assert_eq!(page.runs[0].archived, was_archived);
    assert!(page.next_cursor.is_some());
    store.ensure_page_available(&selected, &page).unwrap();
    {
      let db = store.db().unwrap();
      match change {
        "archive" => {
          db.execute("INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after,status) VALUES('project','worker','run-z',?1,?2,'archived')", params![at, at+3600]).unwrap();
        }
        "restore" => {
          db.execute("DELETE FROM run_retention WHERE run_id='run-z'", [])
            .unwrap();
        }
        "expire" => {
          db.execute(
            "UPDATE run_retention SET delete_after=?1 WHERE run_id='run-z'",
            [at - 1],
          )
          .unwrap();
        }
        "cleanup" => {
          db.execute(
            "UPDATE run_retention SET status='deleting' WHERE run_id='run-z'",
            [],
          )
          .unwrap();
        }
        "missing" => {
          db.execute(
            "DELETE FROM streams WHERE json_extract(target,'$.scope.run_id')='run-z'",
            [],
          )
          .unwrap();
        }
        "project" => {
          db.execute(
            "INSERT INTO project_deletions(project_id,revision) VALUES('project','deleted')",
            [],
          )
          .unwrap();
        }
        _ => unreachable!(),
      }
    }
    let error = store.ensure_page_available(&selected, &page).unwrap_err();
    assert_eq!(error.status, 409, "{change}");
    assert_eq!(
      error.message,
      "run catalog changed during metadata reads; retry the page"
    );
    if change == "project" {
      let explicit = RunListQuery {
        project_id: Some("project".into()),
        ..selected
      };
      assert_eq!(
        store
          .ensure_page_available(&explicit, &page)
          .unwrap_err()
          .status,
        410
      );
    }
  }
}

#[test]
fn published_state_is_normalized_without_exposing_owner_annotations() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let published_scope = scope("project", "worker", "run-state");
  state(
    &store,
    &published_scope,
    serde_json::json!({"schema_version":1,"run_id":published_scope.run_id,
    "task":"train","status":"completed","started_at":"2026-10-12T01:00:00Z",
    "finished_at":"2026-10-12T01:01:00Z","exit_code":0}),
  );
  store
    .set_run_annotations(
      &published_scope,
      Some("private nickname".into()),
      vec!["private tag".into()],
    )
    .unwrap();
  let page = store.browse_runs(&query()).unwrap();
  let run = &page.runs[0];
  assert_eq!(run.task.as_deref(), Some("train"));
  assert_eq!(run.status, "completed");
  assert_eq!(run.exit_code, Some(0));
  assert!(run.warnings.is_empty());
  assert!(!serde_json::to_string(&page).unwrap().contains("private"));

  let bad_scope = scope("project", "worker", "run-bad");
  state(
    &store,
    &bad_scope,
    serde_json::json!({"run_id":"another-run","status":"running"}),
  );
  let bad = store
    .browse_runs(&query())
    .unwrap()
    .runs
    .into_iter()
    .find(|run| run.scope == bad_scope)
    .unwrap();
  assert_eq!(bad.status, "unknown");
  assert!(
    bad
      .warnings
      .iter()
      .any(|warning| warning.contains("does not match"))
  );

  let corrupt_scope = scope("project", "worker", "run-corrupt");
  state(&store, &corrupt_scope, serde_json::json!({}));
  std::fs::write(
    directory
      .path()
      .join("tracking/project/worker/run-corrupt/documents/run-state.json/1"),
    b"{ ",
  )
  .unwrap();
  {
    // Simulate a corrupt retained legacy receipt without a checksum, rather than
    // asking current ingestion to publish an invalid JSON document.
    let db = store.db().unwrap();
    let target = target_json(&FileTarget::Run {
      scope: corrupt_scope.clone(),
      path: "run-state.json".into(),
    })
    .unwrap();
    let raw: String = db
      .query_row(
        "SELECT record FROM files WHERE target=?1",
        [&target],
        |row| row.get(0),
      )
      .unwrap();
    let mut record: FileRecord = serde_json::from_str(&raw).unwrap();
    record.sha256 = None;
    db.execute(
      "UPDATE files SET record=?1 WHERE target=?2",
      params![serde_json::to_string(&record).unwrap(), target],
    )
    .unwrap();
  }
  let corrupt = store
    .browse_runs(&query())
    .unwrap()
    .runs
    .into_iter()
    .find(|run| run.scope == corrupt_scope)
    .unwrap();
  assert_eq!(corrupt.status, "unknown");
  assert!(
    corrupt
      .warnings
      .iter()
      .any(|warning| warning.contains("invalid JSON"))
  );
}

#[test]
fn oversized_or_control_character_fields_are_omitted_without_losing_the_run() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  for (id, task) in [
    ("long-task", "x".repeat(4097)),
    ("control-task", "train\nsecret".into()),
  ] {
    let scope = scope("project", "worker", id);
    state(
      &store,
      &scope,
      serde_json::json!({"run_id":scope.run_id,"task":task,
      "status":"running","started_at":"2026-10-12T01:00:00Z"}),
    );
  }
  let page = store.browse_runs(&query()).unwrap();
  assert_eq!(page.runs.len(), 2);
  assert!(
    page
      .runs
      .iter()
      .all(|run| run.task.is_none() && run.status == "running" && !run.warnings.is_empty())
  );
}

#[derive(Default)]
struct OfflineStorage {
  reads: std::sync::atomic::AtomicUsize,
}

impl ObjectStorage for OfflineStorage {
  fn begin_upload(&self, _: &str, _: &str) -> crate::error::Result<String> {
    panic!("catalog never starts uploads")
  }
  fn presign_part(&self, _: &str, _: &str, _: u32, _: u32) -> crate::error::Result<String> {
    panic!("catalog never signs upload parts")
  }
  fn complete_upload(
    &self,
    _: &str,
    _: &str,
    _: &[CompletedPart],
  ) -> crate::error::Result<crate::service::storage::ObjectMetadata> {
    panic!("catalog never completes uploads")
  }
  fn head(&self, _: &str) -> crate::error::Result<Option<crate::service::storage::ObjectMetadata>> {
    panic!("catalog never reads object sizes")
  }
  fn presign_get(&self, _: &str, _: u32) -> crate::error::Result<String> {
    self
      .reads
      .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Err(crate::error::ExpriError::Message(
      "private-backend-error".into(),
    ))
  }
}

#[test]
fn unavailable_legacy_state_keeps_identity_without_reading_checkpoint_bytes() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), OfflineStorage::default()).unwrap();
  let scope = scope("project", "worker", "legacy");
  {
    let db = store.db().unwrap();
    for (path, size) in [
      ("run-state.json", 100),
      ("outputs/checkpoint.pt", 100_000_000),
    ] {
      let record = FileRecord {
        target: FileTarget::Run {
          scope: scope.clone(),
          path: path.into(),
        },
        size,
        sha256: Some("a".repeat(64)),
        storage: FileStorage::Object,
        provenance: None,
      };
      db.execute(
        "INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,?3,1)",
        params![
          target_json(&record.target).unwrap(),
          serde_json::to_string(&record).unwrap(),
          path
        ],
      )
      .unwrap();
      record_run_activity(&db, &scope).unwrap();
    }
  }
  let page = store.browse_runs(&query()).unwrap();
  assert_eq!(page.runs.len(), 1);
  assert_eq!(page.runs[0].scope, scope);
  assert_eq!(page.runs[0].status, "unknown");
  assert!(!page.runs[0].warnings.is_empty());
  assert!(
    !serde_json::to_string(&page)
      .unwrap()
      .contains("private-backend-error")
  );
  assert_eq!(
    store
      .storage
      .reads
      .load(std::sync::atomic::Ordering::Relaxed),
    1
  );
  let reader = HostedDashboard::new(&store).unwrap();
  assert_eq!(
    reader
      .catalog_document(
        &scope,
        "outputs/checkpoint.pt",
        1024,
        Instant::now() + Duration::from_secs(3)
      )
      .unwrap_err()
      .status,
    400
  );
  assert_eq!(
    store
      .storage
      .reads
      .load(std::sync::atomic::Ordering::Relaxed),
    1
  );
  assert_eq!(
    reader
      .catalog_document(&scope, "run-state.json", 1024, Instant::now())
      .unwrap_err()
      .status,
    504
  );
}

#[test]
fn startup_backfills_legacy_scopes_and_orphaned_activity_does_not_create_runs() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let legacy = scope("project", "worker", "legacy");
  let ghost = scope("project", "worker", "ghost");
  {
    let db = store.db().unwrap();
    seed_stream(&db, &legacy);
    seed_stream(&db, &ghost);
    db.execute(
      "DELETE FROM streams WHERE json_extract(target,'$.scope.run_id')='ghost'",
      [],
    )
    .unwrap();
    db.execute(
      "DELETE FROM dashboard_run_activity WHERE run_id='legacy'",
      [],
    )
    .unwrap();
  }
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let page = store.browse_runs(&query()).unwrap();
  assert_eq!(page.runs.len(), 1);
  assert_eq!(page.runs[0].scope, legacy);
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row(
        "SELECT COUNT(*) FROM dashboard_run_activity WHERE run_id='ghost'",
        [],
        |row| row.get::<_, i64>(0)
      )
      .unwrap(),
    1
  );
}

#[test]
fn live_publication_establishes_catalog_identity_then_state_populates_the_same_entry() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let published_scope = scope("project", "worker", "live");
  store
    .execute(Request::AppendTracking {
      scope: published_scope.clone(),
      path: "logs/stdout.log".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"started\n"),
    })
    .unwrap();
  let initial = store.browse_runs(&query()).unwrap();
  assert_eq!(initial.runs.len(), 1);
  assert_eq!(initial.runs[0].scope, published_scope);
  assert_eq!(initial.runs[0].status, "unknown");
  state(
    &store,
    &published_scope,
    serde_json::json!({"run_id":published_scope.run_id,
    "task":"train","status":"running","started_at":"2026-10-12T01:00:00Z"}),
  );
  let populated = store.browse_runs(&query()).unwrap();
  assert_eq!(populated.runs.len(), 1);
  assert_eq!(populated.runs[0].scope, published_scope);
  assert_eq!(populated.runs[0].status, "running");
  assert_eq!(populated.runs[0].task.as_deref(), Some("train"));
  assert!(populated.runs[0].warnings.is_empty());
}
