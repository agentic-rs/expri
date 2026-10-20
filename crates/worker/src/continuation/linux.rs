use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use expri_core::error::{ExpriError, Result};
use expri_core::lock::LockAttempt;
use expri_core::protocol::ContinueRunRequest;
use expri_core::run_state::{MAX_STATE_BYTES, is_terminal_status, update_state};
use expri_core::task_attempts;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::environment::{self, EnvironmentRequest, PreparedEnvironment};
use crate::run_logs::groups;
use crate::supervisor::linux::transport::{cancel_requested, hex_digest, process_start, read_json};

const CONTROL: &str = ".task-continuation.json";
const RECORD_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedContinuation {
  pub request: ContinueRunRequest,
  pub attempt: u64,
  pub previous_attempt: u64,
  fingerprint: String,
  binary: PathBuf,
  code_dir: PathBuf,
  checkpoint_path: PathBuf,
  checkpoint_digest: String,
  checkpoint_size: u64,
  prepared: PreparedEnvironment,
  stage: String,
  #[serde(default)]
  capture_id: Option<String>,
  #[serde(default)]
  capture_started_at: Option<String>,
  #[serde(default = "legacy_grace")]
  pub(crate) grace_seconds: u32,
}

fn legacy_grace() -> u32 {
  2
}

struct PreparationMode {
  recorded_source: bool,
  grace_seconds: u32,
}

pub fn request_continue(run_dir: &Path, request: &ContinueRunRequest) -> Result<Value> {
  request_continue_inner(run_dir, request, false, 2)
}

pub(crate) fn request_recorded(
  run_dir: &Path,
  request: &ContinueRunRequest,
  grace_seconds: u32,
) -> Result<Value> {
  request_continue_inner(run_dir, request, true, grace_seconds)
}

fn request_continue_inner(
  run_dir: &Path,
  request: &ContinueRunRequest,
  recorded: bool,
  grace_seconds: u32,
) -> Result<Value> {
  validate_request(request)?;
  let run_dir = crate::supervisor::linux::validate_run_directory(run_dir)?;
  let path = run_dir.join("run-state.json");
  let fingerprint = hex_digest(&serde_json::to_vec(request)?);
  let state = read_json(&path, MAX_STATE_BYTES)?;
  validate_identity(&run_dir, &state)?;
  if let Some(attempt) = find_request(&state, &request.request_id) {
    verify_fingerprint(attempt, &fingerprint)?;
    if attempt_result_ready(&state, attempt) {
      return report(&run_dir, attempt);
    }
  }
  let LockAttempt::Acquired(_control) =
    expri_core::lock::try_lock_file(&run_dir.join(".supervisor-request.lock"), true)?
  else {
    return wait_for_result(&run_dir, request, &fingerprint);
  };
  let state = read_json(&path, MAX_STATE_BYTES)?;
  if let Some(attempt) = find_request(&state, &request.request_id) {
    verify_fingerprint(attempt, &fingerprint)?;
    if attempt_result_ready(&state, attempt) {
      return report(&run_dir, attempt);
    }
    // A published intent belongs to the supervisor, even when its requester
    // disconnects. Re-preparing it could undo an in-flight stop or launch.
    if let Some(prepared) = read_prepared(&run_dir)?
      && prepared.request.request_id == request.request_id
    {
      verify_control(&prepared, &fingerprint)?;
      return wait_for_result(&run_dir, request, &fingerprint);
    }
  }
  if let Some(prepared) = read_prepared(&run_dir)?
    && find_request(&state, &prepared.request.request_id)
      .is_some_and(|attempt| attempt["status"] == "preparing")
  {
    return Err(message(
      "another task continuation is still in progress; inspect or retry its request ID",
    ));
  }
  validate_run(&run_dir, &state)?;
  expri_client::publishing::check_attempt_support(&run_dir)?;
  task_attempts::next_metric_step(&run_dir.join("outputs/metrics.jsonl"))?;
  let original_snapshot = read_json(&run_dir.join("snapshot.json"), RECORD_LIMIT).ok();
  let current = if state.get("task_attempts").is_some() {
    task_attempts::current_attempt(&state)?
  } else {
    1
  };
  if request
    .expected_attempt
    .is_some_and(|expected| expected != current)
  {
    return Err(message(
      "current task attempt changed; inspect the run before continuing",
    ));
  }
  let attempt = update_state(&path, |state| {
    task_attempts::ensure_initial(state)?;
    fill_initial_capture(state);
    if let Some(snapshot) = original_snapshot {
      let entry = entry_mut(state, 1)?;
      entry["source"] = source_summary(&snapshot)?;
    }
    if let Some(existing) = find_request(state, &request.request_id) {
      verify_fingerprint(existing, &fingerprint)?;
      return existing["attempt"]
        .as_u64()
        .ok_or_else(|| message("invalid task attempt"));
    }
    let number = task_attempts::next_attempt(state)?;
    if number > expri_core::protocol::MAX_TASK_ATTEMPTS as u64 {
      return Err(message(
        "run reached the maximum retained task attempt count",
      ));
    }
    task_attempts::begin_attempt(
      state,
      json!({"attempt":number,"request_id":request.request_id,"reason":request.reason,"requested_at":now(),"request_fingerprint":fingerprint,"metric_step_offset":"0","task":request.run.name,"command":request.run.command,"checkpoint":{"path":request.checkpoint}}),
    )?;
    Ok(number)
  })?;
  let terminal_before_preparation = matches!(
    state["status"].as_str(),
    Some("completed" | "failed" | "cancelled")
  );
  let mut prepared = match prepare(
    &run_dir,
    request,
    attempt,
    current,
    &fingerprint,
    terminal_before_preparation,
    PreparationMode {
      recorded_source: recorded,
      grace_seconds,
    },
  ) {
    Ok(prepared) => prepared,
    Err(error) => {
      fail_attempt(&run_dir, attempt, &error.to_string())?;
      return report_request(&run_dir, &request.request_id);
    }
  };
  prepared.stage = "ready".into();
  write_prepared(&run_dir, &prepared)?;
  wait_for_result(&run_dir, request, &fingerprint)
}

