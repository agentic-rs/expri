use super::*;
use crate::service::storage::CleanupResult;
use crate::service::storage::ObjectMetadata;
use crate::service::store::tests::{MockStorage, scope};
use serde_json::json;

#[derive(Default)]
struct CleanupState {
  deleted: Vec<String>,
  aborted: Vec<String>,
  fail: bool,
  attention: bool,
}

#[derive(Clone, Default)]
struct Backend {
  objects: MockStorage,
  cleanup: Arc<Mutex<CleanupState>>,
}

impl ObjectStorage for Backend {
  fn begin_upload(&self, key: &str, content_type: &str) -> crate::error::Result<String> {
    self.objects.begin_upload(key, content_type)
  }
  fn presign_part(
    &self,
    key: &str,
    id: &str,
    part: u32,
    expires: u32,
  ) -> crate::error::Result<String> {
    self.objects.presign_part(key, id, part, expires)
  }
  fn complete_upload(
    &self,
    key: &str,
    id: &str,
    parts: &[CompletedPart],
  ) -> crate::error::Result<ObjectMetadata> {
    self.objects.complete_upload(key, id, parts)
  }
  fn head(&self, key: &str) -> crate::error::Result<Option<ObjectMetadata>> {
    self.objects.head(key)
  }
  fn presign_get(&self, key: &str, expires: u32) -> crate::error::Result<String> {
    self.objects.presign_get(key, expires)
  }
  fn delete_object(&self, key: &str) -> CleanupResult<()> {
    let mut state = self.cleanup.lock().unwrap();
    if state.attention {
      return Err(CleanupError::needs_attention(
        "Storage delete permission denied.",
      ));
    }
    if state.fail {
      return Err(CleanupError::retryable("Storage temporarily unavailable."));
    }
    state.deleted.push(key.into());
    Ok(())
  }
  fn abort_upload(&self, _key: &str, id: &str) -> CleanupResult<()> {
    self.cleanup.lock().unwrap().aborted.push(id.into());
    Ok(())
  }
}

fn document(store: &Store<Backend>, scope: &RunScope, status: &str) {
  let bytes = serde_json::to_vec(&json!({"run_id":scope.run_id,"status":status})).unwrap();
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

fn run_file(scope: &RunScope, path: &str) -> FileTarget {
  FileTarget::Run {
    scope: scope.clone(),
    path: path.into(),
  }
}

fn begin(store: &Store<Backend>, target: FileTarget, id: &str) {
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
}

fn publish(store: &Store<Backend>, backend: &Backend, target: FileTarget, id: &str) {
  begin(store, target, id);
  backend.objects.stage(id, 7);
  store
    .execute(Request::CompleteUpload {
      upload_id: id.into(),
    })
    .unwrap();
}

fn drain(store: &Store<Backend>, at: i64) {
  for _ in 0..30 {
    if !store.run_retention_cycle_at(at).unwrap() {
      return;
    }
  }
  panic!("run cleanup did not drain");
}

#[test]
fn only_authoritative_finished_states_can_be_archived() {
  for status in [
    "preparing",
    "running",
    "completed",
    "failed",
    "cancelled",
    "lost",
  ] {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), Backend::default()).unwrap();
    document(&store, &scope(), status);
    let result = store.archive_run_at(&scope(), now());
    assert_eq!(
      result.is_ok(),
      matches!(status, "completed" | "failed" | "cancelled" | "lost"),
      "{status}"
    );
  }
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), Backend::default()).unwrap();
  store
    .dashboard_cache_overview(&scope(), "object:1", &json!({"status":"completed"}))
    .unwrap();
  assert_eq!(
    store.archive_run(&scope()).unwrap_err().status,
    409,
    "a disposable overview cannot authorize archival"
  );
}

