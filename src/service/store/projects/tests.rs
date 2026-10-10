use std::sync::{Condvar, Mutex};

use super::*;
use crate::service::storage::ObjectMetadata;
use crate::service::store::tests::{MockStorage, scope};

#[derive(Default)]
struct CleanupState {
  deleted: Vec<String>,
  aborted: Vec<(String, String)>,
  fail_delete: bool,
  block_completion: bool,
  completion_entered: bool,
}

#[derive(Clone, Default)]
struct CleanupStorage {
  backend: MockStorage,
  state: Arc<(Mutex<CleanupState>, Condvar)>,
}

impl ObjectStorage for CleanupStorage {
  fn begin_upload(&self, key: &str, content_type: &str) -> crate::error::Result<String> {
    self.backend.begin_upload(key, content_type)
  }
  fn presign_part(
    &self,
    key: &str,
    id: &str,
    number: u32,
    expires: u32,
  ) -> crate::error::Result<String> {
    self.backend.presign_part(key, id, number, expires)
  }
  fn complete_upload(
    &self,
    key: &str,
    id: &str,
    parts: &[CompletedPart],
  ) -> crate::error::Result<ObjectMetadata> {
    let (state, changed) = &*self.state;
    let mut state = state.lock().unwrap();
    state.completion_entered = true;
    changed.notify_all();
    while state.block_completion {
      state = changed.wait(state).unwrap();
    }
    drop(state);
    self.backend.complete_upload(key, id, parts)
  }
  fn head(&self, key: &str) -> crate::error::Result<Option<ObjectMetadata>> {
    self.backend.head(key)
  }
  fn presign_get(&self, key: &str, expires: u32) -> crate::error::Result<String> {
    self.backend.presign_get(key, expires)
  }
  fn delete_object(&self, key: &str) -> crate::error::Result<()> {
    let mut state = self.state.0.lock().unwrap();
    if state.fail_delete {
      return Err(crate::error::ExpriError::Message("unavailable".into()));
    }
    state.deleted.push(key.into());
    Ok(())
  }
  fn abort_upload(&self, key: &str, id: &str) -> crate::error::Result<()> {
    self
      .state
      .0
      .lock()
      .unwrap()
      .aborted
      .push((key.into(), id.into()));
    Ok(())
  }
}

fn input(project_id: &str, input_id: &str) -> FileTarget {
  FileTarget::Input {
    project_id: project_id.into(),
    input_id: input_id.into(),
  }
}

fn begin(store: &Store<CleanupStorage>, backend: &CleanupStorage, target: FileTarget, id: &str) {
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
  backend.backend.stage(id, 7);
}

fn publish(store: &Store<CleanupStorage>, backend: &CleanupStorage, target: FileTarget, id: &str) {
  begin(store, backend, target, id);
  store
    .execute(Request::CompleteUpload {
      upload_id: id.into(),
    })
    .unwrap();
}

fn drain(store: &Store<CleanupStorage>) {
  for _ in 0..30 {
    if !store.project_deletion_cycle().unwrap() {
      return;
    }
  }
  panic!("cleanup failed to drain");
}

#[test]
fn project_stats_distinguish_shared_references_retained_and_pending_objects() {
  let directory = tempfile::tempdir().unwrap();
  let backend = CleanupStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  let source = input("project", "source");
  publish(&store, &backend, source.clone(), "source-upload");
  let output = FileTarget::Run {
    scope: scope(),
    path: "outputs/checkpoint.pt".into(),
  };
  store
    .execute(Request::ReferenceFile {
      source: source.clone(),
      target: output.clone(),
      size: 7,
      sha256: "a".repeat(64),
    })
    .unwrap();
  let shared = store.project_storage("project").unwrap();
  assert_eq!(
    (
      shared.file_count,
      shared.logical_bytes,
      shared.object_count,
      shared.object_bytes,
      shared.shared_reference_count
    ),
    (2, 14, 1, 7, 1)
  );
  assert_eq!(
    (
      shared.reclaimable_object_count,
      shared.reclaimable_object_bytes
    ),
    (1, 7)
  );
  publish(&store, &backend, output.clone(), "replacement");
  publish(&store, &backend, output, "newest");
  begin(
    &store,
    &backend,
    input("project", "pending"),
    "pending-upload",
  );
  let selected = store.project_storage("project").unwrap();
  assert_eq!((selected.object_count, selected.object_bytes), (2, 14));
  assert_eq!(
    (
      selected.retained_object_count,
      selected.retained_object_bytes
    ),
    (1, 7)
  );
  assert_eq!(
    (selected.pending_upload_count, selected.pending_upload_bytes),
    (1, 7)
  );
  assert_eq!(
    (
      selected.reclaimable_object_count,
      selected.reclaimable_object_bytes
    ),
    (3, 21)
  );
  assert_ne!(selected.revision, shared.revision);
}