fn prepare(
  run_dir: &Path,
  request: &ContinueRunRequest,
  attempt: u64,
  previous_attempt: u64,
  fingerprint: &str,
  terminal_before_preparation: bool,
  mode: PreparationMode,
) -> Result<PreparedContinuation> {
  let root = repo_root(run_dir)?;
  let directory = run_dir.join("attempts").join(attempt.to_string());
  let snapshot = if directory.exists() {
    let manifest = read_json(&directory.join("snapshot.json"), RECORD_LIMIT)?;
    if manifest["run_id"]
      != run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
    {
      return Err(message(
        "continuation snapshot has a different run identity",
      ));
    }
    crate::environment::snapshot::RunSnapshot {
      run_id: run_dir.file_name().unwrap().to_string_lossy().into_owned(),
      run_dir: run_dir.into(),
      code_dir: directory.join("code"),
      assets: crate::assets::discover(&directory.join("code"))?,
    }
  } else if mode.recorded_source {
    create_directory(&run_dir.join("attempts"))?;
    let state = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
    let (code, manifest) = crate::run_control::recorded_source(run_dir, &state)?;
    environment::snapshot::copy_recorded(
      &code,
      &manifest,
      &directory,
      run_dir.file_name().unwrap().to_str().unwrap(),
    )?
  } else {
    environment::snapshot::create_attempt(
      root,
      run_dir,
      attempt,
      &request.run.remote_managed,
      request.run.expected_sync.as_ref(),
    )?
  };
  let checkpoint_directory = directory.join("checkpoint");
  create_directory(&checkpoint_directory)?;
  let checkpoint = checkpoint_directory.join("checkpoint.bin");
  let (checkpoint_digest, checkpoint_size) = if checkpoint.exists() {
    checkpoint_digest(&checkpoint)?
  } else {
    pin_checkpoint(run_dir, &request.checkpoint, &checkpoint)?
  };
  if mode.recorded_source {
    // A writer cannot replace the finalized file between the initial readiness
    // check and pinning and silently supply a different checkpoint.
    expri_client::registrations::verify_registered(run_dir, &request.checkpoint)?;
  }
  crate::node::run::prepare_assets(
    root,
    &snapshot,
    request
      .run
      .service
      .as_ref()
      .map(|service| service.client_config.as_path()),
    None,
    &mut || Ok(!terminal_before_preparation && cancel_requested(run_dir)?),
  )?;
  let environment = environment::prepare(&EnvironmentRequest {
    environment: request.run.environment.clone(),
    repo_root: snapshot.code_dir.to_string_lossy().into_owned(),
    state_dir: directory.to_string_lossy().into_owned(),
    operation: "run".into(),
    extras: request.run.extras.clone(),
    sync_args: request.run.sync_args.clone(),
    install_project: true,
    cache_dir: Some(
      environment::cache_dir(root, &request.run.sync_args)?
        .to_string_lossy()
        .into_owned(),
    ),
  })?;
  let source = read_json(&directory.join("snapshot.json"), RECORD_LIMIT)?;
  create_directory(&run_dir.join("outputs/attempts"))?;
  let public = run_dir.join("outputs/attempts").join(attempt.to_string());
  create_directory(&public)?;
  for (source_path, name) in [
    (directory.join("snapshot.json"), "snapshot.json"),
    (
      PathBuf::from(&environment.manifest_path),
      "environment-state.json",
    ),
  ] {
    let mut input = File::open(source_path)?;
    let mut output = tempfile::NamedTempFile::new_in(&public)?;
    std::io::copy(&mut input, &mut output)?;
    output.as_file().sync_all()?;
    output
      .persist(public.join(name))
      .map_err(|error| error.error)?;
  }
  update_state(&run_dir.join("run-state.json"), |state| {
    let entry = entry_mut(state, attempt)?;
    entry["source"] = source_summary(&source)?;
    entry["code_dir"] = json!(snapshot.code_dir);
    entry["environment_manifest"] = json!(environment.manifest_path);
    entry["checkpoint"] = json!({"path":request.checkpoint,"pinned_path":checkpoint,"sha256":checkpoint_digest,"size":checkpoint_size});
    Ok(())
  })?;
  super::sync_attempt_records(run_dir)?;
  Ok(PreparedContinuation {
    request: request.clone(),
    attempt,
    previous_attempt,
    fingerprint: fingerprint.into(),
    binary: std::env::current_exe()?,
    code_dir: snapshot.code_dir,
    checkpoint_path: checkpoint,
    checkpoint_digest,
    checkpoint_size,
    prepared: environment,
    stage: "preparing".into(),
    capture_id: None,
    capture_started_at: None,
    grace_seconds: mode.grace_seconds,
  })
}

pub(crate) fn pending(run_dir: &Path) -> Result<Option<PreparedContinuation>> {
  let Some(prepared) = read_prepared(run_dir)? else {
    return Ok(None);
  };
  if prepared.stage != "ready" {
    return Ok(None);
  }
  let state = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
  let entry = find_request(&state, &prepared.request.request_id)
    .ok_or_else(|| message("continuation intent has no recorded attempt"))?;
  verify_fingerprint(entry, &prepared.fingerprint)?;
  if entry["status"] != "preparing" {
    return Ok(None);
  }
  if task_attempts::current_attempt(&state)? != prepared.previous_attempt {
    return Err(message("continuation belongs to an older task attempt"));
  }
  Ok(Some(prepared))
}

