use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use expri_core::error::{ExpriError, Result};
use expri_core::experiment_assignment::RunExperimentIntent;
use expri_core::lock::LockAttempt;
use expri_core::protocol::{ContinueRunRequest, RunRequest};
use expri_core::run_control::{ControlAction, ControlCommand, ControlResult};
use expri_core::run_state::{MAX_STATE_BYTES, is_terminal_status, update_state};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::run_logs::groups;
use crate::supervisor::linux::transport::{hex_digest, read_json, write_json_bounded};

pub(crate) const MAILBOX: &str = ".run-control-pending.json";
const LIMIT: usize = 256 * 1024;

fn write_json(path: &Path, value: &Value) -> Result<()> {
  write_json_bounded(path, value, LIMIT)
}

#[derive(Serialize, Deserialize)]
struct Journal {
  command: ControlCommand,
  fingerprint: String,
  result: ControlResult,
  #[serde(default)]
  stage: String,
  #[serde(default)]
  new_run_dir: Option<PathBuf>,
}

fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}
fn state(run_dir: &Path) -> Result<Value> {
  Ok(read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?)
}
fn terminal(value: &Value) -> bool {
  value["status"].as_str().is_some_and(is_terminal_status)
}
fn attempt(value: &Value) -> u64 {
  value["task_attempts"]["current_attempt"]
    .as_u64()
    .unwrap_or(1)
}
fn generation(value: &Value) -> u64 {
  value["supervisor"]["generation"].as_u64().unwrap_or(0)
}
fn journal_path(run_dir: &Path, request_id: &str) -> PathBuf {
  run_dir
    .join(".run-controls")
    .join(format!("{request_id}.json"))
}
fn result(
  command: &ControlCommand,
  status: &str,
  error: Option<String>,
  new_run_id: Option<String>,
) -> ControlResult {
  ControlResult {
    request_id: command.request_id.clone(),
    status: status.into(),
    error: error.map(|detail| {
      detail
        .chars()
        .filter(|value| !value.is_control())
        .take(512)
        .collect()
    }),
    new_run_id,
  }
}
fn save(run_dir: &Path, journal: &Journal) -> Result<()> {
  write_json(
    &journal_path(run_dir, &journal.command.request_id),
    &serde_json::to_value(journal)?,
  )?;
  Ok(())
}

fn supported(run_dir: &Path, value: &Value) -> Result<()> {
  if value["queue_cleanup"] == true {
    return Err(message(
      "queued runs require scheduler allocation and do not support website controls yet",
    ));
  }
  if value["detached"] != true || value["run_control_protocol_version"] != 1 {
    return Err(message(
      "this run needs a lifecycle-capable native Linux detached supervisor",
    ));
  }
  if value["task_launch_uncertain"] == true {
    return Err(message(
      "a previous launch or cleanup outcome is uncertain; lifecycle controls are unavailable",
    ));
  }
  if terminal(value) && value["task_process_group_clean"] != true {
    return Err(message(
      "the former task group was not verified stopped; lifecycle replacement is unavailable",
    ));
  }
  if !terminal(value)
    && !matches!(
      expri_core::lock::try_lock_file_readonly(&run_dir.join(".run.lock"))?,
      LockAttempt::Busy
    )
  {
    return Err(message("run supervisor ownership was lost"));
  }
  if value["run_id"].as_str() != run_dir.file_name().and_then(|part| part.to_str()) {
    return Err(message("run directory identity changed"));
  }
  Ok(())
}

