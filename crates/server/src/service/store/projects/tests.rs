use std::sync::{Condvar, Mutex};

use super::*;
use crate::service::storage::ObjectMetadata;
use crate::service::store::tests::{MockStorage, scope};

#[derive(Default)]
struct CleanupState {
  deleted: Vec<String>,
  aborted: Vec<(String, String)>,
  fail_delete: bool,
  attention_key: Option<String>,
  retryable_key: Option<String>,
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
  fn delete_object(&self, key: &str) -> CleanupResult<()> {
    let mut state = self.state.0.lock().unwrap();
    if state.attention_key.as_deref() == Some(key) {
      return Err(CleanupError::needs_attention(
        "Object cleanup permission denied; grant the service permission to delete stored objects.",
      ));
    }
    if state.fail_delete || state.retryable_key.as_deref() == Some(key) {
      return Err(CleanupError::retryable(
        "Object cleanup connection failed; check storage connectivity. Automatic retries continue.",
      ));
    }
    state.deleted.push(key.into());
    Ok(())
  }
  fn abort_upload(&self, key: &str, id: &str) -> CleanupResult<()> {
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
  assert_eq!(
    (shared.s3_object_count, shared.s3_storage_bytes),
    (Some(1), Some(7))
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
    (selected.s3_object_count, selected.s3_storage_bytes),
    (Some(3), Some(21)),
    "retained objects consume storage, references and pending declared bytes do not"
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
fn project_local_storage_counts_tracking_versions_legacy_streams_and_registered_archives() {
  use std::os::unix::fs::symlink;
  let directory = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), CleanupStorage::default()).unwrap();
  for (revision, total_size, bytes) in [(1, 2, b"{}".as_slice()), (2, 6, b"{\"a".as_slice())] {
    store
      .execute(Request::PutDocument {
        scope: scope(),
        path: "run-state.json".into(),
        revision,
        offset: 0,
        total_size,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
      })
      .unwrap();
  }
  for request in [
    Request::AppendTracking {
      scope: scope(),
      path: "outputs/metrics.jsonl".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"{}\n"),
    },
    Request::AppendStream {
      scope: scope(),
      path: "logs/stdout.log".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"old\n"),
    },
  ] {
    store.execute(request).unwrap();
  }
  let mut foreign = scope();
  foreign.project_id = "foreign".into();
  for (id, owner, status, contents) in [
    (1, scope(), "uploading", b"stage".as_slice()),
    (2, scope(), "archived", b"archive".as_slice()),
    (3, foreign, "archived", b"foreign archive".as_slice()),
  ] {
    store
      .db()
      .unwrap()
      .execute(
        "INSERT INTO result_archives(id,scope,snapshot,status) VALUES(?1,?2,'{}',?3)",
        params![id, serde_json::to_string(&owner).unwrap(), status],
      )
      .unwrap();
    let archive = directory.path().join("archives").join(id.to_string());
    fs::create_dir_all(&archive).unwrap();
    fs::write(archive.join("result.zip"), contents).unwrap();
  }
  // Queued archives have no local payload yet; links and unregistered leftovers are not owned bytes.
  for id in [4, 5] {
    store
      .db()
      .unwrap()
      .execute(
        "INSERT INTO result_archives(id,scope,snapshot) VALUES(?1,?2,'{}')",
        params![id, serde_json::to_string(&scope()).unwrap()],
      )
      .unwrap();
  }
  fs::create_dir_all(directory.path().join("archives/4")).unwrap();
  fs::write(outside.path().join("private.zip"), b"private contents").unwrap();
  symlink(
    outside.path().join("private.zip"),
    directory.path().join("archives/4/result.zip"),
  )
  .unwrap();
  fs::create_dir_all(directory.path().join("archives/999")).unwrap();
  fs::write(directory.path().join("archives/999/result.zip"), b"orphan").unwrap();
  let stats = store.project_storage("project").unwrap();
  assert_eq!(stats.tracking_bytes, 12);
  assert_eq!(stats.local_archive_bytes, Some(12));
  assert_eq!(stats.local_storage_bytes, Some(24));
  assert_eq!(
    (stats.s3_object_count, stats.s3_storage_bytes),
    (Some(0), Some(0))
  );
  assert_eq!(
    store
      .project_storage("foreign")
      .unwrap()
      .local_storage_bytes,
    Some(15)
  );
  assert_eq!(
    store
      .preview_project_delete("project")
      .unwrap()
      .stats
      .local_storage_bytes,
    Some(24)
  );
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
  let failed = store.project_deletion("project").unwrap();
  assert_eq!(failed.status, "pending");
  assert_eq!(
    failed.last_error.as_deref(),
    Some(
      "Object cleanup connection failed; check storage connectivity. Automatic retries continue."
    )
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
  assert!(
    reopened
      .project_deletion("project")
      .unwrap()
      .last_error
      .is_none()
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

#[test]
fn cleanup_preserves_blocked_tasks_when_other_tasks_fail_or_succeed_and_recovers_after_restart() {
  let directory = tempfile::tempdir().unwrap();
  let backend = CleanupStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  for id in ["first", "second", "third"] {
    publish(&store, &backend, input("project", id), id);
  }
  let preview = store.preview_project_delete("project").unwrap();
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  let tasks = store
    .db()
    .unwrap()
    .prepare("SELECT id,value FROM project_cleanup_tasks WHERE kind='object' ORDER BY id")
    .unwrap()
    .query_map([], |row| {
      Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap();
  {
    let mut state = backend.state.0.lock().unwrap();
    state.attention_key = Some(tasks[0].1.clone());
    state.retryable_key = Some(tasks[1].1.clone());
  }
  let before = now();
  let blocked_error = store.project_deletion_cycle().unwrap_err().message;
  let blocked = store.project_deletion("project").unwrap();
  assert_eq!(blocked.status, "needs_attention");
  assert_eq!(blocked.last_error.as_deref(), Some(blocked_error.as_str()));
  let transient_error = store.project_deletion_cycle().unwrap_err().message;
  assert_ne!(transient_error, blocked_error);
  assert_eq!(
    store
      .project_deletion("project")
      .unwrap()
      .last_error
      .as_deref(),
    Some(blocked_error.as_str())
  );
  let failures = store
    .db()
    .unwrap()
    .prepare("SELECT last_error,needs_attention,retry_at FROM project_cleanup_tasks WHERE done=0 AND last_error IS NOT NULL ORDER BY id")
    .unwrap()
    .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?, row.get::<_, i64>(2)?)))
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap();
  assert_eq!(failures.len(), 2);
  assert_eq!((&failures[0].0, failures[0].1), (&blocked_error, true));
  assert_eq!((&failures[1].0, failures[1].1), (&transient_error, false));
  assert!(failures[0].2 >= before + 300 && failures[0].2 <= now() + 300);
  assert!(failures[1].2 >= before + 30 && failures[1].2 <= now() + 30);
  // A successful object and local directory cleanup must retain both unfinished errors.
  drain(&store);
  let waiting = store.project_deletion("project").unwrap();
  assert_eq!(waiting.status, "needs_attention");
  assert_eq!(waiting.pending_tasks, 2);
  assert_eq!(waiting.deleted_objects, 1);
  assert_eq!(waiting.last_error.as_deref(), Some(blocked_error.as_str()));
  drop(store);

  let reopened = Store::open(directory.path(), backend.clone()).unwrap();
  assert_eq!(
    reopened.project_deletion("project").unwrap().status,
    "needs_attention"
  );
  {
    let mut state = backend.state.0.lock().unwrap();
    state.attention_key = None;
    state.retryable_key = None;
  }
  // A recovered transient task must not clear the still-blocked task's message.
  reopened
    .db()
    .unwrap()
    .execute(
      "UPDATE project_cleanup_tasks SET retry_at=0 WHERE id=?1",
      [tasks[1].0],
    )
    .unwrap();
  drain(&reopened);
  let waiting = reopened.project_deletion("project").unwrap();
  assert_eq!(waiting.status, "needs_attention");
  assert_eq!(waiting.pending_tasks, 1);
  assert_eq!(waiting.last_error.as_deref(), Some(blocked_error.as_str()));
  reopened
    .db()
    .unwrap()
    .execute(
      "UPDATE project_cleanup_tasks SET retry_at=0 WHERE id=?1",
      [tasks[0].0],
    )
    .unwrap();
  drain(&reopened);
  let finished = reopened.project_deletion("project").unwrap();
  assert_eq!(finished.status, "deleted");
  assert_eq!(finished.deleted_objects, 3);
  assert_eq!(finished.pending_tasks, 0);
  assert!(finished.last_error.is_none());
  let dirty: i64 = reopened
    .db()
    .unwrap()
    .query_row("SELECT COUNT(*) FROM project_cleanup_tasks WHERE done=0 OR retry_at<>0 OR last_error IS NOT NULL OR needs_attention<>0", [], |row| row.get(0))
    .unwrap();
  assert_eq!(dirty, 0);
}

#[test]
fn cleanup_failure_columns_migrate_existing_schema_three_without_losing_pending_work() {
  let directory = tempfile::tempdir().unwrap();
  let backend = CleanupStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  publish(
    &store,
    &backend,
    input("project", "source"),
    "source-upload",
  );
  let preview = store.preview_project_delete("project").unwrap();
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  backend.state.0.lock().unwrap().fail_delete = true;
  assert_eq!(store.project_deletion_cycle().unwrap_err().status, 502);
  drop(store);

  let db = Connection::open(directory.path().join("metadata.sqlite3")).unwrap();
  db.execute_batch("ALTER TABLE project_cleanup_tasks DROP COLUMN last_error;
    ALTER TABLE project_cleanup_tasks DROP COLUMN needs_attention;
    PRAGMA user_version=3;
    UPDATE project_deletions SET last_error='Cleanup is pending; acknowledged deletion will retry.';")
    .unwrap();
  assert_eq!(
    db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
      .unwrap(),
    3
  );
  drop(db);

  let reopened = Store::open(directory.path(), backend.clone()).unwrap();
  let failed = reopened.project_deletion("project").unwrap();
  assert_eq!(failed.status, "pending");
  assert_eq!(
    failed.last_error.as_deref(),
    Some("Cleanup could not complete; check storage and network. Automatic retries continue.")
  );
  assert_eq!(failed.pending_tasks, 2);
  assert_eq!(
    reopened
      .db()
      .unwrap()
      .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
      .unwrap(),
    4
  );
  backend.state.0.lock().unwrap().fail_delete = false;
  reopened
    .db()
    .unwrap()
    .execute("UPDATE project_cleanup_tasks SET retry_at=0", [])
    .unwrap();
  drain(&reopened);
  let finished = reopened.project_deletion("project").unwrap();
  assert_eq!(finished.status, "deleted");
  assert!(finished.last_error.is_none());
}

#[test]
fn local_cleanup_classifies_operator_problems_without_exposing_paths_or_raw_errors() {
  for code in [
    libc::EACCES,
    libc::EPERM,
    libc::EROFS,
    libc::ENOTDIR,
    libc::ELOOP,
    libc::EINVAL,
    libc::ENAMETOOLONG,
  ] {
    assert!(local_error(std::io::Error::from_raw_os_error(code)).is_needs_attention());
  }
  for code in [
    libc::EIO,
    libc::EMFILE,
    libc::ENFILE,
    libc::ENOSPC,
    libc::EINTR,
    libc::EBUSY,
  ] {
    assert!(!local_error(std::io::Error::from_raw_os_error(code)).is_needs_attention());
  }
  let private_error =
    std::io::Error::other("private /data/user-secret bucket=private credentials=secret");
  assert_eq!(
    local_error(private_error).to_string(),
    "Local cleanup could not complete; check disk availability. Automatic retries continue."
  );
  let directory = tempfile::tempdir().unwrap();
  let error = remove_local_tree(directory.path(), "tracking", "../private").unwrap_err();
  assert!(error.is_needs_attention());
  assert!(!error.to_string().contains("../private"));
  let error = remove_entry(-1, c"private", 65).unwrap_err();
  assert!(error.is_needs_attention());
  assert!(!error.to_string().contains("private"));
}

#[test]
fn local_cleanup_waits_for_an_invalid_parent_to_be_fixed_without_following_it() {
  use std::os::unix::fs::symlink;

  let directory = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  fs::create_dir_all(outside.path().join("project")).unwrap();
  fs::write(outside.path().join("project/keep"), b"private").unwrap();
  let backend = CleanupStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  publish(
    &store,
    &backend,
    input("project", "source"),
    "source-upload",
  );
  let preview = store.preview_project_delete("project").unwrap();
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  symlink(outside.path(), directory.path().join("tracking")).unwrap();
  assert!(store.project_deletion_cycle().unwrap());
  let error = store.project_deletion_cycle().unwrap_err();
  let blocked = store.project_deletion("project").unwrap();
  assert_eq!(blocked.status, "needs_attention");
  assert_eq!(blocked.pending_tasks, 1);
  assert_eq!(blocked.last_error.as_deref(), Some(error.message.as_str()));
  assert!(
    !error
      .message
      .contains(outside.path().to_string_lossy().as_ref())
  );
  assert_eq!(
    fs::read(outside.path().join("project/keep")).unwrap(),
    b"private"
  );
  drop(store);

  fs::remove_file(directory.path().join("tracking")).unwrap();
  fs::create_dir_all(directory.path().join("tracking/project")).unwrap();
  fs::write(directory.path().join("tracking/project/discard"), b"old").unwrap();
  let reopened = Store::open(directory.path(), backend).unwrap();
  reopened
    .db()
    .unwrap()
    .execute(
      "UPDATE project_cleanup_tasks SET retry_at=0 WHERE kind='tracking'",
      [],
    )
    .unwrap();
  drain(&reopened);
  let finished = reopened.project_deletion("project").unwrap();
  assert_eq!(finished.status, "deleted");
  assert!(finished.last_error.is_none());
  assert!(!directory.path().join("tracking/project").exists());
  assert_eq!(
    fs::read(outside.path().join("project/keep")).unwrap(),
    b"private"
  );
}