fn read_prepared(run_dir: &Path) -> Result<Option<PreparedContinuation>> {
  match read_json(&run_dir.join(CONTROL), RECORD_LIMIT) {
    Ok(value) => Ok(Some(serde_json::from_value(value)?)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error.into()),
  }
}

fn verify_control(prepared: &PreparedContinuation, fingerprint: &str) -> Result<()> {
  if prepared.fingerprint != fingerprint {
    return Err(message(
      "continuation intent has different request parameters",
    ));
  }
  Ok(())
}

pub(crate) fn complete_old_attempt(
  run_dir: &Path,
  prepared: &mut PreparedContinuation,
  exit_code: Option<i32>,
) -> Result<()> {
  prepared.stage = "stopped".into();
  write_prepared(run_dir, prepared)?;
  let end = now();
  let mut state = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
  let was_terminal = is_terminal_status(state["status"].as_str().unwrap_or_default());
  let previous_end = if was_terminal {
    state["task_attempts"]["attempts"]
      .as_array()
      .and_then(|attempts| {
        attempts
          .iter()
          .find(|attempt| attempt["attempt"] == prepared.previous_attempt)
      })
      .and_then(|attempt| attempt["finished_at"].as_str())
      .or(state["finished_at"].as_str())
      .unwrap_or(&end)
      .to_owned()
  } else {
    end.clone()
  };
  let capture_end = state["system_capture_finished_at"]
    .as_str()
    .unwrap_or(&previous_end)
    .to_owned();
  let capture_owner = capture_attempt(&state).unwrap_or(prepared.previous_attempt);
  state["status"] = json!("completed");
  state["finished_at"] = json!(capture_end);
  expri_system::store::run_capture::finalize_state(run_dir, &mut state);
  if state["system_capture_id"].is_string() && state["system_capture_status"] == "capturing" {
    return Err(message(
      "previous system capture could not finish; no replacement worker was launched",
    ));
  }
  state["system_capture_finished_at"] = json!(capture_end);
  if was_terminal && cancel_requested(run_dir)? {
    fs::remove_file(run_dir.join(".cancel-request"))?;
  }
  update_state(&run_dir.join("run-state.json"), |current| {
    if task_attempts::current_attempt(current)? != prepared.previous_attempt {
      return Err(message("current task attempt changed during continuation"));
    }
    if !was_terminal {
      task_attempts::finish_attempt(
        current,
        prepared.previous_attempt,
        "continued",
        &end,
        exit_code,
        None,
      )?;
    }
    for field in [
      "system_capture_status",
      "system_capture_error",
      "system_capture_finished_at",
    ] {
      if let Some(value) = state.get(field) {
        current[field] = value.clone();
      }
    }
    copy_capture_to_attempt(current, capture_owner)?;
    entry_mut(current, prepared.attempt)?["previous_worker_stopped_at"] = json!(previous_end);
    current["status"] = json!("preparing");
    // Continue explicitly starts a new attempt; a former manual pause belongs
    // to the replaced task. The current resource policy and hold remain.
    current["manual_paused"] = json!(false);
    current.as_object_mut().unwrap().remove("task_started_at");
    current["finished_at"] = Value::Null;
    current.as_object_mut().unwrap().remove("error");
    current.as_object_mut().unwrap().remove("exit_code");
    Ok(())
  })?;
  let step_offset = task_attempts::next_metric_step(&run_dir.join("outputs/metrics.jsonl"))?;
  update_state(&run_dir.join("run-state.json"), |state| {
    entry_mut(state, prepared.attempt)?["metric_step_offset"] = json!(step_offset.to_string());
    Ok(())
  })?;
  super::sync_attempt_records(run_dir)?;
  expri_client::publishing::begin_attempt(
    run_dir,
    &prepared.request.request_id,
    prepared.previous_attempt,
    prepared.attempt,
  )?;
  Ok(())
}

pub(crate) fn mark_stopping(run_dir: &Path, prepared: &mut PreparedContinuation) -> Result<()> {
  prepared.stage = "stopping".into();
  write_prepared(run_dir, prepared)?;
  update_state(&run_dir.join("run-state.json"), |state| {
    crate::run_control::linux::restore_phase(state);
    Ok(())
  })
}

pub(crate) fn reject_prepared(
  run_dir: &Path,
  prepared: &PreparedContinuation,
  error: &str,
) -> Result<()> {
  fail_attempt(run_dir, prepared.attempt, error)
}

