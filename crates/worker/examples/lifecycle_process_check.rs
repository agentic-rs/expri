//! Isolated Linux process regression: `cargo run -p expri-worker --example lifecycle_process_check`.
//! The helper dispatches detached worker subprocesses itself and never contacts a service.

#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("This lifecycle process check requires Linux.");
}

#[cfg(target_os = "linux")]
fn main() {
  if let Err(error) = linux::run() {
    eprintln!("{error}");
    std::process::exit(1);
  }
}

#[cfg(target_os = "linux")]
mod linux {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::path::{Path, PathBuf};
  use std::process::Command;
  use std::thread;
  use std::time::{Duration, Instant};

  use expri_core::error::{ExpriError, Result};
  use expri_core::protocol::RunRequest;
  use expri_core::run_control::{ControlAction, ControlCommand};
  use expri_worker::{node, run_control};
  use serde_json::{Value, json};

  pub fn run() -> Result<()> {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.get(1).is_some_and(|argument| argument == "node") {
      let run = Path::new(&arguments[4]);
      return match arguments[2].as_str() {
        "run-worker" => node::run::worker(run),
        "continue-worker" => expri_worker::continuation::worker(run, &arguments[6]),
        _ => Err(ExpriError::Message("unknown fixture worker command".into())),
      };
    }
    if arguments.len() != 1 {
      return Err(ExpriError::Message(
        "fixture does not launch shared services or contact a server".into(),
      ));
    }
    let root = tempfile::tempdir()?;
    // Keep collection/profile state away from a developer's real expri store.
    if std::env::var_os("EXPRI_LIFECYCLE_CHECK_CHILD").is_none() {
      let status = Command::new(std::env::current_exe()?)
        .env("EXPRI_LIFECYCLE_CHECK_CHILD", "1")
        .env("EXPRI_STORAGE_DIR", root.path().join("profiles"))
        .status()?;
      if !status.success() {
        return Err(ExpriError::Message(
          "isolated lifecycle fixture failed".into(),
        ));
      }
      return Ok(());
    }
    let repo = root.path().join("repo");
    let bin = root.path().join("bin");
    fs::create_dir_all(repo.join(".expri"))?;
    fs::create_dir(&bin)?;
    fs::write(repo.join(".expri/checkout.manifest"), "train.py\n")?;
    fs::write(bin.join("uv"), FAKE_UV)?;
    fs::set_permissions(bin.join("uv"), fs::Permissions::from_mode(0o700))?;
    fs::write(repo.join("train.py"), TASK)?;
    let request: RunRequest = serde_json::from_value(json!({
      "name":"lifecycle-check", "command":["python3","train.py","1"], "detach":true,
      "environment":{"base_python":null,"env":{"PATH":format!("{}:/usr/bin:/bin", bin.display())}},
    }))?;
    let receipt = node::run::execute_request_at(&request, &repo)?.unwrap();
    let original = PathBuf::from(receipt["run_dir"].as_str().unwrap());
    let mut children = Cleanup(vec![original.clone()]);
    let checked: Result<()> = (|| {
      wait_for(|| original.join("outputs/checkpoint.bin").exists())?;
      let original_metrics = fs::read(original.join("outputs/metrics.jsonl"))?;
      expri_client::registrations::register(&original, "outputs/checkpoint.bin")?;
      fs::write(bin.join("uv"), "#!/bin/sh\nexit 7\n")?;
      let rejected = command(&original, ControlAction::Restart, "preflight-rejected")?;
      let failure = run_control::execute(&original, &rejected)?;
      assert_eq!(failure.status, "failed");
      let failed_run = original
        .parent()
        .unwrap()
        .join(failure.new_run_id.as_ref().unwrap());
      assert_eq!(read_state(&failed_run)?["status"], "failed");
      assert_ne!(
        read_state(&failed_run)?["system_capture_status"],
        "capturing"
      );
      assert_eq!(read_state(&original)?["status"], "running");
      assert_eq!(
        fs::read(original.join("outputs/metrics.jsonl"))?,
        original_metrics
      );
      assert_eq!(run_control::execute(&original, &rejected)?, failure);
      fs::write(bin.join("uv"), FAKE_UV)?;
      // A newer worker checkout must never replace the run's recorded source.
      fs::write(
        repo.join("train.py"),
        "raise Exception('wrong checkout source')\n",
      )?;
      let mut continuation = command(&original, ControlAction::Continue, "continue-one")?;
      continuation.arguments = Some(vec!["train.py".into(), "2".into()]);
      continuation.checkpoint = Some("outputs/checkpoint.bin".into());
      let continued = run_control::execute(&original, &continuation)?;
      assert_eq!(continued.status, "applied", "{continued:?}");
      wait_for(|| {
        fs::read_to_string(original.join("outputs/seen-attempt")).is_ok_and(|value| value == "2")
      })?;
      let after = fs::read(original.join("outputs/metrics.jsonl"))?;
      assert!(after.starts_with(&original_metrics));
      let rows = metrics(&original)?;
      assert_eq!(rows.len(), 2);
      assert_eq!(rows[1]["step"], 1);
      assert_eq!(rows[1]["metrics"]["value"], 2);
      assert_eq!(
        fs::read_to_string(original.join("outputs/source-version"))?,
        "recorded"
      );
      assert_eq!(
        fs::read_to_string(original.join("outputs/used-checkpoint"))?,
        "saved checkpoint"
      );
      let same = run_control::execute(&original, &continuation)?;
      assert_eq!(
        same, continued,
        "continuation replay must retain one attempt"
      );
      let state = read_state(&original)?;
      assert_eq!(
        state["task_attempts"]["attempts"].as_array().unwrap().len(),
        2
      );
      let mut restart = command(&original, ControlAction::Restart, "restart-one")?;
      restart.arguments = Some(vec!["train.py".into(), "3".into()]);
      let restarted = run_control::execute(&original, &restart)?;
      assert_eq!(restarted.status, "applied", "{restarted:?}");
      let new_id = restarted.new_run_id.as_ref().unwrap();
      assert_ne!(new_id, original.file_name().unwrap().to_str().unwrap());
      let successor = original.parent().unwrap().join(new_id);
      children.0.push(successor.clone());
      wait_for(|| successor.join("outputs/checkpoint.bin").exists())?;
      let rows = metrics(&successor)?;
      assert_eq!(rows.len(), 1);
      assert_eq!(rows[0]["step"], 0);
      assert_eq!(rows[0]["metrics"]["value"], 3);
      assert_eq!(
        fs::read_to_string(successor.join("outputs/source-version"))?,
        "recorded"
      );
      assert_eq!(
        fs::read_to_string(successor.join("outputs/used-checkpoint"))?,
        "none"
      );
      assert_eq!(read_state(&successor)?["restarted_from"], receipt["run_id"]);
      assert_eq!(run_control::execute(&original, &restart)?, restarted);
      assert_eq!(fs::read_dir(original.parent().unwrap())?.count(), 3);
      assert_eq!(fs::read(original.join("outputs/metrics.jsonl"))?, after);
      Ok(())
    })();
    checked?;
    fs::write(repo.join("train.py"), TASK)?;
    resource_checks(&repo, &request, &mut children)?;
    println!(
      "Lifecycle process check passed: recorded source, same-run continuation/history, fresh restart, and replay."
    );
    Ok(())
  }

