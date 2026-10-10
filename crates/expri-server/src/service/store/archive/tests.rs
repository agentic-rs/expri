use super::*;
use crate::service::store::tests::MockStorage;

fn stage_saved_archive_part(
  store: &Store<MockStorage>,
  storage: &MockStorage,
  id: &str,
  path: &Path,
  scope: &RunScope,
) -> String {
  let bytes = fs::read(path).unwrap();
  let sha256 = Sha256::digest(&bytes)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect::<String>();
  store
    .begin(
      id,
      FileTarget::Run {
        scope: scope.clone(),
        path: "result.zip".into(),
      },
      bytes.len() as u64,
      sha256.clone(),
    )
    .unwrap();
  storage.stage(id, bytes.len() as u64);
  store
    .record_part(
      id,
      CompletedPart {
        part_number: 1,
        etag: "saved-fixture-part".into(),
      },
    )
    .unwrap();
  sha256
}

#[test]
fn failed_old_archive_cannot_replace_newer_full_archive_even_with_later_upload_sequence() {
  use base64::Engine;
  let directory = tempfile::tempdir().unwrap();
  let scope = crate::service::store::tests::scope();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  let document = |revision, bytes: &[u8]| Request::PutDocument {
    scope: scope.clone(),
    path: "run-state.json".into(),
    revision,
    offset: 0,
    total_size: bytes.len() as u64,
    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
  };
  let append = |offset, bytes: &[u8]| Request::AppendTracking {
    scope: scope.clone(),
    path: "logs/stdout.log".into(),
    offset,
    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
  };
  let capture = |revision, size, incomplete| Request::SealRun {
    scope: scope.clone(),
    documents: [("run-state.json".into(), revision)].into(),
    streams: [("logs/stdout.log".into(), size)].into(),
    incomplete,
  };
  store
    .execute(document(1, br#"{"run_id":"run-1","status":"running"}"#))
    .unwrap();
  store.execute(append(0, b"old\n")).unwrap();
  store.execute(capture(1, 4, true)).unwrap();
  let old: String = store
    .db()
    .unwrap()
    .query_row(
      "SELECT snapshot FROM result_archives WHERE id=1",
      [],
      |row| row.get(0),
    )
    .unwrap();
  let old: ArchiveSnapshot = serde_json::from_str(&old).unwrap();
  let old_dir = directory.path().join("archives/1");
  private_directory(&directory.path().join("archives")).unwrap();
  private_directory(&old_dir).unwrap();
  let old_zip = old_dir.join("result.zip");
  fs::write(&old_zip, b"interrupted invalid archive").unwrap();
  assert!(store.archive_cycle().is_err());
  assert_eq!(store.archive_status(&scope).unwrap().status, "failed");
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row(
        "SELECT COUNT(*) FROM uploads WHERE upload_id='result-archive-1'",
        [],
        |row| row.get::<_, u64>(0)
      )
      .unwrap(),
    0,
    "old failure must occur before multipart sequence allocation"
  );
  store
    .execute(document(2, br#"{"run_id":"run-1","status":"completed"}"#))
    .unwrap();
  store.execute(append(4, b"new\n")).unwrap();
  store.execute(capture(2, 8, false)).unwrap();
  let latest: String = store
    .db()
    .unwrap()
    .query_row(
      "SELECT snapshot FROM result_archives WHERE id=2",
      [],
      |row| row.get(0),
    )
    .unwrap();
  let latest: ArchiveSnapshot = serde_json::from_str(&latest).unwrap();
  let latest_zip = store.build_archive(2, &latest).unwrap();
  let latest_sha =
    stage_saved_archive_part(&store, &storage, "result-archive-2", &latest_zip, &scope);
  assert!(store.archive_cycle().unwrap());
  let receipt = store.archive_status(&scope).unwrap();
  assert_eq!(receipt.status, "archived");
  assert!(!receipt.incomplete);
  assert_eq!(
    receipt.file.unwrap().sha256.as_deref(),
    Some(latest_sha.as_str())
  );
  store
    .db()
    .unwrap()
    .execute("UPDATE result_archives SET retry_at=0 WHERE id=1", [])
    .unwrap();
  assert!(
    !store.archive_cycle().unwrap(),
    "superseded failed jobs must not be selected for retry"
  );

  // Model a formerly in-flight old worker completing after the latest job.
  // Its multipart sequence is deliberately higher than the full archive's.
  fs::remove_file(&old_zip).unwrap();
  let old_zip = store.build_archive(1, &old).unwrap();
  let old_sha = stage_saved_archive_part(&store, &storage, "result-archive-1", &old_zip, &scope);
  assert_ne!(old_sha, latest_sha);
  let sequences: (u64, u64) = store
    .db()
    .unwrap()
    .query_row(
      "SELECT (SELECT sequence FROM uploads WHERE upload_id='result-archive-1'),
      (SELECT sequence FROM uploads WHERE upload_id='result-archive-2')",
      [],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap();
  assert!(sequences.0 > sequences.1);
  let Response::File { file: obsolete } = store.complete("result-archive-1").unwrap() else {
    panic!("missing completion")
  };
  assert_eq!(obsolete.sha256.as_deref(), Some(old_sha.as_str()));
  let receipt = store.archive_status(&scope).unwrap();
  assert_eq!(receipt.status, "archived");
  assert!(!receipt.incomplete);
  assert_eq!(
    receipt.file.unwrap().sha256.as_deref(),
    Some(latest_sha.as_str())
  );
  drop(store);
  let store = Store::open(directory.path(), storage).unwrap();
  assert!(!store.archive_cycle().unwrap());
  assert_eq!(
    store
      .archive_status(&scope)
      .unwrap()
      .file
      .unwrap()
      .sha256
      .as_deref(),
    Some(latest_sha.as_str())
  );
}

#[test]
fn archive_jobs_are_durable_and_repeated_seals_reuse_the_snapshot() {
  let directory = tempfile::tempdir().unwrap();
  let scope = RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run".into(),
  };
  let snapshot = ArchiveSnapshot {
    scope: scope.clone(),
    incomplete: true,
    files: vec![FileRecord {
      target: FileTarget::Run {
        scope: scope.clone(),
        path: "logs/stdout.log".into(),
      },
      size: 2,
      sha256: None,
      storage: FileStorage::Tracking {
        revision: 1,
        sealed: false,
      },
    }],
  };
  {
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    assert_eq!(
      store.queue_archive(snapshot.clone()).unwrap().status,
      "pending"
    );
    store.queue_archive(snapshot.clone()).unwrap();
    assert_eq!(
      store
        .db()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM result_archives", [], |row| row
          .get::<_, u64>(0))
        .unwrap(),
      1
    );
  }
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert!(store.archive_status(&scope).unwrap().incomplete);
  assert_eq!(store.archive_status(&scope).unwrap().status, "pending");
}
#[test]
fn recovery_archive_keeps_captured_versions_and_resumes_acknowledged_upload() {
  use base64::Engine;
  let directory = tempfile::tempdir().unwrap();
  let scope = crate::service::store::tests::scope();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  let document = |revision, bytes: &[u8]| Request::PutDocument {
    scope: scope.clone(),
    path: "run-state.json".into(),
    revision,
    offset: 0,
    total_size: bytes.len() as u64,
    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
  };
  let append = |offset, bytes: &[u8]| Request::AppendTracking {
    scope: scope.clone(),
    path: "outputs/metrics.jsonl".into(),
    offset,
    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
  };
  let original = br#"{"status":"running","marker":"captured"}"#;
  let metrics = b"{\"step\":0,\"metrics\":{\"loss\":2}}\n";
  store.execute(document(1, original)).unwrap();
  store.execute(append(0, metrics)).unwrap();
  store
    .execute(Request::SealRun {
      scope: scope.clone(),
      documents: [("run-state.json".into(), 1)].into(),
      streams: [("outputs/metrics.jsonl".into(), metrics.len() as u64)].into(),
      incomplete: true,
    })
    .unwrap();
  // A recovery export captures a prefix; it must not stop a surviving worker.
  store
    .execute(append(metrics.len() as u64, b"later bytes\n"))
    .unwrap();
  store
    .execute(document(2, br#"{"status":"running","marker":"later"}"#))
    .unwrap();
  let encoded: String = store
    .db()
    .unwrap()
    .query_row(
      "SELECT snapshot FROM result_archives WHERE id=1",
      [],
      |row| row.get(0),
    )
    .unwrap();
  let snapshot: ArchiveSnapshot = serde_json::from_str(&encoded).unwrap();
  let path = store.build_archive(1, &snapshot).unwrap();
  let mut zip = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
  let mut state = Vec::new();
  zip
    .by_name("run-state.json")
    .unwrap()
    .read_to_end(&mut state)
    .unwrap();
  assert_eq!(state, original);
  let mut raw = Vec::new();
  zip
    .by_name("outputs/metrics.jsonl")
    .unwrap()
    .read_to_end(&mut raw)
    .unwrap();
  assert_eq!(raw, metrics);
  let manifest: serde_json::Value =
    serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
  assert_eq!(manifest["incomplete"], true);
  assert_eq!(manifest["files"].as_array().unwrap().len(), 2);
  assert!(zip.by_name("outputs/checkpoint.pt").is_err());
  drop(zip);
  let size = File::open(&path).unwrap().metadata().unwrap().len();
  let digest = Sha256::digest(fs::read(&path).unwrap());
  let hash = digest.iter().map(|byte| format!("{byte:02x}")).collect();
  store
    .begin(
      "result-archive-1",
      FileTarget::Run {
        scope: scope.clone(),
        path: "result.zip".into(),
      },
      size,
      hash,
    )
    .unwrap();
  storage.stage("result-archive-1", size);
  store
    .record_part(
      "result-archive-1",
      CompletedPart {
        part_number: 1,
        etag: "durable-part".into(),
      },
    )
    .unwrap();
  drop(store);
  let store = Store::open(directory.path(), storage).unwrap();
  assert!(store.archive_cycle().unwrap());
  let receipt = store.archive_status(&scope).unwrap();
  assert_eq!(receipt.status, "archived");
  assert!(receipt.incomplete);
  assert_eq!(receipt.file.unwrap().size, size);
  let current = store
    .file(&FileTarget::Run {
      scope,
      path: "outputs/metrics.jsonl".into(),
    })
    .unwrap();
  assert!(current.size > metrics.len() as u64);
  assert_eq!(
    store.tracking_range(&current, 0, metrics.len()).unwrap(),
    metrics
  );
  assert!(!store.archive_cycle().unwrap());
}
