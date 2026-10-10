use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use sha2::{Digest, Sha256};

use super::*;

use super::mock::mock;

fn scope() -> RunScope {
  RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-test".into(),
  }
}

fn owner() -> Value {
  json!({"schema_version":1,"service_endpoint":"http://service.test/v1/request",
    "source":"cached-worker","scope":scope()})
}

fn record(path: &str, size: u64) -> FileRecord {
  FileRecord {
    target: FileTarget::Run {
      scope: scope(),
      path: path.into(),
    },
    size,
    sha256: Some("a".repeat(64)),
    storage: FileStorage::Object,
  }
}

fn root() -> (tempfile::TempDir, PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let root = std::fs::canonicalize(temporary.path()).unwrap();
  (temporary, root)
}

#[test]
#[ignore = "subprocess fixture invoked by the interrupted-process test"]
fn interrupted_download_child() {
  let directory = PathBuf::from(std::env::var_os("EXPRI_TEST_DOWNLOAD_STAGING").unwrap());
  let input = std::env::var_os("EXPRI_TEST_DOWNLOAD_INPUT_DEST").map(PathBuf::from);
  let path = if input.is_some() {
    "input"
  } else {
    "outputs/checkpoint.bin"
  };
  let mut record = record(path, OBJECT_BATCH + 19);
  let mut owner = owner();
  if let Some(destination) = input {
    record.target = FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset".into(),
    };
    owner = json!({"schema_version":1,"service_endpoint":"http://service.test/v1/request",
      "target":record.target,"destination":destination});
  }
  let mut staging = staging::Staging::open(directory.clone(), &owner).unwrap();
  let (mut output, _) = staging.prepare(path, &record, true).unwrap();
  output
    .write_all(&vec![b'x'; OBJECT_BATCH as usize])
    .unwrap();
  staging
    .acknowledge(path, OBJECT_BATCH, false, &output)
    .unwrap();
  output.write_all(b"unacknowledged tail").unwrap();
  output.sync_all().unwrap();
  std::fs::write(directory.parent().unwrap().join("ready"), b"ready").unwrap();
  loop {
    thread::sleep(Duration::from_secs(1));
  }
}

#[test]
fn killed_private_input_download_reopens_only_its_acknowledged_range() {
  let (_temporary, root) = root();
  let directory = root.join("input-transfer");
  let destination = root.join("dataset.bin");
  std::fs::write(&destination, b"previous dataset").unwrap();
  let mut child = Command::new(std::env::current_exe().unwrap())
    .args([
      "--exact",
      "service::client::download::tests::interrupted_download_child",
      "--ignored",
    ])
    .env("EXPRI_TEST_DOWNLOAD_STAGING", &directory)
    .env("EXPRI_TEST_DOWNLOAD_INPUT_DEST", &destination)
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
  let deadline = Instant::now() + Duration::from_secs(10);
  while !root.join("ready").is_file() {
    if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
      let _ = child.kill();
      let _ = child.wait();
      panic!("private-input child did not acknowledge a durable range");
    }
    thread::sleep(Duration::from_millis(10));
  }
  child.kill().unwrap();
  child.wait().unwrap();
  let target = FileTarget::Input {
    project_id: "project".into(),
    input_id: "dataset".into(),
  };
  let owner = json!({"schema_version":1,"service_endpoint":"http://service.test/v1/request",
    "target":target,"destination":destination});
  let mut staging = staging::Staging::open(directory, &owner).unwrap();
  let record = FileRecord {
    target,
    size: OBJECT_BATCH + 19,
    sha256: Some("a".repeat(64)),
    storage: FileStorage::Object,
  };
  let (file, offset) = staging.prepare("input", &record, true).unwrap();
  assert_eq!(offset, OBJECT_BATCH);
  assert_eq!(file.metadata().unwrap().len(), OBJECT_BATCH);
  assert_eq!(std::fs::read(destination).unwrap(), b"previous dataset");
}

#[test]
fn killed_process_releases_lease_and_reopen_truncates_only_unacknowledged_tail() {
  let (_temporary, root) = root();
  let directory = root.join("staging");
  let mut child = Command::new(std::env::current_exe().unwrap())
    .args([
      "--exact",
      "service::client::download::tests::interrupted_download_child",
      "--ignored",
    ])
    .env("EXPRI_TEST_DOWNLOAD_STAGING", &directory)
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
  let deadline = Instant::now() + Duration::from_secs(10);
  while !root.join("ready").is_file() {
    if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
      let _ = child.kill();
      let _ = child.wait();
      panic!("download subprocess did not acknowledge its first range");
    }
    thread::sleep(Duration::from_millis(10));
  }
  let busy = staging::Staging::open(directory.clone(), &owner()).is_err();
  child.kill().unwrap();
  child.wait().unwrap();
  assert!(busy, "a live downloader must keep its exclusive lease");
  let mut staging = staging::Staging::open(directory, &owner()).unwrap();
  let (mut output, offset) = staging
    .prepare(
      "outputs/checkpoint.bin",
      &record("outputs/checkpoint.bin", OBJECT_BATCH + 19),
      true,
    )
    .unwrap();
  assert_eq!(offset, OBJECT_BATCH);
  assert_eq!(output.metadata().unwrap().len(), OBJECT_BATCH);
  output.seek(SeekFrom::Start(OBJECT_BATCH - 4)).unwrap();
  let mut bytes = [0; 4];
  output.read_exact(&mut bytes).unwrap();
  assert_eq!(&bytes, b"xxxx");
}

#[test]
fn ownership_corruption_and_symlinks_cannot_adopt_another_download() {
  let (_temporary, root) = root();
  let directory = root.join("staging");
  drop(staging::Staging::open(directory.clone(), &owner()).unwrap());
  for field in ["service_endpoint", "source"] {
    let mut foreign = owner();
    foreign[field] = json!("foreign");
    assert!(staging::Staging::open(directory.clone(), &foreign).is_err());
  }
  let mut foreign = owner();
  foreign["scope"]["origin"] = json!("other-worker");
  assert!(staging::Staging::open(directory.clone(), &foreign).is_err());
  fs::atomic_json(
    &directory.join("state.json"),
    &json!({"schema_version":1,"files":{
    "outputs/checkpoint.bin":{"record":record("outputs/checkpoint.bin",OBJECT_BATCH*2),
      "offset":3,"verified":false}}}),
  )
  .unwrap();
  assert!(staging::Staging::open(directory.clone(), &owner()).is_err());
  std::fs::write(directory.join("state.json"), b"not JSON").unwrap();
  assert!(staging::Staging::open(directory.clone(), &owner()).is_err());
  #[cfg(unix)]
  {
    use std::os::unix::fs::symlink;
    let linked = root.join("linked");
    symlink(&directory, &linked).unwrap();
    assert!(staging::Staging::open(linked, &owner()).is_err());
    std::fs::remove_file(directory.join("state.json")).unwrap();
    let victim = root.join("victim");
    std::fs::write(&victim, b"keep").unwrap();
    symlink(&victim, directory.join("state.json")).unwrap();
    assert!(staging::Staging::open(directory, &owner()).is_err());
    assert_eq!(std::fs::read(victim).unwrap(), b"keep");
  }
  let unowned = root.join("unowned");
  fs::directories(&unowned).unwrap();
  assert!(staging::Staging::open(unowned, &owner()).is_err());
  #[cfg(unix)]
  {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = root.join("safe-staging");
    let mut staging = staging::Staging::open(directory.clone(), &owner()).unwrap();
    let path = "outputs/checkpoint.bin";
    fs::directories(staging.path(path).parent().unwrap()).unwrap();
    let victim = root.join("untouched");
    std::fs::write(&victim, b"untouched").unwrap();
    symlink(&victim, staging.path(path)).unwrap();
    assert!(staging.prepare(path, &record(path, 9), true).is_err());
    assert_eq!(std::fs::read(victim).unwrap(), b"untouched");
    drop(staging);
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(staging::Staging::open(directory, &owner()).is_err());
  }
}

