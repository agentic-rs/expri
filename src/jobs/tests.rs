use super::*;

fn fixture(status: &str, detached: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let root = fs::canonicalize(temporary.path()).unwrap();
  let directory = root.join(".expri/runs/run-test");
  fs::create_dir_all(directory.join("code")).unwrap();
  fs::create_dir(directory.join("outputs")).unwrap();
  let state = json!({
    "schema_version": 1, "run_id": "run-test", "task": "train",
    "status": status, "detached": detached,
    "code_dir": directory.join("code"), "output_dir": directory.join("outputs"),
  });
  publish(&directory, &state);
  (temporary, root, directory)
}

fn publish(directory: &Path, state: &Value) {
  let temporary = directory.join("run-state.json.test-tmp");
  fs::write(&temporary, serde_json::to_vec(state).unwrap()).unwrap();
  fs::rename(temporary, directory.join("run-state.json")).unwrap();
}

fn cancel() -> JobRequest {
  JobRequest::Cancel {
    run_id: "run-test".to_string(),
  }
}

#[test]
fn live_status_reports_lost_without_mutating_records_or_creating_leases() {
  let (_temporary, root, directory) = fixture("running", true);
  let before = fs::read(directory.join("run-state.json")).unwrap();
  let lost = status_at(&root, "run-test").unwrap();
  assert_eq!(lost["status"], "lost");
  assert_eq!(lost["recorded_status"], "running");
  assert_eq!(lost["alive"], false);
  assert!(
    query_at(&cancel(), &root)
      .unwrap_err()
      .to_string()
      .contains("not active")
  );
  assert!(!directory.join(".run.lock").exists());
  assert!(!directory.join(CANCEL_FILE).exists());
  assert_eq!(fs::read(directory.join("run-state.json")).unwrap(), before);
  let lease = lock::run_lock(&directory).unwrap();
  let live = status_at(&root, "run-test").unwrap();
  assert_eq!(live["status"], "running");
  assert_eq!(live["alive"], true);
  drop(lease);
}

#[test]
fn cancellation_is_idempotent_and_terminal_runs_are_read_only() {
  let (_temporary, root, directory) = fixture("preparing", true);
  let lease = lock::run_lock(&directory).unwrap();
  let before = fs::read(directory.join("run-state.json")).unwrap();
  for _ in 0..2 {
    let result = query_at(&cancel(), &root).unwrap();
    assert_eq!(result["cancel_requested"], true);
    assert_eq!(result["status"], "preparing");
    assert_eq!(fs::metadata(directory.join(CANCEL_FILE)).unwrap().len(), 0);
  }
  assert_eq!(fs::read(directory.join("run-state.json")).unwrap(), before);
  drop(lease);
  for terminal in TERMINAL_STATUSES {
    let (_temporary, root, directory) = fixture(terminal, true);
    assert_eq!(
      query_at(&cancel(), &root).unwrap()["already_finished"],
      true
    );
    assert!(!directory.join(CANCEL_FILE).exists());
    assert!(!directory.join(".run.lock").exists());
  }
}

#[test]
fn foreground_legacy_records_can_be_inspected_but_not_cancelled() {
  let (_temporary, root, directory) = fixture("running", false);
  publish(
    &directory,
    &json!({"run_id":"run-test", "status":"running"}),
  );
  fs::remove_dir(directory.join("code")).unwrap();
  fs::remove_dir(directory.join("outputs")).unwrap();
  let status = status_at(&root, "run-test").unwrap();
  assert_eq!(status["status"], "running");
  assert_eq!(status["detached"], false);
  assert!(status.get("service_sync").is_none());
  assert!(
    query_at(&cancel(), &root)
      .unwrap_err()
      .to_string()
      .contains("detached")
  );
}

#[test]
fn log_tail_preserves_binary_bytes_and_handles_line_boundaries() {
  let (_temporary, root, directory) = fixture("completed", true);
  fs::create_dir(directory.join("logs")).unwrap();
  for bytes in [
    b"first\nsecond\n\xfflast\n".as_slice(),
    b"first\nsecond\n\xfflast".as_slice(),
  ] {
    fs::write(directory.join("logs/stdout.log"), bytes).unwrap();
    for (tail, expected) in [
      (0, b"".as_slice()),
      (
        1,
        bytes.split_inclusive(|b| *b == b'\n').next_back().unwrap(),
      ),
      (100, bytes),
    ] {
      let mut output = Vec::new();
      stream_to(&root, "run-test", "stdout", false, tail, &mut output).unwrap();
      assert_eq!(output, expected);
    }
  }
  fs::write(directory.join("logs/stderr.log"), b"error\0\xff\n").unwrap();
  let mut output = Vec::new();
  stream_to(&root, "run-test", "stderr", false, 100, &mut output).unwrap();
  assert_eq!(output, b"error\0\xff\n");
  assert!(stream_to(&root, "run-test", "other", false, 100, &mut output).is_err());
}