#[test]
fn archive_deadline_is_idempotent_restore_boundary_is_strict_and_persisted() {
  let dir = tempfile::tempdir().unwrap();
  let backend = Backend::default();
  let store = Store::open(dir.path(), backend.clone()).unwrap();
  document(&store, &scope(), "completed");
  let start = now();
  let archived = store.archive_run_at(&scope(), start).unwrap();
  assert_eq!(archived.status, "archived");
  assert_eq!(
    archived.delete_after.as_deref(),
    Some(timestamp(start + RETENTION_SECONDS).unwrap().as_str())
  );
  let repeated = store.archive_run_at(&scope(), start + 60).unwrap();
  assert_eq!(archived.archived_at, repeated.archived_at);
  assert_eq!(archived.delete_after, repeated.delete_after);
  drop(store);
  let store = Store::open(dir.path(), backend).unwrap();
  assert_eq!(
    store.run_archival(&scope()).unwrap().delete_after,
    archived.delete_after
  );
  assert_eq!(
    store
      .restore_run_at(&scope(), start + RETENTION_SECONDS)
      .unwrap_err()
      .status,
    410
  );
  assert_eq!(
    store
      .archive_run_at(&scope(), start + RETENTION_SECONDS)
      .unwrap_err()
      .status,
    410
  );
  assert!(
    ensure_available(
      &store.db().unwrap(),
      &scope(),
      start + RETENTION_SECONDS - 1
    )
    .is_ok()
  );
  assert_eq!(
    ensure_available(&store.db().unwrap(), &scope(), start + RETENTION_SECONDS)
      .unwrap_err()
      .status,
    410
  );
  assert_eq!(
    store
      .restore_run_at(&scope(), start + RETENTION_SECONDS - 1)
      .unwrap()
      .status,
    "active"
  );
  assert_eq!(
    store
      .restore_run_at(&scope(), start + RETENTION_SECONDS - 1)
      .unwrap()
      .status,
    "active"
  );
}

#[test]
fn archival_filter_counts_before_paging_and_refreshes_selected_metadata() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), Backend::default()).unwrap();
  let first = scope();
  let second = RunScope {
    run_id: "second".into(),
    ..first.clone()
  };
  document(&store, &first, "completed");
  document(&store, &second, "completed");
  let before = store
    .dashboard_project_updates("project", &[(first.run_id.clone(), first.clone())])
    .unwrap();
  store.archive_run(&first).unwrap();
  let after = store
    .dashboard_project_updates("project", &[(first.run_id.clone(), first.clone())])
    .unwrap();
  assert_ne!(
    before["runs"][0]["metadata_revision"],
    after["runs"][0]["metadata_revision"]
  );
  assert_ne!(before["source_revision"], after["source_revision"]);
  assert_eq!(
    store
      .dashboard_project_runs("project", None, 1, 0)
      .unwrap()
      .total_count,
    2
  );
  let active = store
    .dashboard_project_runs_with_archival("project", None, 1, 0, false)
    .unwrap();
  assert_eq!(active.total_count, 1);
  assert_eq!(active.items, vec![second]);
  let archived = store
    .dashboard_project_runs_with_archival("project", None, 1, 0, true)
    .unwrap();
  assert_eq!(archived.total_count, 1);
  assert_eq!(archived.items, vec![first.clone()]);
  store.restore_run(&first).unwrap();
  assert_eq!(
    store
      .dashboard_project_runs_with_archival("project", None, 1, 0, false)
      .unwrap()
      .total_count,
    2
  );
}