#[test]
fn changed_records_and_mutable_files_reset_without_mutating_published_hardlinks() {
  let (_temporary, root) = root();
  let directory = root.join("staging");
  let mut staging = staging::Staging::open(directory.clone(), &owner()).unwrap();
  let path = "outputs/checkpoint.bin";
  let original = record(path, 4);
  let (mut output, _) = staging.prepare(path, &original, true).unwrap();
  output.write_all(b"good").unwrap();
  staging.acknowledge(path, 4, true, &output).unwrap();
  let cached = root.join("cached.bin");
  publish(&staging.path(path), &cached).unwrap();
  drop(output);
  drop(staging);
  let mut staging = staging::Staging::open(directory, &owner()).unwrap();
  let mut changed = original;
  changed.sha256 = Some("b".repeat(64));
  let (mut output, offset) = staging.prepare(path, &changed, true).unwrap();
  assert_eq!(offset, 0);
  output.write_all(b"new!").unwrap();
  assert_eq!(std::fs::read(&cached).unwrap(), b"good");
  for (path, stream) in [("run-state.json", false), ("logs/stdout.log", true)] {
    let mut record = record(path, 4);
    if stream {
      record.storage = FileStorage::Stream;
      record.sha256 = None;
    }
    let (mut output, _) = staging.prepare(path, &record, false).unwrap();
    output.write_all(b"old!").unwrap();
    staging.acknowledge(path, 4, true, &output).unwrap();
    drop(output);
    let (output, offset) = staging.prepare(path, &record, false).unwrap();
    assert_eq!(offset, 0);
    assert_eq!(output.metadata().unwrap().len(), 0);
  }
}

fn object_record(path: &str, bytes: &[u8]) -> FileRecord {
  FileRecord {
    target: FileTarget::Run {
      scope: scope(),
      path: path.into(),
    },
    size: bytes.len() as u64,
    sha256: Some(fs::hex(&Sha256::digest(bytes))),
    storage: FileStorage::Object,
  }
}

fn config(root: &Path, url: &str) -> PathBuf {
  let path = root.join("client.toml");
  std::fs::write(&path, format!("url={url:?}\ntoken_env='PATH'\n")).unwrap();
  path
}

fn options(root: &Path, config: PathBuf) -> PullOptions {
  PullOptions {
    config,
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-test".into(),
    repo: root.into(),
    results_dir: "results".into(),
    source: Some("cached-worker".into()),
    artifacts: vec!["outputs/checkpoint.bin".into()],
  }
}

fn tracking_record(path: &str, size: u64, revision: u64, sealed: bool) -> FileRecord {
  FileRecord {
    target: FileTarget::Run {
      scope: scope(),
      path: path.into(),
    },
    size,
    sha256: None,
    storage: FileStorage::Tracking { revision, sealed },
  }
}