pub(crate) fn launch_replacement(
  run_dir: &Path,
  prepared: &mut PreparedContinuation,
) -> Result<Child> {
  if cancel_requested(run_dir)? {
    return Err(message(
      "run cancellation requested before continuation launch",
    ));
  }
  if !crate::resource_guard::wait_before_start(run_dir)? {
    return Err(message(
      "run cancellation requested before continuation launch",
    ));
  }
  let (digest, size) = checkpoint_digest(&prepared.checkpoint_path)?;
  if digest != prepared.checkpoint_digest || size != prepared.checkpoint_size {
    return Err(message("pinned continuation checkpoint changed"));
  }
  let path = run_dir.join("run-state.json");
  let state = read_json(&path, MAX_STATE_BYTES)?;
  let step = find_request(&state, &prepared.request.request_id)
    .and_then(|entry| entry["metric_step_offset"].as_str())
    .ok_or_else(|| message("continuation metric step offset is missing"))?
    .to_owned();
  let collection_ready = expri_core::storage::StoragePaths::resolve(None)
    .and_then(|paths| expri_client::worker_service::ensure_collection_ready(&paths))
    .is_ok();
  let started = now();
  let mut capture_state = state;
  if collection_ready {
    let capture_id = format!("capture-{}", uuid::Uuid::new_v4().simple());
    prepared.capture_id = Some(capture_id.clone());
    prepared.capture_started_at = Some(started.clone());
    // The stopped stage still proves no child was launched. Publish the exact
    // capture identity before SQLite or the canonical stream can expose it.
    write_prepared(run_dir, prepared)?;
    expri_system::store::run_capture::begin_attempt_state_reserved(
      run_dir,
      &mut capture_state,
      &started,
      &capture_id,
    )?;
    if capture_state["system_capture_id"] != capture_id {
      prepared.capture_id = None;
      prepared.capture_started_at = None;
      write_prepared(run_dir, prepared)?;
    }
  } else {
    for field in [
      "system_capture_id",
      "system_capture_status",
      "system_capture_started_at",
      "system_capture_start_offset",
    ] {
      capture_state.as_object_mut().unwrap().remove(field);
    }
    capture_state["system_capture_error"] = json!(
      "System collection was unavailable when the attempt started; local training continues."
    );
  }
  update_state(&path, |state| {
    for field in [
      "system_capture_id",
      "system_capture_status",
      "system_capture_error",
      "system_capture_started_at",
      "system_capture_start_offset",
      "machine_id",
    ] {
      if let Some(value) = capture_state.get(field) {
        state[field] = value.clone();
      } else {
        state.as_object_mut().unwrap().remove(field);
      }
    }
    state
      .as_object_mut()
      .unwrap()
      .remove("system_capture_finished_at");
    state["status"] = json!("running");
    state["task"] = json!(prepared.request.run.name);
    state["command"] = json!(prepared.request.run.command);
    state["task_started_at"] = json!(started);
    state["task_process_group_clean"] = json!(false);
    state["task_launch_uncertain"] = json!(true);
    state["current_code_dir"] = json!(prepared.code_dir);
    state["current_environment_manifest"] = json!(prepared.prepared.manifest_path);
    let entry = entry_mut(state, prepared.attempt)?;
    entry["metrics_start_offset"] = json!(file_size(&run_dir.join("outputs/metrics.jsonl"))?);
    entry["stdout_start_offset"] = json!(file_size(&run_dir.join("logs/stdout.log"))?);
    entry["stderr_start_offset"] = json!(file_size(&run_dir.join("logs/stderr.log"))?);
    Ok(())
  })?;
  prepared.stage = "launch_intent".into();
  write_prepared(run_dir, prepared)?;
  let mut command = task_command(run_dir, prepared, &step)?;
  command
    .process_group(0)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  let mut child = None;
  let activated = update_state(&path, |state| {
    // Keep the short state lease from spawn through activation. A task which
    // immediately reports running/waiting will wait for its own attempt record
    // instead of seeing the continued predecessor as current.
    let spawned = command.spawn()?;
    let pid = spawned.id();
    child = Some(spawned);
    task_attempts::activate_attempt(state, prepared.attempt, &started, pid)?;
    let owner_changed =
      state["supervisor"]["owner_pid"].as_u64() != Some(std::process::id().into());
    if owner_changed && let Some(generation) = state["supervisor"]["generation"].as_u64() {
      state["supervisor"]["generation"] = json!(
        generation
          .checked_add(1)
          .ok_or_else(|| message("supervisor generation exhausted"))?
      );
    }
    state["supervisor"]["worker_pid"] = json!(pid);
    state["supervisor"]["owner_pid"] = json!(std::process::id());
    state["supervisor"]["owner_start"] = json!(process_start(std::process::id())?);
    if state["supervisor"]["protocol_version"].is_null() {
      state["supervisor"]["protocol_version"] = json!(1);
      state["supervisor"]["generation"] = json!(1);
      state["supervisor"]["mode"] = json!("live");
      state["supervisor"]["events"] = json!([]);
    }
    state["task_continuation_protocol_version"] = json!(1);
    copy_capture_to_attempt(state, prepared.attempt)?;
    Ok(())
  });
  if let Err(error) = activated {
    if let Some(child) = child.as_mut() {
      stop_spawned(run_dir, child);
    } else {
      let _ = record_spawn_cleanup(run_dir, true);
    }
    return Err(error);
  }
  let mut child = child.expect("successful task activation spawned its child");
  prepared.stage = "running".into();
  if let Err(error) = write_prepared(run_dir, prepared) {
    stop_spawned(run_dir, &mut child);
    return Err(error);
  }
  if let Err(error) = update_state(&path, |state| {
    state["task_launch_uncertain"] = json!(false);
    Ok(())
  }) {
    stop_spawned(run_dir, &mut child);
    return Err(error);
  }
  // The run state is authoritative. A metadata-document failure after a child
  // starts must not interrupt that owned task or its log pump.
  let _ = super::sync_attempt_records(run_dir);
  Ok(child)
}

pub fn worker(run_dir: &Path, request_id: &str) -> Result<()> {
  let run_dir = crate::supervisor::linux::validate_run_directory(run_dir)?;
  let LockAttempt::Acquired(lease) =
    expri_core::lock::try_lock_file(&run_dir.join(".run.lock"), true)?
  else {
    return Err(message("run already has an active supervisor"));
  };
  let state = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
  validate_run(&run_dir, &state)?;
  if !matches!(
    state["status"].as_str(),
    Some("completed" | "failed" | "cancelled")
  ) {
    return Err(message(
      "a successor supervisor requires a terminal run; lost child ownership cannot be adopted",
    ));
  }
  let mut prepared =
    pending(&run_dir)?.ok_or_else(|| message("run has no prepared continuation"))?;
  if prepared.request.request_id != request_id {
    return Err(message("continuation request ID does not match"));
  }
  let result = (|| {
    complete_old_attempt(&run_dir, &mut prepared, None)?;
    let stdout = open_log(&run_dir.join("logs/stdout.log"))?;
    let stderr = open_log(&run_dir.join("logs/stderr.log"))?;
    let child = launch_replacement(&run_dir, &mut prepared)?;
    let output = crate::supervisor::linux::resume_task(&run_dir, &lease, child, stdout, stderr)?;
    crate::node::run::finish_resumed(&run_dir, output)
  })();
  if let Err(error) = &result {
    fail_after_stop(&run_dir, &prepared, error)?;
  }
  result
}

