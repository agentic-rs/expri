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
    requirements: None,
    experiment: None,
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

fn disk_policy(blocked: bool) -> expri_core::resource_requirements::ResourceRequirements {
  use expri_core::resource_requirements::{DiskRequirement, ResourceRequirements};
  ResourceRequirements {
    disk: vec![DiskRequirement {
      path: "outputs".into(),
      min_free_bytes: if blocked { 9_000_000_000_000_000 } else { 1 },
      resume_free_bytes: if blocked { 9_000_000_000_000_001 } else { 2 },
    }],
    recovery_seconds: 0,
  }
}

#[test]
fn an_older_live_lifecycle_supervisor_never_receives_a_requirements_mailbox() {
  let (_root, run) = fixture();
  let _lease = expri_core::lock::run_lock(&run).unwrap();
  update_state(&run.join("run-state.json"), |state| {
    state["run_control_protocol_version"] = json!(1);
    state["supervisor"] = json!({"generation":1});
    Ok(())
  })
  .unwrap();
  let capabilities = inspect(&run).unwrap()["capabilities"].clone();
  assert!(capabilities.as_array().unwrap().contains(&json!("pause")));
  assert!(
    !capabilities
      .as_array()
      .unwrap()
      .contains(&json!("set_requirements"))
  );
  let mut command = control(
    ControlAction::SetRequirements,
    "old-supervisor-requirements",
  );
  command.requirements = Some(disk_policy(true));
  let result = execute(&run, &command).unwrap();
  assert_eq!(result.status, "failed");
  assert!(result.error.unwrap().contains("upgrade"));
  assert!(!run.join(MAILBOX).exists());
  assert!(state(&run).unwrap()["requirements"].is_null());
}

fn set_disk_policy(run: &Path, blocked: bool, id: &str) {
  let mut command = control(ControlAction::SetRequirements, id);
  command.requirements = Some(disk_policy(blocked));
  assert_eq!(execute(run, &command).unwrap().status, "applied");
}

#[test]
fn offline_resource_hold_blocks_launch_and_pending_stop_never_starts_a_task() {
  let (_root, run) = fixture();
  update_state(&run.join("run-state.json"), |state| {
    state["requirements"] = serde_json::to_value(disk_policy(true))?;
    Ok(())
  })
  .unwrap();
  let supervisor = start_counter(
    &run,
    "echo started > outputs/started; while :; do sleep 0.1; done",
  );
  wait_for(|| state(&run).unwrap()["status"] == "pending");
  assert!(!run.join("outputs/started").exists());
  assert!(state(&run).unwrap()["supervisor"]["worker_pid"].is_null());
  let capabilities = inspect(&run).unwrap()["capabilities"].clone();
  assert!(
    capabilities
      .as_array()
      .unwrap()
      .contains(&json!("set_requirements"))
  );
  assert!(capabilities.as_array().unwrap().contains(&json!("stop")));
  assert_eq!(
    execute(&run, &control(ControlAction::Stop, "stop-pending"))
      .unwrap()
      .status,
    "applied"
  );
  assert!(supervisor.join().unwrap().cancelled);
  assert!(!run.join("outputs/started").exists());
}

#[test]
fn changing_pending_requirements_allows_the_original_attempt_to_launch() {
  let (_root, run) = fixture();
  update_state(&run.join("run-state.json"), |state| {
    state["requirements"] = serde_json::to_value(disk_policy(true))?;
    Ok(())
  })
  .unwrap();
  let supervisor = start_counter(
    &run,
    "echo started > outputs/started; while :; do sleep 0.1; done",
  );
  wait_for(|| state(&run).unwrap()["status"] == "pending");
  set_disk_policy(&run, false, "release-pending");
  wait_for(|| run.join("outputs/started").exists());
  assert_eq!(state(&run).unwrap()["task_attempts"]["current_attempt"], 1);
  assert_eq!(
    execute(&run, &control(ControlAction::Stop, "stop-after-pending"))
      .unwrap()
      .status,
    "applied"
  );
  assert!(supervisor.join().unwrap().cancelled);
}