#[test]
fn tracking_pull_reopens_cursors_detaches_cache_and_reads_only_new_log_bytes() {
  let (_temporary, root) = root();
  let original = vec![b'a'; STREAM_BATCH + 19];
  let original_size = original.len();
  let mut grown = original.clone();
  grown.extend_from_slice(b"new log");
  let old_state = br#"{"run_id":"run-test","status":"running"}"#.to_vec();
  let new_state = br#"{"run_id":"run-test","status":"completed"}"#.to_vec();
  let cached = root.join("results/cached-worker/runs/run-test");
  let inspected_cache = cached.clone();
  let mut cycle = 0;
  let seen = Arc::new(Mutex::new(Vec::new()));
  let recorded = seen.clone();
  let expected = original.clone();
  let expected_grown = grown.clone();
  let expected_old_state = old_state.clone();
  let expected_new_state = new_state.clone();
  let (url, task) = mock(11, move |request, _| {
    let state = if cycle < 2 {
      &expected_old_state
    } else {
      &expected_new_state
    };
    let log = if cycle < 2 {
      &expected
    } else {
      &expected_grown
    };
    let record = |path: &str| {
      if path == "run-state.json" {
        tracking_record(path, state.len() as u64, cycle.max(1), cycle >= 2)
      } else {
        tracking_record(path, log.len() as u64, 1, cycle >= 2)
      }
    };
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::ListFiles { .. } => {
        cycle += 1;
        let state = if cycle == 1 {
          &expected_old_state
        } else {
          &expected_new_state
        };
        let log = if cycle == 1 {
          &expected
        } else {
          &expected_grown
        };
        Response::Files {
          files: vec![
            tracking_record("run-state.json", state.len() as u64, cycle, cycle >= 2),
            tracking_record("logs/stdout.log", log.len() as u64, 1, cycle >= 2),
          ],
        }
      }
      Request::ReadStream {
        path,
        offset,
        limit,
        ..
      } => {
        recorded.lock().unwrap().push((path.clone(), offset));
        if cycle == 2 {
          assert_eq!(
            std::fs::read(inspected_cache.join("logs/stdout.log")).unwrap(),
            expected
          );
        }
        let bytes = if path == "run-state.json" { state } else { log };
        let end = (offset as usize + limit).min(bytes.len());
        Response::Stream {
          offset,
          total_size: bytes.len() as u64,
          data_base64: STANDARD.encode(&bytes[offset as usize..end]),
        }
      }
      Request::GetFile {
        target: FileTarget::Run { path, .. },
      } => Response::File {
        file: record(&path),
      },
      other => panic!("unexpected tracking pull transport: {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let config = config(&root, &url);
  let mut selected = options(&root, config.clone());
  selected.artifacts.clear();
  let first = pull(selected).unwrap();
  assert_eq!(first["resumed_bytes"], 0);
  assert_eq!(
    std::fs::read(cached.join("logs/stdout.log")).unwrap(),
    original
  );
  let mut selected = options(&root, config);
  selected.artifacts.clear();
  let second = pull(selected).unwrap();
  assert_eq!(second["resumed_files"], 1);
  assert_eq!(second["resumed_bytes"], original_size);
  assert_eq!(second["downloaded_bytes"], 7 + new_state.len());
  assert_eq!(
    std::fs::read(cached.join("logs/stdout.log")).unwrap(),
    grown
  );
  assert_eq!(
    std::fs::read(cached.join("run-state.json")).unwrap(),
    new_state
  );
  assert_eq!(
    *seen.lock().unwrap(),
    vec![
      ("logs/stdout.log".into(), 0),
      ("logs/stdout.log".into(), STREAM_BATCH as u64),
      ("run-state.json".into(), 0),
      ("logs/stdout.log".into(), original_size as u64),
      ("run-state.json".into(), 0),
    ]
  );
  task.join().unwrap();
}

#[test]
fn interrupted_tracking_read_reopens_at_last_durable_chunk() {
  let (_temporary, root) = root();
  let bytes = vec![b'm'; STREAM_BATCH + 9];
  let record = tracking_record("outputs/metrics.jsonl", bytes.len() as u64, 1, false);
  let expected_record = record.clone();
  let mut requests = 0;
  let (url, task) = mock(4, move |request, _| {
    requests += 1;
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::ReadStream { offset, limit, .. } => {
        assert_eq!(
          offset,
          if requests == 1 {
            0
          } else {
            STREAM_BATCH as u64
          }
        );
        if requests == 2 {
          return (503, Vec::new(), br#"{"error":"temporary outage"}"#.to_vec());
        }
        let end = (offset as usize + limit).min(bytes.len());
        Response::Stream {
          offset,
          total_size: bytes.len() as u64,
          data_base64: STANDARD.encode(&bytes[offset as usize..end]),
        }
      }
      Request::GetFile { .. } => Response::File {
        file: expected_record.clone(),
      },
      other => panic!("unexpected range request: {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let directory = root.join("staging");
  let mut staging = staging::Staging::open(directory.clone(), &owner()).unwrap();
  assert!(staged_download(&api, &mut staging, "outputs/metrics.jsonl", &record).is_err());
  drop(staging);
  let mut staging = staging::Staging::open(directory, &owner()).unwrap();
  let (resumed, downloaded) =
    staged_download(&api, &mut staging, "outputs/metrics.jsonl", &record).unwrap();
  assert_eq!(resumed, STREAM_BATCH as u64);
  assert_eq!(downloaded, 9);
  task.join().unwrap();
}

#[test]
fn changed_document_during_read_resets_cursor_without_publishing_mixed_bytes() {
  let (_temporary, root) = root();
  let initial = tracking_record("outputs/params.json", 5, 1, false);
  let current = tracking_record("outputs/params.json", 5, 2, false);
  let expected = current.clone();
  let (url, task) = mock(4, move |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::ReadStream { offset, .. } => {
        assert_eq!(offset, 0);
        Response::Stream {
          offset,
          total_size: 5,
          data_base64: STANDARD.encode(b"newer"),
        }
      }
      Request::GetFile { .. } => Response::File {
        file: expected.clone(),
      },
      other => panic!("unexpected document request: {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let mut staging = staging::Staging::open(root.join("staging"), &owner()).unwrap();
  let cached = root.join("cached-params.json");
  std::fs::write(&cached, b"older").unwrap();
  assert!(staged_download(&api, &mut staging, "outputs/params.json", &initial).is_err());
  assert_eq!(std::fs::read(&cached).unwrap(), b"older");
  assert!(!staging.path("outputs/params.json").exists());
  assert_eq!(
    staged_download(&api, &mut staging, "outputs/params.json", &current).unwrap(),
    (0, 5)
  );
  publish(&staging.path("outputs/params.json"), &cached).unwrap();
  assert_eq!(std::fs::read(cached).unwrap(), b"newer");
  task.join().unwrap();
}

#[test]
fn edited_tracking_cache_is_refetched_without_truncating_its_shared_inode() {
  let (_temporary, root) = root();
  let path = "logs/stdout.log";
  let record = tracking_record(path, 4, 1, false);
  let directory = root.join("staging");
  let mut staging = staging::Staging::open(directory.clone(), &owner()).unwrap();
  let (mut file, _) = staging.prepare(path, &record, true).unwrap();
  file.write_all(b"good").unwrap();
  staging.acknowledge(path, 4, true, &file).unwrap();
  let cached = root.join("cached.log");
  publish(&staging.path(path), &cached).unwrap();
  drop(file);
  drop(staging);
  std::fs::write(&cached, b"edit").unwrap();
  File::open(&cached)
    .unwrap()
    .set_times(
      std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1)),
    )
    .unwrap();
  let mut staging = staging::Staging::open(directory, &owner()).unwrap();
  let (file, offset) = staging.prepare(path, &record, true).unwrap();
  assert_eq!(offset, 0);
  assert_eq!(file.metadata().unwrap().len(), 0);
  assert_eq!(std::fs::read(cached).unwrap(), b"edit");
}

#[test]
fn pull_reopens_durable_ranges_with_fresh_urls_and_keeps_previous_cache_until_verified() {
  let (_temporary, root) = root();
  let bytes = vec![b'x'; OBJECT_BATCH as usize + 19];
  let state = br#"{"run_id":"run-test","status":"completed"}"#.to_vec();
  let checkpoint = object_record("outputs/checkpoint.bin", &bytes);
  let run_state = object_record("run-state.json", &state);
  let ranges = Arc::new(Mutex::new(Vec::new()));
  let recorded_ranges = ranges.clone();
  let mut failures = 3;
  let mut leases = 0;
  let mut sequence = 0;
  let (url, task) = mock(14, move |request, origin| {
    sequence += 1;
    let result = if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::ListFiles { scope: requested } => {
          assert_eq!(requested, scope());
          Response::Files {
            files: vec![checkpoint.clone(), run_state.clone()],
          }
        }
        Request::DownloadUrl { target } => {
          leases += 1;
          let FileTarget::Run { path, .. } = target else {
            panic!("input request");
          };
          let object = if path == "run-state.json" {
            "state"
          } else {
            "checkpoint"
          };
          Response::Url {
            url: format!("{origin}/{object}?lease={leases}"),
          }
        }
        _ => panic!("unexpected request"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      assert!(!request.headers.contains_key("authorization"));
      let range = request.headers["range"].strip_prefix("bytes=").unwrap();
      let (start, end) = range.split_once('-').unwrap();
      let start: usize = start.parse().unwrap();
      let end: usize = end.parse().unwrap();
      if request.path.starts_with("/checkpoint?") {
        recorded_ranges.lock().unwrap().push(start as u64);
        if start > 0 && failures > 0 {
          failures -= 1;
          (503, Vec::new(), b"temporary object outage".to_vec())
        } else {
          (
            206,
            vec![(
              "Content-Range".into(),
              format!("bytes {start}-{end}/{}", bytes.len()),
            )],
            bytes[start..=end].to_vec(),
          )
        }
      } else {
        assert!(request.path.starts_with("/state?"));
        (
          206,
          vec![(
            "Content-Range".into(),
            format!("bytes {start}-{end}/{}", state.len()),
          )],
          state[start..=end].to_vec(),
        )
      }
    };
    if sequence == 14 {
      assert_eq!(leases, 6, "every range attempt needs a fresh URL");
    }
    result
  });
  let config = config(&root, &url);
  let destination = root.join("results/cached-worker/runs/run-test");
  fs::directories(&destination.join("outputs")).unwrap();
  let expected_owner = json!({"schema_version":1,"service_endpoint":format!("{url}/v1/request"),
    "source":"cached-worker","scope":scope()});
  fs::atomic_json(&destination.join(".pull-owner.json"), &expected_owner).unwrap();
  std::fs::write(destination.join("run-state.json"), b"previous state").unwrap();
  std::fs::write(
    destination.join("outputs/checkpoint.bin"),
    b"previous checkpoint",
  )
  .unwrap();
  assert!(pull(options(&root, config.clone())).is_err());
  assert_eq!(
    std::fs::read(destination.join("run-state.json")).unwrap(),
    b"previous state"
  );
  assert_eq!(
    std::fs::read(destination.join("outputs/checkpoint.bin")).unwrap(),
    b"previous checkpoint"
  );
  let saved: Value = serde_json::from_slice(
    &std::fs::read(root.join("results/cached-worker/.service-pull/run-test/state.json")).unwrap(),
  )
  .unwrap();
  assert_eq!(
    saved["files"]["outputs/checkpoint.bin"]["offset"],
    OBJECT_BATCH
  );
  let report = pull(options(&root, config)).unwrap();
  task.join().unwrap();
  assert_eq!(
    *ranges.lock().unwrap(),
    vec![0, OBJECT_BATCH, OBJECT_BATCH, OBJECT_BATCH, OBJECT_BATCH]
  );
  assert_eq!(report["resumed_files"], 1);
  assert_eq!(report["resumed_bytes"], OBJECT_BATCH);
  assert_eq!(
    std::fs::metadata(destination.join("outputs/checkpoint.bin"))
      .unwrap()
      .len(),
    OBJECT_BATCH + 19
  );
  let receipt: Value =
    serde_json::from_slice(&std::fs::read(destination.join("pull-state.json")).unwrap()).unwrap();
  assert_eq!(receipt["available_files"].as_array().unwrap().len(), 1);
  assert_eq!(
    receipt["available_files"][0]["path"],
    "outputs/checkpoint.bin"
  );
}

#[test]
fn corrupt_acknowledged_prefix_resets_progress_and_next_attempt_fetches_full_object() {
  let (_temporary, root) = root();
  let bytes = vec![b'x'; OBJECT_BATCH as usize + 19];
  let record = object_record("outputs/checkpoint.bin", &bytes);
  let ranges = Arc::new(Mutex::new(Vec::new()));
  let recorded_ranges = ranges.clone();
  let mut sequence = 0;
  let (url, task) = mock(6, move |request, origin| {
    sequence += 1;
    if request.path == "/v1/request" {
      assert!(matches!(
        serde_json::from_slice::<Request>(&request.body).unwrap(),
        Request::DownloadUrl { .. }
      ));
      (
        200,
        Vec::new(),
        serde_json::to_vec(&Response::Url {
          url: format!("{origin}/object?lease={sequence}"),
        })
        .unwrap(),
      )
    } else {
      let range = request.headers["range"].strip_prefix("bytes=").unwrap();
      let (start, end) = range.split_once('-').unwrap();
      let start: usize = start.parse().unwrap();
      let end: usize = end.parse().unwrap();
      recorded_ranges.lock().unwrap().push(start as u64);
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes {start}-{end}/{}", bytes.len()),
        )],
        bytes[start..=end].to_vec(),
      )
    }
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let mut staging = staging::Staging::open(root.join("staging"), &owner()).unwrap();
  let path = "outputs/checkpoint.bin";
  let (mut output, _) = staging.prepare(path, &record, true).unwrap();
  output
    .write_all(&vec![b'!'; OBJECT_BATCH as usize])
    .unwrap();
  staging
    .acknowledge(path, OBJECT_BATCH, false, &output)
    .unwrap();
  drop(output);
  let error = staged_download(&api, &mut staging, path, &record).unwrap_err();
  assert!(error.to_string().contains("SHA256"));
  assert!(!staging.path(path).exists());
  drop(staging);
  let mut staging = staging::Staging::open(root.join("staging"), &owner()).unwrap();
  let (resumed, downloaded) = staged_download(&api, &mut staging, path, &record).unwrap();
  task.join().unwrap();
  assert_eq!(resumed, 0);
  assert_eq!(downloaded, OBJECT_BATCH + 19);
  assert_eq!(*ranges.lock().unwrap(), vec![OBJECT_BATCH, 0, OBJECT_BATCH]);
}

#[test]
fn verified_publication_can_repeat_after_restart_before_the_receipt_is_saved() {
  let (_temporary, root) = root();
  let path = "outputs/checkpoint.bin";
  let record = object_record(path, b"verified");
  let directory = root.join("staging");
  let mut staging = staging::Staging::open(directory.clone(), &owner()).unwrap();
  let (mut output, _) = staging.prepare(path, &record, true).unwrap();
  output.write_all(b"verified").unwrap();
  staging
    .acknowledge(path, record.size, true, &output)
    .unwrap();
  let cached = root.join("cached");
  publish(&staging.path(path), &cached).unwrap();
  drop(output);
  drop(staging);
  let mut staging = staging::Staging::open(directory, &owner()).unwrap();
  let (mut output, offset) = staging.prepare(path, &record, true).unwrap();
  assert_eq!(offset, record.size);
  assert_eq!(
    fs::digest(&mut output, record.size).unwrap(),
    record.sha256.unwrap()
  );
  publish(&staging.path(path), &cached).unwrap();
  staging.clear().unwrap();
  drop(staging);
  assert_eq!(std::fs::read(cached).unwrap(), b"verified");
  let staging = staging::Staging::open(root.join("staging"), &owner()).unwrap();
  assert!(!staging.path(path).exists());
}

#[test]
fn failed_mutable_download_after_interrupted_publication_preserves_the_old_cache_inode() {
  let (_temporary, root) = root();
  let path = "run-state.json";
  let old = br#"{"run_id":"run-test","status":"running"}"#;
  let directory = root.join("staging");
  let cached = root.join("cached-state.json");
  let mut staging = staging::Staging::open(directory.clone(), &owner()).unwrap();
  let (mut output, _) = staging
    .prepare(path, &object_record(path, old), false)
    .unwrap();
  output.write_all(old).unwrap();
  staging
    .acknowledge(path, old.len() as u64, true, &output)
    .unwrap();
  publish(&staging.path(path), &cached).unwrap();
  let before = std::fs::metadata(&cached).unwrap();
  drop(output);
  drop(staging);
  let (url, task) = mock(6, |request, origin| {
    if request.path == "/v1/request" {
      (
        200,
        Vec::new(),
        serde_json::to_vec(&Response::Url {
          url: format!("{origin}/unavailable"),
        })
        .unwrap(),
      )
    } else {
      (503, Vec::new(), Vec::new())
    }
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let mut staging = staging::Staging::open(directory, &owner()).unwrap();
  let new = br#"{"run_id":"run-test","status":"completed"}"#;
  assert!(staged_download(&api, &mut staging, path, &object_record(path, new)).is_err());
  task.join().unwrap();
  assert_eq!(std::fs::read(&cached).unwrap(), old);
  assert!(fs::unchanged(&before, &std::fs::metadata(cached).unwrap()));
  assert_eq!(std::fs::metadata(staging.path(path)).unwrap().len(), 0);
}

#[test]
fn available_file_receipt_is_bounded_and_does_not_fetch_unselected_artifacts() {
  let (_temporary, root) = root();
  let state = br#"{"run_id":"run-test","status":"completed"}"#.to_vec();
  let mut files = vec![object_record("run-state.json", &state)];
  for index in 0..220 {
    let path = format!("outputs/{}/{index}.bin", vec!["x".repeat(90); 10].join("/"));
    files.push(object_record(&path, b""));
  }
  let (url, task) = mock(3, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::ListFiles { .. } => Response::Files {
          files: files.clone(),
        },
        Request::DownloadUrl {
          target: FileTarget::Run { path, .. },
        } => {
          assert_eq!(path, "run-state.json", "unselected artifact was downloaded");
          Response::Url {
            url: format!("{origin}/state"),
          }
        }
        _ => panic!("unexpected request"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes 0-{}/{}", state.len() - 1, state.len()),
        )],
        state.clone(),
      )
    }
  });
  let mut options = options(&root, config(&root, &url));
  options.artifacts.clear();
  let report = pull(options).unwrap();
  task.join().unwrap();
  let destination = PathBuf::from(report["destination"].as_str().unwrap());
  let receipt: Value =
    serde_json::from_slice(&std::fs::read(destination.join("pull-state.json")).unwrap()).unwrap();
  assert_eq!(receipt["available_files_truncated"], true);
  assert!(receipt["available_files"].as_array().unwrap().len() < 200);
  assert!(
    serde_json::to_vec_pretty(&receipt["available_files"])
      .unwrap()
      .len()
      < 64 * 1024
  );
  assert!(!destination.join("outputs").exists());
}

#[test]
fn empty_object_is_verified_once_and_reopened_without_a_range_or_another_request() {
  let (_temporary, root) = root();
  let record = object_record("outputs/empty.bin", b"");
  let (url, task) = mock(2, |request, origin| {
    if request.path == "/v1/request" {
      (
        200,
        Vec::new(),
        serde_json::to_vec(&Response::Url {
          url: format!("{origin}/empty"),
        })
        .unwrap(),
      )
    } else {
      assert!(!request.headers.contains_key("range"));
      (200, Vec::new(), Vec::new())
    }
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let directory = root.join("staging");
  let mut staging = staging::Staging::open(directory.clone(), &owner()).unwrap();
  assert_eq!(
    staged_download(&api, &mut staging, "outputs/empty.bin", &record).unwrap(),
    (0, 0)
  );
  task.join().unwrap();
  drop(staging);
  let mut staging = staging::Staging::open(directory, &owner()).unwrap();
  // The mock listener is closed; a second request would fail this assertion.
  assert_eq!(
    staged_download(&api, &mut staging, "outputs/empty.bin", &record).unwrap(),
    (0, 0)
  );
}

#[test]
fn private_inventory_is_pulled_without_hiding_valid_cached_cloud_artifacts() {
  let (_temporary, root) = root();
  let state = br#"{"run_id":"run-test","status":"completed"}"#.to_vec();
  let inventory =
    br#"{"files":[{"path":"outputs/checkpoint.bin","size":0}],"truncated":false}"#.to_vec();
  let files = vec![
    object_record("run-state.json", &state),
    object_record(crate::run_artifacts::INVENTORY_PATH, &inventory),
    object_record("outputs/checkpoint.bin", b""),
  ];
  let (url, task) = mock(5, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::ListFiles { .. } => Response::Files {
          files: files.clone(),
        },
        Request::DownloadUrl {
          target: FileTarget::Run { path, .. },
        } => {
          let object = if path == "run-state.json" {
            "state"
          } else if path == crate::run_artifacts::INVENTORY_PATH {
            "inventory"
          } else {
            panic!("unselected checkpoint was downloaded");
          };
          Response::Url {
            url: format!("{origin}/{object}"),
          }
        }
        _ => panic!("unexpected request"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      let bytes = if request.path == "/inventory" {
        &inventory
      } else {
        &state
      };
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()),
        )],
        bytes.clone(),
      )
    }
  });
  let mut options = options(&root, config(&root, &url));
  options.artifacts.clear();
  let report = pull(options).unwrap();
  task.join().unwrap();
  let destination = PathBuf::from(report["destination"].as_str().unwrap());
  assert!(
    destination
      .join(crate::run_artifacts::INVENTORY_PATH)
      .is_file()
  );
  let receipt: Value =
    serde_json::from_slice(&std::fs::read(destination.join("pull-state.json")).unwrap()).unwrap();
  let available = receipt["available_files"].as_array().unwrap();
  assert_eq!(available.len(), 1);
  assert_eq!(available[0]["path"], "outputs/checkpoint.bin");
  assert!(
    available
      .iter()
      .all(|file| crate::run_artifacts::validate_path(file["path"].as_str().unwrap()).is_ok())
  );
  assert_eq!(receipt["available_files_truncated"], false);
}

#[test]
fn changed_selection_prunes_old_files_before_preparing_a_new_full_selection() {
  let (_temporary, root) = root();
  let directory = root.join("staging");
  let staging = staging::Staging::open(directory.clone(), &owner()).unwrap();
  let mut saved = BTreeMap::new();
  for path in METADATA
    .iter()
    .chain(STREAMS.iter())
    .map(|path| path.to_string())
    .chain((0..64).map(|index| format!("outputs/old-{index}.bin")))
  {
    saved.insert(
      path.clone(),
      json!({"record":record(&path,4),"offset":0,"verified":false}),
    );
  }
  fs::atomic_json(
    &directory.join("state.json"),
    &json!({"schema_version":1,"files":saved}),
  )
  .unwrap();
  drop(staging);
  let mut staging = staging::Staging::open(directory.clone(), &owner()).unwrap();
  let (mut old, _) = staging
    .prepare("outputs/old-0.bin", &record("outputs/old-0.bin", 4), true)
    .unwrap();
  old.write_all(b"old!").unwrap();
  staging
    .acknowledge("outputs/old-0.bin", 4, true, &old)
    .unwrap();
  drop(old);
  let selected = METADATA
    .iter()
    .chain(STREAMS.iter())
    .map(|path| path.to_string())
    .chain((0..64).map(|index| format!("outputs/new-{index}.bin")))
    .map(|path| (path.clone(), record(&path, 4)))
    .collect::<BTreeMap<_, _>>();
  staging.select(&selected).unwrap();
  assert!(!staging.path("outputs/old-0.bin").exists());
  for (path, record) in &selected {
    drop(staging.prepare(path, record, true).unwrap());
  }
  drop(staging);
  let saved: Value =
    serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
  let saved = saved["files"].as_object().unwrap();
  assert_eq!(saved.len(), selected.len());
  assert!(saved.keys().all(|path| selected.contains_key(path)));
  assert!(staging::Staging::open(directory, &owner()).is_ok());
}

#[test]
#[ignore = "subprocess fixture invoked by the interrupted-initialization test"]
fn interrupted_initialization_child() {
  let root = PathBuf::from(std::env::var_os("EXPRI_TEST_DOWNLOAD_INIT_ROOT").unwrap());
  let target = PathBuf::from(std::env::var_os("EXPRI_TEST_DOWNLOAD_INIT_TARGET").unwrap());
  match std::env::var("EXPRI_TEST_DOWNLOAD_INIT_KIND")
    .unwrap()
    .as_str()
  {
    "staging" => {
      let _staging = staging::Staging::open(target, &owner()).unwrap();
    }
    "cache" => {
      let _transfer = staging::Staging::open(root.join("transfer"), &owner()).unwrap();
      initialization::initialize(&target, ".pull-owner.json", &owner(), None).unwrap();
    }
    "pruning" => {
      let mut staging = staging::Staging::open(target, &owner()).unwrap();
      staging.select(&BTreeMap::new()).unwrap();
    }
    "resetting" => {
      let mut staging = staging::Staging::open(target, &owner()).unwrap();
      staging
        .prepare(
          "outputs/checkpoint.bin",
          &object_record("outputs/checkpoint.bin", b"new!"),
          true,
        )
        .unwrap();
    }
    _ => panic!("unknown subprocess fixture"),
  }
  panic!("initialization hook was not reached");
}

#[test]
fn killed_initialization_never_leaves_an_unowned_permanent_stage_or_cache() {
  for (kind, phases) in [
    (
      "staging",
      ["temporary_created", "owner_synced", "state_synced"].as_slice(),
    ),
    ("cache", ["temporary_created", "owner_synced"].as_slice()),
  ] {
    for phase in phases {
      let (_temporary, root) = root();
      let target = root.join(kind);
      let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
          "--exact",
          "service::client::download::tests::interrupted_initialization_child",
          "--ignored",
        ])
        .env("EXPRI_TEST_DOWNLOAD_INIT_ROOT", &root)
        .env("EXPRI_TEST_DOWNLOAD_INIT_TARGET", &target)
        .env("EXPRI_TEST_DOWNLOAD_INIT_PHASE", phase)
        .env("EXPRI_TEST_DOWNLOAD_INIT_KIND", kind)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
      let deadline = Instant::now() + Duration::from_secs(10);
      while !root.join("ready").is_file() {
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
          let _ = child.kill();
          let _ = child.wait();
          panic!("initialization subprocess did not reach {kind}/{phase}");
        }
        thread::sleep(Duration::from_millis(10));
      }
      let absent = !target.exists();
      let busy = if kind == "staging" {
        staging::Staging::open(target.clone(), &owner()).is_err()
      } else {
        staging::Staging::open(root.join("transfer"), &owner()).is_err()
      };
      child.kill().unwrap();
      child.wait().unwrap();
      assert!(
        absent,
        "partially initialized directory was published at {kind}/{phase}"
      );
      assert!(
        busy,
        "initialization or transfer lease was not held at {kind}/{phase}"
      );
      if kind == "staging" {
        let staging = staging::Staging::open(target.clone(), &owner()).unwrap();
        let saved: Value = serde_json::from_slice(
          &fs::read_bounded(&staging.directory.join("state.json"), 256 * 1024).unwrap(),
        )
        .unwrap();
        assert!(saved["files"].as_object().unwrap().is_empty());
        drop(staging);
      } else {
        let _transfer = staging::Staging::open(root.join("transfer"), &owner()).unwrap();
        initialization::initialize(&target, ".pull-owner.json", &owner(), None).unwrap();
        cache_owner(&target, &owner()).unwrap();
      }
      assert!(
        target.is_dir(),
        "same operation could not recover after {kind}/{phase}"
      );
    }
  }
}