pub(crate) fn fail_after_stop(
  run_dir: &Path,
  prepared: &PreparedContinuation,
  error: &ExpriError,
) -> Result<()> {
  let observed = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
  if task_attempts::current_attempt(&observed)? == prepared.attempt
    && is_terminal_status(observed["status"].as_str().unwrap_or_default())
    && find_request(&observed, &prepared.request.request_id)
      .is_some_and(|entry| is_terminal_status(entry["status"].as_str().unwrap_or_default()))
  {
    // The regular finalizer already recorded the replacement task's actual
    // failure/cancellation. Its returned error is not another launch failure.
    return super::sync_attempt_records(run_dir);
  }
  recover_reserved_capture(run_dir, prepared)?;
  fail_attempt(run_dir, prepared.attempt, &error.to_string())?;
  let mut terminal = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
  let failure_time = find_request(&terminal, &prepared.request.request_id)
    .and_then(|entry| entry["finished_at"].as_str())
    .map(str::to_owned)
    .unwrap_or_else(now);
  let capture_end = terminal["system_capture_finished_at"]
    .as_str()
    .unwrap_or(&failure_time)
    .to_owned();
  let cancelled = cancel_requested(run_dir)?;
  let phase = if cancelled { "cancelled" } else { "failed" };
  terminal["status"] = json!(phase);
  terminal["finished_at"] = json!(capture_end);
  expri_system::store::run_capture::close_state(run_dir, &mut terminal);
  update_state(&run_dir.join("run-state.json"), |state| {
    state["status"] = json!(phase);
    state["finished_at"] = json!(failure_time);
    state["exit_code"] = json!(if cancelled { 130 } else { 1 });
    if cancelled {
      entry_mut(state, prepared.attempt)?["status"] = json!("cancelled");
    }
    state["error"] = json!(error.to_string());
    for field in ["system_capture_status", "system_capture_error"] {
      if let Some(value) = terminal.get(field) {
        state[field] = value.clone();
      }
    }
    let previous_capture =
      entry_mut(state, prepared.previous_attempt)?["system_capture_id"].clone();
    if state["system_capture_id"].is_string() && state["system_capture_id"] != previous_capture {
      state["system_capture_finished_at"] = json!(capture_end);
      copy_capture_to_attempt(state, prepared.attempt)?;
    }
    Ok(())
  })?;
  super::sync_attempt_records(run_dir)
}

fn recover_reserved_capture(run_dir: &Path, prepared: &PreparedContinuation) -> Result<()> {
  let (Some(capture_id), Some(started_at)) = (&prepared.capture_id, &prepared.capture_started_at)
  else {
    return Ok(());
  };
  let path = run_dir.join("run-state.json");
  let mut restored = read_json(&path, MAX_STATE_BYTES)?;
  if !expri_system::store::run_capture::recover_attempt_state(
    run_dir,
    &mut restored,
    capture_id,
    started_at,
  )? {
    // No registration means no header could have been written. Keep the old
    // capture association rather than inventing an empty successor segment.
    return Ok(());
  }
  update_state(&path, |state| {
    let entry = find_request(state, &prepared.request.request_id)
      .ok_or_else(|| message("journaled capture attempt disappeared"))?;
    verify_fingerprint(entry, &prepared.fingerprint)?;
    for field in [
      "system_capture_id",
      "system_capture_started_at",
      "system_capture_start_offset",
      "system_capture_status",
      "system_capture_finished_at",
    ] {
      if let Some(value) = restored.get(field) {
        state[field] = value.clone();
      } else {
        state.as_object_mut().unwrap().remove(field);
      }
    }
    copy_capture_to_attempt(state, prepared.attempt)
  })
}

fn task_command(run_dir: &Path, prepared: &PreparedContinuation, step: &str) -> Result<Command> {
  let argv = environment::task_argv(&prepared.request.run.command)?;
  let mut command = Command::new(&argv[0]);
  environment::configure_command(
    &mut command,
    &prepared.request.run.environment,
    &prepared.prepared,
  );
  command
    .args(&argv[1..])
    .current_dir(&prepared.code_dir)
    .env("EXPRI_BIN", &prepared.binary)
    .env("EXPRI_RUN_ID", run_dir.file_name().unwrap())
    .env("EXPRI_RUN_DIR", run_dir)
    .env("EXPRI_OUTPUT_DIR", run_dir.join("outputs"))
    .env("EXPRI_ATTEMPT", prepared.attempt.to_string())
    .env("EXPRI_STEP_OFFSET", step)
    .env("EXPRI_RESUME_CHECKPOINT", &prepared.checkpoint_path)
    .env_remove("EXPRI_INPUT_DIR");
  crate::run_logs::unbuffered_if_absent(&mut command);
  Ok(command)
}

fn spawn_worker(run_dir: &Path, request_id: &str, binary: &Path) -> Result<()> {
  let mut command = Command::new(binary);
  command
    .args(["node", "continue-worker", "--run-dir"])
    .arg(run_dir)
    .args(["--request-id", request_id])
    .current_dir(repo_root(run_dir)?)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
  unsafe {
    command.pre_exec(|| {
      if libc::setsid() < 0 {
        Err(std::io::Error::last_os_error())
      } else {
        Ok(())
      }
    });
  }
  command.spawn()?;
  Ok(())
}