#[test]
fn log_follow_with_zero_tail_waits_for_creation_and_drains_terminal_bytes() {
  let (_temporary, root, directory) = fixture("preparing", true);
  let lease = lock::run_lock(&directory).unwrap();
  let worker_directory = directory.clone();
  let worker = thread::spawn(move || {
    thread::sleep(Duration::from_millis(100));
    fs::create_dir(worker_directory.join("logs")).unwrap();
    fs::write(worker_directory.join("logs/stdout.log"), b"hello\n").unwrap();
    thread::sleep(Duration::from_millis(100));
    let mut log = OpenOptions::new()
      .append(true)
      .open(worker_directory.join("logs/stdout.log"))
      .unwrap();
    log.write_all(b"final\0\xff\n").unwrap();
    let mut state: Value =
      serde_json::from_slice(&fs::read(worker_directory.join("run-state.json")).unwrap()).unwrap();
    state["status"] = json!("completed");
    publish(&worker_directory, &state);
    drop(lease);
  });
  let mut output = Vec::new();
  stream_to(&root, "run-test", "stdout", true, 0, &mut output).unwrap();
  worker.join().unwrap();
  assert_eq!(output, b"hello\nfinal\0\xff\n");
}

#[test]
fn state_size_identity_and_detached_paths_are_checked() {
  let (_temporary, root, directory) = fixture("completed", true);
  assert!(
    status_at(&root, "../run-test")
      .unwrap_err()
      .to_string()
      .contains("invalid run ID")
  );
  let original: Value =
    serde_json::from_slice(&fs::read(directory.join("run-state.json")).unwrap()).unwrap();
  for (field, value) in [
    ("run_id", json!("other")),
    ("code_dir", json!("/tmp/other")),
    ("output_dir", json!("/tmp/other")),
    ("schema_version", json!(2)),
    ("detached", json!("yes")),
  ] {
    let mut state = original.clone();
    state[field] = value;
    publish(&directory, &state);
    assert!(status_at(&root, "run-test").is_err(), "field: {field}");
  }
  fs::write(
    directory.join("run-state.json"),
    vec![b' '; STATE_LIMIT as usize + 1],
  )
  .unwrap();
  assert!(
    status_at(&root, "run-test")
      .unwrap_err()
      .to_string()
      .contains("size limit")
  );
}

#[cfg(unix)]
#[test]
fn linked_boundaries_control_files_and_logs_are_rejected() {
  use std::os::unix::fs::symlink;
  for component in [".expri", ".expri/runs", ".expri/runs/run-test"] {
    let (_temporary, root, _directory) = fixture("running", true);
    let path = root.join(component);
    let actual = root.join("moved");
    fs::rename(&path, &actual).unwrap();
    symlink(&actual, &path).unwrap();
    assert!(status_at(&root, "run-test").is_err());
  }
  let (_temporary, root, directory) = fixture("running", true);
  let lease = lock::run_lock(&directory).unwrap();
  let outside = root.join("outside");
  fs::write(&outside, b"secret").unwrap();
  symlink(&outside, directory.join(CANCEL_FILE)).unwrap();
  assert!(query_at(&cancel(), &root).is_err());
  assert_eq!(fs::read(&outside).unwrap(), b"secret");
  fs::remove_file(directory.join(CANCEL_FILE)).unwrap();
  fs::create_dir(directory.join("logs")).unwrap();
  symlink(&outside, directory.join("logs/stdout.log")).unwrap();
  assert!(stream_to(&root, "run-test", "stdout", false, 100, &mut Vec::new()).is_err());
  fs::remove_file(directory.join("logs/stdout.log")).unwrap();
  fs::remove_dir(directory.join("logs")).unwrap();
  symlink(&root, directory.join("logs")).unwrap();
  assert!(stream_to(&root, "run-test", "stdout", false, 100, &mut Vec::new()).is_err());
  drop(lease);
}