#[test]
fn atomic_initialization_never_replaces_or_adopts_an_existing_unowned_directory() {
  let (_temporary, root) = root();
  let prepared = root.join("prepared");
  fs::directories(&prepared).unwrap();
  fs::atomic_json(&prepared.join("owner.json"), &owner()).unwrap();
  let target = root.join("unowned");
  fs::directories(&target).unwrap();
  let before = std::fs::metadata(&target).unwrap();
  let error = initialization::publish_directory(&prepared, &target).unwrap_err();
  assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
  assert!(fs::same_identity(
    &before,
    &std::fs::metadata(&target).unwrap()
  ));
  assert!(!target.join("owner.json").exists());
  assert!(prepared.join("owner.json").is_file());
  assert!(staging::Staging::open(target.clone(), &owner()).is_err());
  initialization::initialize(&target, ".pull-owner.json", &owner(), None).unwrap();
  assert!(cache_owner(&target, &owner()).is_err());
  assert!(!target.join(".pull-owner.json").exists());
  let legacy_partial = root.join("legacy-partial");
  fs::directories(&legacy_partial).unwrap();
  fs::atomic_json(&legacy_partial.join("owner.json"), &owner()).unwrap();
  assert!(staging::Staging::open(legacy_partial.clone(), &owner()).is_err());
  assert!(!legacy_partial.join("state.json").exists());
}