pub fn inspect(run_dir: &Path) -> Result<Value> {
  let run_dir = crate::supervisor::linux::validate_run_directory(run_dir)?;
  let value = state(&run_dir)?;
  let mut capabilities = Vec::new();
  let mut reason = None;
  match supported(&run_dir, &value) {
    Ok(()) => {
      match value["status"].as_str().unwrap_or_default() {
        "running" | "waiting" => capabilities.extend([ControlAction::Pause, ControlAction::Stop]),
        "paused" => {
          capabilities.extend([ControlAction::Resume, ControlAction::Stop]);
          if value["resource_paused"] == true && value["manual_paused"] != true {
            capabilities.push(ControlAction::Pause);
          }
        }
        "pending" => capabilities.push(ControlAction::Stop),
        _ => {}
      }
      if matches!(
        value["status"].as_str(),
        Some("running" | "waiting" | "paused" | "completed" | "failed" | "cancelled")
      ) {
        capabilities.extend([ControlAction::Continue, ControlAction::Restart]);
      }
      if value["run_requirements_protocol_version"] == 1 || terminal(&value) {
        capabilities.push(ControlAction::SetRequirements);
      }
    }
    Err(error) => reason = Some(error.to_string()),
  }
  let mut request: Option<RunRequest> = read_json(&run_dir.join("run-request.json"), 1024 * 1024)
    .ok()
    .and_then(|value| serde_json::from_value(value).ok());
  if request.is_none() {
    capabilities.clear();
    reason = Some("recorded run request is unavailable".into());
  }
  let experiment_assignment_supported = request
    .as_ref()
    .and_then(|request| request.service.as_ref())
    .is_some_and(|service| service.publish);
  let command = value["command"].as_array().cloned().unwrap_or_else(|| {
    request
      .take()
      .map(|run| run.command.into_iter().map(Value::String).collect())
      .unwrap_or_default()
  });
  let mut checkpoints = registered_checkpoints(&run_dir)?;
  checkpoints.sort();
  let recorded_status = value["status"].as_str().unwrap_or("unknown");
  let status = if matches!(
    recorded_status,
    "preparing" | "pending" | "running" | "waiting" | "paused"
  ) && !matches!(
    expri_core::lock::try_lock_file_readonly(&run_dir.join(".run.lock"))?,
    LockAttempt::Busy
  ) {
    "lost"
  } else {
    recorded_status
  };
  if status == "lost" {
    capabilities.clear();
    reason = Some("run supervisor ownership was lost".into());
  }
  let exit_code = value
    .get("task_exit_code")
    .filter(|value| !value.is_null())
    .unwrap_or(&value["exit_code"]);
  let mut facts = json!({"supported":reason.is_none(),"protocol_version":value["run_control_protocol_version"].as_u64().unwrap_or(0),"attempt":attempt(&value),"generation":generation(&value),"status":status,"capabilities":capabilities,"command":command,"checkpoints":checkpoints,"reason":reason,"requirements":crate::resource_guard::requirements(&value)?,"resource_status":value["resource_status"],"manual_paused":value["manual_paused"].as_bool().unwrap_or(value["status"] == "paused" && value["resource_paused"] != true),"lifecycle":{"started_at":value["started_at"],"finished_at":value["finished_at"],"exit_code":exit_code}});
  facts["experiment_assignment_supported"] = json!(experiment_assignment_supported);
  Ok(facts)
}

fn registered_checkpoints(run_dir: &Path) -> Result<Vec<String>> {
  let registered = expri_client::registrations::list(run_dir)?;
  let mut found = Vec::new();
  for record in registered["files"]
    .as_array()
    .into_iter()
    .flatten()
    .take(128)
  {
    let Some(path) = record["path"].as_str() else {
      continue;
    };
    if !path.starts_with("outputs/")
      || path.len() > 1024
      || path.contains('\\')
      || path.chars().any(char::is_control)
    {
      continue;
    }
    let parts: Vec<_> = Path::new(path).components().collect();
    if parts.len() > 16
      || parts
        .iter()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
      continue;
    }
    let mut current = run_dir.to_path_buf();
    let safe = parts.iter().enumerate().all(|(index, part)| {
      current.push(part);
      fs::symlink_metadata(&current).is_ok_and(|metadata| {
        !metadata.file_type().is_symlink()
          && if index + 1 == parts.len() {
            metadata.is_file()
          } else {
            metadata.is_dir()
          }
      })
    });
    if safe {
      found.push(path.to_owned());
    }
  }
  Ok(found)
}

