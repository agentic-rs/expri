use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use expri_core::error::Result;
use expri_core::lock::{LockAttempt, try_lock_file, try_lock_file_readonly};
use expri_core::storage::StoragePaths;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{ServiceComponent, ServiceContext, error, files, preferences, registry};

const START_TIMEOUT: Duration = Duration::from_secs(3);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const RESTART_TIMEOUT: Duration = Duration::from_secs(45);
pub(super) const TICK: Duration = Duration::from_millis(50);

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
  Status,
  Stop,
  Wake,
  StartComponent { component: ServiceComponent },
  StopComponent { component: ServiceComponent },
  RestartComponent { component: ServiceComponent },
}

pub(super) fn socket(directory: &Path) -> Result<PathBuf> {
  let path = directory.join("control.sock");
  if path.as_os_str().as_encoded_bytes().len() < 104 {
    return Ok(path);
  }
  // A user-owned 0700 directory within the system temporary directory keeps
  // long custom storage paths compatible with macOS's small sockaddr_un.
  // The canonical registry path supplies a stable, non-secret socket identity.
  let parent = Path::new("/tmp")
    .canonicalize()?
    .join(format!("expri-worker-{}", unsafe { libc::geteuid() }));
  if parent.try_exists()? {
    files::validate(&fs::symlink_metadata(&parent)?, true)?;
  }
  let digest = Sha256::digest(directory.as_os_str().as_encoded_bytes());
  let id: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
  let path = parent.join(id);
  if path.as_os_str().as_encoded_bytes().len() >= 104 {
    return Err(error(
      "the system temporary directory is too long for a private worker service socket",
    ));
  }
  Ok(path)
}

pub(super) fn validate_socket(path: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.file_type().is_socket()
    || metadata.uid() != unsafe { libc::geteuid() }
    || metadata.permissions().mode() & 0o077 != 0
  {
    return Err(error(
      "worker service control socket must be private and owned by the current user",
    ));
  }
  Ok(())
}

fn request(directory: &Path, request: Request) -> Result<Value> {
  let path = socket(directory)?;
  validate_socket(&path)?;
  let mut stream = UnixStream::connect(path)?;
  // Status is best-effort and may fall back to its durable snapshot. Stop needs
  // time for the runtime to finish a local registry scan before acknowledging.
  let timeout = match request {
    Request::Stop
    | Request::StartComponent { .. }
    | Request::StopComponent { .. }
    | Request::RestartComponent { .. } => STOP_TIMEOUT,
    _ => Duration::from_millis(200),
  };
  stream.set_read_timeout(Some(timeout))?;
  stream.set_write_timeout(Some(timeout))?;
  serde_json::to_writer(&mut stream, &request)?;
  stream.write_all(b"\n")?;
  let reply: Value = serde_json::from_slice(&read_frame(&mut stream, files::LIMIT as usize)?)
    .map_err(|_| error("worker service control response is invalid"))?;
  if reply["ok"] != true {
    return Err(error("worker service control request failed"));
  }
  Ok(reply["value"].clone())
}

fn read_frame(stream: &mut UnixStream, limit: usize) -> Result<Vec<u8>> {
  let mut reader = BufReader::new(stream);
  let mut bytes = Vec::new();
  loop {
    let available = reader.fill_buf()?;
    if available.is_empty() {
      return Err(error(
        "worker service control connection ended before its response",
      ));
    }
    let count = available
      .iter()
      .position(|byte| *byte == b'\n')
      .map(|position| position + 1)
      .unwrap_or(available.len());
    if bytes.len() + count > limit {
      return Err(error("worker service control message exceeds its limit"));
    }
    bytes.extend_from_slice(&available[..count]);
    reader.consume(count);
    if bytes.last() == Some(&b'\n') {
      return Ok(bytes);
    }
  }
}