fn wait_for_result(
  run_dir: &Path,
  request: &ContinueRunRequest,
  fingerprint: &str,
) -> Result<Value> {
  let grace = read_prepared(run_dir)?.map_or(2, |prepared| prepared.grace_seconds);
  let deadline = Instant::now() + Duration::from_secs(u64::from(grace) + 180);
  let mut launched = false;
  loop {
    let state = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
    validate_identity(run_dir, &state)?;
    if let Some(attempt) = find_request(&state, &request.request_id) {
      verify_fingerprint(attempt, fingerprint)?;
      if attempt_result_ready(&state, attempt) {
        return report(run_dir, attempt);
      }
      if !launched
        && let Some(prepared) = read_prepared(run_dir)?
        && prepared.request.request_id == request.request_id
      {
        verify_control(&prepared, fingerprint)?;
        let lease_available = matches!(
          expri_core::lock::try_lock_file_readonly(&run_dir.join(".run.lock"))?,
          LockAttempt::Acquired(_) | LockAttempt::Missing
        );
        if prepared.stage == "stopped"
          && !lease_available
          && state["status"] == "pending"
          && state["resource_status"]["blocked"] == true
          && state["task_process_group_clean"] == true
          && state["supervisor"]["worker_pid"].is_null()
          && state["task_launch_uncertain"] != true
        {
          // The old task is gone and this durable intent is owned locally.
          // Returning its pending receipt frees the lifecycle command lock so
          // the user can change the policy or stop the blocked continuation.
          return report(run_dir, attempt);
        }
        if prepared.stage == "ready"
          && matches!(
            state["status"].as_str(),
            Some("completed" | "failed" | "cancelled")
          )
          && lease_available
        {
          // The old task can naturally finish between preparation and
          // consuming the intent. Only a terminal state plus the released
          // lease permits a fresh supervisor; its own lease is the final
          // exclusion guard if several retrying clients observe this.
          spawn_worker(run_dir, &request.request_id, &prepared.binary)?;
          launched = true;
        } else if prepared.stage == "stopped" && lease_available {
          if let LockAttempt::Acquired(_lease) =
            expri_core::lock::try_lock_file(&run_dir.join(".run.lock"), true)?
          {
            let fresh = read_prepared(run_dir)?;
            if fresh.as_ref().is_some_and(|intent| {
              intent.stage == "stopped" && intent.request.request_id == request.request_id
            }) {
              // This stage is written only after the owned old group has
              // stopped, and before the launch intent. It proves no task
              // was launched, so an interrupted handoff can fail durably.
              record_spawn_cleanup(run_dir, true)?;
              fail_after_stop(
                run_dir,
                &prepared,
                &message(
                  "continuation supervisor stopped after the previous task exited; no replacement task was launched",
                ),
              )?;
            }
          }
        } else if prepared.stage != "ready" && lease_available {
          return Err(message(
            "supervisor ownership was lost during continuation; the task was not relaunched because its launch outcome is uncertain",
          ));
        } else if lease_available {
          return Err(message(
            "the previous supervisor stopped without a verified terminal state; continuation cannot adopt or relaunch its task",
          ));
        }
      }
    }
    if Instant::now() >= deadline {
      return Err(message(
        "continuation acknowledgment timed out; retry the same request ID to inspect its durable result",
      ));
    }
    thread::sleep(Duration::from_millis(50));
  }
}

fn validate_request(request: &ContinueRunRequest) -> Result<()> {
  expri_core::identifiers::validate_component(&request.request_id)?;
  if request.reason.trim().is_empty()
    || request.reason.len() > 1024
    || request.reason.chars().any(char::is_control)
  {
    return Err(message(
      "continuation reason must contain 1–1024 bytes without controls",
    ));
  }
  if !request.run.detach {
    return Err(message("task continuation requires a detached task"));
  }
  request.run.environment.validate()?;
  request.run.requirements.validate()?;
  environment::task_argv(&request.run.command)?;
  if !request.checkpoint.starts_with("outputs/")
    || Path::new(&request.checkpoint)
      .components()
      .any(|component| !matches!(component, Component::Normal(_)))
  {
    return Err(message(
      "checkpoint must be a safe run-relative path below outputs",
    ));
  }
  Ok(())
}

fn validate_run(run_dir: &Path, state: &Value) -> Result<()> {
  validate_identity(run_dir, state)?;
  if state["task_launch_uncertain"] == true {
    return Err(message(
      "a previous task launch or cleanup could not be verified; continuation cannot safely start another worker",
    ));
  }
  if state["queue_cleanup"] == true {
    return Err(message(
      "queued run continuation is unsupported; it requires scheduler resource allocation",
    ));
  }
  if state["detached"] != true {
    return Err(message("task continuation requires a detached run"));
  }
  let status = state["status"].as_str().unwrap_or_default();
  if matches!(status, "running" | "waiting" | "paused") {
    if state["task_continuation_protocol_version"] != 1 {
      return Err(message(
        "this active supervisor does not support checkpoint continuation; upgrade its supervisor first",
      ));
    }
    if !matches!(
      expri_core::lock::try_lock_file_readonly(&run_dir.join(".run.lock"))?,
      LockAttempt::Busy
    ) {
      return Err(message(
        "active run has lost supervisor ownership; it cannot continue safely",
      ));
    }
    let owner = state["supervisor"]["owner_pid"]
      .as_u64()
      .and_then(|pid| u32::try_from(pid).ok())
      .filter(|pid| *pid > 1)
      .ok_or_else(|| message("active run has no valid supervisor identity"))?;
    if state["supervisor"]["protocol_version"] != 1
      || state["supervisor"]["mode"] != "live"
      || state["supervisor"]["owner_start"].as_u64() != Some(process_start(owner)?)
    {
      return Err(message("active run supervisor process identity changed"));
    }
    if cancel_requested(run_dir)? {
      return Err(message("run cancellation is in progress"));
    }
  } else if !matches!(status, "completed" | "failed" | "cancelled") {
    return Err(message(
      "task continuation requires running, waiting, paused, completed, failed, or cancelled state",
    ));
  } else if state["task_process_group_clean"] != true {
    // Historical supervisors did not necessarily clean descendants after their
    // direct child exited. An absent process group is safe evidence; a stored
    // numeric identity alone never authorizes signalling or adopting a group.
    let worker = state["supervisor"]["worker_pid"]
      .as_u64()
      .and_then(|pid| i32::try_from(pid).ok())
      .filter(|pid| *pid > 1)
      .ok_or_else(|| message("historical run has no verifiable task process-group identity; continuation cannot prove its former workers stopped"))?;
    if unsafe { libc::kill(-worker, 0) } == 0
      || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
      return Err(message(
        "former task process group may still contain workers; continuation cannot safely replace it",
      ));
    }
  }
  Ok(())
}

