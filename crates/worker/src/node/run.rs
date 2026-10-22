use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use expri_core::storage::StoragePaths;
use expri_system::store::LocalStore;

use crate::environment::snapshot::RunSnapshot;
use crate::environment::{self, EnvironmentRequest};
use crate::error::{ExpriError, Result, command_exit_code};
use crate::protocol::RunRequest;
use crate::run_logs::{RunLogs, STDERR_LOG, STDOUT_LOG};

pub fn apply_request_file(path: &Path) -> Result<()> {
  let request: RunRequest = serde_json::from_slice(&fs::read(path)?)?;
  apply_request(&request)
}

pub fn apply_request(request: &RunRequest) -> Result<()> {
  apply_request_at(request, &std::env::current_dir()?)
}

pub fn apply_request_at(request: &RunRequest, repo_root: &Path) -> Result<()> {
  if let Some(receipt) = execute_request_at(request, repo_root)? {
    println!("{receipt}");
  }
  Ok(())
}

/// Execute a run while leaving a detached receipt's presentation to the caller.
pub fn execute_request_at(
  request: &RunRequest,
  repo_root: &Path,
) -> Result<Option<serde_json::Value>> {
  execute_request_at_with_initializer(request, repo_root, || Ok(()))
}

/// The CLI supplies worker-service startup after validation and before capture
/// metadata is created. Library callers can retain control over their runtime.
pub fn execute_request_at_with_initializer(
  request: &RunRequest,
  repo_root: &Path,
  initialize: impl FnOnce() -> Result<()>,
) -> Result<Option<serde_json::Value>> {
  start_prepared(
    request,
    prepare_request_at_with_initializer(request, repo_root, initialize)?,
  )
}

pub struct PreparedRun {
  pub snapshot: RunSnapshot,
  repo_root: std::path::PathBuf,
  state: serde_json::Value,
}

/// Queue supervisors must verify their preparation/task groups stopped before
/// their terminal record can release a server-side resource reservation.
pub fn enable_queue_cleanup(prepared: &mut PreparedRun) -> Result<()> {
  if !prepared.state["requirements"]["disk"]
    .as_array()
    .is_none_or(Vec::is_empty)
  {
    return Err(ExpriError::Message(
      "queued runs do not support local resource requirements yet".into(),
    ));
  }
  prepared.state["queue_cleanup"] = json!(true);
  write_state(
    &prepared.snapshot.run_dir.join("run-state.json"),
    &prepared.state,
  )
}

/// Allocate the immutable run and metadata without launching any subprocess.
/// Queue callers journal `snapshot.run_dir` before calling `start_prepared`.
pub fn prepare_request_at(request: &RunRequest, repo_root: &Path) -> Result<PreparedRun> {
  prepare_request_at_with_initializer(request, repo_root, || Ok(()))
}

/// Prepare a run with worker-service initialization before capture metadata.
pub fn prepare_request_at_with_initializer(
  request: &RunRequest,
  repo_root: &Path,
  initialize: impl FnOnce() -> Result<()>,
) -> Result<PreparedRun> {
  let repo_root = fs::canonicalize(repo_root)?;
  request.environment.validate()?;
  request.requirements.validate()?;
  if !request.requirements.is_empty() && (!cfg!(target_os = "linux") || !request.detach) {
    return Err(ExpriError::Message(
      "resource requirements require a native Linux detached direct run".into(),
    ));
  }
  if let Some(service) = &request.service {
    service.validate()?;
  }
  validate_experiment_request(request)?;
  environment::task_argv(&request.command)?;
  let snapshot = environment::snapshot::create_expected_with_smoke(
    &repo_root,
    &request.remote_managed,
    request.expected_sync.as_ref(),
    request.smoke,
  )?;
  prepare_snapshot(request, repo_root, snapshot, initialize)
}

