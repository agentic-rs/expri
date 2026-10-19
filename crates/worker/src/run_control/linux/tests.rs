use super::*;
use std::process::Command;

fn fixture() -> (tempfile::TempDir, PathBuf) {
  let root = tempfile::tempdir().unwrap();
  let run = root
    .path()
    .canonicalize()
    .unwrap()
    .join(".expri/runs/lifecycle-test");
  for path in ["code", "outputs", "logs"] {
    fs::create_dir_all(run.join(path)).unwrap();
  }
  write_json(&run.join("run-state.json"), &json!({"schema_version":1,"run_id":"lifecycle-test","task":"train","command":["/bin/sh","train.sh"],"status":"running","detached":true,"started_at":"2026-10-19T00:00:00Z","finished_at":null,"code_dir":run.join("code"),"output_dir":run.join("outputs")})).unwrap();
  write_json(&run.join("run-request.json"), &json!({"name":"train","command":["/bin/sh","train.sh"],"environment":{"base_python":null},"detach":true})).unwrap();
  (root, run)
}

fn control(action: ControlAction, id: &str) -> ControlCommand {
  ControlCommand {
    request_id: id.into(),
    binding_id: "binding".into(),
    expected_attempt: 1,
    expected_generation: 1,
    action,
    arguments: None,
    checkpoint: None,
    reason: "test lifecycle".into(),
    grace_seconds: 3,
    expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
  }
}

fn wait_for(mut predicate: impl FnMut() -> bool) {
  let deadline = Instant::now() + Duration::from_secs(5);
  while !predicate() {
    assert!(Instant::now() < deadline, "lifecycle test timed out");
    thread::sleep(Duration::from_millis(10));
  }
}

struct Counter {
  run_dir: PathBuf,
  handle: Option<thread::JoinHandle<crate::supervisor::TaskOutput>>,
}

impl Counter {
  fn join(mut self) -> thread::Result<crate::supervisor::TaskOutput> {
    self.handle.take().unwrap().join()
  }
}

impl Drop for Counter {
  fn drop(&mut self) {
    if let Some(handle) = self.handle.take() {
      let _ = fs::write(self.run_dir.join(".cancel-request"), []);
      let _ = handle.join();
    }
  }
}

fn start_counter(run: &Path, script: &'static str) -> Counter {
  let path = run.to_owned();
  let handle = thread::spawn(move || {
    let lease = expri_core::lock::run_lock(&path).unwrap();
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script]).current_dir(&path);
    let stdout = fs::File::create(path.join("logs/stdout.log")).unwrap();
    let stderr = fs::File::create(path.join("logs/stderr.log")).unwrap();
    let output =
      crate::supervisor::task(&mut command, &path, &lease, stdout, stderr, || Ok(())).unwrap();
    update_state(&path.join("run-state.json"), |value| {
      value["status"] = json!("cancelled");
      value["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
      Ok(())
    })
    .unwrap();
    output
  });
  Counter {
    run_dir: run.to_owned(),
    handle: Some(handle),
  }
}

#[test]
fn owned_pause_freezes_progress_resume_restores_waiting_and_stop_unfreezes_for_cleanup() {
  let (_root, run) = fixture();
  let supervisor = start_counter(
    &run,
    "trap 'sleep 2.2; echo cleaned > outputs/cleaned; exit 0' TERM; while :; do echo tick >> outputs/counter; sleep 0.02; done",
  );
  wait_for(|| {
    state(&run).unwrap()["run_control_protocol_version"] == 1
      && run.join("outputs/counter").exists()
  });
  expri_core::run_state::set_task_status(&run, "waiting").unwrap();
  let pause = control(ControlAction::Pause, "pause-one");
  assert_eq!(execute(&run, &pause).unwrap().status, "applied");
  thread::sleep(Duration::from_millis(50));
  let frozen = fs::read(run.join("outputs/counter")).unwrap();
  thread::sleep(Duration::from_millis(100));
  assert_eq!(fs::read(run.join("outputs/counter")).unwrap(), frozen);
  assert_eq!(state(&run).unwrap()["status"], "paused");
  assert_eq!(execute(&run, &pause).unwrap().status, "applied");
  assert_eq!(
    state(&run).unwrap()["control_events"]
      .as_array()
      .unwrap()
      .len(),
    1
  );
  let mut changed = pause.clone();
  changed.reason = "different payload".into();
  assert!(execute(&run, &changed).is_err());
  let mut stale = control(ControlAction::Resume, "stale");
  stale.expected_attempt = 2;
  assert!(execute(&run, &stale).is_err());
  stale.request_id = "stale-generation".into();
  stale.expected_attempt = 1;
  stale.expected_generation = 2;
  assert!(execute(&run, &stale).is_err());
  let state_lease = expri_core::lock::run_state_lock(&run).unwrap();
  let resume_run = run.clone();
  let resume =
    thread::spawn(move || execute(&resume_run, &control(ControlAction::Resume, "resume-one")));
  thread::sleep(Duration::from_millis(150));
  assert_eq!(
    fs::read(run.join("outputs/counter")).unwrap(),
    frozen,
    "resume must retain suspension until it can publish the restored phase"
  );
  drop(state_lease);
  assert_eq!(resume.join().unwrap().unwrap().status, "applied");
  assert_eq!(state(&run).unwrap()["status"], "waiting");
  wait_for(|| fs::read(run.join("outputs/counter")).unwrap().len() > frozen.len());
  assert_eq!(
    execute(&run, &control(ControlAction::Pause, "pause-two"))
      .unwrap()
      .status,
    "applied"
  );
  assert_eq!(
    execute(&run, &control(ControlAction::Stop, "stop-one"))
      .unwrap()
      .status,
    "applied"
  );
  let output = supervisor.join().unwrap();
  assert!(output.cancelled);
  assert!(!output.cleanup_uncertain);
  assert!(
    run.join("outputs/cleaned").exists(),
    "paused stop must run the TERM handler"
  );
}

