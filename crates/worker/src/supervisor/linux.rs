use crate::run_logs::groups;
use expri_core::error::{ExpriError, Result};
use expri_core::lock::{FileLock, LockAttempt};
use expri_core::protocol::SupervisorRestartRequest;
use expri_core::run_state::{MAX_STATE_BYTES, try_update_state, update_state};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

mod pump;
mod transport;
use pump::{LogPipe, pump};
pub(crate) use pump::{TaskOutput, task};
use transport::{
  Descriptor, Inheritance, cancel_requested, exec_target, hex_digest, pinned_target, preflight,
  process_start, read_json, sealed_memfd, set_cloexec, validate_named_descriptor, write_json,
};

const REQUEST_FILE: &str = ".supervisor-request.json";
const RESULT_FILE: &str = ".supervisor-result.json";
const REQUEST_LIMIT: usize = 16 * 1024;
const CAPABILITY: &str = "run-supervisor-restart-v1";
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const REQUIRED_SEALS: i32 =
  libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
  restart: SupervisorRestartRequest,
  binary: PathBuf,
  generation: u64,
  requested_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handoff {
  protocol_version: u32,
  run_dir: PathBuf,
  supervisor_pid: u32,
  supervisor_start: u64,
  child_pid: u32,
  lease: Descriptor,
  stdout_pipe: Descriptor,
  stderr_pipe: Descriptor,
  stdout_log: Descriptor,
  stderr_log: Descriptor,
  request_id: String,
  generation: u64,
  target_digest: String,
  cleanup_groups: bool,
}

pub fn request_restart(run_dir: &Path, request: &SupervisorRestartRequest) -> Result<Value> {
  validate_request(request)?;
  let run_dir = validate_run_directory(run_dir)?;
  let state_path = run_dir.join("run-state.json");
  let state = read_json(&state_path, MAX_STATE_BYTES)?;
  if let Some(event) = request_result(&run_dir, &state, &request.request_id)? {
    validate_retry(&event, request)?;
    if event["status"] != "restarting" {
      return restart_report(&run_dir, &event);
    }
  }
  // A retry of a completed request remains idempotent even after run completion.
  if let Some(event) = find_event(&state, &request.request_id) {
    validate_retry(event, request)?;
    if event["status"] != "restarting" {
      return restart_report(&run_dir, event);
    }
  } else {
    validate_restartable(&run_dir, &state)?;
    validate_history_capacity(&state)?;
    let generation = state["supervisor"]["generation"]
      .as_u64()
      .ok_or_else(|| message("invalid supervisor generation"))?;
    if request
      .expected_generation
      .is_some_and(|expected| expected != generation)
    {
      return Err(message(
        "supervisor generation changed; inspect the run and retry with its current generation",
      ));
    }
    let LockAttempt::Acquired(_control) =
      expri_core::lock::try_lock_file(&run_dir.join(".supervisor-request.lock"), true)?
    else {
      return Err(message(
        "another supervisor restart request is being submitted",
      ));
    };
    let state = read_json(&state_path, MAX_STATE_BYTES)?;
    validate_history_capacity(&state)?;
    if !reconcile_failure(&run_dir)? {
      return Err(message(
        "run metadata is busy reconciling the previous restart result; retry the same request",
      ));
    }
    if let Some(event) = find_event(&state, &request.request_id) {
      validate_retry(event, request)?;
    } else if let Ok(previous) = read_json(&run_dir.join(REQUEST_FILE), REQUEST_LIMIT) {
      let previous_id = previous["restart"]["request_id"]
        .as_str()
        .unwrap_or_default();
      if previous_id != request.request_id
        && request_result(&run_dir, &state, previous_id)?
          .is_none_or(|event| event["status"] == "restarting")
      {
        return Err(message(
          "another supervisor restart is pending; retry its request ID or wait for it to finish",
        ));
      }
      if previous_id == request.request_id && previous["restart"] != serde_json::to_value(request)?
      {
        return Err(message(
          "restart request ID was already used with different parameters",
        ));
      }
    }
    let control = Control {
      restart: request.clone(),
      binary: request.binary.clone().unwrap_or(std::env::current_exe()?),
      generation: state["supervisor"]["generation"]
        .as_u64()
        .and_then(|generation| generation.checked_add(1))
        .ok_or_else(|| message("supervisor generation exhausted"))?,
      requested_at: now(),
    };
    write_json(&run_dir.join(REQUEST_FILE), &serde_json::to_value(control)?)?;
  }
  let deadline = Instant::now() + Duration::from_secs(30);
  loop {
    let state = read_json(&state_path, MAX_STATE_BYTES)?;
    if let Some(event) = request_result(&run_dir, &state, &request.request_id)?
      && event["status"] != "restarting"
    {
      validate_retry(&event, request)?;
      return restart_report(&run_dir, &event);
    }
    if !lease_busy(&run_dir)? {
      return Err(message(
        "supervisor ownership was lost during restart; the worker was not relaunched. Inspect the run before recovery",
      ));
    }
    if !matches!(state["status"].as_str(), Some("running" | "waiting"))
      || !state["finished_at"].is_null()
    {
      return Err(message(
        "run finished before supervisor restart completed; it was not restarted",
      ));
    }
    if Instant::now() >= deadline {
      return Err(message(
        "supervisor restart acknowledgment timed out; retry the same request ID to inspect its durable result",
      ));
    }
    thread::sleep(Duration::from_millis(50));
  }
}

