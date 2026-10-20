use super::*;
use serde_json::json;
use std::sync::{Arc, Barrier};

#[test]
fn resource_pending_runs_are_active_but_tasks_cannot_set_pending() {
  assert!(is_active_status("pending"));
  assert!(!is_terminal_status("pending"));
  assert!(!TASK_STATUSES.contains(&"pending"));
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, Value) {
  let root = tempfile::tempdir().unwrap();
  let run_dir = root.path().canonicalize().unwrap().join("run-test");
  fs::create_dir(&run_dir).unwrap();
  let state = json!({
    "schema_version": 1, "run_id": "run-test", "status": "running",
    "started_at": "2026-10-14T01:00:00Z", "system_capture_id": "capture-test",
    "system_capture_status": "capturing", "command": ["python", "train.py"],
  });
  write_state(&run_dir.join("run-state.json"), &state).unwrap();
  (root, run_dir, state)
}

#[test]
fn nonblocking_state_updates_skip_busy_callbacks_and_preserve_noop_bytes() {
  let (_root, directory, original) = fixture();
  let path = directory.join("run-state.json");
  let bytes = fs::read(&path).unwrap();
  let lease = lock::run_state_lock(&directory).unwrap();
  let mut called = false;
  assert_eq!(
    try_update_state(&path, |_| {
      called = true;
      Ok(42)
    })
    .unwrap(),
    None
  );
  assert!(!called);
  assert_eq!(fs::read(&path).unwrap(), bytes);
  drop(lease);
  assert_eq!(try_update_state(&path, |_| Ok(42)).unwrap(), Some(42));
  assert_eq!(fs::read(&path).unwrap(), bytes);
  assert_eq!(
    try_update_state(&path, |state| {
      state["new_field"] = json!(true);
      Ok(())
    })
    .unwrap(),
    Some(())
  );
  let mut expected = original;
  expected["new_field"] = json!(true);
  assert_eq!(read_state(&path).unwrap(), expected);
}

#[test]
fn tasks_round_trip_waiting_without_changing_provenance_or_capture() {
  let (_root, directory, state) = fixture();
  let _supervisor = lock::run_lock(&directory).unwrap();
  let path = directory.join("run-state.json");
  let update = set_task_status(&directory, "waiting").unwrap();
  assert_eq!(update.previous_status, "running");
  assert!(update.changed);
  let saved = fs::read(&path).unwrap();
  assert!(!set_task_status(&directory, "waiting").unwrap().changed);
  assert_eq!(fs::read(&path).unwrap(), saved);
  let waiting = read_state(&path).unwrap();
  for key in [
    "started_at",
    "system_capture_id",
    "system_capture_status",
    "command",
  ] {
    assert_eq!(waiting[key], state[key]);
  }
  assert!(waiting["finished_at"].is_null());
  assert!(set_task_status(&directory, "running").unwrap().changed);
  assert_eq!(read_state(&path).unwrap(), state);
}

#[test]
fn task_phase_updates_the_current_attempt_without_rewriting_previous_attempts() {
  let (_root, directory, mut state) = fixture();
  crate::task_attempts::ensure_initial(&mut state).unwrap();
  let path = directory.join("run-state.json");
  write_state(&path, &state).unwrap();
  let _supervisor = lock::run_lock(&directory).unwrap();
  assert!(set_task_status(&directory, "waiting").unwrap().changed);
  let saved = read_state(&path).unwrap();
  assert_eq!(saved["task_attempts"]["attempts"][0]["status"], "waiting");
  assert_eq!(saved["started_at"], state["started_at"]);
  let bytes = fs::read(&path).unwrap();
  assert!(!set_task_status(&directory, "waiting").unwrap().changed);
  assert_eq!(fs::read(&path).unwrap(), bytes);
  assert!(set_task_status(&directory, "running").unwrap().changed);
  assert_eq!(read_state(&path).unwrap(), state);
}