#[test]
fn expired_requests_and_unsupported_workers_never_change_a_run() {
  let (_root, run) = fixture();
  assert!(
    inspect(&run).unwrap()["capabilities"]
      .as_array()
      .unwrap()
      .is_empty()
  );
  let mut command = control(ControlAction::Pause, "unsupported");
  assert!(execute(&run, &command).is_err());
  update_state(&run.join("run-state.json"), |value| {
    value["status"] = json!("completed");
    value["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
    value["run_control_protocol_version"] = json!(1);
    value["task_process_group_clean"] = json!(true);
    value["supervisor"] = json!({"generation":1});
    Ok(())
  })
  .unwrap();
  command.action = ControlAction::Restart;
  command.request_id = "expired".into();
  command.expires_at_ms = 1;
  assert_eq!(execute(&run, &command).unwrap().status, "failed");
  assert_eq!(fs::read_dir(run.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn recorded_arguments_keep_executable_and_preserve_literal_shell_characters() {
  let (_root, run) = fixture();
  let mut command = control(ControlAction::Restart, "literal-argv");
  command.arguments = Some(vec![
    "train.sh".into(),
    "$(touch should-not-exist); literal".into(),
  ]);
  let request = recorded_request(&run, &command).unwrap();
  assert_eq!(
    request.command,
    ["/bin/sh", "train.sh", "$(touch should-not-exist); literal"]
  );
  assert!(!run.join("should-not-exist").exists());
}

#[test]
fn interrupted_restart_launch_intent_retains_one_successor_and_never_spawns_again() {
  let (_root, run) = fixture();
  let successor = run.parent().unwrap().join("run-successor");
  fs::create_dir(&successor).unwrap();
  write_json(
    &successor.join("run-state.json"),
    &json!({"status":"preparing"}),
  )
  .unwrap();
  fs::create_dir(run.join(".run-controls")).unwrap();
  let command = control(ControlAction::Restart, "interrupted-restart");
  save(
    &run,
    &Journal {
      command: command.clone(),
      fingerprint: hex_digest(&serde_json::to_vec(&command).unwrap()),
      result: result(&command, "executing", None, None),
      stage: "launch_intent".into(),
      new_run_dir: Some(successor.clone()),
    },
  )
  .unwrap();
  let failed = execute(&run, &command).unwrap();
  assert_eq!(failed.status, "failed");
  assert_eq!(failed.new_run_id.as_deref(), Some("run-successor"));
  assert!(failed.error.as_deref().unwrap().contains("uncertain"));
  assert_eq!(execute(&run, &command).unwrap(), failed);
  assert_eq!(fs::read_dir(run.parent().unwrap()).unwrap().count(), 2);
  assert!(!successor.join(".worker-ready").exists());
}

#[test]
fn accepted_restart_can_stop_after_its_original_delivery_deadline() {
  let (_root, run) = fixture();
  let supervisor = start_counter(
    &run,
    "trap 'exit 0' TERM; while :; do echo tick >> outputs/counter; sleep 0.02; done",
  );
  wait_for(|| {
    state(&run).unwrap()["run_control_protocol_version"] == 1
      && run.join("outputs/counter").exists()
  });
  let successor = run.parent().unwrap().join("run-delayed-successor");
  fs::create_dir(&successor).unwrap();
  // This deliberately invalid successor prevents a test executable being used
  // as the CLI launcher after we verify that the accepted stop was applied.
  write_json(
    &successor.join("run-state.json"),
    &json!({"status":"completed"}),
  )
  .unwrap();
  fs::create_dir(run.join(".run-controls")).unwrap();
  let mut command = control(ControlAction::Restart, "delayed-restart");
  command.expires_at_ms = 1;
  save(
    &run,
    &Journal {
      command: command.clone(),
      fingerprint: hex_digest(&serde_json::to_vec(&command).unwrap()),
      result: result(&command, "executing", None, None),
      stage: "prepared".into(),
      new_run_dir: Some(successor),
    },
  )
  .unwrap();
  let failed = execute(&run, &command).unwrap();
  assert_eq!(failed.status, "failed");
  assert!(
    failed
      .error
      .as_deref()
      .unwrap()
      .contains("no longer awaiting launch"),
    "{failed:?}"
  );
  assert_eq!(state(&run).unwrap()["status"], "cancelled");
  assert!(supervisor.join().unwrap().cancelled);
}

#[test]
fn replayed_stop_acceptance_waits_for_terminal_state_and_verified_cleanup() {
  let (_root, run) = fixture();
  let supervisor = start_counter(
    &run,
    "trap 'sleep 1.2; echo cleaned > outputs/cleaned; exit 0' TERM; while :; do echo tick >> outputs/counter; sleep 0.02; done",
  );
  wait_for(|| {
    state(&run).unwrap()["run_control_protocol_version"] == 1
      && run.join("outputs/counter").exists()
  });
  let stop = control(ControlAction::Stop, "replayed-stop");
  let first_run = run.clone();
  let first_command = stop.clone();
  let first = thread::spawn(move || supervisor_action(&first_run, &first_command));
  wait_for(|| saved_event(&state(&run).unwrap(), &stop).is_some());
  assert_ne!(state(&run).unwrap()["status"], "cancelled");
  assert!(!run.join("outputs/cleaned").exists());
  supervisor_action(&run, &stop).unwrap();
  assert_eq!(state(&run).unwrap()["status"], "cancelled");
  assert_eq!(state(&run).unwrap()["task_process_group_clean"], true);
  assert!(run.join("outputs/cleaned").exists());
  first.join().unwrap().unwrap();
  assert!(supervisor.join().unwrap().cancelled);
  update_state(&run.join("run-state.json"), |state| {
    state["task_process_group_clean"] = json!(false);
    Ok(())
  })
  .unwrap();
  assert!(
    supervisor_action(&run, &stop).is_err(),
    "a saved acceptance must not hide uncertain cleanup"
  );
}

#[test]
fn retained_command_history_does_not_lock_out_pause_or_stop_and_old_replays_do_not_actuate() {
  let (_root, run) = fixture();
  let supervisor = start_counter(
    &run,
    "trap 'exit 0' TERM; while :; do echo tick >> outputs/counter; sleep 0.02; done",
  );
  wait_for(|| {
    state(&run).unwrap()["run_control_protocol_version"] == 1
      && run.join("outputs/counter").exists()
  });
  fs::create_dir(run.join(".run-controls")).unwrap();
  let mut old = control(ControlAction::Resume, "old-0");
  old.expires_at_ms = 1;
  for number in 0..300 {
    let mut command = old.clone();
    command.request_id = format!("old-{number}");
    save(
      &run,
      &Journal {
        command: command.clone(),
        fingerprint: hex_digest(&serde_json::to_vec(&command).unwrap()),
        result: result(&command, "applied", None, None),
        stage: "accepted".into(),
        new_run_dir: None,
      },
    )
    .unwrap();
  }
  assert_eq!(
    execute(&run, &control(ControlAction::Pause, "after-history-pause"))
      .unwrap()
      .status,
    "applied"
  );
  thread::sleep(Duration::from_millis(50));
  let frozen = fs::read(run.join("outputs/counter")).unwrap();
  assert_eq!(
    execute(&run, &old).unwrap(),
    result(&old, "applied", None, None)
  );
  thread::sleep(Duration::from_millis(100));
  assert_eq!(state(&run).unwrap()["status"], "paused");
  assert_eq!(
    fs::read(run.join("outputs/counter")).unwrap(),
    frozen,
    "an expired retained Resume replay must not actuate again"
  );
  assert_eq!(
    execute(&run, &control(ControlAction::Stop, "after-history-stop"))
      .unwrap()
      .status,
    "applied"
  );
  assert!(supervisor.join().unwrap().cancelled);
  assert_eq!(
    fs::read_dir(run.join(".run-controls")).unwrap().count(),
    302
  );
  assert_eq!(
    execute(&run, &old).unwrap(),
    result(&old, "applied", None, None)
  );
}
