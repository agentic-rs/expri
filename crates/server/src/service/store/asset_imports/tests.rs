use super::*;
use crate::service::store::tests::MockStorage;

fn scope() -> RunScope {
  RunScope {
    project_id: "example-project".into(),
    origin: "worker-1".into(),
    run_id: "run-abc123".into(),
  }
}

fn output(scope: &RunScope) -> FileTarget {
  FileTarget::Run {
    scope: scope.clone(),
    path: "outputs/best.pt".into(),
  }
}

fn publish(store: &Store<MockStorage>, storage: &MockStorage, target: FileTarget, id: &str) {
  store
    .execute(Request::BeginUpload {
      upload_id: id.into(),
      target,
      size: 7,
      sha256: "a".repeat(64),
    })
    .unwrap();
  store
    .execute(Request::RecordPart {
      upload_id: id.into(),
      part: CompletedPart {
        part_number: 1,
        etag: "part".into(),
      },
    })
    .unwrap();
  storage.stage(id, 7);
  store
    .execute(Request::CompleteUpload {
      upload_id: id.into(),
    })
    .unwrap();
}

fn document(store: &Store<MockStorage>, scope: &RunScope, path: &str, revision: u64, value: Value) {
  let bytes = serde_json::to_vec(&value).unwrap();
  store
    .execute(Request::PutDocument {
      scope: scope.clone(),
      path: path.into(),
      revision,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

fn imported(store: &Store<MockStorage>, scope: &RunScope, input_id: Option<&str>) -> FileRecord {
  let Response::File { file } = store
    .execute(Request::ImportOutput {
      scope: scope.clone(),
      path: "outputs/best.pt".into(),
      input_id: input_id.map(str::to_string),
      size: 7,
      sha256: "a".repeat(64),
    })
    .unwrap()
  else {
    panic!("imported file")
  };
  file
}

#[test]
fn imports_share_one_object_capture_safe_provenance_and_survive_restart() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  publish(&store, &storage, output(&scope()), "original");
  document(
    &store,
    &scope(),
    "run-state.json",
    1,
    json!({"task":"example-task","status":"running","command":"secret=hidden https://example.net/private","code_dir":"/private/training","token":"hidden"}),
  );
  document(
    &store,
    &scope(),
    "snapshot.json",
    1,
    json!({"source":{"kind":"git","git_head":"a".repeat(40),"paths":["private.txt"]},"files":[{"secret":"hidden"}]}),
  );
  document(
    &store,
    &scope(),
    "environment/environment-state.json",
    1,
    json!({"lock_sha256":"b".repeat(64),"environment_path":"/private/environment","credentials":"hidden"}),
  );
  let first = imported(&store, &scope(), None);
  assert!(
    matches!(&first.target, FileTarget::Input { input_id, .. } if input_id.starts_with("asset-"))
  );
  let provenance = first.provenance.as_ref().unwrap();
  assert_eq!(provenance.producer.origin, "worker-1");
  assert_eq!(provenance.snapshot["source"]["git_head"], "a".repeat(40));
  assert_eq!(provenance.environment["lock_sha256"], "b".repeat(64));
  let encoded = serde_json::to_string(provenance).unwrap();
  assert!(
    !encoded.contains("hidden")
      && !encoded.contains("https://")
      && !encoded.contains("/private/")
      && !encoded.contains("private.txt")
  );
  let second = imported(&store, &scope(), None);
  assert_eq!(
    serde_json::to_value(&first).unwrap(),
    serde_json::to_value(second).unwrap()
  );
  let url = |target| match store.execute(Request::DownloadUrl { target }).unwrap() {
    Response::Url { url } => url,
    _ => panic!("download URL"),
  };
  assert_eq!(url(output(&scope())), url(first.target.clone()));
  let stats = store.project_storage("example-project").unwrap();
  assert_eq!(stats.object_count, 1);
  assert_eq!(stats.object_bytes, 7);
  drop(store);
  let reopened = Store::open(directory.path(), storage).unwrap();
  assert_eq!(
    serde_json::to_value(&first).unwrap(),
    serde_json::to_value(imported(&reopened, &scope(), None)).unwrap()
  );
}

#[test]
fn producer_identity_and_input_id_are_immutable_even_for_identical_bytes() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  publish(&store, &storage, output(&scope()), "first");
  let first = imported(&store, &scope(), Some("named-release"));
  let other = RunScope {
    run_id: "run-other".into(),
    ..scope()
  };
  publish(&store, &storage, output(&other), "other");
  assert_eq!(
    store
      .execute(Request::ImportOutput {
        scope: other.clone(),
        path: "outputs/best.pt".into(),
        input_id: Some("named-release".into()),
        size: 7,
        sha256: "a".repeat(64)
      })
      .unwrap_err()
      .status,
    409
  );
  let independent = imported(&store, &other, None);
  assert_ne!(independent.target, first.target);
  publish(&store, &storage, first.target.clone(), "redundant");
  let current = store.file(&first.target).unwrap();
  assert_eq!(
    serde_json::to_value(current.provenance).unwrap(),
    serde_json::to_value(first.provenance).unwrap()
  );
}

#[test]
fn imports_reject_stale_identity_missing_objects_and_pending_destinations() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  publish(&store, &storage, output(&scope()), "first");
  for (size, sha256) in [(8, "a".repeat(64)), (7, "b".repeat(64))] {
    assert_eq!(
      store
        .execute(Request::ImportOutput {
          scope: scope(),
          path: "outputs/best.pt".into(),
          input_id: None,
          size,
          sha256
        })
        .unwrap_err()
        .status,
      409
    );
  }
  assert_eq!(
    store
      .execute(Request::ImportOutput {
        scope: scope(),
        path: "outputs/missing.pt".into(),
        input_id: None,
        size: 7,
        sha256: "a".repeat(64)
      })
      .unwrap_err()
      .status,
    404
  );
  assert_eq!(
    store
      .execute(Request::ImportOutput {
        scope: scope(),
        path: "outputs/../secret".into(),
        input_id: None,
        size: 7,
        sha256: "a".repeat(64)
      })
      .unwrap_err()
      .status,
    400
  );
  store
    .execute(Request::BeginUpload {
      upload_id: "pending".into(),
      target: FileTarget::Input {
        project_id: "example-project".into(),
        input_id: "pending".into(),
      },
      size: 7,
      sha256: "a".repeat(64),
    })
    .unwrap();
  assert_eq!(
    store
      .execute(Request::ImportOutput {
        scope: scope(),
        path: "outputs/best.pt".into(),
        input_id: Some("pending".into()),
        size: 7,
        sha256: "a".repeat(64)
      })
      .unwrap_err()
      .status,
    409
  );
}