#[test]
fn project_delete_rejects_stale_preview_and_preserves_foreign_references() {
  let directory = tempfile::tempdir().unwrap();
  let backend = CleanupStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  publish(
    &store,
    &backend,
    input("project", "source"),
    "source-upload",
  );
  let old = store.preview_project_delete("project").unwrap();
  begin(
    &store,
    &backend,
    input("project", "pending"),
    "pending-upload",
  );
  assert_eq!(
    store
      .delete_project("project", &old.revision, "project")
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .delete_project("project", &old.revision, "wrong")
      .unwrap_err()
      .status,
    400
  );
  let before_foreign = store.preview_project_delete("project").unwrap();
  let foreign = input("foreign", "protected");
  let foreign_record = FileRecord {
    target: foreign.clone(),
    size: 7,
    sha256: Some("a".repeat(64)),
    storage: FileStorage::Object,
  };
  let key = object_key(&input("project", "source"), "source-upload");
  store
    .db()
    .unwrap()
    .execute(
      "INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,?3,1)",
      params![
        target_json(&foreign).unwrap(),
        serde_json::to_string(&foreign_record).unwrap(),
        key
      ],
    )
    .unwrap();
  let preview = store.preview_project_delete("project").unwrap();
  assert_ne!(preview.revision, before_foreign.revision);
  assert_eq!(
    store
      .delete_project("project", &before_foreign.revision, "project")
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(preview.stats.reclaimable_object_count, 0);
  let before = store.dashboard_updates(None, &[]).unwrap()["catalog_revision"].clone();
  let started = store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  assert_eq!(started.status, "pending");
  assert_ne!(
    store.dashboard_updates(None, &[]).unwrap()["catalog_revision"],
    before
  );
  assert!(
    store
      .dashboard_project_sources(10, 0)
      .unwrap()
      .items
      .iter()
      .all(|source| source.project_id != "project")
  );
  drain(&store);
  let finished = store.project_deletion("project").unwrap();
  assert_eq!(finished.status, "deleted");
  assert_eq!(finished.aborted_uploads, 1);
  assert!(!backend.state.0.lock().unwrap().deleted.contains(&key));
  assert_eq!(store.file(&foreign).unwrap().size, 7);
  assert_eq!(
    store
      .execute(Request::BeginUpload {
        upload_id: "resurrect".into(),
        target: input("project", "source"),
        size: 7,
        sha256: "a".repeat(64)
      })
      .unwrap_err()
      .status,
    410
  );
  assert_eq!(
    store
      .delete_project("project", &preview.revision, "project")
      .unwrap()
      .status,
    "deleted"
  );
}

#[test]
fn project_cleanup_retries_after_restart_and_removes_raw_tracking_and_archives_safely() {
  use std::os::unix::fs::symlink;
  let directory = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  fs::write(outside.path().join("keep"), b"private").unwrap();
  let backend = CleanupStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  publish(
    &store,
    &backend,
    input("project", "source"),
    "source-upload",
  );
  store
    .execute(Request::AppendTracking {
      scope: scope(),
      path: "outputs/metrics.jsonl".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"{}\n"),
    })
    .unwrap();
  let stats = store.project_storage("project").unwrap();
  assert_eq!(stats.tracking_bytes, 3);
  store
    .execute(Request::PutDocument {
      scope: scope(),
      path: "run-state.json".into(),
      revision: 1,
      offset: 0,
      total_size: 2,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"{"),
    })
    .unwrap();
  assert_eq!(store.project_storage("project").unwrap().tracking_bytes, 4);
  assert_eq!(
    store.preview_project_delete("project").unwrap().run_count,
    1
  );
  fs::create_dir_all(directory.path().join("archives/42")).unwrap();
  fs::write(directory.path().join("archives/42/result.zip"), b"staged").unwrap();
  store
    .db()
    .unwrap()
    .execute(
      "INSERT INTO result_archives(id,scope,snapshot) VALUES(42,?1,'{}')",
      [serde_json::to_string(&scope()).unwrap()],
    )
    .unwrap();
  symlink(
    outside.path(),
    directory.path().join("tracking/project/linked"),
  )
  .unwrap();
  let preview = store.preview_project_delete("project").unwrap();
  backend.state.0.lock().unwrap().fail_delete = true;
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  assert_eq!(store.project_deletion_cycle().unwrap_err().status, 502);
  assert!(
    store
      .project_deletion("project")
      .unwrap()
      .last_error
      .is_some()
  );
  drop(store);
  backend.state.0.lock().unwrap().fail_delete = false;
  let reopened = Store::open(directory.path(), backend.clone()).unwrap();
  // Advance only the test's retry clock without sleeping.
  reopened
    .db()
    .unwrap()
    .execute("UPDATE project_cleanup_tasks SET retry_at=0", [])
    .unwrap();
  drain(&reopened);
  assert_eq!(
    reopened.project_deletion("project").unwrap().status,
    "deleted"
  );
  assert!(!directory.path().join("tracking/project").exists());
  assert!(!directory.path().join("archives/42").exists());
  assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"private");
  assert_eq!(
    reopened
      .db()
      .unwrap()
      .query_row("SELECT COUNT(*) FROM tracking_versions", [], |row| row
        .get::<_, i64>(0))
      .unwrap(),
    0
  );
  assert_eq!(
    reopened
      .execute(Request::AppendTracking {
        scope: scope(),
        path: "outputs/metrics.jsonl".into(),
        offset: 0,
        data_base64: "".into()
      })
      .unwrap_err()
      .status,
    410
  );
}