fn process_request(
  run_dir: &Path,
  lease: &impl AsRawFd,
  pid: u32,
  stdout: &mut LogPipe,
  stderr: &mut LogPipe,
  cleanup_groups: bool,
) -> Result<()> {
  let control: Control =
    serde_json::from_value(read_json(&run_dir.join(REQUEST_FILE), REQUEST_LIMIT)?)?;
  let request_id = control.restart.request_id.clone();
  let fingerprint = request_fingerprint(&control.restart)?;
  let reason = control.restart.reason.clone();
  let generation = control.generation;
  let requested_at = control.requested_at.clone();
  let result = process_control(run_dir, lease, pid, stdout, stderr, cleanup_groups, control);
  if let Err(error) = &result {
    // A request rejected before its event could be persisted must not leave a
    // permanent unacknowledged file that blocks all subsequent requests.
    write_json(
      &run_dir.join(RESULT_FILE),
      &json!({
        "request_id": request_id, "request_fingerprint": fingerprint, "reason": reason,
        "generation": generation, "requested_at": requested_at,
        "mode": "live", "status": "failed", "failed_at": now(), "worker_restarted": false,
        "error": bounded_error(error),
      }),
    )?;
    let _ = reconcile_failure(run_dir);
  }
  result
}

fn process_control(
  run_dir: &Path,
  lease: &impl AsRawFd,
  pid: u32,
  stdout: &mut LogPipe,
  stderr: &mut LogPipe,
  cleanup_groups: bool,
  control: Control,
) -> Result<()> {
  let request = control.restart;
  validate_request(&request)?;
  let state = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
  if let Some(event) = find_event(&state, &request.request_id) {
    validate_retry(event, &request)?;
    return Ok(());
  }
  let generation = state["supervisor"]["generation"]
    .as_u64()
    .ok_or_else(|| message("invalid supervisor generation"))?;
  let event = json!({
    "generation": generation.checked_add(1).ok_or_else(|| message("supervisor generation exhausted"))?,
    "request_id": request.request_id, "reason": request.reason, "mode": "live", "status": "restarting",
    "requested_at": control.requested_at, "worker_restarted": false, "request_fingerprint": request_fingerprint(&request)?,
  });
  update_state(&run_dir.join("run-state.json"), |current| {
    validate_restartable(run_dir, current)?;
    validate_history_capacity(current)?;
    if current["supervisor"]["owner_pid"] != std::process::id()
      || current["supervisor"]["owner_start"].as_u64() != Some(process_start(std::process::id())?)
      || current["supervisor"]["worker_pid"] != pid
      || request
        .expected_generation
        .is_some_and(|expected| expected != generation)
    {
      return Err(message("supervisor process identity or generation changed"));
    }
    current["supervisor"]["events"]
      .as_array_mut()
      .ok_or_else(|| message("invalid supervisor event history"))?
      .push(event);
    Ok(())
  })?;
  let handoff = (|| {
    if stdout.error.is_some() || stderr.error.is_some() {
      return Err(message(
        "supervisor restart is unavailable because task log capture has already failed; the existing worker continues",
      ));
    }
    let (target, target_digest) = pinned_target(&control.binary)?;
    preflight(&target)?;
    if cancel_requested(run_dir)? || groups::exit_pending(pid)? {
      return Err(message(
        "task finished or cancellation began before supervisor handoff",
      ));
    }
    stdout.log.flush()?;
    stderr.log.flush()?;
    let payload = Handoff {
      protocol_version: 1,
      run_dir: run_dir.into(),
      supervisor_pid: std::process::id(),
      supervisor_start: process_start(std::process::id())?,
      child_pid: pid,
      lease: Descriptor::from_file(lease)?,
      stdout_pipe: Descriptor::from_file(&stdout.source)?,
      stderr_pipe: Descriptor::from_file(&stderr.source)?,
      stdout_log: Descriptor::from_file(&stdout.log)?,
      stderr_log: Descriptor::from_file(&stderr.log)?,
      request_id: request.request_id.clone(),
      generation: generation + 1,
      target_digest: target_digest.clone(),
      cleanup_groups,
    };
    let payload_fd = sealed_memfd("expri-supervisor-handoff", &serde_json::to_vec(&payload)?)?;
    update_event(run_dir, &request.request_id, |event, _| {
      event["target_digest"] = json!(target_digest);
      Ok(())
    })?;
    let inherited = [
      lease.as_raw_fd(),
      stdout.source.as_raw_fd(),
      stderr.source.as_raw_fd(),
      stdout.log.as_raw_fd(),
      stderr.log.as_raw_fd(),
      payload_fd.as_raw_fd(),
    ];
    let _inheritance = Inheritance::new(&inherited)?;
    // exec failure returns in the old process. Restore every descriptor before
    // continuing the same pump and task; there is never a lease gap.
    exec_target(&target, payload_fd.as_raw_fd())
  })();
  if let Err(error) = handoff {
    update_event(run_dir, &request.request_id, |event, _| {
      event["status"] = json!("failed");
      event["failed_at"] = json!(now());
      event["error"] = json!(bounded_error(&error));
      Ok(())
    })?;
  }
  Ok(())
}

