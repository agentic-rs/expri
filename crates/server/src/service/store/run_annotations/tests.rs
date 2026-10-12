use super::*;
use crate::service::store::tests::{MockStorage, scope};
use serde_json::{Value, json};

fn document(store: &Store<MockStorage>, scope: &RunScope, revision: u64) {
  let bytes = serde_json::to_vec(&json!({
    "schema_version": 1, "run_id": scope.run_id, "task": "train", "status": "completed",
    "started_at": "2026-10-12T00:00:00Z", "finished_at": "2026-10-12T00:00:01Z", "exit_code": 0,
    "nickname": "worker-owned name", "tags": ["worker-owned tag"],
  }))
  .unwrap();
  store
    .execute(Request::PutDocument {
      scope: scope.clone(),
      path: "run-state.json".into(),
      revision,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

fn annotations(store: &Store<MockStorage>, scope: &RunScope) -> RunAnnotations {
  store
    .set_run_annotations(
      scope,
      Some("  baseline  ".into()),
      vec![" fast ".into(), "fast".into(), "Fast".into()],
    )
    .unwrap()
}

fn updates(store: &Store<MockStorage>, scope: &RunScope) -> Value {
  store
    .dashboard_project_updates(&scope.project_id, &[(scope.run_id.clone(), scope.clone())])
    .unwrap()
}

#[test]
fn normalized_annotations_survive_reopen_worker_publication_and_archive_restore() {
  let directory = tempfile::tempdir().unwrap();
  let backend = MockStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  let selected = scope();
  document(&store, &selected, 1);
  assert_eq!(
    store.run_annotations(&selected).unwrap(),
    RunAnnotations {
      scope: selected.clone(),
      nickname: None,
      tags: Vec::new(),
    }
  );
  let saved = annotations(&store, &selected);
  assert_eq!(saved.nickname.as_deref(), Some("baseline"));
  assert_eq!(saved.tags, ["fast", "Fast"]);
  document(&store, &selected, 2);
  assert_eq!(store.run_annotations(&selected).unwrap(), saved);
  store.archive_run(&selected).unwrap();
  assert_eq!(store.run_annotations(&selected).unwrap(), saved);
  store.restore_run(&selected).unwrap();
  assert_eq!(store.run_annotations(&selected).unwrap(), saved);
  drop(store);
  let reopened = Store::open(directory.path(), backend).unwrap();
  assert_eq!(reopened.run_annotations(&selected).unwrap(), saved);
}

#[test]
fn annotations_are_scoped_to_project_origin_and_run() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let selected = scope();
  let others = [
    RunScope {
      project_id: "other-project".into(),
      ..selected.clone()
    },
    RunScope {
      origin: "other-worker".into(),
      ..selected.clone()
    },
    RunScope {
      run_id: "other-run".into(),
      ..selected.clone()
    },
  ];
  document(&store, &selected, 1);
  for other in &others {
    document(&store, other, 1);
  }
  annotations(&store, &selected);
  for other in &others {
    assert_eq!(store.run_annotations(other).unwrap().nickname, None);
    assert!(store.run_annotations(other).unwrap().tags.is_empty());
  }
}

#[test]
fn edits_advance_refresh_and_project_revisions_only_when_values_change() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let selected = scope();
  document(&store, &selected, 1);
  let before = updates(&store, &selected);
  let preview = store.preview_project_delete(&selected.project_id).unwrap();
  let saved = annotations(&store, &selected);
  let after = updates(&store, &selected);
  for field in ["catalog_revision", "source_revision"] {
    assert_ne!(before[field], after[field], "{field}");
  }
  assert_ne!(
    before["runs"][0]["metadata_revision"],
    after["runs"][0]["metadata_revision"]
  );
  for field in ["metrics_revision", "stdout_revision", "stderr_revision"] {
    assert_eq!(before["runs"][0][field], after["runs"][0][field]);
  }
  assert_ne!(
    preview.revision,
    store
      .preview_project_delete(&selected.project_id)
      .unwrap()
      .revision
  );
  let repeated = annotations(&store, &selected);
  assert_eq!(repeated, saved);
  assert_eq!(after, updates(&store, &selected));
  let cleared = store
    .set_run_annotations(&selected, Some("  ".into()), Vec::new())
    .unwrap();
  assert_eq!(cleared.nickname, None);
  assert!(cleared.tags.is_empty());
  assert_ne!(
    after["runs"][0]["metadata_revision"],
    updates(&store, &selected)["runs"][0]["metadata_revision"]
  );
}

#[test]
fn bounded_validation_rejects_controls_and_never_partially_updates_annotations() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let selected = scope();
  document(&store, &selected, 1);
  let saved = annotations(&store, &selected);
  let before = updates(&store, &selected);
  for (nickname, tags) in [
    (Some("a".repeat(129)), Vec::new()),
    (Some("\tname".into()), Vec::new()),
    (Some("name\n".into()), Vec::new()),
    (None, vec!["tag".into(); 17]),
    (None, vec![" ".into()]),
    (None, vec!["a".repeat(49)]),
    (None, vec!["tag\0".into()]),
    (None, vec!["valid".into(), "invalid\n".into()]),
  ] {
    assert_eq!(
      store
        .set_run_annotations(&selected, nickname, tags)
        .unwrap_err()
        .status,
      400
    );
    assert_eq!(store.run_annotations(&selected).unwrap(), saved);
    assert_eq!(updates(&store, &selected), before);
  }
  let unicode = store
    .set_run_annotations(&selected, Some("😀".repeat(128)), vec!["😀".repeat(48); 16])
    .unwrap();
  assert_eq!(unicode.nickname.unwrap().chars().count(), 128);
  assert_eq!(unicode.tags.len(), 1);
}