pub fn execute(run_dir: &Path, command: &ControlCommand) -> Result<ControlResult> {
  expri_core::identifiers::validate_component(&command.request_id)?;
  if command.reason.trim().is_empty()
    || command.reason.len() > 1024
    || command.reason.chars().any(char::is_control)
    || command.grace_seconds == 0
    || command.grace_seconds > 3600
  {
    return Err(message("lifecycle reason or grace period is invalid"));
  }
  if let Some(requirements) = &command.requirements {
    requirements.validate()?;
  }
  if let Some(experiment) = &command.experiment {
    if command.action != ControlAction::Restart {
      return Err(message("only restart accepts an experiment selection"));
    }
    experiment.validate()?;
  }
  if (command.action == ControlAction::SetRequirements) != command.requirements.is_some() {
    return Err(message(
      "set_requirements requires a resource policy, and other actions must not supply one",
    ));
  }
  let run_dir = crate::supervisor::linux::validate_run_directory(run_dir)?;
  let LockAttempt::Acquired(_lock) =
    expri_core::lock::try_lock_file(&run_dir.join(".run-control.lock"), true)?
  else {
    return Err(message(
      "another lifecycle request is being executed for this run",
    ));
  };
  let directory = run_dir.join(".run-controls");
  match fs::create_dir(&directory) {
    Ok(()) => {}
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
    Err(error) => return Err(error.into()),
  }
  let metadata = fs::symlink_metadata(&directory)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message("lifecycle journal must be a real directory"));
  }
  let fingerprint = hex_digest(&serde_json::to_vec(command)?);
  let mut journal = match read_json(&journal_path(&run_dir, &command.request_id), LIMIT) {
    Ok(value) => {
      let journal: Journal = serde_json::from_value(value)?;
      if journal.fingerprint != fingerprint || journal.command != *command {
        return Err(message(
          "lifecycle request ID was already used with different parameters",
        ));
      }
      if journal.result.status != "executing" {
        return Ok(journal.result);
      }
      journal
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      let value = state(&run_dir)?;
      supported(&run_dir, &value)?;
      fence(&value, command)?;
      let expired = chrono::Utc::now().timestamp_millis() >= command.expires_at_ms;
      let journal = Journal {
        command: command.clone(),
        fingerprint,
        result: if expired {
          result(
            command,
            "failed",
            Some("lifecycle request expired before execution".into()),
            None,
          )
        } else {
          result(command, "executing", None, None)
        },
        stage: "accepted".into(),
        new_run_dir: None,
      };
      save(&run_dir, &journal)?;
      if expired {
        return Ok(journal.result);
      }
      journal
    }
    Err(error) => return Err(error.into()),
  };
  let performed = match command.action {
    ControlAction::Pause | ControlAction::Resume | ControlAction::Stop => {
      supervisor_action(&run_dir, command)
    }
    ControlAction::SetRequirements => {
      let current = state(&run_dir)?;
      if !terminal(&current) && current["run_requirements_protocol_version"] != 1 {
        Err(message(
          "this live supervisor does not enforce resource requirements; upgrade it before changing the policy",
        ))
      } else if terminal(&current) {
        update_state(&run_dir.join("run-state.json"), |value| {
          fence(value, command)?;
          if !terminal(value) {
            return Err(message(
              "run became active before requirements were updated",
            ));
          }
          value["requirements"] = serde_json::to_value(command.requirements.as_ref().unwrap())?;
          value.as_object_mut().unwrap().remove("resource_status");
          Ok(())
        })
        .map(|_| None)
      } else {
        supervisor_action(&run_dir, command)
      }
    }
    ControlAction::Continue => continue_recorded(&run_dir, command),
    ControlAction::Restart => restart_recorded(&run_dir, &mut journal),
  };
  if let Err(error) = &performed
    && command.action == ControlAction::Restart
    && matches!(journal.stage.as_str(), "accepted" | "prepared")
    && let Some(new_dir) = &journal.new_run_dir
    && let Ok(prepared) = crate::node::run::load_prepared(new_dir)
  {
    // Preflight never owns a task. Close its optional capture and retain a
    // failed successor record while the original task continues unchanged.
    crate::node::run::abort_prepared(prepared, false, &error.to_string())?;
  }
  journal.result = match performed {
    Ok(new_id) => result(command, "applied", None, new_id),
    Err(error) => result(
      command,
      "failed",
      Some(error.to_string()),
      journal.result.new_run_id.clone(),
    ),
  };
  save(&run_dir, &journal)?;
  Ok(journal.result)
}