/// Inspection never creates storage, leases or configuration.
fn service_status(paths: &StoragePaths) -> Result<Value> {
  let Some(directory) = files::existing(paths)? else {
    let choices = preferences::Preferences::load(paths.root())?;
    let components: std::collections::BTreeMap<_, _> = ServiceComponent::ALL
      .into_iter()
      .map(|component| {
        (
          component.as_str(),
          json!({"desired_running": choices.enabled(component), "generation": 0,
        "phase": "stopped", "status": "stopped", "parts": {}}),
        )
      })
      .collect();
    return Ok(
      json!({"running": false, "ready": false, "collection_ready": false,
      "status": "stopped", "components": components}),
    );
  };
  let running = matches!(
    try_lock_file_readonly(&directory.join("service.lock"))?,
    LockAttempt::Busy
  );
  let mut value = if directory.join("status.json").try_exists()? {
    serde_json::from_slice::<Value>(&files::read(&directory.join("status.json"))?)
      .map_err(|_| error("worker service status is invalid"))?
  } else {
    json!({"ready": false, "status": "starting"})
  };
  if running {
    if let Ok(current) = request(&directory, Request::Status) {
      value = current;
    } else {
      // A held lease proves a process exists, not that its control plane or
      // collector is ready. Cached readiness can belong to a prior generation.
      value["ready"] = json!(false);
      value["collection_ready"] = json!(false);
    }
  } else {
    value["ready"] = json!(false);
    value["active_jobs"] = json!(0);
    value["collection_ready"] = json!(false);
    value["status"] = json!("stopped");
    let choices = preferences::Preferences::load(paths.root())?;
    for component in ServiceComponent::ALL {
      let detail = &mut value["components"][component.as_str()];
      if !detail.is_object() {
        *detail = json!({"generation": 0});
      }
      if !detail["parts"].is_object() {
        detail["parts"] = json!({});
      }
      detail["desired_running"] = json!(choices.enabled(component));
      if detail["phase"] != "failed" || !choices.enabled(component) {
        detail["phase"] = json!("stopped");
        detail["status"] = json!("stopped");
      }
      detail["ready"] = json!(false);
      if let Some(parts) = detail.get_mut("parts").and_then(Value::as_object_mut) {
        for part in parts.values_mut().filter(|part| part.is_object()) {
          if matches!(
            part["phase"].as_str(),
            Some("starting" | "running" | "stopping")
          ) {
            part["phase"] = json!("stopped");
            part["status"] = json!("stopped");
          }
          part["ready"] = json!(false);
          if part.get("online").is_some() {
            part["online"] = json!(false);
          }
        }
      }
    }
    if let Some(execution) = value
      .get_mut("execution")
      .filter(|detail| detail.is_object())
    {
      execution["online"] = json!(false);
      if !matches!(
        execution["status"].as_str(),
        Some("failed" | "disabled" | "unconfigured")
      ) {
        execution["status"] = json!("stopped");
      }
    }
    if let Some(collection) = value
      .get_mut("collection")
      .filter(|detail| detail.is_object())
      && matches!(
        collection["status"].as_str(),
        Some("starting" | "collecting" | "ready")
      )
    {
      collection["status"] = json!("stopped");
    }
  }
  value["running"] = json!(running);

  Ok(value)
}

pub fn status(paths: &StoragePaths) -> Result<Value> {
  let mut value = service_status(paths)?;
  if let Some(directory) = files::existing(paths)? {
    match registry::summaries(&directory) {
      Ok(summaries) => {
        for (key, summary) in summaries.as_object().unwrap() {
          value[key] = summary.clone();
        }
      }
      Err(_) => value["jobs_warning"] = json!("Saved publishing job summaries could not be read"),
    }
  }
  Ok(value)
}

/// Stop the service cooperatively; training leases and process groups are untouched.
pub fn stop(paths: &StoragePaths) -> Result<Value> {
  let current = service_status(paths)?;
  if let Some(report) = stopped_or_stopping(&current) {
    return Ok(report);
  }
  let directory =
    files::existing(paths)?.ok_or_else(|| error("worker service storage disappeared"))?;
  match request(&directory, Request::Stop) {
    Ok(report) => Ok(report),
    Err(failure) => {
      // The service may accept stop but exit before its reply reaches us. Its
      // lease and durable state settle that ambiguous acknowledgment safely.
      if let Ok(current) = service_status(paths)
        && let Some(report) = stopped_or_stopping(&current)
      {
        return Ok(report);
      }
      Err(failure)
    }
  }
}

/// Start the selected component, preserving all other saved manual choices.
pub fn start_component(paths: &StoragePaths, component: ServiceComponent) -> Result<Value> {
  let current = service_status(paths)?;
  if current["running"] != true {
    save_offline_choice(paths, component, true)?;
  }
  ensure_started(paths)?;
  let directory =
    files::existing(paths)?.ok_or_else(|| error("worker service storage disappeared"))?;
  request(&directory, Request::StartComponent { component })
}