#[test]
fn purge_preserves_shared_private_inputs_other_runs_and_all_their_local_tracking() {
  let dir = tempfile::tempdir().unwrap();
  let backend = Backend::default();
  let store = Store::open(dir.path(), backend.clone()).unwrap();
  let selected = scope();
  let other = RunScope {
    run_id: "other".into(),
    ..selected.clone()
  };
  let other_origin = RunScope {
    origin: "other-worker".into(),
    ..selected.clone()
  };
  for scope in [&selected, &other, &other_origin] {
    document(&store, scope, "completed");
  }
  let owned = run_file(&selected, "outputs/owned.pt");
  publish(&store, &backend, owned.clone(), "old");
  publish(&store, &backend, owned, "new");
  let shared = run_file(&selected, "outputs/shared.pt");
  publish(&store, &backend, shared.clone(), "shared");
  for target in [
    FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset".into(),
    },
    run_file(&other, "outputs/shared.pt"),
  ] {
    store
      .execute(Request::ReferenceFile {
        source: shared.clone(),
        target,
        size: 7,
        sha256: "a".repeat(64),
      })
      .unwrap();
  }
  begin(&store, run_file(&selected, "outputs/pending.pt"), "pending");
  let start = now();
  store.archive_run_at(&selected, start).unwrap();
  let before = store.dashboard_project_updates("project", &[]).unwrap();
  drain(&store, start + RETENTION_SECONDS);
  assert_eq!(store.run_archival(&selected).unwrap().status, "deleted");
  let deleted = backend.cleanup.lock().unwrap();
  assert_eq!(
    deleted.deleted.len(),
    3,
    "old, new, and pending object keys"
  );
  assert!(deleted.deleted.iter().all(|key| !key.ends_with("/shared")));
  assert_eq!(deleted.aborted.len(), 1);
  drop(deleted);
  assert!(store.file(&run_file(&other, "outputs/shared.pt")).is_ok());
  assert!(
    store
      .file(&FileTarget::Input {
        project_id: "project".into(),
        input_id: "dataset".into()
      })
      .is_ok()
  );
  assert!(!dir.path().join("tracking/project/worker/run-1").exists());
  for scope in [&other, &other_origin] {
    assert!(
      dir
        .path()
        .join("tracking")
        .join(&scope.project_id)
        .join(&scope.origin)
        .join(&scope.run_id)
        .exists()
    );
    assert!(store.file(&run_file(scope, "run-state.json")).is_ok());
  }
  let after = store.dashboard_project_updates("project", &[]).unwrap();
  assert_ne!(before["storage_revision"], after["storage_revision"]);
}

#[test]
fn cleanup_failure_retries_after_restart_and_keeps_the_permanent_write_fence() {
  let dir = tempfile::tempdir().unwrap();
  let backend = Backend::default();
  let store = Store::open(dir.path(), backend.clone()).unwrap();
  document(&store, &scope(), "completed");
  publish(
    &store,
    &backend,
    run_file(&scope(), "outputs/model.pt"),
    "model",
  );
  let start = now();
  store.archive_run_at(&scope(), start).unwrap();
  let due = start + RETENTION_SECONDS;
  assert!(store.run_retention_cycle_at(due).unwrap());
  backend.cleanup.lock().unwrap().fail = true;
  assert_eq!(store.run_retention_cycle_at(due).unwrap_err().status, 502);
  assert_eq!(store.run_archival(&scope()).unwrap().status, "deleting");
  assert!(store.run_archival(&scope()).unwrap().last_error.is_some());
  drop(store);
  let store = Store::open(dir.path(), backend.clone()).unwrap();
  assert_eq!(
    store
      .execute(Request::BeginUpload {
        upload_id: "late".into(),
        target: run_file(&scope(), "outputs/model.pt"),
        size: 7,
        sha256: "a".repeat(64)
      })
      .unwrap_err()
      .status,
    410
  );
  assert_eq!(
    store
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
  assert_eq!(
    store
      .dashboard_cache_overview(&scope(), "object:1", &json!({}))
      .unwrap_err()
      .status,
    410
  );
  backend.cleanup.lock().unwrap().fail = false;
  drain(&store, due + 31);
  assert_eq!(store.run_archival(&scope()).unwrap().status, "deleted");
  assert_eq!(store.restore_run(&scope()).unwrap_err().status, 410);
  assert_eq!(
    store
      .file(&run_file(&scope(), "run-state.json"))
      .unwrap_err()
      .status,
    410
  );
}

#[test]
fn expired_references_upload_sessions_and_zip_jobs_cannot_revive_a_run() {
  let dir = tempfile::tempdir().unwrap();
  let backend = Backend::default();
  let store = Store::open(dir.path(), backend.clone()).unwrap();
  document(&store, &scope(), "completed");
  let output = run_file(&scope(), "outputs/model.pt");
  publish(&store, &backend, output.clone(), "model");
  begin(&store, run_file(&scope(), "outputs/pending.pt"), "pending");
  store
    .queue_archive(ArchiveSnapshot {
      scope: scope(),
      files: vec![store.file(&run_file(&scope(), "run-state.json")).unwrap()],
      incomplete: true,
    })
    .unwrap();
  store.archive_run(&scope()).unwrap();
  store
    .db()
    .unwrap()
    .execute("UPDATE run_retention SET delete_after=0", [])
    .unwrap();
  for request in [
    Request::PartUrl {
      upload_id: "pending".into(),
      part_number: 1,
    },
    Request::RecordPart {
      upload_id: "pending".into(),
      part: CompletedPart {
        part_number: 1,
        etag: "part".into(),
      },
    },
    Request::CompleteUpload {
      upload_id: "pending".into(),
    },
    Request::ReferenceFile {
      source: output,
      target: FileTarget::Input {
        project_id: "project".into(),
        input_id: "new".into(),
      },
      size: 7,
      sha256: "a".repeat(64),
    },
  ] {
    assert_eq!(store.execute(request).unwrap_err().status, 410);
  }
  assert!(!store.archive_cycle().unwrap());
  assert_eq!(
    store
      .dashboard_project_runs_with_archival("project", None, 10, 0, true)
      .unwrap()
      .total_count,
    0
  );
  assert!(store.run_retention_cycle().unwrap());
  assert_eq!(
    store
      .execute(Request::SealRun {
        scope: scope(),
        documents: BTreeMap::new(),
        streams: BTreeMap::new(),
        incomplete: false
      })
      .unwrap_err()
      .status,
    410
  );
}

#[test]
fn cleanup_permissions_remain_visible_and_can_recover() {
  let dir = tempfile::tempdir().unwrap();
  let backend = Backend::default();
  let store = Store::open(dir.path(), backend.clone()).unwrap();
  document(&store, &scope(), "completed");
  publish(
    &store,
    &backend,
    run_file(&scope(), "outputs/model.pt"),
    "model",
  );
  let start = now();
  store.archive_run_at(&scope(), start).unwrap();
  let due = start + RETENTION_SECONDS;
  store.run_retention_cycle_at(due).unwrap();
  backend.cleanup.lock().unwrap().attention = true;
  assert_eq!(store.run_retention_cycle_at(due).unwrap_err().status, 502);
  assert_eq!(
    store.run_archival(&scope()).unwrap().status,
    "needs_attention"
  );
  backend.cleanup.lock().unwrap().attention = false;
  drain(&store, due + 301);
  assert_eq!(store.run_archival(&scope()).unwrap().status, "deleted");
}

#[test]
fn run_cleanup_does_not_follow_a_replaced_tracking_parent() {
  let dir = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  fs::write(outside.path().join("keep"), b"private").unwrap();
  fs::create_dir_all(dir.path().join("tracking/project")).unwrap();
  std::os::unix::fs::symlink(outside.path(), dir.path().join("tracking/project/worker")).unwrap();
  let error = projects::remove_local_run_tree(dir.path(), &scope()).unwrap_err();
  assert!(error.is_needs_attention());
  assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"private");
}

