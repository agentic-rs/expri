use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::context::Environment;
use super::*;

fn standalone(path: &Path) -> Result<Run> {
  Run::builder()
    .output_dir(path)
    .with_environment(Environment::default())
}

fn managed(path: &Path, attempt: u64, offset: Option<u64>) -> Result<Run> {
  Run::builder()
    .output_dir(path)
    .with_environment(Environment {
      attempt: Some(attempt.to_string().into()),
      step_offset: offset.map(|value| value.to_string().into()),
      ..Environment::default()
    })
}

fn rows(path: &Path) -> Vec<Value> {
  fs::read_to_string(path.join("metrics.jsonl"))
    .unwrap()
    .lines()
    .filter(|row| !row.trim().is_empty())
    .map(|row| serde_json::from_str(row).unwrap())
    .collect()
}

fn seed(path: &Path, steps: &[u64]) {
  fs::create_dir_all(path).unwrap();
  let mut bytes = Vec::new();
  for step in steps {
    bytes.extend(serde_json::to_vec(&json!({"step": step, "metrics": {"loss": 1}})).unwrap());
    bytes.push(b'\n');
  }
  fs::write(path.join("metrics.jsonl"), bytes).unwrap();
}

#[test]
fn virtual_history_uses_maximum_and_preserves_relative_gaps_and_prefix() {
  let temp = TempDir::new().unwrap();
  seed(temp.path(), &[1000, 10]);
  let prefix = fs::read(temp.path().join("metrics.jsonl")).unwrap();
  let run = standalone(temp.path()).unwrap();
  assert_eq!(run.initial_step(), 1001);
  run.log(10, [("loss", 0.5)]).unwrap();
  run.log(12, [("loss", 0.4)]).unwrap();
  run
    .log_with_mode(2000, [("loss", 0.3)], StepMode::Global)
    .unwrap();
  let events = rows(temp.path());
  assert_eq!(events[2]["step"], 1011);
  assert_eq!(events[2]["local_step"], 10);
  assert_eq!(events[2]["step_mode"], "relative");
  assert!(events[2].get("attempt").is_none());
  assert_eq!(events[3]["step"], 1013);
  assert_eq!(events[4]["step"], 2000);
  assert_eq!(events[4]["step_mode"], "global");
  assert!(events[4].get("local_step").is_none());
  assert!(
    fs::read(temp.path().join("metrics.jsonl"))
      .unwrap()
      .starts_with(&prefix)
  );
  assert_eq!(standalone(temp.path()).unwrap().initial_step(), 2001);
}

#[test]
fn managed_attempt_base_is_committed_before_global_rows_and_shared() {
  let temp = TempDir::new().unwrap();
  seed(temp.path(), &[1000]);
  let first = managed(temp.path(), 2, Some(1001)).unwrap();
  let origin_path = temp.path().join("attempts/2/step-origin.json");
  let origin: Value = serde_json::from_slice(&fs::read(&origin_path).unwrap()).unwrap();
  assert_eq!(
    origin,
    json!({"schema_version": 2, "attempt": 2, "step_offset": "1001"})
  );
  first
    .log_with_mode(u64::MAX, [("loss", 1_u64)], StepMode::Global)
    .unwrap();
  let second = managed(temp.path(), 2, Some(1001)).unwrap();
  assert_eq!(second.initial_step(), 1001);
  second.log(0, [("loss", 0.5)]).unwrap();
  assert_eq!(rows(temp.path())[2]["attempt"], 2);
  assert_eq!(rows(temp.path())[2]["step"], 1001);
  assert!(managed(temp.path(), 2, Some(2000)).is_err());
  assert!(standalone(temp.path()).is_err());
}