  fn disk_policy(blocked: bool) -> expri_core::resource_requirements::ResourceRequirements {
    serde_json::from_value(json!({"disk":[{"path":"outputs", "min_free_bytes":if blocked {9_000_000_000_000_000_u64} else {1}, "resume_free_bytes":if blocked {9_000_000_000_000_001_u64} else {2}}], "recovery_seconds":0})).unwrap()
  }

  fn set_policy(run: &Path, blocked: bool, id: &str) -> Result<()> {
    let mut change = command(run, ControlAction::SetRequirements, id)?;
    change.requirements = Some(disk_policy(blocked));
    assert_eq!(run_control::execute(run, &change)?.status, "applied");
    Ok(())
  }

  fn resource_checks(repo: &Path, request: &RunRequest, children: &mut Cleanup) -> Result<()> {
    let mut request = request.clone();
    request.requirements = disk_policy(true);
    let receipt = node::run::execute_request_at(&request, repo)?.unwrap();
    let run = PathBuf::from(receipt["run_dir"].as_str().unwrap());
    children.0.push(run.clone());
    wait_for(|| read_state(&run).is_ok_and(|state| state["status"] == "pending"))?;
    assert!(!run.join("outputs/metrics.jsonl").exists());
    set_policy(&run, false, "resources-before-start")?;
    wait_for(|| run.join("outputs/checkpoint.bin").exists())?;
    expri_client::registrations::register(&run, "outputs/checkpoint.bin")?;
    set_policy(&run, true, "resources-before-continue")?;
    wait_for(|| read_state(&run).is_ok_and(|state| state["status"] == "paused"))?;
    let mut continuation = command(&run, ControlAction::Continue, "resource-continue")?;
    continuation.checkpoint = Some("outputs/checkpoint.bin".into());
    let continued = run_control::execute(&run, &continuation)?;
    assert_eq!(continued.status, "applied", "{continued:?}");
    assert_eq!(read_state(&run)?["status"], "pending");
    assert_eq!(metrics(&run)?.len(), 1);
    let mut release = command(
      &run,
      ControlAction::SetRequirements,
      "resources-for-continuation",
    )?;
    let mut healthy = disk_policy(false);
    healthy.recovery_seconds = 1;
    release.requirements = Some(healthy.clone());
    assert_eq!(run_control::execute(&run, &release)?.status, "applied");
    wait_for(|| {
      fs::read_to_string(run.join("outputs/seen-attempt")).is_ok_and(|value| value == "2")
    })?;
    assert_eq!(
      read_state(&run)?["requirements"],
      serde_json::to_value(healthy)?
    );
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
      read_state(&run)?["status"],
      "running",
      "replacement must not inherit the old monitor's resource hold"
    );
    assert_eq!(metrics(&run)?.len(), 2);
    fs::copy(
      run.join("outputs/checkpoint.bin"),
      run.join("outputs/checkpoint-next.bin"),
    )?;
    expri_client::registrations::register(&run, "outputs/checkpoint-next.bin")?;
    set_policy(&run, true, "resources-block-next-continuation")?;
    wait_for(|| read_state(&run).is_ok_and(|state| state["status"] == "paused"))?;
    let mut next = command(&run, ControlAction::Continue, "resource-continue-stop")?;
    next.checkpoint = Some("outputs/checkpoint-next.bin".into());
    let continued = run_control::execute(&run, &next)?;
    assert_eq!(continued.status, "applied", "{continued:?}");
    assert_eq!(read_state(&run)?["status"], "pending");
    let stop = command(&run, ControlAction::Stop, "stop-blocked-continuation")?;
    assert_eq!(run_control::execute(&run, &stop)?.status, "applied");
    assert_eq!(read_state(&run)?["status"], "cancelled");
    assert_eq!(metrics(&run)?.len(), 2);
    let restart = command(&run, ControlAction::Restart, "restart-blocked-resources")?;
    let restarted = run_control::execute(&run, &restart)?;
    assert_eq!(restarted.status, "applied", "{restarted:?}");
    let successor = run.parent().unwrap().join(restarted.new_run_id.unwrap());
    children.0.push(successor.clone());
    wait_for(|| read_state(&successor).is_ok_and(|state| state["status"] == "pending"))?;
    assert_eq!(
      read_state(&successor)?["requirements"],
      serde_json::to_value(disk_policy(true))?
    );
    assert!(!successor.join("outputs/metrics.jsonl").exists());
    set_policy(&successor, false, "resources-for-restart")?;
    wait_for(|| successor.join("outputs/checkpoint.bin").exists())?;
    assert_eq!(metrics(&successor)?[0]["step"], 0);
    Ok(())
  }

  struct Cleanup(Vec<PathBuf>);
  impl Drop for Cleanup {
    fn drop(&mut self) {
      for path in &self.0 {
        if let Ok(stop) = command(path, ControlAction::Stop, "fixture-cleanup") {
          let _ = run_control::execute(path, &stop);
        }
      }
    }
  }

  fn command(run: &Path, action: ControlAction, request_id: &str) -> Result<ControlCommand> {
    let facts = run_control::inspect(run)?;
    Ok(ControlCommand {
      request_id: request_id.into(),
      binding_id: "fixture".into(),
      expected_attempt: facts["attempt"].as_u64().unwrap(),
      expected_generation: facts["generation"].as_u64().unwrap(),
      action,
      arguments: None,
      checkpoint: None,
      requirements: None,
      reason: "isolated lifecycle verification".into(),
      grace_seconds: 3,
      expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
    })
  }
  fn read_state(run: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(
      run.join("run-state.json"),
    )?)?)
  }
  fn metrics(run: &Path) -> Result<Vec<Value>> {
    fs::read_to_string(run.join("outputs/metrics.jsonl"))?
      .lines()
      .map(|line| Ok(serde_json::from_str(line)?))
      .collect()
  }
  fn wait_for(mut predicate: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
      if Instant::now() >= deadline {
        return Err(ExpriError::Message(
          "lifecycle fixture task timed out".into(),
        ));
      }
      thread::sleep(Duration::from_millis(20));
    }
    Ok(())
  }

  const FAKE_UV: &str = r#"#!/usr/bin/python3