fn validate_identity(run_dir: &Path, state: &Value) -> Result<()> {
  if state["schema_version"] != 1
    || state["run_id"].as_str() != run_dir.file_name().and_then(|name| name.to_str())
  {
    return Err(message(
      "run metadata has an unsupported schema or mismatched identity",
    ));
  }
  let started = state["started_at"]
    .as_str()
    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
    .ok_or_else(|| message("run-state.json has invalid or missing started_at"))?;
  for (field, child) in [("code_dir", "code"), ("output_dir", "outputs")] {
    let expected = run_dir.join(child);
    if state[field].as_str() != expected.to_str() {
      return Err(message(
        "run metadata requires its original owned code and output directories",
      ));
    }
    let metadata = fs::symlink_metadata(expected)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
      return Err(message(
        "run metadata requires real code and output directories",
      ));
    }
  }
  if is_terminal_status(state["status"].as_str().unwrap_or_default()) {
    let finished = state["finished_at"]
      .as_str()
      .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
      .ok_or_else(|| message("terminal run metadata has invalid or missing finished_at"))?;
    if finished < started {
      return Err(message(
        "terminal run finished_at precedes its original started_at",
      ));
    }
  } else if !state["finished_at"].is_null() {
    return Err(message(
      "active run metadata unexpectedly records a finish time",
    ));
  }
  Ok(())
}

fn pin_checkpoint(run_dir: &Path, relative: &str, destination: &Path) -> Result<(String, u64)> {
  let mut current = run_dir.to_path_buf();
  let components: Vec<_> = Path::new(relative).components().collect();
  for component in components.iter().take(components.len().saturating_sub(1)) {
    current.push(component.as_os_str());
    let metadata = fs::symlink_metadata(&current)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
      return Err(message("checkpoint parents must be real run directories"));
    }
  }
  let mut file = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
    .open(run_dir.join(relative))?;
  let before = file.metadata()?;
  if !before.is_file() {
    return Err(message("checkpoint must be a finalized regular file"));
  }
  let mut copy = tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
  std::io::copy(&mut file, &mut copy)?;
  copy.as_file().sync_all()?;
  let after = file.metadata()?;
  if (
    before.dev(),
    before.ino(),
    before.len(),
    before.mtime(),
    before.mtime_nsec(),
  ) != (
    after.dev(),
    after.ino(),
    after.len(),
    after.mtime(),
    after.mtime_nsec(),
  ) {
    return Err(message(
      "checkpoint changed during immutable staging; finalize it before continuation",
    ));
  }
  file.seek(SeekFrom::Start(0))?;
  let mut hash = Sha256::new();
  let mut buffer = [0_u8; 64 * 1024];
  loop {
    let size = file.read(&mut buffer)?;
    if size == 0 {
      break;
    }
    hash.update(&buffer[..size]);
  }
  let original_hash: String = hash
    .finalize()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  let (digest, size) = crate::archive::sha256_file(copy.path())?;
  if original_hash != digest || before.len() != size {
    return Err(message("checkpoint changed while being pinned"));
  }
  copy
    .as_file()
    .set_permissions(fs::Permissions::from_mode(0o400))?;
  copy
    .persist_noclobber(destination)
    .map_err(|error| error.error)?;
  File::open(destination.parent().unwrap())?.sync_all()?;
  Ok((digest, size))
}