#[test]
fn partial_new_state_cannot_be_archived_using_an_older_terminal_version() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), Backend::default()).unwrap();
  document(&store, &scope(), "completed");
  store
    .execute(Request::PutDocument {
      scope: scope(),
      path: "run-state.json".into(),
      revision: 2,
      offset: 0,
      total_size: 20,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"{"),
    })
    .unwrap();
  assert_eq!(store.archive_run(&scope()).unwrap_err().status, 409);
}

#[test]
fn purge_removes_only_the_runs_registered_zip_payloads() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), Backend::default()).unwrap();
  let other = RunScope {
    run_id: "other".into(),
    ..scope()
  };
  for selected in [&scope(), &other] {
    document(&store, selected, "completed");
  }
  for (id, selected) in [(1, scope()), (2, scope()), (3, other)] {
    let path = dir.path().join("archives").join(id.to_string());
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("result.zip"), b"zip payload").unwrap();
    store
      .db()
      .unwrap()
      .execute(
        "INSERT INTO result_archives(id,scope,snapshot,status) VALUES(?1,?2,'{}','archived')",
        params![id, serde_json::to_string(&selected).unwrap()],
      )
      .unwrap();
  }
  let start = now();
  store.archive_run_at(&scope(), start).unwrap();
  drain(&store, start + RETENTION_SECONDS);
  assert!(!dir.path().join("archives/1").exists());
  assert!(!dir.path().join("archives/2").exists());
  assert!(dir.path().join("archives/3/result.zip").is_file());
}