#[test]
fn tasks_reject_nonlive_preparing_terminal_and_invalid_records_without_mutation() {
  let (_root, directory, state) = fixture();
  let path = directory.join("run-state.json");
  assert!(
    set_task_status(&directory, "waiting")
      .unwrap_err()
      .to_string()
      .contains("not active")
  );
  let _supervisor = lock::run_lock(&directory).unwrap();
  for value in [
    json!({"status":"preparing"}),
    json!({"status":"completed", "finished_at":"2026-10-14T02:00:00Z"}),
    json!({"status":"lost"}),
    json!({"status":"waiting", "finished_at":"invalid"}),
    json!({"schema_version":2}),
    json!({"run_id":"other"}),
    json!({"started_at":"invalid"}),
    json!({"detached":"true"}),
    json!({"detached":true}),
    json!({"output_dir":"/other/outputs"}),
  ] {
    let mut invalid = state.clone();
    invalid
      .as_object_mut()
      .unwrap()
      .extend(value.as_object().unwrap().clone());
    fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
    let original = fs::read(&path).unwrap();
    assert!(set_task_status(&directory, "waiting").is_err(), "{invalid}");
    assert_eq!(fs::read(&path).unwrap(), original);
  }
  for phase in [
    "pending",
    "preparing",
    "completed",
    "unknown",
    "Waiting",
    "",
  ] {
    assert!(set_task_status(&directory, phase).is_err());
  }
}

#[test]
fn supervisor_terminal_publication_wins_over_concurrent_task_updates() {
  let (_root, directory, mut terminal) = fixture();
  let _supervisor = lock::run_lock(&directory).unwrap();
  let path = directory.join("run-state.json");
  update_state(&path, |state| {
    state["additional_metadata"] = json!({"value":42});
    Ok(())
  })
  .unwrap();
  terminal["status"] = json!("completed");
  terminal["finished_at"] = json!("2026-10-14T02:00:00Z");
  terminal["exit_code"] = json!(0);
  let ready = Arc::new(Barrier::new(2));
  let task_dir = directory.clone();
  let task_ready = ready.clone();
  let task = std::thread::spawn(move || {
    task_ready.wait();
    for phase in ["waiting", "running"].into_iter().cycle().take(100) {
      if set_task_status(&task_dir, phase).is_err() {
        break;
      }
    }
  });
  ready.wait();
  write_state(&path, &terminal).unwrap();
  task.join().unwrap();
  let completed = read_state(&path).unwrap();
  assert_eq!(completed["status"], "completed");
  assert_eq!(completed["exit_code"], 0);
  assert_eq!(completed["started_at"], terminal["started_at"]);
  assert_eq!(completed["finished_at"], terminal["finished_at"]);
  assert_eq!(completed["additional_metadata"]["value"], 42);
  assert!(set_task_status(&directory, "waiting").is_err());
  let mut stale = terminal.clone();
  stale["status"] = json!("running");
  assert!(write_state(&path, &stale).is_err());
  assert_eq!(read_state(&path).unwrap(), completed);
}

#[test]
fn malformed_oversized_or_symlink_state_never_rewrites_a_record() {
  let (_root, directory, state) = fixture();
  let _supervisor = lock::run_lock(&directory).unwrap();
  let path = directory.join("run-state.json");
  for bytes in [
    b"corrupt".to_vec(),
    b"[]".to_vec(),
    vec![b' '; MAX_STATE_BYTES + 1],
  ] {
    fs::write(&path, &bytes).unwrap();
    assert!(set_task_status(&directory, "waiting").is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
  }
  fs::remove_file(&path).unwrap();
  #[cfg(unix)]
  {
    let target = directory.join("other.json");
    fs::write(&target, serde_json::to_vec(&state).unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(set_task_status(&directory, "waiting").is_err());
    assert_eq!(
      fs::read(&target).unwrap(),
      serde_json::to_vec(&state).unwrap()
    );
  }
}