#[test]
fn only_existing_available_runs_can_be_annotated_and_project_deletion_removes_rows() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let selected = scope();
  assert_eq!(store.run_annotations(&selected).unwrap_err().status, 404);
  assert_eq!(
    store
      .set_run_annotations(&selected, None, Vec::new())
      .unwrap_err()
      .status,
    404
  );
  let invalid = RunScope {
    run_id: "../run".into(),
    ..selected.clone()
  };
  assert_eq!(
    store
      .set_run_annotations(&invalid, None, Vec::new())
      .unwrap_err()
      .status,
    400
  );
  document(&store, &selected, 1);
  let gate = store.project_gate(&selected.project_id).unwrap();
  let operation = gate.write().unwrap();
  assert_eq!(
    store
      .set_run_annotations(&selected, None, Vec::new())
      .unwrap_err()
      .status,
    409
  );
  drop(operation);
  annotations(&store, &selected);
  let preview = store.preview_project_delete(&selected.project_id).unwrap();
  store
    .delete_project(
      &selected.project_id,
      &preview.revision,
      &selected.project_id,
    )
    .unwrap();
  assert_eq!(store.run_annotations(&selected).unwrap_err().status, 410);
  assert_eq!(
    store
      .set_run_annotations(&selected, None, Vec::new())
      .unwrap_err()
      .status,
    410
  );
  assert_eq!(
    store
      .dashboard_run_annotations(&selected)
      .unwrap_err()
      .status,
    410
  );
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row("SELECT COUNT(*) FROM run_annotations", [], |row| row
        .get::<_, u64>(0))
      .unwrap(),
    0
  );
}

#[test]
fn expired_run_annotations_are_unavailable_and_removed_without_touching_other_scopes() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let selected = scope();
  let other = RunScope {
    origin: "other-worker".into(),
    ..selected.clone()
  };
  for scope in [&selected, &other] {
    document(&store, scope, 1);
    annotations(&store, scope);
  }
  let at = run_retention::now();
  store.db().unwrap().execute(
    "INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after) VALUES(?1,?2,?3,?4,?4)",
    params![selected.project_id, selected.origin, selected.run_id, at - 1],
  ).unwrap();
  assert_eq!(store.run_annotations(&selected).unwrap_err().status, 410);
  assert_eq!(
    store
      .dashboard_run_annotations(&selected)
      .unwrap()
      .nickname
      .as_deref(),
    Some("baseline")
  );
  assert_eq!(
    store
      .set_run_annotations(&selected, None, Vec::new())
      .unwrap_err()
      .status,
    410
  );
  assert!(store.run_retention_cycle().unwrap());
  let displayed = store.dashboard_run_annotations(&selected).unwrap();
  assert_eq!(displayed.nickname, None);
  assert!(displayed.tags.is_empty());
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row("SELECT COUNT(*) FROM run_annotations", [], |row| row
        .get::<_, u64>(0))
      .unwrap(),
    1
  );
  assert_eq!(
    store.run_annotations(&other).unwrap().nickname.as_deref(),
    Some("baseline")
  );
}

#[test]
fn api_dispatch_reads_and_replaces_annotations() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let selected = scope();
  document(&store, &selected, 1);
  let Response::RunAnnotations { annotations: saved } = store
    .execute(Request::SetRunAnnotations {
      scope: selected.clone(),
      nickname: Some("chosen".into()),
      tags: vec!["tag".into()],
    })
    .unwrap()
  else {
    panic!("invalid set response")
  };
  let Response::RunAnnotations { annotations: read } = store
    .execute(Request::RunAnnotations { scope: selected })
    .unwrap()
  else {
    panic!("invalid read response")
  };
  assert_eq!(saved, read);
}

#[test]
fn schema_five_migrates_without_changing_worker_records() {
  let directory = tempfile::tempdir().unwrap();
  let backend = MockStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  let selected = scope();
  document(&store, &selected, 1);
  let original = store
    .dashboard_artifact(&selected, "run-state.json")
    .unwrap()
    .unwrap();
  store
    .db()
    .unwrap()
    .execute_batch("DROP TABLE run_annotations; PRAGMA user_version=5;")
    .unwrap();
  drop(store);
  let reopened = Store::open(directory.path(), backend).unwrap();
  assert_eq!(
    reopened
      .db()
      .unwrap()
      .pragma_query_value(None, "user_version", |row| row.get::<_, u64>(0))
      .unwrap(),
    METADATA_SCHEMA_VERSION as u64
  );
  assert_eq!(
    serde_json::to_value(
      reopened
        .dashboard_artifact(&selected, "run-state.json")
        .unwrap()
        .unwrap()
    )
    .unwrap(),
    serde_json::to_value(original).unwrap()
  );
  assert_eq!(reopened.run_annotations(&selected).unwrap().nickname, None);
  annotations(&reopened, &selected);
}