pub fn resume_from_fd(fd: i32) -> Result<()> {
  if fd < 3 {
    return Err(message("invalid inherited supervisor handoff descriptor"));
  }
  set_cloexec(fd, true)?;
  let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
  if seals < 0 || seals & REQUIRED_SEALS != REQUIRED_SEALS {
    return Err(message("supervisor handoff must be a sealed memory file"));
  }
  let mut handoff_file = unsafe { File::from_raw_fd(fd) };
  handoff_file.seek(SeekFrom::Start(0))?;
  let mut bytes = Vec::new();
  handoff_file
    .take((REQUEST_LIMIT + 1) as u64)
    .read_to_end(&mut bytes)?;
  if bytes.len() > REQUEST_LIMIT {
    return Err(message("supervisor handoff exceeds its size limit"));
  }
  let payload: Handoff = serde_json::from_slice(&bytes)?;
  if payload.protocol_version != 1
    || payload.supervisor_pid != std::process::id()
    || payload.supervisor_start != process_start(std::process::id())?
  {
    return Err(message("supervisor handoff belongs to a different process"));
  }
  let descriptors = [
    &payload.lease,
    &payload.stdout_pipe,
    &payload.stderr_pipe,
    &payload.stdout_log,
    &payload.stderr_log,
  ];
  let mut numbers: Vec<_> = descriptors.iter().map(|descriptor| descriptor.fd).collect();
  numbers.push(fd);
  numbers.sort_unstable();
  numbers.dedup();
  if numbers.len() != 6 {
    return Err(message("supervisor handoff descriptors must be distinct"));
  }
  payload.lease.validate(libc::S_IFREG)?;
  payload.stdout_pipe.validate(libc::S_IFIFO)?;
  payload.stderr_pipe.validate(libc::S_IFIFO)?;
  payload.stdout_log.validate(libc::S_IFREG)?;
  payload.stderr_log.validate(libc::S_IFREG)?;
  groups::verify_parent()?;
  groups::exit_pending(payload.child_pid)?; // ECHILD rejects arbitrary numeric IDs.
  // All resources become owned before any fallible filesystem/state update.
  // A recoverable acknowledgment failure then keeps the same child and pipes.
  let run_dir = payload.run_dir.clone();
  let lease = unsafe { File::from_raw_fd(payload.lease.fd) };
  let stdout = LogPipe::inherited(
    unsafe { File::from_raw_fd(payload.stdout_pipe.fd) },
    unsafe { File::from_raw_fd(payload.stdout_log.fd) },
  );
  let stderr = LogPipe::inherited(
    unsafe { File::from_raw_fd(payload.stderr_pipe.fd) },
    unsafe { File::from_raw_fd(payload.stderr_log.fd) },
  );
  let output = pump(
    &run_dir,
    &lease,
    payload.child_pid,
    stdout,
    stderr,
    payload.cleanup_groups,
    Some(&payload),
  )?;
  crate::node::run::finish_resumed(&run_dir, output)
}