#[test]
fn project_gate_keeps_archival_and_purge_out_of_in_flight_publication() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), Backend::default()).unwrap();
  document(&store, &scope(), "completed");
  let gate = store.project_gate("project").unwrap();
  let publication = gate.read().unwrap();
  assert_eq!(store.archive_run(&scope()).unwrap_err().status, 409);
  drop(publication);
  let start = now();
  store.archive_run_at(&scope(), start).unwrap();
  let publication = gate.read().unwrap();
  assert!(
    !store
      .run_retention_cycle_at(start + RETENTION_SECONDS)
      .unwrap()
  );
  drop(publication);
  assert!(
    store
      .run_retention_cycle_at(start + RETENTION_SECONDS)
      .unwrap()
  );
}

#[test]
fn schema_three_upgrade_adds_retention_without_changing_existing_runs() {
  let dir = tempfile::tempdir().unwrap();
  let backend = Backend::default();
  let store = Store::open(dir.path(), backend.clone()).unwrap();
  document(&store, &scope(), "completed");
  let original = store.file(&run_file(&scope(), "run-state.json")).unwrap();
  drop(store);
  let db = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
  db.execute_batch(
    "DROP TABLE run_cleanup_tasks; DROP TABLE run_retention; PRAGMA user_version=3;",
  )
  .unwrap();
  drop(db);
  let store = Store::open(dir.path(), backend).unwrap();
  assert_eq!(
    store
      .db()
      .unwrap()
      .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
      .unwrap(),
    4
  );
  assert_eq!(
    serde_json::to_value(store.file(&original.target).unwrap()).unwrap(),
    serde_json::to_value(original).unwrap()
  );
  assert_eq!(store.run_archival(&scope()).unwrap().status, "active");
  assert_eq!(store.archive_run(&scope()).unwrap().status, "archived");
}

#[test]
fn project_s3_totals_include_pending_cleanup_without_counting_shared_keys_twice() {
  let dir = tempfile::tempdir().unwrap();
  let backend = Backend::default();
  let store = Store::open(dir.path(), backend.clone()).unwrap();
  document(&store, &scope(), "completed");
  let own = run_file(&scope(), "outputs/own.pt");
  publish(&store, &backend, own.clone(), "old");
  publish(&store, &backend, own, "current");
  let shared = run_file(&scope(), "outputs/shared.pt");
  publish(&store, &backend, shared.clone(), "shared");
  store
    .execute(Request::ReferenceFile {
      source: shared,
      target: FileTarget::Input {
        project_id: "project".into(),
        input_id: "dataset".into(),
      },
      size: 7,
      sha256: "a".repeat(64),
    })
    .unwrap();
  begin(
    &store,
    run_file(&scope(), "outputs/incomplete.pt"),
    "incomplete",
  );
  let initial = store.project_storage("project").unwrap();
  assert_eq!(
    (initial.s3_object_count, initial.s3_storage_bytes),
    (Some(3), Some(21))
  );
  let start = now();
  store.archive_run_at(&scope(), start).unwrap();
  let due = start + RETENTION_SECONDS;
  store.run_retention_cycle_at(due).unwrap();
  let retained = store.project_storage("project").unwrap();
  assert_eq!(
    (retained.s3_object_count, retained.s3_storage_bytes),
    (Some(3), Some(21)),
    "physically retained completed objects are counted, including cleanup retries"
  );
  backend.cleanup.lock().unwrap().fail = true;
  // Multipart abort is independent of object deletion and may be selected first.
  store.run_retention_cycle_at(due).unwrap();
  assert_eq!(store.run_retention_cycle_at(due).unwrap_err().status, 502);
  let retrying = store.project_storage("project").unwrap();
  assert_eq!(
    (retrying.s3_object_count, retrying.s3_storage_bytes),
    (Some(3), Some(21))
  );
  backend.cleanup.lock().unwrap().fail = false;
  drain(&store, due + 31);
  let cleaned = store.project_storage("project").unwrap();
  assert_eq!(
    (cleaned.s3_object_count, cleaned.s3_storage_bytes),
    (Some(1), Some(7)),
    "only the shared private input remains"
  );
}