import json, os, sys
if sys.argv[1:5] == ['run', '--no-sync', '--no-env-file', '--']:
  os.execvp(sys.argv[5], sys.argv[5:])
request = json.loads(sys.argv[-1])
manifest = os.path.join(request['state_dir'], 'environment-state.json')
with open(manifest, 'w') as stream:
  json.dump({'fixture': True}, stream)
print(json.dumps({'environment_path': os.path.join(request['state_dir'], '.venv'), 'python': '/usr/bin/python3', 'manifest_path': manifest}))
"#;
  const TASK: &str = r#"import json, os, signal, sys, time
from pathlib import Path
out = Path(os.environ['EXPRI_OUTPUT_DIR'])
offset = int(os.environ.get('EXPRI_STEP_OFFSET', '0'))
checkpoint = os.environ.get('EXPRI_RESUME_CHECKPOINT')
with (out / 'metrics.jsonl').open('a') as stream:
  stream.write(json.dumps({'step': offset, 'metrics': {'value': int(sys.argv[1])}}) + '\n')
(out / 'source-version').write_text('recorded')
(out / 'used-checkpoint').write_text(Path(checkpoint).read_text() if checkpoint else 'none')
(out / 'seen-attempt').write_text(os.environ.get('EXPRI_ATTEMPT', '1'))
(out / 'checkpoint.bin').write_text('saved checkpoint')
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
while True:
  time.sleep(0.02)
"#;
}