#[test]
fn aliased_outputs_and_inputs_delete_one_object_and_empty_projects_cannot_be_deleted() {
  let directory = tempfile::tempdir().unwrap();
  let backend = CleanupStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  assert_eq!(
    store.preview_project_delete("missing").unwrap_err().status,
    404
  );
  let source = input("project", "dataset");
  publish(&store, &backend, source.clone(), "source-upload");
  for target in [
    input("project", "dataset-alias"),
    FileTarget::Run {
      scope: scope(),
      path: "outputs/checkpoint.pt".into(),
    },
  ] {
    store
      .execute(Request::ReferenceFile {
        source: source.clone(),
        target,
        size: 7,
        sha256: "a".repeat(64),
      })
      .unwrap();
  }
  let preview = store.preview_project_delete("project").unwrap();
  assert_eq!(preview.stats.logical_bytes, 21);
  assert_eq!(preview.stats.reclaimable_object_bytes, 7);
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  drain(&store);
  assert_eq!(
    store.project_deletion("project").unwrap().deleted_objects,
    1
  );
  assert_eq!(backend.state.0.lock().unwrap().deleted.len(), 1);
}

#[test]
fn in_flight_completion_fences_deletion_without_blocking_another_project() {
  let directory = tempfile::tempdir().unwrap();
  let backend = CleanupStorage::default();
  let store = Arc::new(Store::open(directory.path(), backend.clone()).unwrap());
  publish(&store, &backend, input("other", "source"), "other-upload");
  begin(
    &store,
    &backend,
    input("project", "source"),
    "source-upload",
  );
  let preview = store.preview_project_delete("project").unwrap();
  backend.state.0.lock().unwrap().block_completion = true;
  backend.state.0.lock().unwrap().completion_entered = false;
  let completing = store.clone();
  let task = std::thread::spawn(move || {
    completing
      .execute(Request::CompleteUpload {
        upload_id: "source-upload".into(),
      })
      .unwrap()
  });
  let (state, changed) = &*backend.state;
  let state = state.lock().unwrap();
  let (state, timed) = changed
    .wait_timeout_while(state, Duration::from_secs(3), |state| {
      !state.completion_entered
    })
    .unwrap();
  assert!(!timed.timed_out());
  drop(state);
  assert_eq!(
    store
      .delete_project("project", &preview.revision, "project")
      .unwrap_err()
      .status,
    409
  );
  let other = store.preview_project_delete("other").unwrap();
  store
    .delete_project("other", &other.revision, "other")
    .unwrap();
  backend.state.0.lock().unwrap().block_completion = false;
  changed.notify_all();
  task.join().unwrap();
  assert_eq!(store.file(&input("project", "source")).unwrap().size, 7);
  let refreshed = store.preview_project_delete("project").unwrap();
  store
    .delete_project("project", &refreshed.revision, "project")
    .unwrap();
  drain(&store);
  assert_eq!(store.project_deletion("project").unwrap().status, "deleted");
}