fn stop_spawned(run_dir: &Path, child: &mut Child) {
  let cleaned = groups::stop(child).is_ok();
  let _ = child.kill();
  let reaped = child.wait().is_ok();
  let _ = record_spawn_cleanup(run_dir, cleaned && reaped);
}
fn record_spawn_cleanup(run_dir: &Path, verified: bool) -> Result<()> {
  update_state(&run_dir.join("run-state.json"), |state| {
    state["task_process_group_clean"] = json!(verified);
    state["task_launch_uncertain"] = json!(!verified);
    Ok(())
  })
}
fn checkpoint_digest(path: &Path) -> Result<(String, u64)> {
  let mut file = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
    .open(path)?;
  if !file.metadata()?.is_file() {
    return Err(message("pinned checkpoint must remain a regular file"));
  }
  let mut hash = Sha256::new();
  let mut size = 0_u64;
  let mut bytes = [0_u8; 64 * 1024];
  loop {
    let read = file.read(&mut bytes)?;
    if read == 0 {
      break;
    }
    hash.update(&bytes[..read]);
    size += read as u64;
  }
  Ok((
    hash
      .finalize()
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect(),
    size,
  ))
}
fn create_directory(path: &Path) -> Result<()> {
  if !path.exists() {
    fs::create_dir(path)?;
  }
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message("continuation directories must not be symlinks"));
  }
  Ok(())
}
fn open_log(path: &Path) -> Result<File> {
  let file = OpenOptions::new()
    .append(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
    .open(path)?;
  if !file.metadata()?.is_file() {
    return Err(message("existing run log must be a regular file"));
  }
  Ok(file)
}
fn file_size(path: &Path) -> Result<u64> {
  match fs::metadata(path) {
    Ok(metadata) => Ok(metadata.len()),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
    Err(error) => Err(error.into()),
  }
}
fn entry_mut(state: &mut Value, number: u64) -> Result<&mut Value> {
  state["task_attempts"]["attempts"]
    .as_array_mut()
    .and_then(|entries| entries.iter_mut().find(|entry| entry["attempt"] == number))
    .ok_or_else(|| message("task attempt disappeared"))
}
fn find_request<'a>(state: &'a Value, id: &str) -> Option<&'a Value> {
  state["task_attempts"]["attempts"]
    .as_array()?
    .iter()
    .find(|entry| entry["request_id"] == id)
}
fn attempt_result_ready(state: &Value, attempt: &Value) -> bool {
  attempt["status"] != "preparing"
    && !(state["task_launch_uncertain"] == true
      && matches!(
        attempt["status"].as_str(),
        Some("running" | "waiting" | "paused")
      ))
}
fn verify_fingerprint(entry: &Value, fingerprint: &str) -> Result<()> {
  if entry["request_fingerprint"] != fingerprint {
    Err(message(
      "continuation request ID was already used with different parameters",
    ))
  } else {
    Ok(())
  }
}
fn fail_attempt(run_dir: &Path, attempt: u64, error: &str) -> Result<()> {
  update_state(&run_dir.join("run-state.json"), |state| {
    if is_terminal_status(
      entry_mut(state, attempt)?["status"]
        .as_str()
        .unwrap_or_default(),
    ) {
      return Ok(());
    }
    task_attempts::finish_attempt(state, attempt, "failed", &now(), None, Some(error))
  })?;
  super::sync_attempt_records(run_dir)
}
fn report_request(run_dir: &Path, id: &str) -> Result<Value> {
  let state = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
  report(
    run_dir,
    find_request(&state, id).ok_or_else(|| message("continuation result disappeared"))?,
  )
}
fn report(run_dir: &Path, attempt: &Value) -> Result<Value> {
  let id = run_dir
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| message("invalid run ID"))?;
  let mut report = crate::jobs::status_at(repo_root(run_dir)?, id)?;
  report["continuation"] = attempt.clone();
  Ok(report)
}
fn write_prepared(run_dir: &Path, prepared: &PreparedContinuation) -> Result<()> {
  let bytes = serde_json::to_vec(prepared)?;
  if bytes.len() > RECORD_LIMIT {
    return Err(message("continuation intent is too large"));
  }
  let mut file = tempfile::NamedTempFile::new_in(run_dir)?;
  file.write_all(&bytes)?;
  file.as_file().sync_all()?;
  file
    .persist(run_dir.join(CONTROL))
    .map_err(|error| error.error)?;
  File::open(run_dir)?.sync_all()?;
  Ok(())
}
fn repo_root(run_dir: &Path) -> Result<&Path> {
  run_dir
    .parent()
    .and_then(Path::parent)
    .and_then(Path::parent)
    .ok_or_else(|| message("invalid continuation run directory"))
}
fn fill_initial_capture(state: &mut Value) {
  let fields: [(&str, Value); 5] = [
    ("system_capture_id", state["system_capture_id"].clone()),
    ("machine_id", state["machine_id"].clone()),
    (
      "system_capture_started_at",
      state
        .get("system_capture_started_at")
        .unwrap_or(&state["started_at"])
        .clone(),
    ),
    (
      "system_capture_finished_at",
      state
        .get("system_capture_finished_at")
        .unwrap_or(&state["finished_at"])
        .clone(),
    ),
    (
      "system_capture_status",
      state["system_capture_status"].clone(),
    ),
  ];
  if let Ok(entry) = entry_mut(state, 1) {
    for (field, value) in fields {
      if entry[field].is_null() && !value.is_null() {
        entry[field] = value;
      }
    }
  }
}
fn copy_capture_to_attempt(state: &mut Value, attempt: u64) -> Result<()> {
  let fields: Vec<_> = [
    "system_capture_id",
    "machine_id",
    "system_capture_started_at",
    "system_capture_finished_at",
    "system_capture_status",
  ]
  .into_iter()
  .filter_map(|field| state.get(field).map(|value| (field, value.clone())))
  .collect();
  let entry = entry_mut(state, attempt)?;
  for (field, value) in fields {
    entry[field] = value;
  }
  Ok(())
}
fn capture_attempt(state: &Value) -> Option<u64> {
  let capture_id = state["system_capture_id"].as_str()?;
  state["task_attempts"]["attempts"]
    .as_array()?
    .iter()
    .find(|attempt| attempt["system_capture_id"].as_str() == Some(capture_id))?["attempt"]
    .as_u64()
}
fn now() -> String {
  chrono::Utc::now().to_rfc3339()
}
fn source_summary(snapshot: &Value) -> Result<Value> {
  let source = &snapshot["source"];
  Ok(
    json!({"kind":source["kind"],"git_head":source["git_head"],"checkout_manifest_sha256":source["checkout_manifest_sha256"],"sync_state_sha256":source["sync_state_sha256"],"files_sha256":hex_digest(&serde_json::to_vec(&snapshot["files"])?)}),
  )
}
fn message(detail: &str) -> ExpriError {
  ExpriError::Message(detail.into())
}

#[cfg(test)]
mod tests;