#[test]
fn killed_selection_prune_or_record_reset_can_retry_the_previous_selection() {
  for (kind, phase) in [
    ("pruning", "selection_pruned"),
    ("resetting", "record_reset"),
  ] {
    let (_temporary, root) = root();
    let target = root.join("staging");
    let path = "outputs/checkpoint.bin";
    let record = object_record(path, b"old!");
    let mut staging = staging::Staging::open(target.clone(), &owner()).unwrap();
    let (mut output, _) = staging.prepare(path, &record, true).unwrap();
    output.write_all(b"old!").unwrap();
    staging.acknowledge(path, 4, true, &output).unwrap();
    let cached = root.join("cached-checkpoint.bin");
    publish(&staging.path(path), &cached).unwrap();
    let cache_metadata = std::fs::metadata(&cached).unwrap();
    drop(output);
    drop(staging);
    let mut child = Command::new(std::env::current_exe().unwrap())
      .args([
        "--exact",
        "service::client::download::tests::interrupted_initialization_child",
        "--ignored",
      ])
      .env("EXPRI_TEST_DOWNLOAD_INIT_ROOT", &root)
      .env("EXPRI_TEST_DOWNLOAD_INIT_TARGET", &target)
      .env("EXPRI_TEST_DOWNLOAD_INIT_PHASE", phase)
      .env("EXPRI_TEST_DOWNLOAD_INIT_KIND", kind)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("ready").is_file() {
      if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("subprocess did not reach durable {kind}");
      }
      thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
      std::fs::read(target.join("files").join(path)).unwrap(),
      b"old!"
    );
    let saved: Value =
      serde_json::from_slice(&fs::read_bounded(&target.join("state.json"), 256 * 1024).unwrap())
        .unwrap();
    if kind == "pruning" {
      assert!(saved["files"].as_object().unwrap().is_empty());
    } else {
      assert_eq!(saved["files"][path]["offset"], 0);
    }
    let mut staging = staging::Staging::open(target, &owner()).unwrap();
    staging
      .select(&BTreeMap::from([(path.into(), record.clone())]))
      .unwrap();
    let (output, offset) = staging.prepare(path, &record, true).unwrap();
    assert_eq!(offset, 0);
    assert_eq!(output.metadata().unwrap().len(), 0);
    assert_eq!(std::fs::read(&cached).unwrap(), b"old!");
    assert!(fs::unchanged(
      &cache_metadata,
      &std::fs::metadata(cached).unwrap()
    ));
  }
}