fn fence(value: &Value, command: &ControlCommand) -> Result<()> {
  if attempt(value) != command.expected_attempt || generation(value) != command.expected_generation
  {
    return Err(message(
      "task attempt or supervisor generation changed; refresh the run before retrying",
    ));
  }
  Ok(())
}

fn saved_event(value: &Value, command: &ControlCommand) -> Option<ControlResult> {
  value["control_events"]
    .as_array()?
    .iter()
    .find(|entry| entry["request_id"] == command.request_id)
    .and_then(|entry| serde_json::from_value(entry["result"].clone()).ok())
}

fn stop_complete(value: &Value) -> Result<bool> {
  if !terminal(value) {
    return Ok(false);
  }
  if !matches!(
    value["status"].as_str(),
    Some("completed" | "failed" | "cancelled")
  ) || value["task_process_group_clean"] != true
    || value["task_launch_uncertain"] == true
  {
    return Err(message(
      "task ended without verified process-group cleanup; stop completion is uncertain",
    ));
  }
  Ok(true)
}

fn supervisor_action(run_dir: &Path, command: &ControlCommand) -> Result<Option<String>> {
  let value = state(run_dir)?;
  let saved = saved_event(&value, command);
  if let Some(done) = &saved {
    if done.status != "applied" {
      return Err(message(
        done
          .error
          .as_deref()
          .unwrap_or("supervisor rejected lifecycle command"),
      ));
    }
    if command.action != ControlAction::Stop {
      return Ok(None);
    }
  }
  fence(&value, command)?;
  if command.action == ControlAction::Stop && stop_complete(&value)? {
    return Ok(None);
  }
  supported(run_dir, &value)?;
  if saved.is_none() {
    write_json(&run_dir.join(MAILBOX), &serde_json::to_value(command)?)?;
  }
  let deadline = Instant::now() + Duration::from_secs(u64::from(command.grace_seconds) + 30);
  loop {
    let value = state(run_dir)?;
    if let Some(done) = saved_event(&value, command) {
      if done.status == "failed" {
        return Err(message(
          done
            .error
            .as_deref()
            .unwrap_or("supervisor rejected lifecycle command"),
        ));
      }
      if command.action != ControlAction::Stop {
        return Ok(None);
      }
    }
    if command.action == ControlAction::Stop && stop_complete(&value)? {
      return Ok(None);
    }
    if Instant::now() >= deadline {
      return Err(message(
        "supervisor lifecycle acknowledgment timed out; inspect the run before retrying",
      ));
    }
    if !matches!(
      expri_core::lock::try_lock_file_readonly(&run_dir.join(".run.lock"))?,
      LockAttempt::Busy
    ) {
      return Err(message(
        "supervisor ownership was lost before lifecycle acknowledgment",
      ));
    }
    thread::sleep(Duration::from_millis(50));
  }
}

