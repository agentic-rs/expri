use super::super::tests::{MockStorage, scope};
use super::projection::POINT_LIMIT;
use super::raw_io::raw_path;
use super::*;
use crate::metrics::RunMetrics;
use serde_json::json;

mod outage;

fn run_target(path: &str) -> FileTarget {
  FileTarget::Run {
    scope: scope(),
    path: path.into(),
  }
}

fn append<S: ObjectStorage>(
  store: &Store<S>,
  path: &str,
  offset: u64,
  bytes: &[u8],
) -> ApiResult<Response> {
  store.execute(Request::AppendTracking {
    scope: scope(),
    path: path.into(),
    offset,
    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
  })
}

fn document<S: ObjectStorage>(
  store: &Store<S>,
  path: &str,
  revision: u64,
  offset: u64,
  total_size: u64,
  bytes: &[u8],
) -> ApiResult<Response> {
  store.execute(Request::PutDocument {
    scope: scope(),
    path: path.into(),
    revision,
    offset,
    total_size,
    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
  })
}

fn put<S: ObjectStorage>(store: &Store<S>, path: &str, revision: u64, bytes: &[u8]) {
  document(store, path, revision, 0, bytes.len() as u64, bytes).unwrap();
}

fn metrics<S: ObjectStorage>(store: &Store<S>, points: bool) -> RunMetrics {
  let mut result = RunMetrics {
    run_id: scope().run_id,
    run: Value::Null,
    params: None,
    first_metric_timestamp: None,
    metrics: BTreeMap::new(),
    warnings: Vec::new(),
  };
  store
    .tracking_metrics(&scope(), &mut result, &[], points)
    .unwrap();
  result
}

fn seal<S: ObjectStorage>(store: &Store<S>, incomplete: bool) -> ApiResult<Response> {
  let Response::Files { files } = store.list_files(&scope())? else {
    panic!("file catalog")
  };
  let mut documents = BTreeMap::new();
  let mut streams = BTreeMap::new();
  for record in files {
    if let FileStorage::Tracking { revision, .. } = record.storage {
      let FileTarget::Run { path, .. } = record.target else {
        unreachable!()
      };
      if document_path(&path) {
        documents.insert(path, revision);
      } else {
        streams.insert(path, record.size);
      }
    }
  }
  store.execute(Request::SealRun {
    scope: scope(),
    documents,
    streams,
    incomplete,
  })
}

#[test]
fn version_one_migration_preserves_legacy_objects_and_live_streams() {
  let dir = tempfile::tempdir().unwrap();
  let backend = MockStorage::default();
  let store = Store::open(dir.path(), backend.clone()).unwrap();
  let checkpoint = run_target("outputs/model.pt");
  store
    .begin("legacy-model", checkpoint.clone(), 7, "a".repeat(64))
    .unwrap();
  store
    .execute(Request::RecordPart {
      upload_id: "legacy-model".into(),
      part: CompletedPart {
        part_number: 1,
        etag: "original-part".into(),
      },
    })
    .unwrap();
  backend.stage("legacy-model", 7);
  store.complete("legacy-model").unwrap();
  let original_object = serde_json::to_value(store.file(&checkpoint).unwrap()).unwrap();
  store
    .execute(Request::AppendStream {
      scope: scope(),
      path: "logs/stdout.log".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"original\n"),
    })
    .unwrap();
  // Recreate the version-1 schema: neither raw tracking nor archive jobs existed.
  store
    .db()
    .unwrap()
    .execute_batch(
      "DROP TABLE tracking_versions;
       DROP TABLE tracking_run_seals;
       DROP TABLE tracking_metric_state;
       DROP TABLE tracking_metric_rows;
       DROP TABLE tracking_metric_summaries;
       DROP TABLE result_archives;
       PRAGMA user_version=1;",
    )
    .unwrap();
  drop(store);

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
    serde_json::to_value(store.file(&checkpoint).unwrap()).unwrap(),
    original_object
  );
  let Response::Stream {
    offset,
    total_size,
    data_base64,
  } = store
    .execute(Request::ReadStream {
      scope: scope(),
      path: "logs/stdout.log".into(),
      offset: 0,
      limit: 64,
    })
    .unwrap()
  else {
    panic!("legacy stream response");
  };
  assert_eq!((offset, total_size), (0, 9));
  assert_eq!(
    base64::engine::general_purpose::STANDARD
      .decode(data_base64)
      .unwrap(),
    b"original\n"
  );
  let Response::Files { files } = store.list_files(&scope()).unwrap() else {
    panic!("legacy catalog response");
  };
  assert_eq!(files.len(), 2);
  assert!(
    files
      .iter()
      .any(|file| matches!(file.storage, FileStorage::Object))
  );
  assert!(
    files
      .iter()
      .any(|file| matches!(file.storage, FileStorage::Stream))
  );
  put(&store, "run-state.json", 1, br#"{"status":"running"}"#);
  assert!(matches!(
    store.file(&run_target("run-state.json")).unwrap().storage,
    FileStorage::Tracking { revision: 1, .. }
  ));
}

