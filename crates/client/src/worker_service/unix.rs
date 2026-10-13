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

use super::{ServiceContext, error, files, registry};

const START_TIMEOUT: Duration = Duration::from_secs(3);
pub(super) const TICK: Duration = Duration::from_millis(50);

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
  Status,
  Stop,
  Wake,
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
  stream.set_read_timeout(Some(Duration::from_millis(200)))?;
  stream.set_write_timeout(Some(Duration::from_millis(200)))?;
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
    return Ok(json!({"running": false, "ready": false, "status": "stopped"}));
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
    }
  } else {
    value["ready"] = json!(false);
    value["active_jobs"] = json!(0);
    value["status"] = json!(if value["collection"]["status"] == "failed" {
      "collection_failed"
    } else {
      "stopped"
    });
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
  if current["running"] != true {
    return Ok(json!({"stopped": true, "already_stopped": true, "jobs_preserved": true}));
  }
  if current["status"] == "stopping" {
    return Ok(
      json!({"stopping": true, "already_stopping": true, "jobs_preserved": true, "training_preserved": true}),
    );
  }
  let directory =
    files::existing(paths)?.ok_or_else(|| error("worker service storage disappeared"))?;
  request(&directory, Request::Stop)
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
  let mut child = None;
  if service_status(paths)?["running"] != true {
    child = launch(&directory, paths)?;
  }
  loop {
    let current = service_status(paths)?;
    if current["running"] == true && current["ready"] == true {
      return Ok(current);
    }
    if current["running"] == true && current["status"] == "collection_failed" {
      return Err(error(
        "worker service collection could not become ready; inspect service status",
      ));
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
        "worker service collection is not ready yet; run service status",
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

pub(super) fn handle(
  stream: &mut UnixStream,
  context: &ServiceContext,
  active: usize,
  pending: usize,
) -> Result<()> {
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
  };
  serde_json::to_writer(&mut *stream, &json!({"ok": true, "value": value}))?;
  stream.write_all(b"\n")?;
  Ok(())
}

mod runtime;
mod signals;
pub use runtime::run_foreground;
#[cfg(test)]
pub(super) use runtime::run_runtime;