#[test]
fn unverified_empty_shared_stage_is_detached_without_touching_the_cache() {
  let (_temporary, root) = root();
  let path = "outputs/empty.bin";
  let record = object_record(path, b"");
  let target = root.join("staging");
  let mut staging = staging::Staging::open(target.clone(), &owner()).unwrap();
  let (output, _) = staging.prepare(path, &record, true).unwrap();
  staging.acknowledge(path, 0, true, &output).unwrap();
  let cached = root.join("cached-empty.bin");
  publish(&staging.path(path), &cached).unwrap();
  let before = std::fs::metadata(&cached).unwrap();
  // Simulate reset/new-record persistence followed by interruption before unlink.
  staging.acknowledge(path, 0, false, &output).unwrap();
  drop(output);
  drop(staging);
  let mut staging = staging::Staging::open(target, &owner()).unwrap();
  let (output, offset) = staging.prepare(path, &record, true).unwrap();
  assert_eq!(offset, 0);
  assert!(!fs::same_identity(&before, &output.metadata().unwrap()));
  assert!(fs::unchanged(&before, &std::fs::metadata(cached).unwrap()));
}

#[test]
fn finalized_stream_objects_refresh_instead_of_claiming_resumed_bytes() {
  let (_temporary, root) = root();
  let bytes = b"line\n";
  let (url, task) = mock(12, |request, origin| {
    if request.path == "/v1/request" {
      assert!(matches!(
        serde_json::from_slice::<Request>(&request.body).unwrap(),
        Request::DownloadUrl {
          target: FileTarget::Run { .. }
        }
      ));
      (
        200,
        Vec::new(),
        serde_json::to_vec(&Response::Url {
          url: format!("{origin}/stream-object"),
        })
        .unwrap(),
      )
    } else {
      assert_eq!(request.headers["range"], "bytes=0-4");
      (
        206,
        vec![("Content-Range".into(), "bytes 0-4/5".into())],
        b"line\n".to_vec(),
      )
    }
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let mut staging = staging::Staging::open(root.join("staging"), &owner()).unwrap();
  for _ in 0..2 {
    for path in STREAMS {
      let progress =
        staged_download(&api, &mut staging, path, &object_record(path, bytes)).unwrap();
      assert_eq!(
        progress,
        (0, bytes.len() as u64),
        "finalized stream was incorrectly resumed: {path}"
      );
    }
  }
  task.join().unwrap();
}

#[test]
fn private_input_resumes_durable_ranges_reuses_verified_files_and_works_offline() {
  let (_temporary, root) = root();
  let bytes = vec![b'd'; OBJECT_BATCH as usize + 19];
  let target = FileTarget::Input {
    project_id: "project".into(),
    input_id: "dataset".into(),
  };
  let record = FileRecord {
    target,
    size: bytes.len() as u64,
    sha256: Some(fs::hex(&Sha256::digest(&bytes))),
    storage: FileStorage::Object,
  };
  let expected = record.clone();
  let payload = bytes.clone();
  let seen = Arc::new(Mutex::new(Vec::new()));
  let recorded = seen.clone();
  let mut cycle = 0;
  let (url, task) = mock(13, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::GetFile { target } => {
          assert_eq!(target, expected.target);
          cycle += 1;
          Response::File {
            file: expected.clone(),
          }
        }
        Request::DownloadUrl { target } => {
          assert_eq!(target, expected.target);
          Response::Url {
            url: format!("{origin}/dataset?private-signature"),
          }
        }
        other => panic!("unexpected input request: {other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      assert!(!request.headers.contains_key("authorization"));
      let range = request.headers["range"].strip_prefix("bytes=").unwrap();
      let (start, end) = range.split_once('-').unwrap();
      let start: usize = start.parse().unwrap();
      let end: usize = end.parse().unwrap();
      recorded.lock().unwrap().push(start as u64);
      if cycle == 1 && start > 0 {
        return (503, Vec::new(), Vec::new());
      }
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes {start}-{end}/{}", payload.len()),
        )],
        payload[start..=end].to_vec(),
      )
    }
  });
  let config = config(&root, &url);
  let destination = root.join("dataset.bin");
  std::fs::write(&destination, b"keep previous input").unwrap();
  let input = || InputGetOptions {
    config: config.clone(),
    project_id: "project".into(),
    input_id: "dataset".into(),
    destination: destination.clone(),
  };
  assert!(input_get(input()).is_err());
  assert_eq!(std::fs::read(&destination).unwrap(), b"keep previous input");
  let progress: Value = serde_json::from_slice(
    &std::fs::read(root.join(".expri-input-downloads/dataset.bin/state.json")).unwrap(),
  )
  .unwrap();
  assert_eq!(progress["files"]["input"]["offset"], OBJECT_BATCH);
  let report = input_get(input()).unwrap();
  assert_eq!(report["resumed_bytes"], OBJECT_BATCH);
  assert_eq!(report["downloaded_bytes"], 19);
  assert_eq!(std::fs::read(&destination).unwrap(), bytes);
  let before = std::fs::metadata(&destination).unwrap();
  let reused = input_get(input()).unwrap();
  assert_eq!(reused["reused"], true);
  assert_eq!(reused["downloaded_bytes"], 0);
  assert!(fs::unchanged(
    &before,
    &std::fs::metadata(&destination).unwrap()
  ));
  task.join().unwrap();
  assert_eq!(
    *seen.lock().unwrap(),
    vec![0, OBJECT_BATCH, OBJECT_BATCH, OBJECT_BATCH, OBJECT_BATCH]
  );
  // The server is gone. Only the durable, target-owned receipt can permit reuse.
  let offline = input_get(input()).unwrap();
  assert_eq!(offline["offline"], true);
  assert_eq!(offline["reused"], true);
  std::fs::write(&destination, b"unverified replacement").unwrap();
  assert!(input_get(input()).is_err());
  assert_eq!(
    std::fs::read(&destination).unwrap(),
    b"unverified replacement"
  );
}