#[test]
fn future_schema_is_rejected_without_creating_tables_or_changing_data() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("metadata.sqlite3");
  let db = Connection::open(&path).unwrap();
  db.execute_batch(
    "CREATE TABLE future_data (value TEXT NOT NULL);
     INSERT INTO future_data VALUES('untouched');
     PRAGMA user_version=5;",
  )
  .unwrap();
  drop(db);
  let error = Store::open(dir.path(), MockStorage::default())
    .err()
    .unwrap();
  assert!(error.to_string().contains("schema is newer"));
  let db = Connection::open(path).unwrap();
  assert_eq!(
    db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
      .unwrap(),
    5
  );
  assert_eq!(
    db.query_row("SELECT value FROM future_data", [], |row| row
      .get::<_, String>(0))
      .unwrap(),
    "untouched"
  );
  assert_eq!(
    db.query_row(
      "SELECT COUNT(*) FROM sqlite_schema WHERE type='table'",
      [],
      |row| row.get::<_, i64>(0)
    )
    .unwrap(),
    1
  );
}

#[test]
fn chunked_documents_acknowledge_durable_prefix_and_keep_old_revisions() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  let raw = br#"{"status":"running","note":"original"}"#;
  assert!(matches!(
    document(&store, "run-state.json", 1, 0, raw.len() as u64, &raw[..10]).unwrap(),
    Response::DocumentAcknowledged {
      offset: 10,
      complete: false,
      ..
    }
  ));
  assert_eq!(
    store
      .file(&run_target("run-state.json"))
      .unwrap_err()
      .status,
    404
  );
  document(
    &store,
    "run-state.json",
    1,
    10,
    raw.len() as u64,
    &raw[10..],
  )
  .unwrap();
  let old = store.file(&run_target("run-state.json")).unwrap();
  assert!(
    matches!(document(&store,"run-state.json",1,0,raw.len() as u64,&raw[..5]).unwrap(),Response::DocumentAcknowledged{offset,complete:true,..} if offset==raw.len() as u64)
  );
  assert_eq!(
    document(&store, "run-state.json", 1, 0, raw.len() as u64, b"WRONG")
      .unwrap_err()
      .status,
    409
  );
  put(&store, "run-state.json", 2, br#"{"status":"completed"}"#);
  assert_eq!(store.tracking_range(&old, 0, raw.len()).unwrap(), raw);
  assert_eq!(
    document(&store, "run-state.json", 1, 0, raw.len() as u64, raw)
      .unwrap_err()
      .status,
    409
  );
  drop(store);
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  assert_eq!(store.tracking_range(&old, 0, raw.len()).unwrap(), raw);
}