/// Enable and restart one component atomically. Its previous generation drains
/// before a new one starts; other components and training remain running.
pub fn restart_component(paths: &StoragePaths, component: ServiceComponent) -> Result<Value> {
  let current = service_status(paths)?;
  if current["running"] != true {
    // A fresh worker can already accept an assignment during startup. Enable
    // and start it once instead of immediately cancelling that new generation.
    let mut report = start_component(paths, component)?;
    report["restarting"] = json!(true);
    return Ok(report);
  }
  ensure_started(paths)?;
  let directory =
    files::existing(paths)?.ok_or_else(|| error("worker service storage disappeared"))?;
  request(&directory, Request::RestartComponent { component })
}

/// Restart the process after all old components and network transfers drain.
/// Saved enable/disable choices, durable jobs, and training are preserved.
pub fn restart(paths: &StoragePaths) -> Result<Value> {
  restart_with_launcher(paths, spawn_service, RESTART_TIMEOUT)
}

pub(super) fn restart_with_launcher(
  paths: &StoragePaths,
  launch: impl FnOnce(&Path, &StoragePaths) -> Result<Option<Child>>,
  timeout: Duration,
) -> Result<Value> {
  let directory = files::ensure(paths)?;
  let deadline = Instant::now() + timeout;
  let _lease = start_lock(&directory, deadline)?;
  stop(paths)?;
  // Probe only the lease while shutdown drains. The old control listener may
  // intentionally stop accepting requests, so repeated IPC probes can fill it.
  while matches!(
    try_lock_file_readonly(&directory.join("service.lock"))?,
    LockAttempt::Busy
  ) {
    if Instant::now() >= deadline {
      return Err(error(
        "worker service restart timed out waiting for shutdown; saved work and training remain preserved; run service status",
      ));
    }
    thread::sleep(TICK);
  }
  if Instant::now() >= deadline {
    return Err(error(
      "worker service restart timed out before startup; run service start to retry",
    ));
  }
  let mut report = launch_locked(
    paths,
    &directory,
    launch,
    deadline.min(Instant::now() + START_TIMEOUT),
  )?;
  report["restarted"] = json!(true);
  report["jobs_preserved"] = json!(true);
  report["training_preserved"] = json!(true);
  Ok(report)
}

/// Pause only this component. The choice survives whole-service restart.
pub fn stop_component(paths: &StoragePaths, component: ServiceComponent) -> Result<Value> {
  let current = service_status(paths)?;
  if current["running"] != true && save_offline_choice(paths, component, false)? {
    let mut report = service_status(paths)?;
    report["component"] = json!(component);
    report["stopped"] = json!(true);
    report["jobs_preserved"] = json!(true);
    report["training_preserved"] = json!(true);
    return Ok(report);
  }
  let directory =
    files::existing(paths)?.ok_or_else(|| error("worker service storage disappeared"))?;
  match request(&directory, Request::StopComponent { component }) {
    Ok(report) => Ok(report),
    Err(failure) => {
      // A lost acknowledgment is safe only after establishing the saved pause.
      let current = service_status(paths)?;
      if current["components"][component.as_str()]["desired_running"] == false {
        Ok(current)
      } else {
        Err(failure)
      }
    }
  }
}

fn save_offline_choice(
  paths: &StoragePaths,
  component: ServiceComponent,
  enabled: bool,
) -> Result<bool> {
  let directory = files::ensure(paths)?;
  let lock_path = directory.join("start.lock");
  files::private_file(&lock_path)?;
  let deadline = Instant::now() + START_TIMEOUT;
  let _lease = loop {
    if let LockAttempt::Acquired(lease) = try_lock_file(&lock_path, false)? {
      break lease;
    }
    if Instant::now() >= deadline {
      return Err(error(
        "worker service startup is still pending; retry component control",
      ));
    }
    thread::sleep(TICK);
  };
  let _choice = preferences::Preferences::lock(&directory)?;
  // Foreground bootstrap holds this preference lock through lease/socket
  // initialization, while lazy launch is additionally guarded by start.lock.
  if service_status(paths)?["running"] == true {
    return Ok(false);
  }
  preferences::Preferences::save_choice_locked(&directory, component, enabled)?;
  Ok(true)
}

fn stopped_or_stopping(current: &Value) -> Option<Value> {
  if current["running"] != true {
    Some(json!({"stopped": true, "already_stopped": true, "jobs_preserved": true}))
  } else if current["status"] == "stopping" {
    Some(
      json!({"stopping": true, "already_stopping": true, "jobs_preserved": true, "training_preserved": true}),
    )
  } else {
    None
  }
}