#[test]
fn completed_checkpoint_is_reused_and_download_receipt_survives_metadata_only_pull() {
  let (_temporary, root) = root();
  let state = br#"{"run_id":"run-test","status":"running"}"#.to_vec();
  let checkpoint = b"completed checkpoint".to_vec();
  let checkpoint_record = object_record("outputs/checkpoint.bin", &checkpoint);
  let files = vec![
    object_record("run-state.json", &state),
    checkpoint_record.clone(),
  ];
  let checkpoint_bytes = checkpoint.clone();
  let mut checkpoint_requests = 0;
  let (url, task) = mock(11, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::ListFiles { .. } => Response::Files {
          files: files.clone(),
        },
        Request::DownloadUrl {
          target: FileTarget::Run { path, .. },
        } => {
          if path == "outputs/checkpoint.bin" {
            checkpoint_requests += 1;
            assert_eq!(checkpoint_requests, 1);
          }
          Response::Url {
            url: format!("{origin}/{path}"),
          }
        }
        other => panic!("unexpected pull request: {other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      let bytes = if request.path == "/run-state.json" {
        &state
      } else {
        &checkpoint_bytes
      };
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()),
        )],
        bytes.clone(),
      )
    }
  });
  let config = config(&root, &url);
  let first = pull(options(&root, config.clone())).unwrap();
  let second = pull(options(&root, config.clone())).unwrap();
  assert_eq!(second["reused_bytes"], checkpoint.len());
  let mut metadata = options(&root, config);
  metadata.artifacts.clear();
  pull(metadata).unwrap();
  task.join().unwrap();
  let destination = PathBuf::from(first["destination"].as_str().unwrap());
  let receipt: Value =
    serde_json::from_slice(&std::fs::read(destination.join("pull-state.json")).unwrap()).unwrap();
  assert_eq!(receipt["downloaded_files"].as_array().unwrap().len(), 1);
  assert_eq!(
    receipt["downloaded_files"][0]["path"],
    "outputs/checkpoint.bin"
  );
  assert_eq!(
    receipt["downloaded_files"][0]["sha256"],
    checkpoint_record.sha256.unwrap()
  );
}

#[test]
fn matching_existing_input_is_reused_without_changing_its_permissions_or_inode() {
  let (_temporary, root) = root();
  let destination = root.join("existing-dataset.bin");
  let bytes = b"verified user input";
  std::fs::write(&destination, bytes).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o644)).unwrap();
  }
  let before = std::fs::metadata(&destination).unwrap();
  let record = FileRecord {
    target: FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset".into(),
    },
    size: bytes.len() as u64,
    sha256: Some(fs::hex(&Sha256::digest(bytes))),
    storage: FileStorage::Object,
  };
  let (url, task) = mock(1, move |request, _| {
    assert!(matches!(
      serde_json::from_slice::<Request>(&request.body).unwrap(),
      Request::GetFile { .. }
    ));
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::File {
        file: record.clone(),
      })
      .unwrap(),
    )
  });
  let report = input_get(InputGetOptions {
    config: config(&root, &url),
    project_id: "project".into(),
    input_id: "dataset".into(),
    destination: destination.clone(),
  })
  .unwrap();
  task.join().unwrap();
  assert_eq!(report["reused"], true);
  let after = std::fs::metadata(&destination).unwrap();
  assert!(fs::unchanged(&before, &after));
  assert_eq!(before.permissions(), after.permissions());
}

#[test]
fn prepared_input_binds_verified_destination_while_transfer_lease_is_held() {
  let (_temporary, root) = root();
  let destination = root.join("dataset.bin");
  let bytes = b"existing dataset";
  std::fs::write(&destination, bytes).unwrap();
  let record = FileRecord {
    target: FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset".into(),
    },
    size: bytes.len() as u64,
    sha256: Some(fs::hex(&Sha256::digest(bytes))),
    storage: FileStorage::Object,
  };
  let (url, server) = mock(1, move |_, _| {
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::File {
        file: record.clone(),
      })
      .unwrap(),
    )
  });
  let config = config(&root, &url);
  let options = || InputGetOptions {
    config: config.clone(),
    project_id: "project".into(),
    input_id: "dataset".into(),
    destination: destination.clone(),
  };
  let mut called = false;
  input_get_prepared(options(), &mut || Ok(false), &mut |path, report, _| {
    called = true;
    assert_eq!(path, destination);
    assert_eq!(report["reused"], true);
    assert!(
      input_get(options())
        .unwrap_err()
        .to_string()
        .contains("another pull is downloading")
    );
    Ok(())
  })
  .unwrap();
  server.join().unwrap();
  assert!(called);
}