#[test]
fn parameter_revisions_follow_existing_immutable_params_contract() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  put(&store, "outputs/params.json", 1, br#"{"seed":42}"#);
  assert_eq!(
    document(&store, "outputs/params.json", 2, 0, 11, br#"{"seed":43}"#)
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    document(&store, "snapshot.json", u64::MAX, 0, 2, b"{}")
      .unwrap_err()
      .status,
    413
  );
  assert_eq!(
    append(&store, "inputs/private", 0, b"x")
      .unwrap_err()
      .status,
    400
  );
}

#[test]
fn ordered_projection_preserves_raw_bytes_u64_steps_timestamps_and_partial_lines() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  let raw = concat!(
    "{\"step\":18446744073709551615,\"timestamp\":\"2026-10-09T00:00:00+08:00\",\"metrics\":{\"loss\":4}}\n",
    "bad JSON\n",
    "{\"step\":0,\"metrics\":{\"loss\":2}}\n",
    "{\"step\":0,\"metrics\":{\"loss\":3}}\n"
  );
  let split = raw.len() - 5;
  append(&store, "outputs/metrics.jsonl", 0, &raw.as_bytes()[..split]).unwrap();
  assert_eq!(metrics(&store, true).metrics["loss"].summary.count, 2);
  append(
    &store,
    "outputs/metrics.jsonl",
    split as u64,
    &raw.as_bytes()[split..],
  )
  .unwrap();
  append(&store, "outputs/metrics.jsonl", 0, raw.as_bytes()).unwrap();
  let result = metrics(&store, true);
  let loss = &result.metrics["loss"];
  assert_eq!(loss.summary.count, 3);
  assert_eq!(loss.summary.last.value, 3.0);
  assert_eq!(loss.summary.min.value, 2.0);
  assert_eq!(
    loss
      .points
      .iter()
      .map(|point| point.step)
      .collect::<Vec<_>>(),
    [u64::MAX, 0, 0]
  );
  assert_eq!(
    result.first_metric_timestamp.as_deref(),
    Some("2026-10-09T00:00:00+08:00")
  );
  assert!(result.warnings.iter().any(|warning| {
    warning["message"]
      .as_str()
      .unwrap()
      .contains("steps decreased")
  }));
  let record = store.file(&run_target("outputs/metrics.jsonl")).unwrap();
  assert_eq!(
    store.tracking_range(&record, 0, raw.len()).unwrap(),
    raw.as_bytes()
  );
  assert_eq!(
    append(&store, "outputs/metrics.jsonl", 0, b"different")
      .unwrap_err()
      .status,
    409
  );
}

#[test]
fn completed_tail_is_indexed_once_and_duplicate_seal_is_revision_stable() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  put(&store, "run-state.json", 1, br#"{"status":"completed"}"#);
  let raw = br#"{"step":1,"metrics":{"loss":7}}"#;
  append(&store, "outputs/metrics.jsonl", 0, raw).unwrap();
  assert!(metrics(&store, true).metrics.is_empty());
  seal(&store, false).unwrap();
  assert_eq!(metrics(&store, true).metrics["loss"].summary.count, 1);
  let revision = store
    .dashboard_updates(
      Some(&DashboardSource {
        project_id: scope().project_id,
        origin: scope().origin,
      }),
      &[scope().run_id],
    )
    .unwrap();
  seal(&store, false).unwrap();
  assert_eq!(metrics(&store, true).metrics["loss"].summary.count, 1);
  assert_eq!(
    revision,
    store
      .dashboard_updates(
        Some(&DashboardSource {
          project_id: scope().project_id,
          origin: scope().origin
        }),
        &[scope().run_id]
      )
      .unwrap()
  );
  assert_eq!(
    append(&store, "outputs/metrics.jsonl", raw.len() as u64, b"\n")
      .unwrap_err()
      .status,
    409
  );
  assert!(append(&store, "outputs/metrics.jsonl", 0, raw).is_ok());
  assert!(matches!(
    store
      .file(&run_target("outputs/metrics.jsonl"))
      .unwrap()
      .storage,
    FileStorage::Tracking { sealed: true, .. }
  ));
}

#[test]
fn partial_archive_keeps_stream_live_and_normal_archive_requires_terminal_state() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  put(&store, "run-state.json", 1, br#"{"status":"running"}"#);
  append(&store, "logs/stdout.log", 0, b"first").unwrap();
  assert_eq!(seal(&store, false).unwrap_err().status, 409);
  assert!(matches!(seal(&store,true).unwrap(),Response::Archive{archive} if archive.incomplete));
  append(&store, "logs/stdout.log", 5, b" second").unwrap();
  assert!(matches!(
    store.file(&run_target("logs/stdout.log")).unwrap().storage,
    FileStorage::Tracking { sealed: false, .. }
  ));
}

