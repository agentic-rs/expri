use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

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
  let repo_root = fs::canonicalize(repo_root)?;
  request.environment.validate()?;
  if let Some(service) = &request.service {
    service.validate()?;
  }
  environment::task_argv(&request.command)?;
  let snapshot = environment::snapshot::create_expected(
    &repo_root,
    &request.remote_managed,
    request.expected_sync.as_ref(),
  )?;
  let state_path = snapshot.run_dir.join("run-state.json");
  let mut state = json!({
    "schema_version": 1,
    "run_id": snapshot.run_id,
    "task": request.name,
    "command": request.command,
    "code_dir": snapshot.code_dir,
    "output_dir": snapshot.run_dir.join("outputs"),
    "status": "preparing",
    "detached": request.detach,
    "started_at": chrono::Utc::now().to_rfc3339(),
    "logs": {"stdout": STDOUT_LOG, "stderr": STDERR_LOG},
    "assets": snapshot.assets.iter().map(|asset| json!({
      "path": asset.path,
      "source": asset.descriptor.source,
      "size": asset.descriptor.size,
      "sha256": asset.descriptor.sha256,
    })).collect::<Vec<_>>(),
  });
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
    return start_detached(request, &repo_root, &snapshot, &mut state);
  }
  let _run_lock = crate::lock::run_lock(&snapshot.run_dir)?;
  execute_snapshot(request, &repo_root, &snapshot, state, false)
}

fn execute_snapshot(
  request: &RunRequest,
  repo_root: &Path,
  snapshot: &RunSnapshot,
  mut state: serde_json::Value,
  worker: bool,
) -> Result<()> {
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
  let mut logs_created = None;
  let mut result = (|| {
    let logs = logs_created.insert(if worker {
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
    state["status"] = json!("running");
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
    command.env_remove("EXPRI_INPUT_DIR");
    let output = logs.task(&mut command)?;
    state["task_exit_code"] = json!(command_exit_code(&output.status));
    if let Some(error) = &output.log_error {
      state["logging_error"] = json!(error.to_string());
    }
    if !output.status.success() {
      return Err(ExpriError::CommandFailed {
        program: argv[0].clone(),
        code: command_exit_code(&output.status),
      });
    }
    if let Some(error) = output.log_error {
      return Err(error.into());
    }
    Ok(())
  })();
  cancelled |= logs_created.as_ref().is_some_and(RunLogs::was_cancelled);
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
  if cancelled {
    state["error"] = json!("run cancellation requested");
    state["exit_code"] = json!(130);
  } else if let Err(error) = &result {
    state["error"] = json!(error.to_string());
    state["exit_code"] = json!(error.exit_code());
  } else {
    state["exit_code"] = json!(0);
  }
  write_state(&state_path, &state)?;
  if cancelled {
    return Err(ExpriError::CommandFailed {
      program: "cancelled run".to_string(),
      code: Some(130),
    });
  }
  result
}

fn prepare_assets(
  repo_root: &Path,
  snapshot: &RunSnapshot,
  client_config: Option<&Path>,
  cancel: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  for asset in &snapshot.assets {
    if cancel()? {
      return Err(ExpriError::Message("run cancellation requested".into()));
    }
    let cached =
      crate::assets::download::ensure(repo_root, &asset.descriptor, client_config, cancel)?;
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
  let mut temporary = tempfile::NamedTempFile::new_in(path.parent().expect("state directory"))?;
  temporary.write_all(&bytes)?;
  temporary.persist(path).map_err(|error| error.error)?;
  Ok(())
}

#[cfg(unix)]
fn start_detached(
  request: &RunRequest,
  repo_root: &Path,
  snapshot: &RunSnapshot,
  state: &mut serde_json::Value,
) -> Result<()> {
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
        if let Some(url) = request
          .service
          .as_ref()
          .and_then(|service| crate::service::publishing::dashboard_url(service, &snapshot.run_id))
        {
          receipt["dashboard_url"] = json!(url);
        }
        println!("{receipt}");
        return Ok(());
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
) -> Result<()> {
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
  let crate::lock::LockAttempt::Acquired(_lease) =
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
  execute_snapshot(&request, repo_root, &snapshot, state, true)
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
      version: 1,
      source: Source::Url {
        url: "https://unavailable.invalid/data.bin".into(),
      },
      size,
      sha256,
    };
    let cache = crate::assets::cache_file(root, &descriptor.sha256);
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    fs::write(cache, bytes).unwrap();
    crate::assets::save(&root.join("data/train.bin.expri.toml"), &descriptor).unwrap();
    descriptor
  }

  #[test]
  fn preparation_uses_snapshot_descriptor_and_pins_old_cache_bytes() {
    let root = asset_repository();
    let first_descriptor = cached_asset(root.path(), b"first");
    let first = environment::snapshot::create(root.path(), &[]).unwrap();
    let second_descriptor = cached_asset(root.path(), b"second");
    let second = environment::snapshot::create(root.path(), &[]).unwrap();
    let canonical = root.path().canonicalize().unwrap();
    prepare_assets(&canonical, &first, None, &mut || Ok(false)).unwrap();
    prepare_assets(&canonical, &second, None, &mut || Ok(false)).unwrap();
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
      &mut || Ok(true),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("cancellation"), "{error}");
    assert!(!snapshot.code_dir.join("data/train.bin").exists());
    assert!(!snapshot.run_dir.join(".venv").exists());
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