#[test]
fn project_deletion_adopts_failed_run_cleanup_with_one_retry_owner() {
  for needs_attention in [false, true] {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::default();
    let store = Store::open(dir.path(), backend.clone()).unwrap();
    document(&store, &scope(), "completed");
    publish(
      &store,
      &backend,
      run_file(&scope(), "outputs/own.pt"),
      "own",
    );
    if needs_attention {
      let shared = run_file(&scope(), "outputs/shared.pt");
      publish(&store, &backend, shared.clone(), "shared");
      store
        .execute(Request::ReferenceFile {
          source: shared,
          target: FileTarget::Input {
            project_id: "project".into(),
            input_id: "dataset".into(),
          },
          size: 7,
          sha256: "a".repeat(64),
        })
        .unwrap();
    }
    let zip = dir.path().join("archives/1");
    fs::create_dir_all(&zip).unwrap();
    fs::write(zip.join("result.zip"), b"retained zip").unwrap();
    store
      .db()
      .unwrap()
      .execute(
        "INSERT INTO result_archives(id,scope,snapshot,status) VALUES(1,?1,'{}','archived')",
        [serde_json::to_string(&scope()).unwrap()],
      )
      .unwrap();
    let start = now();
    let local_before = store
      .project_storage("project")
      .unwrap()
      .local_storage_bytes;
    store.archive_run_at(&scope(), start).unwrap();
    let due = start + RETENTION_SECONDS;
    store.run_retention_cycle_at(due).unwrap();
    {
      let mut failures = backend.cleanup.lock().unwrap();
      failures.fail = !needs_attention;
      failures.attention = needs_attention;
    }
    assert_eq!(store.run_retention_cycle_at(due).unwrap_err().status, 502);
    let preview = store.preview_project_delete("project").unwrap();
    assert_eq!(
      preview.stats.local_storage_bytes, local_before,
      "registered raw tracking and ZIP payloads remain counted until physical deletion"
    );
    assert!(
      store
        .dashboard_project_exists_without_runs("project")
        .unwrap()
    );
    if !needs_attention {
      let sources = store.dashboard_project_sources(10, 0).unwrap();
      assert_eq!(sources.total_count, 1);
      assert_eq!(sources.items[0].project_id, "project");
      assert!(
        sources.items[0].origin.is_none(),
        "cleanup-only projects remain discoverable"
      );
    }
    assert_eq!(
      preview.stats.s3_storage_bytes,
      Some(if needs_attention { 14 } else { 7 })
    );
    let deletion = store
      .delete_project("project", &preview.revision, "project")
      .unwrap();
    assert_eq!(
      deletion.status,
      if needs_attention {
        "needs_attention"
      } else {
        "pending"
      }
    );
    assert_eq!(
      deletion.last_error.as_deref(),
      Some(if needs_attention {
        "Storage delete permission denied."
      } else {
        "Storage temporarily unavailable."
      })
    );
    assert!(
      deletion.pending_tasks >= 3,
      "owned object, raw tracking, and retained ZIP cleanup survive logical row deletion"
    );
    assert!(
      !store.run_retention_cycle_at(due + 301).unwrap(),
      "project worker is the only remaining cleanup owner"
    );
    drop(store);
    let store = Store::open(dir.path(), backend.clone()).unwrap();
    assert_eq!(
      store.project_deletion("project").unwrap().status,
      deletion.status
    );
    {
      let mut failures = backend.cleanup.lock().unwrap();
      failures.fail = false;
      failures.attention = false;
    }
    store
      .db()
      .unwrap()
      .execute("UPDATE project_cleanup_tasks SET retry_at=0", [])
      .unwrap();
    for _ in 0..20 {
      if !store.project_deletion_cycle().unwrap() {
        break;
      }
    }
    assert_eq!(store.project_deletion("project").unwrap().status, "deleted");
    assert!(!zip.exists());
    assert!(!dir.path().join("tracking/project").exists());
    let cleanup = backend.cleanup.lock().unwrap();
    assert_eq!(cleanup.deleted.len(), if needs_attention { 2 } else { 1 });
    assert_eq!(
      cleanup
        .deleted
        .iter()
        .filter(|key| key.ends_with("/own"))
        .count(),
      1
    );
    assert_eq!(
      cleanup
        .deleted
        .iter()
        .filter(|key| key.ends_with("/shared"))
        .count(),
      usize::from(needs_attention)
    );
  }
}