#[test]
fn legacy_nonzero_origin_accepts_only_global_without_rewriting() {
  let temp = TempDir::new().unwrap();
  seed(temp.path(), &[100]);
  let directory = temp.path().join("attempts/2");
  fs::create_dir_all(&directory).unwrap();
  let origin =
    b"{\"schema_version\":1,\"attempt\":2,\"step_offset\":\"101\",\"first_local_step\":\"10\"}\n";
  fs::write(directory.join("step-origin.json"), origin).unwrap();
  let run = managed(temp.path(), 2, Some(101)).unwrap();
  assert_eq!(run.initial_step(), 101);
  assert!(run.log(11, [("loss", 1)]).is_err());
  assert_eq!(rows(temp.path()).len(), 1);
  run
    .log_with_mode(112, [("loss", 1)], StepMode::Global)
    .unwrap();
  assert_eq!(
    fs::read(directory.join("step-origin.json")).unwrap(),
    origin
  );
  assert_eq!(rows(temp.path())[1]["step"], 112);
}

#[test]
fn native_first_attempt_context_shares_base_without_attempt_environment() {
  let temp = TempDir::new().unwrap();
  let run_dir = temp.path().join("run-one");
  let output = run_dir.join("outputs");
  seed(&output, &[50]);
  fs::write(
    run_dir.join("run-state.json"),
    serde_json::to_vec(&json!({
      "schema_version": 1, "run_id": "run-one",
      "task_attempts": {"protocol_version": 1, "current_attempt": 1},
    }))
    .unwrap(),
  )
  .unwrap();
  let build = || {
    Run::builder()
      .with_environment(Environment {
        output_dir: Some(output.clone()),
        run_dir: Some(run_dir.clone()),
        run_id: Some("run-one".into()),
        ..Environment::default()
      })
      .unwrap()
  };
  let first = build();
  first.log(0, [("loss", 1)]).unwrap();
  let second = build();
  assert_eq!(second.initial_step(), 51);
  second.log(2, [("loss", 0.5)]).unwrap();
  assert_eq!(rows(&output)[2]["step"], 53);
  assert_eq!(rows(&output)[2]["attempt"], 1);
}

#[test]
fn redirected_output_does_not_inherit_a_run_or_attempt() {
  let temp = TempDir::new().unwrap();
  let output = temp.path().join("other");
  let run_dir = temp.path().join("run-one");
  let run = Run::builder()
    .output_dir(&output)
    .with_environment(Environment {
      output_dir: Some(run_dir.join("outputs")),
      run_dir: Some(run_dir),
      attempt: Some("2".into()),
      step_offset: Some("invalid".into()),
      ..Environment::default()
    })
    .unwrap();
  assert_eq!(run.initial_step(), 0);
  assert!(run.run_dir().is_none());
  run.log(0, [("loss", 1)]).unwrap();
  assert!(rows(&output)[0].get("attempt").is_none());
  assert!(run.set_status(TaskStatus::Running).is_err());
}

#[test]
fn malformed_history_and_exhaustion_never_change_existing_bytes() {
  let temp = TempDir::new().unwrap();
  for raw in [
    "{\"step\":1,\"metrics\":{\"loss\":1}}",
    "{\"step\":true,\"metrics\":{\"loss\":1}}\n",
    "{\"step\":1,\"metrics\":{\"loss\":true}}\n",
    "{\"step\":1,\"metrics\":{},\"timestamp\":\"garbage\"}\n",
    "{\"step\":1,\"metrics\":{\"loss\":1},\"schema_version\":2}\n",
    "{\"step\":18446744073709551615,\"metrics\":{\"loss\":1}}\n",
  ] {
    fs::write(temp.path().join("metrics.jsonl"), raw).unwrap();
    assert!(standalone(temp.path()).is_err(), "{raw}");
    assert_eq!(
      fs::read(temp.path().join("metrics.jsonl")).unwrap(),
      raw.as_bytes()
    );
  }
}