#[test]
fn imported_inputs_retry_after_the_producing_run_is_deleted() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  publish(&store, &storage, output(&scope()), "first");
  let first = imported(&store, &scope(), None);
  store.db().unwrap().execute("INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after,status) VALUES(?1,?2,?3,0,0,'deleted')", params![scope().project_id,scope().origin,scope().run_id]).unwrap();
  store
    .db()
    .unwrap()
    .execute(
      "DELETE FROM files WHERE json_extract(target,'$.kind')='run'",
      [],
    )
    .unwrap();
  assert_eq!(
    serde_json::to_value(&first).unwrap(),
    serde_json::to_value(imported(&store, &scope(), None)).unwrap()
  );
  let relationships = store.dashboard_asset_relationships(&first).unwrap();
  assert_eq!(relationships["producer"]["run_available"], false);
  assert_eq!(relationships["producer"]["run_id"], "run-abc123");
}

#[test]
fn asset_use_projection_is_identity_and_project_scoped_and_incrementally_replaced() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  publish(&store, &storage, output(&scope()), "first");
  let asset = imported(&store, &scope(), Some("release"));
  let consumer = RunScope {
    run_id: "run-consumer".into(),
    ..scope()
  };
  let usage = json!({"path":"models/base.pt","source":{"kind":"expri","input_id":"release"},"size":7,"sha256":"a".repeat(64)});
  document(
    &store,
    &consumer,
    "run-state.json",
    1,
    json!({"status":"running","assets":[usage.clone()]}),
  );
  let relationships = store.dashboard_asset_relationships(&asset).unwrap();
  assert_eq!(relationships["used_by_count"], 1);
  assert_eq!(relationships["used_by"][0]["path"], "models/base.pt");
  let other_project = RunScope {
    project_id: "other-project".into(),
    ..consumer.clone()
  };
  document(
    &store,
    &other_project,
    "run-state.json",
    1,
    json!({"assets":[usage.clone()]}),
  );
  assert_eq!(
    store.dashboard_asset_relationships(&asset).unwrap()["used_by_count"],
    1
  );
  let revision = store
    .dashboard_project_updates("example-project", &[])
    .unwrap()["storage_revision"]
    .clone();
  document(
    &store,
    &consumer,
    "run-state.json",
    2,
    json!({"status":"completed","assets":[usage]}),
  );
  assert_eq!(
    store
      .dashboard_project_updates("example-project", &[])
      .unwrap()["storage_revision"],
    revision
  );
  document(&store, &consumer, "run-state.json", 3, json!({"assets":[]}));
  assert_eq!(
    store.dashboard_asset_relationships(&asset).unwrap()["used_by_count"],
    0
  );
  assert_ne!(
    store
      .dashboard_project_updates("example-project", &[])
      .unwrap()["storage_revision"],
    revision
  );
  assert_eq!(
    store
      .dashboard_asset_relationships(&store.file(&output(&scope())).unwrap())
      .unwrap()["assets"][0]["input_id"],
    "release"
  );
}

