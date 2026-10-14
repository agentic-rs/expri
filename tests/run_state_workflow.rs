//! A task controls its active phase locally without project/server configuration.

use std::fs;
use std::process::{Command, Output};

use expri_core::{lock, run_state};
use serde_json::{Value, json};

fn success(output: Output) -> String {
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  String::from_utf8(output.stdout).unwrap()
}

#[test]
fn task_cli_uses_run_environment_in_a_frozen_working_directory() {
  let root = tempfile::tempdir().unwrap();
  let root_path = root.path().canonicalize().unwrap();
  let directory = root_path.join("run-test");
  fs::create_dir(&directory).unwrap();
  let state_path = directory.join("run-state.json");
  let state = json!({"schema_version":1,"run_id":"run-test","status":"running",
    "started_at":"2026-10-14T01:00:00Z","system_capture_id":"capture-test"});
  run_state::write_state(&state_path, &state).unwrap();
  let lease = lock::run_lock(&directory).unwrap();
  let invoke = |args: &[&str]| {
    Command::new(env!("CARGO_BIN_EXE_expri"))
      .args(args)
      .env("EXPRI_RUN_DIR", &directory)
      .current_dir(&root_path)
      .output()
      .unwrap()
  };
  let report: Value = serde_json::from_str(&success(invoke(&[
    "runs",
    "set-status",
    "waiting",
    "--json",
  ])))
  .unwrap();
  assert_eq!(
    report,
    json!({"run_id":"run-test","status":"waiting","previous_status":"running","changed":true})
  );
  let waiting = fs::read(&state_path).unwrap();
  let unchanged: Value = serde_json::from_str(&success(invoke(&[
    "runs",
    "set-status",
    "waiting",
    "--quiet",
    "--json",
  ])))
  .unwrap();
  assert_eq!(unchanged["changed"], false);
  assert_eq!(fs::read(&state_path).unwrap(), waiting);
  assert!(success(invoke(&["runs", "set-status", "running", "--quiet"])).is_empty());
  let saved: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
  assert_eq!(saved, state);
  let shown = success(invoke(&["runs", "set-status", "waiting"]));
  assert!(shown.contains("waiting") && shown.contains("run-test"));
  assert!(serde_json::from_str::<Value>(&shown).is_err());
  let invalid = invoke(&["runs", "set-status", "completed"]);
  assert!(!invalid.status.success());
  let remote = invoke(&["-T", "gpu", "runs", "set-status", "running"]);
  assert!(!remote.status.success());
  assert!(String::from_utf8_lossy(&remote.stderr).contains("omit -T"));
  drop(lease);
  let orphaned = invoke(&["runs", "set-status", "running"]);
  assert!(!orphaned.status.success());
  assert!(String::from_utf8_lossy(&orphaned.stderr).contains("not active"));
}