fn acknowledge_handoff(payload: &Handoff, lease: &impl AsRawFd) -> Result<bool> {
  let run_dir = validate_run_directory(&payload.run_dir)?;
  validate_named_descriptor(&payload.stdout_log, &run_dir.join("logs/stdout.log"))?;
  validate_named_descriptor(&payload.stderr_log, &run_dir.join("logs/stderr.log"))?;
  // A borrowed duplicate validates the inherited open description without
  // introducing an unlock-on-drop wrapper around the supervisor's actual lease.
  let duplicated = unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
  if duplicated < 0 {
    return Err(io::Error::last_os_error().into());
  }
  let borrowed = unsafe { File::from_raw_fd(duplicated) };
  FileLock::verify_inherited_file(&borrowed, &run_dir.join(".run.lock"))?;
  let state = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
  let event = find_event(&state, &payload.request_id)
    .ok_or_else(|| message("supervisor handoff has no durable event"))?;
  if state["supervisor"]["owner_pid"] != std::process::id()
    || state["supervisor"]["owner_start"].as_u64() != Some(payload.supervisor_start)
    || state["supervisor"]["worker_pid"] != payload.child_pid
    || state["queue_cleanup"].as_bool().unwrap_or(false) != payload.cleanup_groups
  {
    return Err(message(
      "supervisor handoff no longer matches the owned run",
    ));
  }
  if state["supervisor"]["generation"] == payload.generation
    && event["status"] == "completed"
    && event["target_digest"] == payload.target_digest
  {
    // A directory fsync can fail after the atomic rename committed the event.
    // Acknowledgment retry observes that completed generation idempotently.
    return Ok(true);
  }
  if state["supervisor"]["generation"]
    .as_u64()
    .and_then(|generation| generation.checked_add(1))
    != Some(payload.generation)
  {
    return Err(message(
      "supervisor handoff no longer matches the owned run",
    ));
  }
  if !matches!(state["status"].as_str(), Some("running" | "waiting"))
    || !state["finished_at"].is_null()
  {
    return Err(message(
      "terminal or inactive runs cannot resume a supervisor",
    ));
  }
  if event["status"] != "restarting"
    || event["target_digest"] != payload.target_digest
    || event["generation"] != payload.generation
  {
    return Err(message(
      "supervisor handoff event does not match its generation",
    ));
  }
  try_update_state(&run_dir.join("run-state.json"), |state| {
    mutate_event(state, &payload.request_id, |event, supervisor| {
      event["status"] = json!("completed");
      event["completed_at"] = json!(now());
      supervisor["generation"] = json!(payload.generation);
      supervisor["target_digest"] = json!(payload.target_digest);
      Ok(())
    })
  })
  .map(|updated| updated.is_some())
}