fn recorded_request(run_dir: &Path, command: &ControlCommand) -> Result<RunRequest> {
  let value = state(run_dir)?;
  let active = read_json(&run_dir.join(".task-continuation.json"), 2 * 1024 * 1024)
    .ok()
    .filter(|intent| {
      intent["attempt"].as_u64() == Some(attempt(&value)) && intent["stage"] == "running"
    })
    .map(|intent| intent["request"]["run"].clone());
  let mut request: RunRequest = serde_json::from_value(match active {
    Some(request) => request,
    None => read_json(&run_dir.join("run-request.json"), 1024 * 1024)?,
  })?;
  let recorded = value["command"]
    .as_array()
    .ok_or_else(|| message("recorded command is unavailable"))?
    .iter()
    .map(|item| {
      item
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| message("recorded command is invalid"))
    })
    .collect::<Result<Vec<_>>>()?;
  request.command = recorded;
  if let Some(arguments) = &command.arguments {
    request.command.truncate(1);
    request.command.extend(arguments.clone());
  }
  if request.command.is_empty()
    || request.command.len() > 257
    || request
      .command
      .iter()
      .skip(1)
      .map(String::len)
      .sum::<usize>()
      > 32 * 1024
    || request
      .command
      .iter()
      .any(|argument| argument.len() > 8192 || argument.contains('\0'))
  {
    return Err(message("recorded task arguments are invalid"));
  }
  request.expected_sync = None;
  if command.action == ControlAction::Restart {
    // A fresh run must not replay the source run's assignment request.
    request.experiment = command
      .experiment
      .clone()
      .map(|selection| RunExperimentIntent {
        request_id: command.request_id.clone(),
        selection,
      });
  }
  request.requirements = crate::resource_guard::requirements(&value)?;
  request.name = value["task"].as_str().unwrap_or(&request.name).to_owned();
  request.detach = true;
  crate::environment::task_argv(&request.command)?;
  Ok(request)
}

fn continue_recorded(run_dir: &Path, command: &ControlCommand) -> Result<Option<String>> {
  let current = state(run_dir)?;
  let already_started = current["task_attempts"]["attempts"]
    .as_array()
    .is_some_and(|entries| {
      entries
        .iter()
        .any(|entry| entry["request_id"] == command.request_id)
    });
  if !already_started {
    fence(&current, command)?;
    if chrono::Utc::now().timestamp_millis() >= command.expires_at_ms {
      return Err(message(
        "lifecycle request expired before continuation preparation",
      ));
    }
    if !registered_checkpoints(run_dir)?
      .iter()
      .any(|path| Some(path) == command.checkpoint.as_ref())
    {
      return Err(message(
        "continue requires a locally available registered checkpoint",
      ));
    }
    expri_client::registrations::verify_registered(
      run_dir,
      command.checkpoint.as_deref().unwrap(),
    )?;
  }
  let request = ContinueRunRequest {
    request_id: command.request_id.clone(),
    reason: command.reason.clone(),
    expected_attempt: Some(command.expected_attempt),
    checkpoint: command
      .checkpoint
      .clone()
      .ok_or_else(|| message("continue requires a recorded checkpoint path"))?,
    run: recorded_request(run_dir, command)?,
  };
  let response = crate::continuation::request_recorded(run_dir, &request, command.grace_seconds)?;
  if response["continuation"]["status"] == "failed" {
    return Err(message(
      response["continuation"]["error"]
        .as_str()
        .unwrap_or("checkpoint continuation failed"),
    ));
  }
  Ok(None)
}