#[test]
fn prepared_input_waits_for_shared_transfer_but_cancel_does_not_touch_cache() {
  let (_temporary, root) = root();
  let destination = root.join("dataset.bin");
  let config = config(&root, "http://127.0.0.1:9");
  let target = FileTarget::Input {
    project_id: "project".into(),
    input_id: "dataset".into(),
  };
  let owner = json!({"schema_version":1,"service_endpoint":"http://127.0.0.1:9/v1/request",
    "target":target,"destination":destination});
  let _active =
    staging::Staging::open(root.join(".expri-input-downloads/dataset.bin"), &owner).unwrap();
  let mut checks = 0;
  let error = input_get_prepared(
    InputGetOptions {
      config,
      project_id: "project".into(),
      input_id: "dataset".into(),
      destination: destination.clone(),
    },
    &mut || {
      checks += 1;
      Ok(checks >= 3)
    },
    &mut |_, _, _| panic!("cancelled input must not bind"),
  )
  .unwrap_err();
  assert!(error.to_string().contains("cancelled"));
  assert_eq!(checks, 3);
  assert!(!destination.exists());
}

#[test]
fn prepared_input_cancels_at_a_range_boundary_and_retains_resumable_progress() {
  let (_temporary, root) = root();
  let destination = root.join("dataset.bin");
  std::fs::write(&destination, b"keep previous input").unwrap();
  let bytes = vec![b'i'; OBJECT_BATCH as usize + 23];
  let record = FileRecord {
    target: FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset".into(),
    },
    size: bytes.len() as u64,
    sha256: Some(fs::hex(&Sha256::digest(&bytes))),
    storage: FileStorage::Object,
  };
  let payload = bytes.clone();
  let (url, task) = mock(6, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::GetFile { .. } => Response::File {
          file: record.clone(),
        },
        Request::DownloadUrl { .. } => Response::Url {
          url: format!("{origin}/dataset"),
        },
        other => panic!("unexpected cancelled input request: {other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      let (start, end) = request.headers["range"]
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
      let start: usize = start.parse().unwrap();
      let end: usize = end.parse().unwrap();
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes {start}-{end}/{}", payload.len()),
        )],
        payload[start..=end].to_vec(),
      )
    }
  });
  let config = config(&root, &url);
  let input = || InputGetOptions {
    config: config.clone(),
    project_id: "project".into(),
    input_id: "dataset".into(),
    destination: destination.clone(),
  };
  let progress = root.join(".expri-input-downloads/dataset.bin/state.json");
  let error = input_get_prepared(
    input(),
    &mut || {
      let offset = std::fs::read(&progress)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|saved| saved["files"]["input"]["offset"].as_u64())
        .unwrap_or(0);
      Ok(offset == OBJECT_BATCH)
    },
    &mut |_, _, _| panic!("cancelled input must not bind"),
  )
  .unwrap_err();
  assert!(matches!(error, crate::error::ExpriError::DownloadCancelled));
  assert_eq!(std::fs::read(&destination).unwrap(), b"keep previous input");
  let saved: Value = serde_json::from_slice(&std::fs::read(progress).unwrap()).unwrap();
  assert_eq!(saved["files"]["input"]["offset"], OBJECT_BATCH);
  let report = input_get(input()).unwrap();
  assert_eq!(report["resumed_bytes"], OBJECT_BATCH);
  assert_eq!(report["downloaded_bytes"], 23);
  assert_eq!(std::fs::read(destination).unwrap(), bytes);
  task.join().unwrap();
}

#[test]
fn prepared_input_cancels_during_cached_file_hash_verification() {
  let (_temporary, root) = root();
  let destination = root.join("dataset.bin");
  let bytes = vec![b'v'; 256 * 1024];
  std::fs::write(&destination, &bytes).unwrap();
  let before = std::fs::metadata(&destination).unwrap();
  let record = FileRecord {
    target: FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset".into(),
    },
    size: bytes.len() as u64,
    sha256: Some(fs::hex(&Sha256::digest(&bytes))),
    storage: FileStorage::Object,
  };
  let (url, task) = mock(1, move |_, _| {
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::File {
        file: record.clone(),
      })
      .unwrap(),
    )
  });
  let mut checks = 0;
  let error = input_get_prepared(
    InputGetOptions {
      config: config(&root, &url),
      project_id: "project".into(),
      input_id: "dataset".into(),
      destination: destination.clone(),
    },
    &mut || {
      checks += 1;
      Ok(checks == 8)
    },
    &mut |_, _, _| panic!("cancelled input must not bind"),
  )
  .unwrap_err();
  task.join().unwrap();
  assert!(matches!(error, crate::error::ExpriError::DownloadCancelled));
  assert!(fs::unchanged(
    &before,
    &std::fs::metadata(&destination).unwrap()
  ));
  assert_eq!(std::fs::read(destination).unwrap(), bytes);
  assert!(
    !root
      .join(".expri-input-downloads/dataset.bin/input-record.json")
      .exists()
  );
}

#[test]
fn prepared_input_cancels_inside_an_object_range_without_acknowledging_partial_bytes() {
  let (_temporary, root) = root();
  let destination = root.join("dataset.bin");
  std::fs::write(&destination, b"keep previous input").unwrap();
  let bytes = vec![b'b'; 128 * 1024];
  let record = FileRecord {
    target: FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset".into(),
    },
    size: bytes.len() as u64,
    sha256: Some(fs::hex(&Sha256::digest(&bytes))),
    storage: FileStorage::Object,
  };
  let payload = bytes.clone();
  let (url, task) = mock(6, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::GetFile { .. } => Response::File {
          file: record.clone(),
        },
        Request::DownloadUrl { .. } => Response::Url {
          url: format!("{origin}/dataset"),
        },
        other => panic!("unexpected partial cancellation request: {other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      assert_eq!(
        request.headers["range"],
        format!("bytes=0-{}", payload.len() - 1)
      );
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes 0-{}/{}", payload.len() - 1, payload.len()),
        )],
        payload.clone(),
      )
    }
  });
  let config = config(&root, &url);
  let input = || InputGetOptions {
    config: config.clone(),
    project_id: "project".into(),
    input_id: "dataset".into(),
    destination: destination.clone(),
  };
  let stage = root.join(".expri-input-downloads/dataset.bin");
  let error = input_get_prepared(
    input(),
    &mut || {
      Ok(
        std::fs::metadata(stage.join("files/input"))
          .is_ok_and(|metadata| metadata.len() >= 64 * 1024),
      )
    },
    &mut |_, _, _| panic!("cancelled input must not bind"),
  )
  .unwrap_err();
  assert!(matches!(error, crate::error::ExpriError::DownloadCancelled));
  assert_eq!(std::fs::read(&destination).unwrap(), b"keep previous input");
  let progress: Value =
    serde_json::from_slice(&std::fs::read(stage.join("state.json")).unwrap()).unwrap();
  assert_eq!(progress["files"]["input"]["offset"], 0);
  assert_eq!(progress["files"]["input"]["verified"], false);
  let report = input_get(input()).unwrap();
  assert_eq!(report["resumed_bytes"], 0);
  assert_eq!(std::fs::read(destination).unwrap(), bytes);
  task.join().unwrap();
}

#[test]
fn private_input_accepts_a_destination_at_the_filesystem_filename_limit() {
  let (_temporary, root) = root();
  let destination = root.join(format!("{}.bin", "x".repeat(251)));
  let bytes = b"long input filename";
  std::fs::write(&destination, bytes).unwrap();
  let record = FileRecord {
    target: FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset".into(),
    },
    size: bytes.len() as u64,
    sha256: Some(fs::hex(&Sha256::digest(bytes))),
    storage: FileStorage::Object,
  };
  let (url, server) = mock(1, move |_, _| {
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::File {
        file: record.clone(),
      })
      .unwrap(),
    )
  });
  let report = input_get(InputGetOptions {
    config: config(&root, &url),
    project_id: "project".into(),
    input_id: "dataset".into(),
    destination: destination.clone(),
  })
  .unwrap();
  server.join().unwrap();
  assert_eq!(report["reused"], true);
  assert_eq!(std::fs::read(destination).unwrap(), bytes);
}