#[test]
fn partial_archives_use_captured_old_documents_and_prefixes_during_live_writes() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  put(&store, "run-state.json", 1, br#"{"status":"running"}"#);
  append(&store, "logs/stdout.log", 0, b"first").unwrap();
  put(
    &store,
    "run-state.json",
    2,
    br#"{"status":"running","note":"new"}"#,
  );
  append(&store, "logs/stdout.log", 5, b" second").unwrap();
  store
    .execute(Request::SealRun {
      scope: scope(),
      documents: BTreeMap::from([("run-state.json".into(), 1)]),
      streams: BTreeMap::from([("logs/stdout.log".into(), 5)]),
      incomplete: true,
    })
    .unwrap();
  let raw: String = store
    .db()
    .unwrap()
    .query_row(
      "SELECT snapshot FROM result_archives ORDER BY id DESC LIMIT 1",
      [],
      |row| row.get(0),
    )
    .unwrap();
  let snapshot: ArchiveSnapshot = serde_json::from_str(&raw).unwrap();
  let log = snapshot
    .files
    .iter()
    .find(|record| matches!(&record.target,FileTarget::Run{path,..} if path=="logs/stdout.log"))
    .unwrap();
  assert_eq!(log.size, 5);
  assert_eq!(store.tracking_range(log, 0, 5).unwrap(), b"first");
  let doc = snapshot
    .files
    .iter()
    .find(|record| matches!(&record.target,FileTarget::Run{path,..} if path=="run-state.json"))
    .unwrap();
  assert!(matches!(
    doc.storage,
    FileStorage::Tracking { revision: 1, .. }
  ));
  assert_eq!(
    store.tracking_range(doc, 0, doc.size as usize).unwrap(),
    br#"{"status":"running"}"#
  );
}

#[test]
fn legacy_upload_cannot_replace_accepted_tracking_bytes() {
  let dir = tempfile::tempdir().unwrap();
  let backend = MockStorage::default();
  let store = Store::open(dir.path(), backend).unwrap();
  let target = run_target("run-state.json");
  store
    .begin("old-upload", target.clone(), 2, "a".repeat(64))
    .unwrap();
  put(&store, "run-state.json", 1, b"{}");
  assert_eq!(store.complete("old-upload").unwrap_err().status, 409);
  assert_eq!(
    store
      .begin("new-upload", target.clone(), 2, "a".repeat(64))
      .unwrap_err()
      .status,
    409
  );
  let record = store.file(&target).unwrap();
  assert_eq!(store.tracking_range(&record, 0, 2).unwrap(), b"{}");
}

#[test]
fn raw_symlinks_and_hardlinks_never_modify_external_files() {
  use std::os::unix::fs::symlink;
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  append(&store, "logs/stdout.log", 0, b"safe").unwrap();
  let path = raw_path(&store.directory, &run_target("logs/stdout.log"), 1).unwrap();
  let outside = dir.path().join("outside");
  std::fs::write(&outside, b"do not alter").unwrap();
  std::fs::remove_file(&path).unwrap();
  symlink(&outside, &path).unwrap();
  assert_eq!(
    append(&store, "logs/stdout.log", 4, b"x")
      .unwrap_err()
      .status,
    500
  );
  std::fs::remove_file(&path).unwrap();
  std::fs::hard_link(&outside, &path).unwrap();
  assert_eq!(
    append(&store, "logs/stdout.log", 4, b"x")
      .unwrap_err()
      .status,
    500
  );
  assert_eq!(std::fs::read(outside).unwrap(), b"do not alter");
}

#[test]
fn metric_queries_use_bounded_index_samples_and_complete_summaries() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  let mut raw = String::new();
  for index in 0..5000 {
    raw.push_str(&json!({"step":index,"metrics":{"loss":index}}).to_string());
    raw.push('\n');
  }
  let mut offset = 0;
  for bytes in raw.as_bytes().chunks(STREAM_BATCH) {
    append(&store, "outputs/metrics.jsonl", offset, bytes).unwrap();
    offset += bytes.len() as u64;
  }
  let path = raw_path(&store.directory, &run_target("outputs/metrics.jsonl"), 1).unwrap();
  // Queries must use the durable projection, even when raw reads are unavailable.
  std::fs::rename(&path, path.with_extension("held")).unwrap();
  let result = metrics(&store, true);
  let series = &result.metrics["loss"];
  assert_eq!(series.summary.count, 5000);
  assert_eq!(series.summary.last.step, 4999);
  assert!(series.points.len() <= POINT_LIMIT);
  assert_eq!(series.points.first().unwrap().step, 0);
  assert_eq!(series.points.last().unwrap().step, 4999);
}

