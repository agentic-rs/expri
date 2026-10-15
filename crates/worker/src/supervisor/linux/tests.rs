use super::*;
use std::fs::{self, OpenOptions};
use std::os::fd::IntoRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn run_fixture() -> (tempfile::TempDir, PathBuf) {
  let root = tempfile::tempdir().unwrap();
  let run = root
    .path()
    .canonicalize()
    .unwrap()
    .join(".expri/runs/test-run");
  fs::create_dir_all(run.join("code")).unwrap();
  fs::create_dir(run.join("outputs")).unwrap();
  fs::create_dir(run.join("logs")).unwrap();
  fs::write(run.join("run-state.json"), serde_json::to_vec(&json!({
    "schema_version": 1, "run_id": "test-run", "status": "running", "detached": true,
    "started_at": "2026-10-15T00:00:00Z", "code_dir": run.join("code"), "output_dir": run.join("outputs"),
  })).unwrap()).unwrap();
  (root, run)
}

fn log(run: &Path, name: &str) -> File {
  OpenOptions::new()
    .append(true)
    .create_new(true)
    .open(run.join("logs").join(name))
    .unwrap()
}

#[test]
fn sealed_payload_and_descriptor_checks_reject_untrusted_or_reused_handles() {
  let unsealed = tempfile::tempfile().unwrap();
  assert!(resume_from_fd(unsealed.as_raw_fd()).is_err());
  let sealed = sealed_memfd("supervisor-test", b"payload").unwrap();
  assert!(
    unsafe { libc::fcntl(sealed.as_raw_fd(), libc::F_GET_SEALS) } & REQUIRED_SEALS
      == REQUIRED_SEALS
  );
  assert!(sealed.try_clone().unwrap().write_all(b"rewrite").is_err());
  let mut descriptor = Descriptor::from_file(&sealed).unwrap();
  descriptor.validate(libc::S_IFREG).unwrap();
  descriptor.ino += 1;
  assert!(descriptor.validate(libc::S_IFREG).is_err());
  assert!(
    groups::exit_pending(std::process::id()).is_err(),
    "the supervisor itself is not its own child"
  );
}

#[test]
fn inheritance_restores_close_on_exec_after_a_failed_replacement() {
  let file = sealed_memfd("supervisor-invalid-executable", b"not executable").unwrap();
  let descriptor = file.as_raw_fd();
  assert!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) } & libc::FD_CLOEXEC != 0);
  {
    let _inheritance = Inheritance::new(&[descriptor]).unwrap();
    assert_eq!(
      unsafe { libc::fcntl(descriptor, libc::F_GETFD) } & libc::FD_CLOEXEC,
      0
    );
    assert!(exec_target(&file, descriptor).is_err());
  }
  assert!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) } & libc::FD_CLOEXEC != 0);
}

#[test]
fn successful_non_expri_elf_cannot_pass_the_handoff_preflight() {
  let (target, _) = pinned_target(Path::new("/bin/true")).unwrap();
  assert!(
    preflight(&target)
      .unwrap_err()
      .to_string()
      .contains("capability response")
  );
}

#[test]
fn malformed_cancellation_stops_the_owned_worker_before_returning() {
  let (_root, run) = run_fixture();
  let lease = expri_core::lock::run_lock(&run).unwrap();
  let mut command = Command::new("sh");
  command.args(["-c", "while :; do printf 'alive\\n'; sleep 0.01; done"]);
  let result = task(
    &mut command,
    &run,
    &lease,
    log(&run, "stdout.log"),
    log(&run, "stderr.log"),
    || fs::create_dir(run.join(".cancel-request")),
  )
  .unwrap();
  assert!(
    result
      .output
      .log_error
      .unwrap()
      .to_string()
      .contains("cancellation request")
  );
  assert!(!result.cleanup_uncertain);
  let state = read_json(&run.join("run-state.json"), MAX_STATE_BYTES).unwrap();
  let child = state["supervisor"]["worker_pid"].as_u64().unwrap() as u32;
  assert!(
    groups::exit_pending(child).is_err(),
    "the owned worker must already be reaped"
  );
}

#[test]
fn metadata_capacity_rejection_never_enqueues_a_restart_request() {
  let (_root, run) = run_fixture();
  let _lease = expri_core::lock::run_lock(&run).unwrap();
  update_state(&run.join("run-state.json"), |state| {
    state["supervisor"] = json!({"protocol_version":1,"generation":1,"mode":"live","events":[],"owner_pid":std::process::id(),"owner_start":process_start(std::process::id())?});
    state["large_metadata"] = json!("x".repeat(MAX_STATE_BYTES - 10 * 1024));
    Ok(())
  }).unwrap();
  let request = SupervisorRestartRequest {
    request_id: "capacity-test".into(),
    reason: "upgrade".into(),
    expected_generation: None,
    binary: None,
  };
  let error = request_restart(&run, &request).unwrap_err();
  assert!(error.to_string().contains("insufficient space"));
  assert!(!run.join(REQUEST_FILE).exists());
}