pub(super) fn wake(paths: &StoragePaths) -> Result<()> {
  if let Some(directory) = files::existing(paths)? {
    request(&directory, Request::Wake)?;
  }
  Ok(())
}

/// Idempotent lazy startup, with a bounded local readiness handshake.
pub fn ensure_started(paths: &StoragePaths) -> Result<Value> {
  ensure_with_launcher(paths, spawn_service)
}

/// Training capture needs a committed collection observation, independently of
/// process readiness. Explicitly paused collection is never restarted here.
pub fn ensure_collection_ready(paths: &StoragePaths) -> Result<Value> {
  ensure_started(paths)?;
  let deadline = Instant::now() + START_TIMEOUT;
  loop {
    let current = service_status(paths)?;
    if current["collection_ready"] == true {
      return Ok(current);
    }
    let collection = &current["components"][ServiceComponent::Collection.as_str()];
    if collection["desired_running"] == false {
      return Err(error(
        "system collection is paused; run service start --component collection",
      ));
    }
    if current["running"] != true
      || collection["phase"] == "failed"
      || collection["parts"]["collection"]["phase"] == "failed"
    {
      return Err(error(
        "worker service collection could not become ready; inspect service status",
      ));
    }
    if Instant::now() >= deadline {
      return Err(error(
        "worker service collection is not ready yet; run service status",
      ));
    }
    thread::sleep(TICK);
  }
}

pub(super) fn ensure_with_launcher(
  paths: &StoragePaths,
  launch: impl FnOnce(&Path, &StoragePaths) -> Result<Option<Child>>,
) -> Result<Value> {
  let initial = service_status(paths)?;
  if initial["running"] == true && initial["ready"] == true {
    return Ok(initial);
  }
  let directory = files::ensure(paths)?;
  socket(&directory)?;
  let lock_path = directory.join("start.lock");
  files::private_file(&lock_path)?;
  let deadline = Instant::now() + START_TIMEOUT;
  let _lease = loop {
    if let LockAttempt::Acquired(lease) = try_lock_file(&lock_path, false)? {
      break lease;
    }
    let current = service_status(paths)?;
    if current["running"] == true && current["ready"] == true {
      return Ok(current);
    }
    if Instant::now() >= deadline {
      return Err(error(
        "worker service startup is still pending; run service status",
      ));
    }
    thread::sleep(TICK);
  };
  launch_locked(paths, &directory, launch, deadline)
}

fn start_lock(directory: &Path, deadline: Instant) -> Result<expri_core::lock::FileLock> {
  let path = directory.join("start.lock");
  files::private_file(&path)?;
  loop {
    if let LockAttempt::Acquired(lease) = try_lock_file(&path, false)? {
      return Ok(lease);
    }
    if Instant::now() >= deadline {
      return Err(error(
        "worker service control is busy; retry restart after service startup or shutdown settles",
      ));
    }
    thread::sleep(TICK);
  }
}

fn launch_locked(
  paths: &StoragePaths,
  directory: &Path,
  launch: impl FnOnce(&Path, &StoragePaths) -> Result<Option<Child>>,
  deadline: Instant,
) -> Result<Value> {
  let mut child = None;
  if service_status(paths)?["running"] != true {
    child = launch(directory, paths)?;
  }
  loop {
    let current = service_status(paths)?;
    if current["running"] == true && current["ready"] == true {
      return Ok(current);
    }
    if let Some(child) = child.as_mut()
      && child.try_wait()?.is_some()
    {
      return Err(error(
        "worker service exited during startup; inspect the private service log",
      ));
    }
    if Instant::now() >= deadline {
      return Err(error(
        "worker service control process is not ready yet; run service status",
      ));
    }
    thread::sleep(TICK);
  }
}

fn spawn_service(directory: &Path, paths: &StoragePaths) -> Result<Option<Child>> {
  let log_path = directory.join("service.log");
  let log = files::private_file(&log_path)?;
  // Append avoids concurrent startup diagnostics overwriting earlier failures.
  let mut options = fs::OpenOptions::new();
  use std::os::unix::fs::OpenOptionsExt;
  options.append(true).custom_flags(libc::O_NOFOLLOW);
  let output = options.open(log_path)?;
  drop(log);
  let mut command = Command::new(std::env::current_exe()?);
  command
    .args(["service", "start", "--foreground", "--storage-dir"])
    .arg(paths.root())
    .current_dir("/")
    .stdin(Stdio::null())
    .stdout(Stdio::from(output.try_clone()?))
    .stderr(Stdio::from(output));
  unsafe {
    command.pre_exec(|| {
      if libc::setsid() < 0 {
        Err(std::io::Error::last_os_error())
      } else {
        Ok(())
      }
    });
  }
  Ok(Some(command.spawn()?))
}