fn restart_recorded(run_dir: &Path, journal: &mut Journal) -> Result<Option<String>> {
  let command = journal.command.clone();
  let request = recorded_request(run_dir, &command)?;
  let value = state(run_dir)?;
  let root = run_dir
    .parent()
    .and_then(Path::parent)
    .and_then(Path::parent)
    .ok_or_else(|| message("invalid run repository path"))?;
  if journal.new_run_dir.is_none() {
    fence(&value, &command)?;
    if chrono::Utc::now().timestamp_millis() >= command.expires_at_ms {
      return Err(message(
        "lifecycle request expired before restart preparation",
      ));
    }
    let new_dir = run_dir
      .parent()
      .unwrap()
      .join(format!("run-{}", uuid::Uuid::new_v4().simple()));
    journal.new_run_dir = Some(new_dir);
    save(run_dir, journal)?;
  }
  let new_dir = journal.new_run_dir.clone().unwrap();
  let new_id = new_dir.file_name().unwrap().to_str().unwrap().to_owned();
  journal.result.new_run_id = Some(new_id.clone());
  save(run_dir, journal)?;
  if journal.stage == "accepted" {
    crate::node::run::validate_experiment_request(&request)?;
    let (source, manifest) = super::recorded_source(run_dir, &value)?;
    // Retrying preparation may remove only this request's still-unpublished clone.
    if new_dir.exists() && !new_dir.join("run-state.json").exists() {
      fs::remove_dir_all(&new_dir)?;
    }
    if !new_dir.join("run-state.json").exists() {
      let snapshot =
        crate::environment::snapshot::copy_recorded(&source, &manifest, &new_dir, &new_id)?;
      fs::create_dir(new_dir.join("outputs"))?;
      crate::node::run::prepare_recorded(
        &request,
        root,
        snapshot,
        value["run_id"].as_str().unwrap(),
      )?;
    }
    let prepared = crate::node::run::load_prepared(&new_dir)?;
    crate::node::run::prepare_assets(
      root,
      &prepared.snapshot,
      request
        .service
        .as_ref()
        .map(|service| service.client_config.as_path()),
      None,
      &mut || Ok(false),
    )?;
    crate::environment::prepare(&crate::environment::EnvironmentRequest {
      environment: request.environment.clone(),
      repo_root: prepared.snapshot.code_dir.to_string_lossy().into_owned(),
      state_dir: new_dir.to_string_lossy().into_owned(),
      operation: "run".into(),
      extras: request.extras.clone(),
      sync_args: request.sync_args.clone(),
      install_project: true,
      cache_dir: Some(
        crate::environment::cache_dir(root, &request.sync_args)?
          .to_string_lossy()
          .into_owned(),
      ),
    })?;
    journal.stage = "prepared".into();
    save(run_dir, journal)?;
  }
  if journal.stage == "prepared" {
    let value = state(run_dir)?;
    fence(&value, &command)?;
    if terminal(&value) {
      stop_complete(&value)?;
    } else {
      let mut stop = command.clone();
      stop.action = ControlAction::Stop;
      stop.experiment = None;
      // Delivery expiry fences unstarted requests. This transition has already
      // been accepted and may have spent minutes preparing its environment.
      stop.expires_at_ms = chrono::Utc::now().timestamp_millis().saturating_add(60_000);
      supervisor_action(run_dir, &stop)?;
    }
    journal.stage = "stopped".into();
    save(run_dir, journal)?;
  }
  if journal.stage == "stopped" {
    let original = state(run_dir)?;
    fence(&original, &command)?;
    if !stop_complete(&original)? {
      return Err(message(
        "original task is no longer stopped; restart successor was not launched",
      ));
    }
    let prepared = crate::node::run::load_prepared(&new_dir)?;
    // Persist launch intent before spawning. An ambiguous crash is never relaunched.
    journal.stage = "launch_intent".into();
    save(run_dir, journal)?;
    crate::node::run::start_prepared(&request, prepared)?;
    journal.stage = "launched".into();
    save(run_dir, journal)?;
  } else if journal.stage == "launch_intent" {
    let value = state(&new_dir)?;
    if value["status"] == "preparing" && !new_dir.join(".worker-ready").exists() {
      return Err(message(
        "restart launch outcome is uncertain; successor was not relaunched",
      ));
    }
  }
  Ok(Some(new_id))
}

/// Called only by the supervisor that still owns an unreaped direct child.
pub(crate) fn process(run_dir: &Path, pid: u32) -> Result<Option<u32>> {
  process_owned(run_dir, Some(pid))
}

pub(crate) fn process_pending(run_dir: &Path) -> Result<Option<u32>> {
  process_owned(run_dir, None)
}