#[test]
fn schema_four_migration_backfills_declared_uses_from_acknowledged_documents() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  publish(&store, &storage, output(&scope()), "first");
  let asset = imported(&store, &scope(), Some("release"));
  let consumer = RunScope {
    run_id: "run-consumer".into(),
    ..scope()
  };
  document(
    &store,
    &consumer,
    "run-state.json",
    1,
    json!({"assets":[{"path":"models/base.pt","source":{"kind":"expri","input_id":"release"},"size":7,"sha256":"a".repeat(64)}]}),
  );
  store
    .db()
    .unwrap()
    .execute_batch("DROP TABLE tracking_asset_uses; PRAGMA user_version=4;")
    .unwrap();
  drop(store);
  let reopened = Store::open(directory.path(), storage).unwrap();
  assert_eq!(
    reopened.dashboard_asset_relationships(&asset).unwrap()["used_by_count"],
    1
  );
  assert_eq!(
    reopened
      .db()
      .unwrap()
      .pragma_query_value(None, "user_version", |row| row.get::<_, u64>(0))
      .unwrap(),
    5
  );
}

#[test]
fn relationship_previews_bound_escaped_paths_as_well_as_entry_counts() {
  let values = (0..16)
    .map(
      |index| json!({"origin":"worker-1","run_id":format!("run-{index}"),"path":"\"".repeat(1024)}),
    )
    .collect();
  let preview = bounded_relationships(values).unwrap();
  assert!(!preview.is_empty());
  assert!(preview.len() < 16);
  assert!(serde_json::to_vec(&preview).unwrap().len() <= RELATIONSHIP_PREVIEW_BYTES);
}

#[test]
fn producer_relationship_queries_use_the_producer_identity_index() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let db = store.db().unwrap();
  let plan = db.prepare("EXPLAIN QUERY PLAN SELECT record FROM files WHERE json_extract(target,'$.kind')='input' AND json_extract(target,'$.project_id')=?1 AND json_extract(record,'$.provenance.producer.origin')=?2 AND json_extract(record,'$.provenance.producer.run_id')=?3 AND json_extract(record,'$.provenance.producer.path')=?4 AND json_extract(record,'$.size')=?5 AND json_extract(record,'$.sha256')=?6 ORDER BY target LIMIT 1").unwrap().query_map(params!["example-project","worker-1","run-abc123","outputs/best.pt",7,"a".repeat(64)], |row| row.get::<_,String>(3)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap().join(" ");
  assert!(plan.contains("files_asset_producer"), "{plan}");
  assert!(!plan.contains("SCAN files"), "{plan}");
}