#[test]
fn invalid_new_events_preserve_the_prefix() {
  let temp = TempDir::new().unwrap();
  seed(temp.path(), &[100]);
  let run = standalone(temp.path()).unwrap();
  let prefix = fs::read(temp.path().join("metrics.jsonl")).unwrap();
  for name in ["", " ", "bad\nname", "bad\u{80}name"] {
    assert!(run.log(0, [(name, 1)]).is_err());
  }
  for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
    assert!(run.log(0, [("loss", value)]).is_err());
  }
  assert!(run.log(u64::MAX, [("loss", 1)]).is_err());
  assert!(run.log(0, [("loss", 1), ("loss", 2)]).is_err());
  assert!(run.log(0, std::iter::empty::<(&str, i64)>()).is_err());
  assert_eq!(fs::read(temp.path().join("metrics.jsonl")).unwrap(), prefix);
  run.log(0, [("count", u64::MAX)]).unwrap();
  assert_eq!(
    rows(temp.path())[1]["metrics"]["count"].as_u64(),
    Some(u64::MAX)
  );
}

#[test]
fn an_incomplete_row_added_after_init_cannot_be_concatenated() {
  let temp = TempDir::new().unwrap();
  let run = standalone(temp.path()).unwrap();
  fs::write(temp.path().join("metrics.jsonl"), b"partial").unwrap();
  assert!(run.log(0, [("loss", 1)]).is_err());
  assert_eq!(
    fs::read(temp.path().join("metrics.jsonl")).unwrap(),
    b"partial"
  );
}

#[test]
fn immutable_params_are_isolated_by_attempt_and_limit_depth() {
  let temp = TempDir::new().unwrap();
  let first = managed(temp.path(), 1, None).unwrap();
  let params = json!({"seed": 42, "optimizer": {"lr": 0.125}});
  first.params(&params).unwrap();
  first.params(&params).unwrap();
  assert!(first.params(&json!({"seed": 43})).is_err());
  assert!(first.params(&json!([42])).is_err());
  let second = managed(temp.path(), 2, Some(0)).unwrap();
  second.params(&json!({"seed": 43})).unwrap();
  assert!(temp.path().join("attempts/2/params.json").is_file());
  let mut nested = Value::Null;
  for _ in 0..64 {
    nested = json!([nested]);
  }
  assert!(second.params(&json!({"nested": nested})).is_err());
}

#[test]
fn symlinked_metrics_params_origins_and_attempt_directories_are_rejected() {
  let temp = TempDir::new().unwrap();
  let other = temp.path().join("target");
  fs::write(&other, b"untouched").unwrap();
  let output = temp.path().join("outputs");
  fs::create_dir(&output).unwrap();
  symlink(&other, output.join("metrics.jsonl")).unwrap();
  assert!(standalone(&output).is_err());
  fs::remove_file(output.join("metrics.jsonl")).unwrap();
  let run = standalone(&output).unwrap();
  symlink(&other, output.join("params.json")).unwrap();
  assert!(run.params(&json!({})).is_err());
  symlink(temp.path(), output.join("attempts")).unwrap();
  assert!(managed(&output, 2, Some(0)).is_err());
  fs::remove_file(output.join("attempts")).unwrap();
  fs::create_dir_all(output.join("attempts/2")).unwrap();
  symlink(&other, output.join("attempts/2/step-origin.json")).unwrap();
  assert!(managed(&output, 2, Some(0)).is_err());
  assert_eq!(fs::read(&other).unwrap(), b"untouched");
}

#[test]
fn concurrent_managed_writers_keep_complete_rows_and_one_base() {
  let temp = TempDir::new().unwrap();
  seed(temp.path(), &[1000]);
  std::thread::scope(|scope| {
    for writer in 0..4 {
      let path = temp.path();
      scope.spawn(move || {
        let run = managed(path, 2, Some(1001)).unwrap();
        assert_eq!(run.initial_step(), 1001);
        for step in 0..30 {
          run.log(step, [("writer", writer)]).unwrap();
        }
      });
    }
  });
  let events = rows(temp.path());
  assert_eq!(events.len(), 121);
  for event in &events[1..] {
    assert_eq!(
      event["step"].as_u64().unwrap(),
      1001 + event["local_step"].as_u64().unwrap()
    );
  }
}