#[test]
fn archive_status_transitions_refresh_selected_metadata_without_file_changes() {
  let dir = tempfile::tempdir().unwrap();
  let store = Store::open(dir.path(), MockStorage::default()).unwrap();
  put(&store, "run-state.json", 1, br#"{"status":"completed"}"#);
  seal(&store, false).unwrap();
  let source = DashboardSource {
    project_id: scope().project_id,
    origin: scope().origin,
  };
  let ids = [scope().run_id];
  let before = store.dashboard_updates(Some(&source), &ids).unwrap();
  store
    .db()
    .unwrap()
    .execute("UPDATE result_archives SET status='uploading'", [])
    .unwrap();
  let after = store.dashboard_updates(Some(&source), &ids).unwrap();
  assert_ne!(
    before["runs"][0]["metadata_revision"],
    after["runs"][0]["metadata_revision"]
  );
}

#[test]
fn crash_child() {
  let Ok(root) = std::env::var("EXPRI_TRACKING_CRASH_DIRECTORY") else {
    return;
  };
  let store = Store::open(Path::new(&root), MockStorage::default()).unwrap();
  let bytes = b"{\"step\":2,\"metrics\":{\"loss\":2}}\n";
  let file = store.file(&run_target("outputs/metrics.jsonl")).unwrap();
  append(&store, "outputs/metrics.jsonl", file.size, bytes).unwrap();
}

#[test]
fn process_kills_before_and_after_commit_preserve_acknowledged_bytes_and_dedupe_projection() {
  use std::process::{Command, Stdio};
  use std::time::{Duration, Instant};
  let original = b"{\"step\":1,\"metrics\":{\"loss\":1}}\n";
  let added = b"{\"step\":2,\"metrics\":{\"loss\":2}}\n";
  for phase in ["raw_synced", "projection_written", "tracking_committed"] {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), MockStorage::default()).unwrap();
    append(&store, "outputs/metrics.jsonl", 0, original).unwrap();
    drop(store);
    let mut child = Command::new(std::env::current_exe().unwrap())
      .args([
        "--exact",
        "service::store::tracking::tests::crash_child",
        "--nocapture",
      ])
      .env("EXPRI_TRACKING_CRASH_DIRECTORY", dir.path())
      .env("EXPRI_TRACKING_CRASH_PHASE", phase)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .unwrap();
    let started = Instant::now();
    while !dir.path().join("tracking-test-ready").exists() {
      if child.try_wait().unwrap().is_some() || started.elapsed() > Duration::from_secs(10) {
        let _ = child.kill();
        let _ = child.wait();
        panic!("checkpoint {phase} not reached");
      }
      std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let store = Store::open(dir.path(), MockStorage::default()).unwrap();
    let committed = phase == "tracking_committed";
    let file = store.file(&run_target("outputs/metrics.jsonl")).unwrap();
    assert_eq!(
      file.size,
      original.len() as u64 + if committed { added.len() as u64 } else { 0 }
    );
    assert_eq!(
      store
        .tracking_open(&file)
        .unwrap()
        .metadata()
        .unwrap()
        .len(),
      file.size
    );
    assert_eq!(
      metrics(&store, true).metrics["loss"].summary.count,
      if committed { 2 } else { 1 }
    );
    append(
      &store,
      "outputs/metrics.jsonl",
      original.len() as u64,
      added,
    )
    .unwrap();
    assert_eq!(metrics(&store, true).metrics["loss"].summary.count, 2);
    let file = store.file(&run_target("outputs/metrics.jsonl")).unwrap();
    assert_eq!(
      store
        .tracking_range(&file, 0, original.len() + added.len())
        .unwrap(),
      [original.as_slice(), added.as_slice()].concat()
    );
  }
}