fn process_owned(run_dir: &Path, pid: Option<u32>) -> Result<Option<u32>> {
  let command: ControlCommand = serde_json::from_value(read_json(&run_dir.join(MAILBOX), LIMIT)?)?;
  let value = state(run_dir)?;
  if saved_event(&value, &command).is_some() {
    return Ok(None);
  }
  let action = (|| {
    fence(&value, &command)?;
    if chrono::Utc::now().timestamp_millis() >= command.expires_at_ms {
      return Err(message(
        "lifecycle request expired before supervisor execution",
      ));
    }
    if value["queue_cleanup"] == true {
      return Err(message("queued lifecycle control is unsupported"));
    }
    let status = value["status"].as_str().unwrap_or_default();
    match command.action {
      ControlAction::SetRequirements => {
        if value["run_requirements_protocol_version"] != 1 {
          return Err(message(
            "this supervisor does not enforce resource requirements; upgrade it before changing the policy",
          ));
        }
        let policy = command
          .requirements
          .as_ref()
          .ok_or_else(|| message("resource requirements are missing"))?;
        policy.validate()?;
        update_state(&run_dir.join("run-state.json"), |current| {
          fence(current, &command)?;
          current["requirements"] = serde_json::to_value(policy)?;
          current.as_object_mut().unwrap().remove("resource_status");
          Ok(())
        })?;
        Ok(None)
      }
      ControlAction::Pause if matches!(status, "running" | "waiting" | "paused") => {
        let pid = pid.ok_or_else(|| message("task has not started"))?;
        update_state(&run_dir.join("run-state.json"), |current| {
          // Acquire the state lease first: freezing a task which currently owns
          // it would strand that lease and prevent the pause acknowledgement.
          fence(current, &command)?;
          groups::signal(pid, libc::SIGSTOP)?;
          if current["status"] != "paused" {
            current["paused_previous_status"] = current["status"].clone();
          }
          current["manual_paused"] = json!(true);
          current["status"] = json!("paused");
          set_attempt_phase(current, "paused");
          Ok(())
        })?;
        Ok(None)
      }
      ControlAction::Resume if matches!(status, "paused" | "running" | "waiting") => {
        let pid = pid.ok_or_else(|| message("task has not started"))?;
        update_state(&run_dir.join("run-state.json"), |current| {
          fence(current, &command)?;
          current["manual_paused"] = json!(false);
          if current["resource_paused"] != true {
            groups::signal(pid, libc::SIGCONT)?;
            restore_phase(current);
          }
          Ok(())
        })?;
        Ok(None)
      }
      ControlAction::Stop
        if matches!(
          status,
          "preparing" | "pending" | "running" | "waiting" | "paused"
        ) =>
      {
        update_state(&run_dir.join("run-state.json"), |current| {
          fence(current, &command)?;
          // Deliver TERM before CONT so a paused task resumes into its handler,
          // after the shared state lease makes its former phase visible again.
          if let Some(pid) = pid {
            groups::signal(pid, libc::SIGTERM)?;
            groups::signal(pid, libc::SIGCONT)?;
          } else {
            fs::OpenOptions::new()
              .write(true)
              .create_new(true)
              .open(run_dir.join(".cancel-request"))?;
          }
          restore_phase(current);
          Ok(())
        })?;
        Ok(Some(command.grace_seconds))
      }
      _ => Err(message(
        "lifecycle action is unavailable in the task's current state",
      )),
    }
  })();
  let done = match &action {
    Ok(_) => result(&command, "applied", None, None),
    Err(error) => result(&command, "failed", Some(error.to_string()), None),
  };
  update_state(&run_dir.join("run-state.json"), |current| {
    if !current["control_events"].is_array() {
      current["control_events"] = json!([]);
    }
    let events = current["control_events"].as_array_mut().unwrap();
    events.push(json!({"request_id":command.request_id,"action":command.action,"at":chrono::Utc::now().to_rfc3339(),"result":done}));
    if events.len() > 16 {
      events.drain(..events.len() - 16);
    }
    Ok(())
  })?;
  // A stop acceptance is visible here; its caller waits for terminal state.
  match action {
    Ok(grace) => Ok(grace),
    Err(_) => Ok(None),
  }
}

pub(crate) fn set_attempt_phase(value: &mut Value, phase: &str) {
  let number = attempt(value);
  if let Some(entry) = value["task_attempts"]["attempts"]
    .as_array_mut()
    .and_then(|entries| entries.iter_mut().find(|entry| entry["attempt"] == number))
  {
    entry["status"] = json!(phase);
  }
}

pub(crate) fn restore_phase(value: &mut Value) {
  if value["status"] == "paused" {
    let phase = if value["paused_previous_status"] == "waiting" {
      "waiting"
    } else {
      "running"
    };
    value["status"] = json!(phase);
    set_attempt_phase(value, phase);
  }
}

#[cfg(test)]
mod tests;