#[test]
fn cli_wrappers_use_captured_context_and_argv_without_a_shell() {
  let temp = TempDir::new().unwrap();
  let binary = temp.path().join("expri binary");
  fs::write(
    &binary,
    "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf '{\"changed\":true}\\n'\n",
  )
  .unwrap();
  fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
  let run_dir = temp.path().join("run-one");
  let run = Run::builder()
    .run_dir(&run_dir)
    .cli_binary(binary.as_os_str())
    .with_environment(Environment::default())
    .unwrap();
  assert_eq!(
    run.set_status(TaskStatus::Waiting).unwrap(),
    json!({"changed": true})
  );
  let args = fs::read_to_string(temp.path().join("expri binary.args")).unwrap();
  assert_eq!(
    args.lines().collect::<Vec<_>>(),
    [
      "runs",
      "set-status",
      "waiting",
      "--json",
      "--run-dir",
      run_dir.to_str().unwrap()
    ]
  );
  run
    .register_artifact("checkpoint with spaces;$.pt", ["latest"])
    .unwrap();
  let args = fs::read_to_string(temp.path().join("expri binary.args")).unwrap();
  assert_eq!(
    args.lines().collect::<Vec<_>>(),
    [
      "artifact",
      "--json",
      "register",
      run_dir
        .join("outputs/checkpoint with spaces;$.pt")
        .to_str()
        .unwrap(),
      "--run-dir",
      run_dir.to_str().unwrap(),
      "--label",
      "latest",
    ]
  );
  assert!(run.register_artifact("x", ["--help"]).is_err());
  assert!(run.register_artifact("x", ["best", "best"]).is_err());
  run.close().unwrap();
  assert!(run.is_closed());
  assert!(run.log(0, [("loss", 1)]).is_err());
  assert!(run.set_status(TaskStatus::Running).is_err());
  assert!(!run_dir.join("run-state.json").exists());
}

#[test]
fn explicit_run_and_output_cannot_point_at_different_runs() {
  let temp = TempDir::new().unwrap();
  assert!(
    Run::builder()
      .run_dir(temp.path().join("run"))
      .output_dir(temp.path().join("other"))
      .with_environment(Environment::default())
      .is_err()
  );
  assert!(
    Run::builder()
      .run_dir(temp.path().join("run/../other"))
      .with_environment(Environment::default())
      .is_err()
  );
}

#[test]
fn inherited_process_context_is_rejected_before_local_or_cli_operations() {
  let temp = TempDir::new().unwrap();
  let mut run = Run::builder()
    .run_dir(temp.path().join("run-one"))
    .cli_binary("must-not-be-executed")
    .with_environment(Environment::default())
    .unwrap();
  // Model the PID change observed by a forked child without forking Rust's
  // multithreaded test runner. Every operation must reject before taking locks.
  run.owner_pid = std::process::id().wrapping_add(1);
  for error in [
    run.log(0, [("loss", 1)]).unwrap_err(),
    run.params(&json!({})).unwrap_err(),
    run.flush().unwrap_err(),
    run.set_status(TaskStatus::Running).unwrap_err(),
    run
      .register_artifact("checkpoint.pt", ["best"])
      .unwrap_err(),
    run.close().unwrap_err(),
  ] {
    assert!(error.to_string().contains("inherited through fork"));
  }
  assert!(run.is_closed());
  assert!(rows(run.output_dir()).is_empty());
}

#[test]
fn cli_success_requires_a_json_object_report() {
  for response in ["[]", "null", "42", "\"text\""] {
    let error = cli_report(Command::new("/bin/echo").arg(response)).unwrap_err();
    assert!(error.to_string().contains("JSON object report"));
  }
}