#[test]
fn resource_recovery_never_undoes_manual_pause_and_resume_cannot_bypass_a_hold() {
  let (_root, run) = fixture();
  let supervisor = start_counter(
    &run,
    "while :; do echo tick >> outputs/counter; sleep 0.02; done",
  );
  wait_for(|| run.join("outputs/counter").exists());
  set_disk_policy(&run, true, "disk-low");
  wait_for(|| state(&run).unwrap()["status"] == "paused");
  assert_eq!(state(&run).unwrap()["resource_paused"], true);
  assert_eq!(state(&run).unwrap()["manual_paused"], false);
  assert_eq!(
    execute(&run, &control(ControlAction::Resume, "resume-blocked"))
      .unwrap()
      .status,
    "applied"
  );
  assert_eq!(state(&run).unwrap()["status"], "paused");
  assert_eq!(
    execute(&run, &control(ControlAction::Pause, "keep-paused"))
      .unwrap()
      .status,
    "applied"
  );
  let bytes = fs::metadata(run.join("outputs/counter")).unwrap().len();
  set_disk_policy(&run, false, "disk-recovered");
  wait_for(|| state(&run).unwrap()["resource_paused"] == false);
  thread::sleep(Duration::from_millis(100));
  assert_eq!(state(&run).unwrap()["status"], "paused");
  assert_eq!(state(&run).unwrap()["manual_paused"], true);
  assert_eq!(
    fs::metadata(run.join("outputs/counter")).unwrap().len(),
    bytes
  );
  assert_eq!(
    execute(&run, &control(ControlAction::Resume, "manual-resume"))
      .unwrap()
      .status,
    "applied"
  );
  wait_for(|| fs::metadata(run.join("outputs/counter")).unwrap().len() > bytes);
  assert_eq!(state(&run).unwrap()["task_attempts"]["current_attempt"], 1);
  set_disk_policy(&run, true, "disk-low-again");
  wait_for(|| state(&run).unwrap()["status"] == "paused");
  assert_eq!(
    execute(&run, &control(ControlAction::Stop, "stop-resource-paused"))
      .unwrap()
      .status,
    "applied"
  );
  assert!(supervisor.join().unwrap().cancelled);
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
fn restart_replaces_experiment_intent_while_continuation_keeps_the_original() {
  use expri_core::experiment_assignment::ExperimentSelection;
  let (_root, run) = fixture();
  let path = run.join("run-request.json");
  let mut request = read_json(&path, LIMIT).unwrap();
  let original = RunExperimentIntent {
    request_id: "original-launch".into(),
    selection: ExperimentSelection::New {
      name: "Original experiment".into(),
    },
  };
  request["experiment"] = json!(original);
  write_json(&path, &request).unwrap();
  let mut command = control(ControlAction::Continue, "continue-existing");
  assert_eq!(
    recorded_request(&run, &command).unwrap().experiment,
    Some(original)
  );
  command.action = ControlAction::Restart;
  assert_eq!(recorded_request(&run, &command).unwrap().experiment, None);
  command.experiment = Some(ExperimentSelection::JoinRun {
    run_id: "other-run".into(),
    origin: Some("other-worker".into()),
  });
  let intent = recorded_request(&run, &command)
    .unwrap()
    .experiment
    .unwrap();
  assert_eq!(intent.request_id, command.request_id);
  assert_eq!(Some(intent.selection), command.experiment);
}

#[test]
fn rejected_experiment_restart_preserves_the_running_task() {
  use expri_core::experiment_assignment::ExperimentSelection;
  let (_root, run) = fixture();
  let _supervisor = start_counter(
    &run,
    "trap 'exit 0' TERM; while :; do echo tick >> outputs/counter; sleep 0.02; done",
  );
  wait_for(|| {
    state(&run).unwrap()["run_control_protocol_version"] == 1
      && run.join("outputs/counter").exists()
  });
  let before = fs::read(run.join("outputs/counter")).unwrap().len();
  let mut command = control(ControlAction::Restart, "grouping-without-publishing");
  command.experiment = Some(ExperimentSelection::New {
    name: "Unavailable publishing".into(),
  });
  let result = execute(&run, &command).unwrap();
  assert_eq!(result.status, "failed");
  assert!(
    result
      .error
      .unwrap()
      .contains("automatic service publishing")
  );
  assert_eq!(state(&run).unwrap()["status"], "running");
  assert!(!run.join(MAILBOX).exists());
  assert_eq!(fs::read_dir(run.parent().unwrap()).unwrap().count(), 1);
  wait_for(|| fs::read(run.join("outputs/counter")).unwrap().len() > before);
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