#[test]
fn early_failed_requests_remain_idempotent_after_the_scratch_result_is_replaced() {
  let (_root, run) = run_fixture();
  let _lease = expri_core::lock::run_lock(&run).unwrap();
  update_state(&run.join("run-state.json"), |state| {
    state["supervisor"] = json!({"protocol_version":1,"generation":1,"mode":"live","events":[]});
    Ok(())
  })
  .unwrap();
  let request = |id: &str| SupervisorRestartRequest {
    request_id: id.into(),
    reason: format!("restart {id}"),
    expected_generation: Some(1),
    binary: None,
  };
  let first = request("failed-a");
  for operation in [&first, &request("failed-b")] {
    let failed = json!({
      "request_id":operation.request_id,"request_fingerprint":request_fingerprint(operation).unwrap(),
      "reason":operation.reason,"generation":2,"requested_at":now(),"failed_at":now(),
      "mode":"live","status":"failed","worker_restarted":false,"error":"state lease was busy",
    });
    write_json(&run.join(RESULT_FILE), &failed).unwrap();
    let state_lease = expri_core::lock::run_state_lock(&run).unwrap();
    assert!(!reconcile_failure(&run).unwrap());
    drop(state_lease);
    assert!(reconcile_failure(&run).unwrap());
  }
  let state = read_json(&run.join("run-state.json"), MAX_STATE_BYTES).unwrap();
  assert_eq!(
    state["supervisor"]["generation"], 1,
    "failed handoffs do not install a generation"
  );
  assert_eq!(state["supervisor"]["events"].as_array().unwrap().len(), 2);
  let retained = request_result(&run, &state, &first.request_id)
    .unwrap()
    .unwrap();
  validate_retry(&retained, &first).unwrap();
  assert_eq!(
    request_restart(&run, &first).unwrap()["restart"]["status"],
    "failed"
  );
  assert!(
    !run.join(REQUEST_FILE).exists(),
    "retrying an old failure must never enqueue execution"
  );
}

#[test]
fn transient_handoff_acknowledgment_failure_keeps_draining_the_same_child() {
  let (_root, run) = run_fixture();
  let lease = expri_core::lock::run_lock(&run).unwrap();
  let mut command = Command::new("sh");
  command
    .args([
      "-c",
      "i=0; while [ $i -lt 20 ]; do printf 'continued\\n'; i=$((i+1)); sleep 0.02; done",
    ])
    .process_group(0)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  let mut child = command.spawn().unwrap();
  let child_pid = child.id();
  let stdout = LogPipe::new(
    unsafe { File::from_raw_fd(child.stdout.take().unwrap().into_raw_fd()) },
    log(&run, "stdout.log"),
  )
  .unwrap();
  let stderr = LogPipe::new(
    unsafe { File::from_raw_fd(child.stderr.take().unwrap().into_raw_fd()) },
    log(&run, "stderr.log"),
  )
  .unwrap();
  drop(child);
  let payload = Handoff {
    protocol_version: 1,
    run_dir: run.clone(),
    supervisor_pid: std::process::id(),
    supervisor_start: process_start(std::process::id()).unwrap(),
    child_pid,
    lease: Descriptor::from_file(&lease).unwrap(),
    stdout_pipe: Descriptor::from_file(&stdout.source).unwrap(),
    stderr_pipe: Descriptor::from_file(&stderr.source).unwrap(),
    stdout_log: Descriptor::from_file(&stdout.log).unwrap(),
    stderr_log: Descriptor::from_file(&stderr.log).unwrap(),
    request_id: "ack-retry".into(),
    generation: 2,
    target_digest: "test-digest".into(),
    cleanup_groups: false,
  };
  update_state(&run.join("run-state.json"), |state| {
    state["supervisor"] = json!({"protocol_version":1,"generation":1,"mode":"live","owner_pid":std::process::id(),"owner_start":payload.supervisor_start,"worker_pid":child_pid,
      "events":[{"request_id":"ack-retry","generation":2,"status":"restarting","target_digest":"test-digest","reason":"test","worker_restarted":false}]});
    Ok(())
  }).unwrap();
  let state_lease = expri_core::lock::run_state_lock(&run).unwrap();
  assert!(
    !acknowledge_handoff(&payload, &lease).unwrap(),
    "a busy state lease must not block the live pump"
  );
  drop(state_lease);
  fs::rename(run.join("logs"), run.join("temporarily-unavailable-logs")).unwrap();
  assert!(acknowledge_handoff(&payload, &lease).is_err());
  let restored_run = run.clone();
  let restore = thread::spawn(move || {
    thread::sleep(Duration::from_millis(100));
    fs::rename(
      restored_run.join("temporarily-unavailable-logs"),
      restored_run.join("logs"),
    )
    .unwrap();
  });
  let result = pump(
    &run,
    &lease,
    child_pid,
    stdout,
    stderr,
    false,
    Some(&payload),
  )
  .unwrap();
  restore.join().unwrap();
  assert!(result.output.status.success());
  assert!(result.output.log_error.is_none());
  assert_eq!(
    fs::read_to_string(run.join("logs/stdout.log"))
      .unwrap()
      .lines()
      .count(),
    20
  );
  let state = read_json(&run.join("run-state.json"), MAX_STATE_BYTES).unwrap();
  assert_eq!(state["supervisor"]["generation"], 2);
  assert_eq!(state["supervisor"]["events"][0]["status"], "completed");
}