fn validate_request(request: &SupervisorRestartRequest) -> Result<()> {
  expri_core::identifiers::validate_component(&request.request_id)?;
  if request.reason.trim().is_empty()
    || request.reason.len() > 1024
    || request.reason.chars().any(char::is_control)
  {
    return Err(message(
      "supervisor restart reason must contain 1–1024 bytes without control characters",
    ));
  }
  if request
    .binary
    .as_ref()
    .is_some_and(|path| !path.is_absolute())
  {
    return Err(message(
      "supervisor binary must be an absolute path on the worker",
    ));
  }
  Ok(())
}

fn validate_restartable(run_dir: &Path, state: &Value) -> Result<()> {
  if state["detached"] != true
    || state["supervisor"]["protocol_version"] != 1
    || state["supervisor"]["mode"] != "live"
  {
    return Err(message(
      "live supervisor restart is unsupported for this run; launch it with a supporting native Linux detached worker. Older, Python fallback, and foreground supervisors cannot hand off an existing task",
    ));
  }
  if !matches!(state["status"].as_str(), Some("running" | "waiting"))
    || !state["finished_at"].is_null()
  {
    return Err(message(
      "supervisor restart requires a running or waiting task; preparing and terminal runs cannot restart",
    ));
  }
  if cancel_requested(run_dir)? {
    return Err(message(
      "supervisor restart is unavailable while cancellation is requested",
    ));
  }
  if !lease_busy(run_dir)? {
    return Err(message(
      "run supervisor is no longer active; refusing to adopt or relaunch its worker",
    ));
  }
  let owner = state["supervisor"]["owner_pid"]
    .as_u64()
    .and_then(|pid| u32::try_from(pid).ok())
    .ok_or_else(|| message("missing supervisor process identity"))?;
  if state["supervisor"]["owner_start"].as_u64() != Some(process_start(owner)?) {
    return Err(message("supervisor process identity changed"));
  }
  Ok(())
}

fn validate_run_directory(path: &Path) -> Result<PathBuf> {
  let absolute = std::path::absolute(path)?;
  let root = absolute
    .parent()
    .and_then(Path::parent)
    .and_then(Path::parent)
    .ok_or_else(|| message("invalid supervisor run directory"))?;
  let run_id = absolute
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| message("invalid supervisor run ID"))?;
  if crate::jobs::run_directory(root, run_id)? != absolute {
    return Err(message("supervisor must use its owned run directory"));
  }
  Ok(absolute)
}

fn lease_busy(run_dir: &Path) -> Result<bool> {
  Ok(matches!(
    expri_core::lock::try_lock_file_readonly(&run_dir.join(".run.lock"))?,
    LockAttempt::Busy
  ))
}

fn find_event<'a>(state: &'a Value, request_id: &str) -> Option<&'a Value> {
  state["supervisor"]["events"]
    .as_array()?
    .iter()
    .find(|event| event["request_id"] == request_id)
}