pub(crate) fn validate_experiment_request(request: &RunRequest) -> Result<()> {
  if let Some(intent) = &request.experiment {
    intent.validate()?;
    let service = request
      .service
      .as_ref()
      .filter(|service| service.publish)
      .ok_or_else(|| {
        ExpriError::Message("experiment assignment requires automatic service publishing".into())
      })?;
    crate::service::experiment_assignment::preflight(
      &service.client_config,
      &service.project_id,
      intent,
    )?;
    crate::service::worker_service::ensure_experiment_assignment_ready(&StoragePaths::resolve(
      None,
    )?)?;
  }
  Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn prepare_recorded(
  request: &RunRequest,
  repo_root: &Path,
  snapshot: RunSnapshot,
  parent_run_id: &str,
) -> Result<PreparedRun> {
  let mut prepared = prepare_snapshot(request, repo_root.to_owned(), snapshot, || Ok(()))?;
  prepared.state["restarted_from"] = json!(parent_run_id);
  write_state(
    &prepared.snapshot.run_dir.join("run-state.json"),
    &prepared.state,
  )?;
  Ok(prepared)
}

#[cfg(target_os = "linux")]
pub(crate) fn load_prepared(run_dir: &Path) -> Result<PreparedRun> {
  let state = read_record(&run_dir.join("run-state.json"), 256 * 1024)?;
  if state["status"] != "preparing" || state["detached"] != true {
    return Err(ExpriError::Message(
      "restart successor is no longer awaiting launch".into(),
    ));
  }
  let repo_root = run_dir
    .parent()
    .and_then(Path::parent)
    .and_then(Path::parent)
    .ok_or_else(|| ExpriError::Message("invalid restart run directory".into()))?
    .to_owned();
  Ok(PreparedRun {
    snapshot: RunSnapshot {
      run_id: state["run_id"]
        .as_str()
        .ok_or_else(|| ExpriError::Message("missing restart run identity".into()))?
        .into(),
      code_dir: run_dir.join("code"),
      run_dir: run_dir.into(),
      assets: crate::assets::discover(&run_dir.join("code"))?,
    },
    repo_root,
    state,
  })
}

fn prepare_snapshot(
  request: &RunRequest,
  repo_root: std::path::PathBuf,
  snapshot: RunSnapshot,
  initialize: impl FnOnce() -> Result<()>,
) -> Result<PreparedRun> {
  let service_unavailable = initialize().is_err();
  if service_unavailable {
    let _ = writeln!(
      std::io::stderr(),
      "warning: worker service could not become ready; training will continue locally. Inspect expri service status for details."
    );
  }
  let state_path = snapshot.run_dir.join("run-state.json");
  let mut state = json!({
    "schema_version": 1,
    "run_id": snapshot.run_id,
    "task": request.name,
    "experiment_intent": request.experiment,
    "command": request.command,
    "code_dir": snapshot.code_dir,
    "output_dir": snapshot.run_dir.join("outputs"),
    "status": "preparing",
    "detached": request.detach,
    "requirements": request.requirements,
    "started_at": chrono::Utc::now().to_rfc3339(),
    "logs": {"stdout": STDOUT_LOG, "stderr": STDERR_LOG},
    "assets": snapshot.assets.iter().map(|asset| json!({
      "path": asset.path,
      "source": asset.descriptor.source,
      "size": asset.descriptor.size,
      "sha256": asset.descriptor.sha256,
      "availability": asset.descriptor.availability,
      "producer": asset.descriptor.producer,
    })).collect::<Vec<_>>(),
  });
  if request.smoke {
    state["smoke"] = json!(true);
  }
  #[cfg(target_os = "linux")]
  if request.detach {
    // Before task launch there is no task group to recover. The detached pump
    // clears this as soon as it owns a spawned child.
    state["task_process_group_clean"] = json!(true);
  }
  if service_unavailable {
    state["worker_service_error"] =
      json!("Worker service could not become ready; local training continues");
  }
  if let Some(machine_id) = existing_machine_id() {
    state["machine_id"] = json!(machine_id);
  }
  if service_unavailable {
    // An older observation can identify the machine, but it cannot certify
    // collection for this run when the collector is paused or unavailable.
    state["system_capture_error"] = json!("System collection was unavailable when the run started");
  } else {
    expri_system::store::run_capture::begin_state(&snapshot.run_dir, &mut state);
  }
  write_state(&state_path, &state)?;
  let _ = writeln!(std::io::stderr(), "run: {}", snapshot.run_id);
  let _ = writeln!(
    std::io::stderr(),
    "run directory: {}",
    snapshot.run_dir.display()
  );
  if let Some(url) = request
    .service
    .as_ref()
    .and_then(|service| crate::service::publishing::dashboard_url(service, &snapshot.run_id))
  {
    let _ = writeln!(
      std::io::stderr(),
      "dashboard: {url} (available after publishing)"
    );
  }
  if request.detach {
    fs::write(
      snapshot.run_dir.join("run-request.json"),
      serde_json::to_vec(request)?,
    )?;
  }
  Ok(PreparedRun {
    snapshot,
    repo_root,
    state,
  })
}

pub fn start_prepared(
  request: &RunRequest,
  mut prepared: PreparedRun,
) -> Result<Option<serde_json::Value>> {
  if request.detach {
    return start_detached(
      request,
      &prepared.repo_root,
      &prepared.snapshot,
      &mut prepared.state,
    )
    .map(Some);
  }
  let run_lock = crate::lock::run_lock(&prepared.snapshot.run_dir)?;
  execute_snapshot(
    request,
    &prepared.repo_root,
    &prepared.snapshot,
    prepared.state,
    false,
    &run_lock,
  )?;
  Ok(None)
}

/// Publish a definitive prelaunch cancellation/failure without spawning a task.
pub fn abort_prepared(mut prepared: PreparedRun, cancelled: bool, reason: &str) -> Result<()> {
  let _lease = crate::lock::run_lock(&prepared.snapshot.run_dir)?;
  prepared.state["status"] = json!(if cancelled { "cancelled" } else { "failed" });
  prepared.state["exit_code"] = json!(if cancelled { 130 } else { 1 });
  prepared.state["error"] = json!(reason);
  prepared.state["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
  expri_system::store::run_capture::close_state(&prepared.snapshot.run_dir, &mut prepared.state);
  write_state(
    &prepared.snapshot.run_dir.join("run-state.json"),
    &prepared.state,
  )
}

/// A profile belongs to this machine only when an existing local store says so.
/// Monitoring is optional: lookup failures must not create storage or stop a run.
fn existing_machine_id() -> Option<String> {
  existing_machine_id_at(&StoragePaths::resolve(None).ok()?)
}

fn existing_machine_id_at(paths: &StoragePaths) -> Option<String> {
  paths.validate_existing().ok()?;
  let store = LocalStore::open_readonly(&paths.profile_database()).ok()??;
  Some(store.machine_id().to_owned())
}

fn execute_snapshot(
  request: &RunRequest,
  repo_root: &Path,
  snapshot: &RunSnapshot,
  mut state: serde_json::Value,
  worker: bool,
  lease: &crate::lock::FileLock,
) -> Result<()> {
  #[cfg(not(target_os = "linux"))]
  let _ = lease;
  let state_path = snapshot.run_dir.join("run-state.json");
  if let Some(service) = &request.service
    && service.publish
    && crate::service::publishing::start(repo_root, &snapshot.run_dir, service).is_err()
  {
    let _ = writeln!(
      std::io::stderr(),
      "warning: service publishing could not start; the run will continue. Inspect runs status for publishing details."
    );
  }
  let mut cancelled = false;
  let mut cleanup_uncertain = false;
  let mut logs_created = None;
  let result = (|| {
    let logs = logs_created.insert(if worker && state["queue_cleanup"] == true {
      RunLogs::create_queued_detached(&snapshot.run_dir)?
    } else if worker {
      RunLogs::create_detached(&snapshot.run_dir)?
    } else {
      RunLogs::create(&snapshot.run_dir)?
    });
    if worker {
      // The launcher acknowledges only once the lease and logs are established.
      write_state(&state_path, &state)?;
      fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(snapshot.run_dir.join(".worker-ready"))?;
    }
    if logs.cancel_requested()? {
      cancelled = true;
      return Err(ExpriError::Message(
        "run cancellation requested".to_string(),
      ));
    }
    prepare_assets(
      repo_root,
      snapshot,
      request
        .service
        .as_ref()
        .map(|service| service.client_config.as_path()),
      if state["queue_cleanup"] == true {
        request
          .service
          .as_ref()
          .map(|service| service.project_id.as_str())
      } else {
        None
      },
      &mut || {
        let requested = logs.cancel_requested()?;
        cancelled |= requested;
        Ok(requested)
      },
    )?;
    if logs.cancel_requested()? {
      cancelled = true;
      return Err(ExpriError::Message("run cancellation requested".into()));
    }
    let prepared = environment::prepare_logged(
      &EnvironmentRequest {
        environment: request.environment.clone(),
        repo_root: snapshot.code_dir.to_string_lossy().into_owned(),
        state_dir: snapshot.run_dir.to_string_lossy().into_owned(),
        operation: "run".to_string(),
        extras: request.extras.clone(),
        sync_args: request.sync_args.clone(),
        install_project: true,
        cache_dir: Some(
          environment::cache_dir(repo_root, &request.sync_args)?
            .to_string_lossy()
            .into_owned(),
        ),
      },
      logs,
    )?;
    if logs.cancel_requested()? {
      cancelled = true;
      return Err(ExpriError::Message(
        "run cancellation requested".to_string(),
      ));
    }
    let argv = environment::task_argv(&request.command)?;
    expri_core::run_state::set_phase(&mut state, expri_core::run_state::RunPhase::Running)?;
    state["environment_manifest"] = json!(prepared.manifest_path);
    state["python"] = json!(prepared.python);
    write_state(&state_path, &state)?;
    let mut command = Command::new(&argv[0]);
    environment::configure_command(&mut command, &request.environment, &prepared);
    command
      .args(&argv[1..])
      .current_dir(&snapshot.code_dir)
      .env("EXPRI_BIN", std::env::current_exe()?)
      .env("EXPRI_RUN_ID", &snapshot.run_id)
      .env("EXPRI_RUN_DIR", &snapshot.run_dir)
      .env("EXPRI_OUTPUT_DIR", snapshot.run_dir.join("outputs"));
    for field in [
      "EXPRI_INPUT_DIR",
      "EXPRI_ATTEMPT",
      "EXPRI_STEP_OFFSET",
      "EXPRI_RESUME_CHECKPOINT",
    ] {
      command.env_remove(field);
    }
    let on_started = || {
      expri_core::run_state::update_state(&state_path, |current| {
        current["task_started_at"] = json!(chrono::Utc::now().to_rfc3339());
        Ok(())
      })
      .map_err(std::io::Error::other)
    };
    #[cfg(target_os = "linux")]
    let output = if worker {
      let (stdout, stderr) = logs.supervisor_files()?;
      let output = crate::supervisor::task(
        &mut command,
        &snapshot.run_dir,
        lease,
        stdout,
        stderr,
        on_started,
      )?;
      cancelled |= output.cancelled;
      cleanup_uncertain |= output.cleanup_uncertain;
      output.output
    } else {
      logs.task_with_started(&mut command, on_started)?
    };
    #[cfg(not(target_os = "linux"))]
    let output = logs.task_with_started(&mut command, on_started)?;
    let program = read_record(&state_path, expri_core::run_state::MAX_STATE_BYTES as u64)
      .ok()
      .and_then(|current| {
        current["command"]
          .as_array()?
          .first()?
          .as_str()
          .map(str::to_owned)
      })
      .unwrap_or_else(|| argv[0].clone());
    task_result(output, &mut state, &program)
  })();
  cancelled |= logs_created.as_ref().is_some_and(RunLogs::was_cancelled);
  cleanup_uncertain |= logs_created
    .as_ref()
    .is_some_and(RunLogs::cleanup_uncertain);
  finalize_snapshot(
    snapshot,
    state,
    worker,
    result,
    cancelled,
    cleanup_uncertain,
  )
}

fn task_result(
  output: crate::run_logs::LoggedOutput,
  state: &mut serde_json::Value,
  program: &str,
) -> Result<()> {
  state["task_exit_code"] = json!(command_exit_code(&output.status));
  if let Some(error) = &output.log_error {
    state["logging_error"] = json!(error.to_string());
  }
  if !output.status.success() {
    return Err(ExpriError::CommandFailed {
      program: program.into(),
      code: command_exit_code(&output.status),
    });
  }
  if let Some(error) = output.log_error {
    return Err(error.into());
  }
  Ok(())
}

fn finalize_snapshot(
  snapshot: &RunSnapshot,
  mut state: serde_json::Value,
  worker: bool,
  mut result: Result<()>,
  cancelled: bool,
  cleanup_uncertain: bool,
) -> Result<()> {
  let state_path = snapshot.run_dir.join("run-state.json");
  // A continuation can replace the task and capture while the original
  // execution frame is still alive. Only carry its final output into the latest
  // authoritative metadata; its old snapshot must not restore prior attempts.
  let task_exit_code = state.get("task_exit_code").cloned();
  let logging_error = state.get("logging_error").cloned();
  state = read_record(&state_path, 256 * 1024)?;
  if let Some(exit_code) = task_exit_code {
    state["task_exit_code"] = exit_code;
  }
  if let Some(error) = logging_error {
    state["logging_error"] = error;
  }
  // Restart events are updated independently of this execution snapshot.
  state
    .as_object_mut()
    .expect("run state object")
    .remove("supervisor");
  if cleanup_uncertain {
    state["queue_cleanup_uncertain"] = json!(true);
    state["task_process_group_clean"] = json!(false);
  }
  if worker {
    match regular_cancel_request(&snapshot.run_dir) {
      // A request racing terminal publication is allowed to lose to completion.
      // Claim cancellation only if a prelaunch guard or the supervisor consumed it.
      Ok(_) => {}
      Err(error) if result.is_ok() => result = Err(error),
      Err(_) => {}
    }
  }
  state["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
  state["status"] = json!(if cancelled {
    "cancelled"
  } else if result.is_ok() {
    "completed"
  } else {
    "failed"
  });
  expri_system::store::run_capture::close_state(&snapshot.run_dir, &mut state);
  if cancelled {
    state["error"] = json!("run cancellation requested");
    state["exit_code"] = json!(130);
  } else if let Err(error) = &result {
    state["error"] = json!(error.to_string());
    state["exit_code"] = json!(error.exit_code());
  } else {
    state["exit_code"] = json!(0);
  }
  expri_core::run_state::update_state(&state_path, |current| {
    for field in [
      "status",
      "finished_at",
      "exit_code",
      "error",
      "task_exit_code",
      "logging_error",
      "queue_cleanup_uncertain",
      "task_process_group_clean",
      "system_capture_status",
      "system_capture_error",
    ] {
      if let Some(value) = state.get(field) {
        current[field] = value.clone();
      }
    }
    current["system_capture_finished_at"] = state["finished_at"].clone();
    if current.get("task_attempts").is_some() {
      let attempt = expri_core::task_attempts::current_attempt(current)?;
      let active = current["task_attempts"]["attempts"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["attempt"] == attempt))
        .is_some_and(|entry| {
          matches!(
            entry["status"].as_str(),
            Some("preparing" | "pending" | "running" | "waiting" | "paused")
          )
        });
      if active {
        expri_core::task_attempts::finish_attempt(
          current,
          attempt,
          state["status"].as_str().unwrap(),
          state["finished_at"].as_str().unwrap(),
          state["exit_code"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok()),
          state["error"].as_str(),
        )?;
        let fields: Vec<_> = [
          "system_capture_id",
          "machine_id",
          "system_capture_started_at",
          "system_capture_finished_at",
          "system_capture_status",
        ]
        .into_iter()
        .filter_map(|field| current.get(field).map(|value| (field, value.clone())))
        .collect();
        if let Some(entry) = current["task_attempts"]["attempts"]
          .as_array_mut()
          .and_then(|entries| entries.iter_mut().find(|entry| entry["attempt"] == attempt))
        {
          for (field, value) in fields {
            entry[field] = value;
          }
          entry["system_capture_finished_at"] = state["finished_at"].clone();
        }
      }
    }
    Ok(())
  })?;
  if let Err(error) = crate::continuation::sync_attempt_records(&snapshot.run_dir) {
    let _ = writeln!(
      std::io::stderr(),
      "warning: attempt metadata document could not be updated: {error}"
    );
  }
  if cancelled {
    return Err(ExpriError::CommandFailed {
      program: "cancelled run".to_string(),
      code: Some(130),
    });
  }
  result
}

/// Complete the existing task after an exec handoff. This path deliberately
/// skips source/environment preparation, task launch, and capture creation.
#[cfg(target_os = "linux")]
pub(crate) fn finish_resumed(run_dir: &Path, output: crate::supervisor::TaskOutput) -> Result<()> {
  let mut state = read_record(&run_dir.join("run-state.json"), 256 * 1024)?;
  let command = state["command"].clone();
  let program = command
    .as_array()
    .and_then(|args| args.first())
    .and_then(|arg| arg.as_str())
    .unwrap_or("task");
  let result = task_result(output.output, &mut state, program);
  let snapshot = RunSnapshot {
    run_id: state["run_id"]
      .as_str()
      .ok_or_else(|| ExpriError::Message("missing run identity".into()))?
      .into(),
    code_dir: run_dir.join("code"),
    run_dir: run_dir.into(),
    assets: Vec::new(),
  };
  finalize_snapshot(
    &snapshot,
    state,
    true,
    result,
    output.cancelled,
    output.cleanup_uncertain,
  )
}

pub(crate) fn prepare_assets(
  repo_root: &Path,
  snapshot: &RunSnapshot,
  client_config: Option<&Path>,
  project_id: Option<&str>,
  cancel: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  if snapshot.assets.is_empty() {
    return Ok(());
  }
  let ignored_config = repo_root.join(".expri/config.toml");
  let client_config = if project_id.is_none() && crate::assets::inspect(&ignored_config)?.is_some()
  {
    crate::assets::optional_regular(&ignored_config)?;
    Some(ignored_config.as_path())
  } else {
    client_config
  };
  for asset in &snapshot.assets {
    if cancel()? {
      return Err(ExpriError::Message("run cancellation requested".into()));
    }
    let cached = crate::assets::download::ensure_scoped(
      repo_root,
      &asset.descriptor,
      client_config,
      project_id,
      cancel,
    )?;
    crate::assets::bind_verified(
      &cached,
      &snapshot.code_dir.join(&asset.path),
      &asset.descriptor,
      cancel,
    )?;
  }
  Ok(())
}

fn regular_cancel_request(run_dir: &Path) -> Result<bool> {
  match fs::symlink_metadata(run_dir.join(".cancel-request")) {
    Ok(metadata)
      if metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == 0 =>
    {
      Ok(true)
    }
    Ok(_) => Err(ExpriError::Message(
      "cancellation request must be an empty regular file".to_string(),
    )),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(error.into()),
  }
}

fn write_state(path: &Path, state: &serde_json::Value) -> Result<()> {
  let bytes = serde_json::to_vec_pretty(state)?;
  // Jobs, dashboard discovery, and detached launch receipts use this same limit.
  if bytes.len() > 256 * 1024 {
    return Err(ExpriError::Message(
      "run-state.json exceeds the 256 KiB metadata size limit; reduce command or asset source metadata".into(),
    ));
  }
  expri_core::run_state::write_state(path, state)
}

#[cfg(unix)]
fn start_detached(
  request: &RunRequest,
  repo_root: &Path,
  snapshot: &RunSnapshot,
  state: &mut serde_json::Value,
) -> Result<serde_json::Value> {
  use std::os::unix::process::CommandExt;
  let launch = (|| {
    fs::write(
      snapshot.run_dir.join("run-request.json"),
      serde_json::to_vec(request)?,
    )?;
    let mut command = Command::new(std::env::current_exe()?);
    command
      .args(["node", "run-worker", "--run-dir"])
      .arg(&snapshot.run_dir)
      .current_dir(repo_root)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null());
    // setsid is async-signal-safe. Re-exec also closes Rust's CLOEXEC file handles.
    unsafe {
      command.pre_exec(|| {
        if libc::setsid() < 0 {
          return Err(std::io::Error::last_os_error());
        }
        Ok(())
      });
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let ready = snapshot.run_dir.join(".worker-ready");
      if fs::symlink_metadata(&ready)
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
      {
        let current = read_record(&snapshot.run_dir.join("run-state.json"), 256 * 1024)?;
        let mut receipt = json!({"run_id": snapshot.run_id, "run_dir": snapshot.run_dir, "status": current["status"], "detached": true});
        if request.smoke {
          receipt["smoke"] = json!(true);
        }
        if let Some(url) = request
          .service
          .as_ref()
          .and_then(|service| crate::service::publishing::dashboard_url(service, &snapshot.run_id))
        {
          receipt["dashboard_url"] = json!(url);
        }
        return Ok(receipt);
      }
      if let Some(status) = child.try_wait()? {
        return Err(ExpriError::CommandFailed {
          program: "detached run supervisor startup".to_string(),
          code: command_exit_code(&status),
        });
      }
      if Instant::now() >= deadline {
        // Ask the worker to stop rather than ever signal a persisted process ID.
        let _ = fs::OpenOptions::new()
          .write(true)
          .create_new(true)
          .open(snapshot.run_dir.join(".cancel-request"));
        return Err(ExpriError::Message(
          "detached supervisor did not become ready within 10 seconds".to_string(),
        ));
      }
      thread::sleep(Duration::from_millis(25));
    }
  })();
  if let Err(error) = &launch {
    // Do not compete with a worker that has already taken ownership of the record.
    if let crate::lock::LockAttempt::Acquired(_lease) =
      crate::lock::try_lock_file(&snapshot.run_dir.join(".run.lock"), true)?
    {
      let current = read_record(&snapshot.run_dir.join("run-state.json"), 256 * 1024)?;
      if matches!(
        current["status"].as_str(),
        Some("completed" | "failed" | "cancelled")
      ) {
        // The worker can fail before readiness, for example while creating its
        // log files. Preserve its specific terminal diagnostic in that case.
        return launch;
      }
      state["status"] = json!("failed");
      state["exit_code"] = json!(error.exit_code());
      state["error"] = json!(error.to_string());
      state["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
      expri_system::store::run_capture::close_state(&snapshot.run_dir, state);
      write_state(&snapshot.run_dir.join("run-state.json"), state)?;
    }
  }
  launch
}

#[cfg(not(unix))]
fn start_detached(
  _: &RunRequest,
  _: &Path,
  _: &RunSnapshot,
  _: &mut serde_json::Value,
) -> Result<serde_json::Value> {
  Err(ExpriError::Message(
    "detached runs require a Unix target".to_string(),
  ))
}

pub fn worker(run_dir: &Path) -> Result<()> {
  let run_dir = std::path::absolute(run_dir)?;
  let run_id = run_dir
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| ExpriError::Message("invalid worker run directory".to_string()))?;
  let repo_root = run_dir
    .parent()
    .and_then(Path::parent)
    .and_then(Path::parent)
    .ok_or_else(|| ExpriError::Message("invalid worker run directory".to_string()))?;
  if crate::jobs::run_directory(repo_root, run_id)? != run_dir {
    return Err(ExpriError::Message(
      "worker must use the owned run directory".to_string(),
    ));
  }
  let request: RunRequest =
    serde_json::from_value(read_record(&run_dir.join("run-request.json"), 1024 * 1024)?)?;
  if !request.detach {
    return Err(ExpriError::Message(
      "worker requires a detached run request".to_string(),
    ));
  }
  let crate::lock::LockAttempt::Acquired(lease) =
    crate::lock::try_lock_file(&run_dir.join(".run.lock"), true)?
  else {
    return Err(ExpriError::Message(
      "run already has an active supervisor".to_string(),
    ));
  };
  let state = read_record(&run_dir.join("run-state.json"), 256 * 1024)?;
  if state["run_id"].as_str() != Some(run_id)
    || state["status"] != "preparing"
    || state["detached"] != true
    || state
      .get("queue_cleanup")
      .is_some_and(|value| !value.is_boolean())
    || state.get("smoke").is_some_and(|value| !value.is_boolean())
    || state["smoke"].as_bool().unwrap_or(false) != request.smoke
  {
    return Err(ExpriError::Message(
      "run is not awaiting a detached supervisor".to_string(),
    ));
  }
  let snapshot = RunSnapshot {
    run_id: run_id.to_string(),
    code_dir: run_dir.join("code"),
    run_dir: run_dir.clone(),
    assets: crate::assets::discover(&run_dir.join("code"))?,
  };
  execute_snapshot(&request, repo_root, &snapshot, state, true, &lease)
}

fn read_record(path: &Path, limit: u64) -> Result<serde_json::Value> {
  let meta = fs::symlink_metadata(path)?;
  if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > limit {
    return Err(ExpriError::Message(format!(
      "invalid worker record: {}",
      path.display()
    )));
  }
  let mut bytes = Vec::new();
  let mut options = fs::OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options.open(path)?;
  if !file.metadata()?.is_file() {
    return Err(ExpriError::Message(
      "worker record must be a regular file".to_string(),
    ));
  }
  file.take(limit + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > limit {
    return Err(ExpriError::Message(
      "worker record exceeds size limit".to_string(),
    ));
  }
  Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::assets::{Descriptor, Source};

  fn asset_repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    assert!(
      Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root.path())
        .status()
        .unwrap()
        .success()
    );
    root
  }

  fn cached_asset(root: &Path, bytes: &[u8]) -> Descriptor {
    let root = &root.canonicalize().unwrap();
    let temporary = root.join("hash-source");
    fs::write(&temporary, bytes).unwrap();
    let (sha256, size) = crate::archive::sha256_file(&temporary).unwrap();
    fs::remove_file(temporary).unwrap();
    let descriptor = Descriptor {
      version: 2,
      source: Source::Url {
        url: "https://unavailable.invalid/data.bin".into(),
      },
      size,
      sha256,
      availability: None,
      producer: None,
    };
    let cache = crate::assets::cache_file(root, &descriptor.sha256);
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    fs::write(cache, bytes).unwrap();
    crate::assets::save(&root.join("data/train.bin.expri.toml"), &descriptor).unwrap();
    descriptor
  }

  #[test]
  fn machine_identity_only_reads_existing_profile_storage() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::from_root(directory.path().join("profiles")).unwrap();
    assert_eq!(existing_machine_id_at(&paths), None);
    assert!(!paths.root().exists());

    let store = LocalStore::open(&paths.profile_database()).unwrap();
    let machine_id = store.machine_id().to_owned();
    drop(store);
    let before = fs::read(paths.profile_database()).unwrap();
    assert_eq!(existing_machine_id_at(&paths), Some(machine_id));
    assert_eq!(fs::read(paths.profile_database()).unwrap(), before);
    assert_eq!(fs::read_dir(paths.root()).unwrap().count(), 1);
  }

  #[test]
  fn malformed_profile_storage_does_not_supply_an_identity() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::from_root(directory.path().join("profiles")).unwrap();
    fs::create_dir(paths.root()).unwrap();
    fs::write(paths.profile_database(), b"not a SQLite database").unwrap();
    assert_eq!(existing_machine_id_at(&paths), None);
    assert_eq!(
      fs::read(paths.profile_database()).unwrap(),
      b"not a SQLite database"
    );
    assert_eq!(fs::read_dir(paths.root()).unwrap().count(), 1);
  }

  #[test]
  fn experiment_intent_is_durable_before_both_foreground_and_detached_launches() {
    use expri_core::experiment_assignment::{ExperimentSelection, RunExperimentIntent};
    let root = asset_repository();
    let intent = RunExperimentIntent {
      request_id: "create-study".into(),
      selection: ExperimentSelection::JoinRun {
        run_id: "run-parent".into(),
        origin: Some("other-worker".into()),
      },
    };
    for detach in [false, true] {
      let request = RunRequest {
        name: "train".into(),
        command: vec!["python".into(), "train.py".into()],
        experiment: Some(intent.clone()),
        environment: expri_core::config::EnvironmentConfig {
          base_python: None,
          reuse_packages: vec![],
          require_cuda: false,
          env: Default::default(),
        },
        service: Some(expri_core::config::RunServiceConfig {
          client_config: "/tmp/not-read-by-preparation.toml".into(),
          project_id: "project".into(),
          origin: "worker".into(),
          dashboard_url: None,
          publish: true,
        }),
        detach,
        smoke: false,
        remote_managed: vec![],
        extras: vec![],
        sync_args: vec![],
        expected_sync: None,
        requirements: Default::default(),
      };
      let snapshot =
        environment::snapshot::create_expected_with_smoke(root.path(), &[], None, false).unwrap();
      let prepared = prepare_snapshot(
        &request,
        root.path().canonicalize().unwrap(),
        snapshot,
        || Err(ExpriError::Message("collection unavailable".into())),
      )
      .unwrap();
      let state: serde_json::Value = serde_json::from_slice(
        &fs::read(prepared.snapshot.run_dir.join("run-state.json")).unwrap(),
      )
      .unwrap();
      assert_eq!(state["experiment_intent"], json!(intent));
      assert_eq!(state["status"], "preparing");
      assert_eq!(
        prepared.snapshot.run_dir.join("run-request.json").exists(),
        detach
      );
    }
  }

  #[test]
  fn failed_collection_initialization_preserves_identity_without_certifying_an_empty_capture() {
    const CHILD: &str = "EXPRI_NODE_CAPTURE_TEST_CHILD";
    const MACHINE: &str = "EXPRI_NODE_CAPTURE_TEST_MACHINE";
    if std::env::var_os(CHILD).is_none() {
      // A separate test process pins its profile store without mutating the
      // environment shared by concurrently running Rust tests.
      let directory = tempfile::tempdir().unwrap();
      let paths = StoragePaths::from_root(directory.path().join("profiles")).unwrap();
      let store = LocalStore::open(&paths.profile_database()).unwrap();
      let machine_id = store.machine_id().to_owned();
      drop(store);
      let output = Command::new(std::env::current_exe().unwrap())
        .args([
          "--exact",
          "node::run::tests::failed_collection_initialization_preserves_identity_without_certifying_an_empty_capture",
          "--nocapture",
        ])
        .env(CHILD, "1")
        .env(MACHINE, machine_id)
        .env("EXPRI_STORAGE_DIR", paths.root())
        .output()
        .unwrap();
      assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
      return;
    }

    let root = asset_repository();
    fs::write(root.path().join("train.sh"), "exit 0\n").unwrap();
    let request = RunRequest {
      experiment: None,
      name: "train".into(),
      command: vec!["/bin/sh".into(), "train.sh".into()],
      environment: expri_core::config::EnvironmentConfig {
        base_python: None,
        reuse_packages: vec![],
        require_cuda: false,
        env: Default::default(),
      },
      service: None,
      detach: true,
      smoke: false,
      remote_managed: vec![],
      extras: vec![],
      sync_args: vec![],
      expected_sync: None,
      requirements: Default::default(),
    };
    let prepared = prepare_request_at_with_initializer(&request, root.path(), || {
      Err(ExpriError::Message("private-initializer-secret".into()))
    })
    .unwrap();
    let run = prepared.snapshot.run_dir.clone();
    let state: serde_json::Value =
      serde_json::from_slice(&fs::read(run.join("run-state.json")).unwrap()).unwrap();
    assert_eq!(state["machine_id"], std::env::var(MACHINE).unwrap());
    assert!(state["system_capture_id"].is_null());
    assert!(state["system_capture_status"].is_null());
    assert!(
      state["system_capture_error"]
        .as_str()
        .unwrap()
        .contains("unavailable")
    );
    assert!(!state.to_string().contains("private-initializer-secret"));
    assert!(!run.join("outputs/system-profile.jsonl").exists());
    abort_prepared(prepared, false, "prelaunch failure").unwrap();
    let terminal: serde_json::Value =
      serde_json::from_slice(&fs::read(run.join("run-state.json")).unwrap()).unwrap();
    assert_eq!(terminal["status"], "failed");
    assert!(terminal["system_capture_id"].is_null());
    assert!(terminal["system_capture_status"].is_null());
    assert!(!run.join("outputs/system-profile.jsonl").exists());
  }

  #[cfg(unix)]
  #[test]
  fn profile_identity_rejects_owned_directory_database_and_sidecar_symlinks() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let actual = StoragePaths::from_root(directory.path().join("actual")).unwrap();
    drop(LocalStore::open(&actual.profile_database()).unwrap());
    let alias = StoragePaths::from_root(directory.path().join("alias")).unwrap();
    symlink(actual.root(), alias.root()).unwrap();
    assert_eq!(existing_machine_id_at(&alias), None);

    let paths = StoragePaths::from_root(directory.path().join("profiles")).unwrap();
    fs::create_dir(paths.root()).unwrap();
    symlink(actual.profile_database(), paths.profile_database()).unwrap();
    assert_eq!(existing_machine_id_at(&paths), None);

    fs::remove_file(paths.profile_database()).unwrap();
    drop(LocalStore::open(&paths.profile_database()).unwrap());
    let sidecar = paths.root().join("profiles.sqlite3-wal");
    symlink(actual.profile_database(), sidecar).unwrap();
    assert_eq!(existing_machine_id_at(&paths), None);
  }

  #[test]
  fn preparation_uses_snapshot_descriptor_and_pins_old_cache_bytes() {
    let root = asset_repository();
    let first_descriptor = cached_asset(root.path(), b"first");
    let first = environment::snapshot::create(root.path(), &[]).unwrap();
    let second_descriptor = cached_asset(root.path(), b"second");
    let second = environment::snapshot::create(root.path(), &[]).unwrap();
    let canonical = root.path().canonicalize().unwrap();
    prepare_assets(&canonical, &first, None, None, &mut || Ok(false)).unwrap();
    prepare_assets(&canonical, &second, None, None, &mut || Ok(false)).unwrap();
    assert_eq!(first.assets[0].descriptor, first_descriptor);
    assert_eq!(second.assets[0].descriptor, second_descriptor);
    assert_eq!(
      fs::read(first.code_dir.join("data/train.bin")).unwrap(),
      b"first"
    );
    assert_eq!(
      fs::read(second.code_dir.join("data/train.bin")).unwrap(),
      b"second"
    );
    let cache = crate::assets::cache_file(root.path(), &first_descriptor.sha256);
    let mut replacement = tempfile::NamedTempFile::new_in(cache.parent().unwrap()).unwrap();
    replacement.write_all(b"later").unwrap();
    replacement.persist(&cache).unwrap();
    assert_eq!(
      fs::read(first.code_dir.join("data/train.bin")).unwrap(),
      b"first"
    );
    assert!(
      fs::metadata(first.code_dir.join("data/train.bin"))
        .unwrap()
        .permissions()
        .readonly()
    );
  }

  #[test]
  fn cancellation_prevents_asset_binding_before_environment_preparation() {
    let root = asset_repository();
    cached_asset(root.path(), b"data");
    let snapshot = environment::snapshot::create(root.path(), &[]).unwrap();
    let error = prepare_assets(
      &root.path().canonicalize().unwrap(),
      &snapshot,
      None,
      None,
      &mut || Ok(true),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("cancellation"), "{error}");
    assert!(!snapshot.code_dir.join("data/train.bin").exists());
    assert!(!snapshot.run_dir.join(".venv").exists());
  }

  #[test]
  fn private_assets_prefer_repository_defaults_over_publishing_configuration() {
    let root = asset_repository();
    let canonical = root.path().canonicalize().unwrap();
    let mut descriptor = cached_asset(&canonical, b"private data");
    descriptor.source = Source::Expri {
      input_id: "input".into(),
    };
    crate::assets::save(&canonical.join("data/train.bin.expri.toml"), &descriptor).unwrap();
    fs::remove_file(crate::assets::cache_file(&canonical, &descriptor.sha256)).unwrap();
    let ignored_config = canonical.join(".expri/config.toml");
    fs::write(
      &ignored_config,
      "url='https://example.net'\ntoken_env='MISSING_TOKEN'\n",
    )
    .unwrap();
    let snapshot = environment::snapshot::create(&canonical, &[]).unwrap();
    let error = prepare_assets(
      &canonical,
      &snapshot,
      Some(&canonical.join("absent-publishing-config")),
      None,
      &mut || Ok(false),
    )
    .unwrap_err()
    .to_string();
    // The selected local defaults are diagnosed before credentials or networking.
    assert!(error.contains("project_id"), "{error}");
    assert!(!snapshot.code_dir.join("data/train.bin").exists());
    assert!(!snapshot.run_dir.join(".venv").exists());
    fs::remove_file(ignored_config).unwrap();
    let error = prepare_assets(&canonical, &snapshot, None, None, &mut || Ok(false))
      .unwrap_err()
      .to_string();
    assert!(error.contains("client config"), "{error}");
  }

  #[test]
  fn descriptor_version_mismatch_is_rejected_before_starting_training() {
    let root = asset_repository();
    let canonical = root.path().canonicalize().unwrap();
    cached_asset(&canonical, b"data");
    let sidecar = canonical.join("data/train.bin.expri.toml");
    let text = fs::read_to_string(&sidecar)
      .unwrap()
      .replace("version = 2", "version = 1");
    fs::write(&sidecar, text).unwrap();
    let marker = canonical.join("training-started");
    let request = RunRequest {
      experiment: None,
      name: "test".into(),
      command: vec!["touch".into(), marker.to_string_lossy().into_owned()],
      environment: expri_core::config::EnvironmentConfig {
        base_python: None,
        reuse_packages: vec![],
        require_cuda: false,
        env: Default::default(),
      },
      service: None,
      detach: false,
      smoke: false,
      remote_managed: vec![],
      extras: vec![],
      sync_args: vec![],
      expected_sync: None,
      requirements: Default::default(),
    };
    let initialized = std::cell::Cell::new(false);
    let error = execute_request_at_with_initializer(&request, &canonical, || {
      initialized.set(true);
      Ok(())
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("assets-v2"), "{error}");
    assert!(
      !initialized.get(),
      "invalid snapshots must not start a service"
    );
    assert!(!marker.exists());
    assert_eq!(
      fs::read_dir(canonical.join(".expri/runs")).unwrap().count(),
      0
    );
  }

  #[test]
  fn oversized_provenance_is_rejected_before_publishing_a_run_state() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("run-state.json");
    let state = json!({"assets": [{"source": {"url": "x".repeat(256 * 1024)}}]});
    assert!(
      write_state(&path, &state)
        .unwrap_err()
        .to_string()
        .contains("metadata size limit")
    );
    assert!(!path.exists());
    fs::write(&path, "original").unwrap();
    assert!(write_state(&path, &state).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "original");
  }
}