/// Wait/watch affects the caller only; it never cancels the durable publishing job.
pub fn observe(paths: &StoragePaths, id: &str, watch: bool) -> Result<Value> {
  let mut previous = None;
  loop {
    let mut value = registry::job_status(paths, id)?;
    let current = service_status(paths)?;
    value["service"] = current.clone();
    if current["components"][ServiceComponent::Publishing.as_str()]["desired_running"] == false
      && !matches!(
        value["status"].as_str(),
        Some("published" | "error" | "paused")
      )
    {
      value["status"] = json!("paused");
      value["warning"] = json!(
        "Publishing is paused; saved work remains queued. Run service start --component publishing to resume."
      );
      return Ok(value);
    }
    let visible = json!({"status": value["status"], "progress": value["progress"],
      "last_error": value["last_error"], "retry_at_ms": value["retry_at_ms"],
      "service_status": current["status"], "service_running": current["running"]});
    if watch && previous.as_ref() != Some(&visible) {
      eprintln!("{}", serde_json::to_string(&value)?);
      previous = Some(visible);
    }
    if matches!(
      value["status"].as_str(),
      Some("published" | "error" | "paused")
    ) || current["running"] != true
    {
      return Ok(value);
    }
    thread::sleep(Duration::from_millis(200));
  }
}

#[cfg(test)]
pub(super) fn handle(
  stream: &mut UnixStream,
  context: &ServiceContext,
  active: usize,
  pending: usize,
) -> Result<()> {
  handle_request(stream, context, active, pending, None)
}

pub(super) fn handle_control(
  stream: &mut UnixStream,
  context: &ServiceContext,
  directory: &Path,
) -> Result<()> {
  handle_request(
    stream,
    context,
    context
      .shared
      .active_jobs
      .load(std::sync::atomic::Ordering::Acquire),
    context
      .shared
      .pending_jobs
      .load(std::sync::atomic::Ordering::Acquire),
    Some(directory),
  )
}

fn handle_request(
  stream: &mut UnixStream,
  context: &ServiceContext,
  active: usize,
  pending: usize,
  directory: Option<&Path>,
) -> Result<()> {
  // Accepted sockets inherit the listener's nonblocking mode on some Unix
  // platforms. Apply bounded blocking I/O before waiting for the request.
  stream.set_nonblocking(false)?;
  stream.set_read_timeout(Some(Duration::from_millis(200)))?;
  stream.set_write_timeout(Some(Duration::from_millis(200)))?;
  let request: Request = serde_json::from_slice(&read_frame(stream, 4 * 1024)?)
    .map_err(|_| error("worker service control request is invalid"))?;
  let value = match request {
    Request::Status => runtime::snapshot(context, active, pending),
    Request::Wake => {
      context.request_scan();
      runtime::snapshot(context, active, pending)
    }
    Request::Stop => {
      context.request_stop();
      json!({"stopping": true, "jobs_preserved": true, "training_preserved": true})
    }
    Request::StartComponent { component }
    | Request::StopComponent { component }
    | Request::RestartComponent { component } => {
      if context.is_stopping() {
        return Err(error("worker service is stopping"));
      }
      let enabled = !matches!(request, Request::StopComponent { .. });
      let restarting = matches!(request, Request::RestartComponent { .. });
      let directory = directory.ok_or_else(|| error("component control needs service storage"))?;
      preferences::Preferences::save_choice(directory, component, enabled)?;
      context.request_component(component, enabled, restarting);
      let mut report = runtime::snapshot(context, active, pending);
      report["component"] = json!(component);
      report["stopping"] = json!(!enabled);
      report["restarting"] = json!(restarting);
      report["jobs_preserved"] = json!(true);
      report["training_preserved"] = json!(true);
      report
    }
  };
  serde_json::to_writer(&mut *stream, &json!({"ok": true, "value": value}))?;
  stream.write_all(b"\n")?;
  Ok(())
}

mod runtime;
mod signals;
#[cfg(test)]
pub(super) use runtime::run_components;
#[cfg(test)]
pub(super) use runtime::run_runtime;
#[cfg(test)]
pub(super) use runtime::snapshot;
pub use runtime::{run_foreground, run_foreground_components};