fn request_result(run_dir: &Path, state: &Value, request_id: &str) -> Result<Option<Value>> {
  match read_json(&run_dir.join(RESULT_FILE), REQUEST_LIMIT) {
    Ok(value) if value["request_id"] == request_id => Ok(Some(value)),
    Ok(_) => Ok(find_event(state, request_id).cloned()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => {
      Ok(find_event(state, request_id).cloned())
    }
    Err(error) => Err(error.into()),
  }
}

fn validate_history_capacity(state: &Value) -> Result<()> {
  // Reserve space for the event, its bounded failure diagnostic and a terminal
  // state update. Reject before accepting a request rather than dropping history.
  if serde_json::to_vec_pretty(state)?.len() > MAX_STATE_BYTES.saturating_sub(12 * 1024) {
    return Err(message(
      "run metadata has insufficient space to retain another supervisor generation; this run was not changed",
    ));
  }
  Ok(())
}

fn bounded_error(error: &impl std::fmt::Display) -> String {
  error.to_string().chars().take(2048).collect()
}

fn request_fingerprint(request: &SupervisorRestartRequest) -> Result<String> {
  Ok(hex_digest(&serde_json::to_vec(request)?))
}

fn validate_retry(event: &Value, request: &SupervisorRestartRequest) -> Result<()> {
  if event["request_fingerprint"] != request_fingerprint(request)? {
    return Err(message(
      "restart request ID was already used with different parameters",
    ));
  }
  Ok(())
}

fn restart_report(run_dir: &Path, event: &Value) -> Result<Value> {
  let root = run_dir
    .parent()
    .and_then(Path::parent)
    .and_then(Path::parent)
    .ok_or_else(|| message("invalid run directory"))?;
  let run_id = run_dir
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| message("invalid run identity"))?;
  let mut report = crate::jobs::status_at(root, run_id)?;
  report["restart"] = event.clone();
  Ok(report)
}

fn update_event(
  run_dir: &Path,
  request_id: &str,
  update: impl FnOnce(&mut Value, &mut Value) -> Result<()>,
) -> Result<()> {
  update_state(&run_dir.join("run-state.json"), |state| {
    mutate_event(state, request_id, update)
  })
}

fn mutate_event(
  state: &mut Value,
  request_id: &str,
  update: impl FnOnce(&mut Value, &mut Value) -> Result<()>,
) -> Result<()> {
  let supervisor = &mut state["supervisor"];
  let index = supervisor["events"]
    .as_array()
    .and_then(|events| {
      events
        .iter()
        .position(|event| event["request_id"] == request_id)
    })
    .ok_or_else(|| message("supervisor restart event disappeared"))?;
  let mut event = supervisor["events"][index].take();
  update(&mut event, supervisor)?;
  supervisor["events"][index] = event;
  Ok(())
}

fn reconcile_failure(run_dir: &Path) -> Result<bool> {
  let failed = match read_json(&run_dir.join(RESULT_FILE), REQUEST_LIMIT) {
    Ok(failed) => failed,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
    Err(error) => return Err(error.into()),
  };
  let request_id = failed["request_id"]
    .as_str()
    .ok_or_else(|| message("invalid supervisor failure result"))?;
  try_update_state(&run_dir.join("run-state.json"), |state| {
    let Some(event) = find_event(state, request_id) else {
      // Preserve failed requests even when their original event append never
      // acquired the state lease. The scratch result may only be replaced once
      // this durable history entry exists, making every old request retry safe.
      validate_history_capacity(state)?;
      state["supervisor"]["events"]
        .as_array_mut()
        .ok_or_else(|| message("invalid supervisor event history"))?
        .push(failed.clone());
      return Ok(());
    };
    if event["status"] != "restarting"
      || event["request_fingerprint"] != failed["request_fingerprint"]
    {
      return Ok(());
    }
    mutate_event(state, request_id, |event, _| {
      for field in ["status", "failed_at", "error"] {
        event[field] = failed[field].clone();
      }
      Ok(())
    })
  })
  .map(|updated| updated.is_some())
}

fn now() -> String {
  chrono::Utc::now().to_rfc3339()
}
fn message(text: &str) -> ExpriError {
  ExpriError::Message(text.into())
}

#[cfg(test)]
mod tests;
