use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::{ExpriError, Result};
use crate::lock::{self, LockAttempt};
use crate::protocol::JobRequest;

const STATE_LIMIT: u64 = 256 * 1024;
const CANCEL_FILE: &str = ".cancel-request";
const TERMINAL_STATUSES: [&str; 3] = ["completed", "failed", "cancelled"];

/// Read live status or request cancellation without signaling a metadata PID.
pub fn query_at(request: &JobRequest, repo_root: &Path) -> Result<Value> {
  match request {
    JobRequest::Status { run_id } => status_at(repo_root, run_id),
    JobRequest::Cancel { run_id } => cancel_at(repo_root, run_id),
    JobRequest::Logs { .. } => Err(message("log requests produce a byte stream")),
  }
}

pub fn execute_at(request: &JobRequest, repo_root: &Path) -> Result<()> {
  if matches!(request, JobRequest::Logs { .. }) {
    stream_at(request, repo_root)
  } else {
    println!(
      "{}",
      serde_json::to_string_pretty(&query_at(request, repo_root)?)?
    );
    Ok(())
  }
}

/// Live lease checks belong here, never in the offline run catalog.
pub fn status_at(repo_root: &Path, run_id: &str) -> Result<Value> {
  let run_dir = run_directory(repo_root, run_id)?;
  let state = read_state(&run_dir, run_id)?;
  let busy = matches!(
    lock::try_lock_file(&run_dir.join(".run.lock"), false)?,
    LockAttempt::Busy
  );
  // A worker publishes terminal state before releasing its lease. Re-read after
  // probing so a completion during inspection does not look like a lost worker.
  let state = if busy {
    state
  } else {
    read_state(&run_dir, run_id)?
  };
  let recorded_status = state["status"].as_str().expect("validated status");
  let detached = state["detached"].as_bool().unwrap_or(false);
  let active = matches!(recorded_status, "preparing" | "running");
  let status = if detached && active && !busy {
    "lost"
  } else {
    recorded_status
  };
  let cancel_requested = cancellation_requested(&run_dir)?;
  let mut report = json!({
    "run_id": run_id, "status": status, "recorded_status": recorded_status,
    "detached": detached, "alive": busy && active,
    "cancel_requested": cancel_requested, "state": state,
  });
  match crate::service::publishing::status(&run_dir) {
    Ok(Some(service_sync)) => report["service_sync"] = service_sync,
    Ok(None) => {}
    Err(_) => {
      report["service_sync"] = json!({
        "status": "error", "last_error": "Service publishing status could not be read."
      });
    }
  }
  Ok(report)
}

fn cancel_at(repo_root: &Path, run_id: &str) -> Result<Value> {
  let mut report = status_at(repo_root, run_id)?;
  if TERMINAL_STATUSES.contains(&report["status"].as_str().unwrap_or_default()) {
    report["already_finished"] = json!(true);
    return Ok(report);
  }
  if report["detached"] != true {
    return Err(message("cancellation requires a detached run"));
  }
  if report["alive"] != true {
    return Err(message(
      "run supervisor is not active; cancellation was not requested",
    ));
  }
  let run_dir = run_directory(repo_root, run_id)?;
  let mut options = OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
  }
  match options.open(run_dir.join(CANCEL_FILE)) {
    Ok(file) => drop(file),
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
      cancellation_requested(&run_dir)?;
    }
    Err(error) => return Err(error.into()),
  }
  report = status_at(repo_root, run_id)?;
  Ok(report)
}

pub fn stream_at(request: &JobRequest, repo_root: &Path) -> Result<()> {
  let JobRequest::Logs {
    run_id,
    stream,
    follow,
    tail,
  } = request
  else {
    return Err(message("expected a log request"));
  };
  let mut output = io::stdout().lock();
  stream_to(repo_root, run_id, stream, *follow, *tail, &mut output)
}

fn stream_to(
  repo_root: &Path,
  run_id: &str,
  stream: &str,
  follow: bool,
  tail: usize,
  output: &mut impl Write,
) -> Result<()> {
  if !matches!(stream, "stdout" | "stderr") {
    return Err(message("log stream must be stdout or stderr"));
  }
  let run_dir = run_directory(repo_root, run_id)?;
  status_at(repo_root, run_id)?;
  let mut file = None;
  let mut first_poll = true;
  let mut buffer = [0_u8; 64 * 1024];
  loop {
    if file.is_none()
      && let Some(mut opened) = open_log(&run_dir, stream)?
    {
      if first_poll {
        seek_tail(&mut opened, tail)?;
      }
      file = Some(opened);
    }
    first_poll = false;
    if !follow {
      if let Some(file) = file.as_mut() {
        copy_available(file, output)?;
      }
      return Ok(());
    }
    if let Some(file) = file.as_mut() {
      // Bound a polling batch even if a task writes faster than this reader.
      for _ in 0..16 {
        let size = file.read(&mut buffer)?;
        if size == 0 {
          break;
        }
        output.write_all(&buffer[..size])?;
      }
      output.flush()?;
    }
    let status = status_at(repo_root, run_id)?;
    if !matches!(status["status"].as_str(), Some("preparing" | "running")) {
      // Terminal state is published only after log draining, so one final copy
      // catches bytes appended between our last read and the status check.
      if file.is_none() {
        // A short task can create and close its logs between our first open
        // attempt and the status check. Those bytes are new since invocation.
        file = open_log(&run_dir, stream)?;
      }
      if let Some(file) = file.as_mut() {
        copy_available(file, output)?;
      }
      return Ok(());
    }
    thread::sleep(Duration::from_millis(100));
  }
}

fn copy_available(file: &mut File, output: &mut impl Write) -> io::Result<()> {
  let position = file.stream_position()?;
  let remaining = file.metadata()?.len().saturating_sub(position);
  io::copy(&mut file.take(remaining), output)?;
  output.flush()
}

fn seek_tail(file: &mut File, lines: usize) -> io::Result<()> {
  let end = file.seek(SeekFrom::End(0))?;
  if lines == 0 || end == 0 {
    return Ok(());
  }
  let mut position = end;
  let mut separators = 0;
  let mut buffer = [0_u8; 8192];
  while position != 0 {
    let size = position.min(buffer.len() as u64) as usize;
    position -= size as u64;
    file.seek(SeekFrom::Start(position))?;
    file.read_exact(&mut buffer[..size])?;
    for offset in (0..size).rev() {
      let absolute = position + offset as u64;
      if buffer[offset] == b'\n' && absolute != end - 1 {
        separators += 1;
        if separators == lines {
          file.seek(SeekFrom::Start(absolute + 1))?;
          return Ok(());
        }
      }
    }
  }
  file.seek(SeekFrom::Start(0))?;
  Ok(())
}

pub fn run_directory(repo_root: &Path, run_id: &str) -> Result<PathBuf> {
  let mut bytes = run_id.bytes();
  if !bytes
    .next()
    .is_some_and(|byte| byte.is_ascii_alphanumeric())
    || !bytes.all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
  {
    return Err(message(format!("invalid run ID: {run_id}")));
  }
  let root = fs::canonicalize(repo_root)?;
  let state_dir = root.join(".expri");
  real_directory(&state_dir)?;
  let runs_dir = state_dir.join("runs");
  real_directory(&runs_dir)?;
  let run_dir = runs_dir.join(run_id);
  real_directory(&run_dir)?;
  Ok(run_dir)
}

fn read_state(run_dir: &Path, run_id: &str) -> Result<Value> {
  let mut file = regular_file(&run_dir.join("run-state.json"))?;
  if file.metadata()?.len() > STATE_LIMIT {
    return Err(message("run-state.json exceeds the metadata size limit"));
  }
  let mut bytes = Vec::new();
  Read::by_ref(&mut file)
    .take(STATE_LIMIT + 1)
    .read_to_end(&mut bytes)?;
  if bytes.len() as u64 > STATE_LIMIT {
    return Err(message("run-state.json exceeds the metadata size limit"));
  }
  let state: Value = serde_json::from_slice(&bytes)?;
  if !state.is_object() || state["run_id"].as_str() != Some(run_id) {
    return Err(message("run state does not match its directory"));
  }
  if state
    .get("schema_version")
    .is_some_and(|value| !value.as_u64().is_some_and(|schema| schema <= 1))
  {
    return Err(message("run state uses an unsupported schema_version"));
  }
  if !matches!(
    state["status"].as_str(),
    Some("preparing" | "running" | "completed" | "failed" | "cancelled" | "lost")
  ) {
    return Err(message("run state has an invalid status"));
  }
  if state
    .get("detached")
    .is_some_and(|value| !value.is_boolean())
  {
    return Err(message("run state has an invalid detached field"));
  }
  if state["detached"] == true {
    for (field, name) in [("code_dir", "code"), ("output_dir", "outputs")] {
      let path = run_dir.join(name);
      if state[field].as_str() != path.to_str() {
        return Err(message("run state does not match its directory"));
      }
      real_directory(&path)?;
    }
  }
  Ok(state)
}

fn cancellation_requested(run_dir: &Path) -> Result<bool> {
  match fs::symlink_metadata(run_dir.join(CANCEL_FILE)) {
    Ok(metadata)
      if metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == 0 =>
    {
      Ok(true)
    }
    Ok(_) => Err(message(
      "cancellation request must be an empty regular file",
    )),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(error.into()),
  }
}

fn open_log(run_dir: &Path, stream: &str) -> Result<Option<File>> {
  let logs_dir = run_dir.join("logs");
  match fs::symlink_metadata(&logs_dir) {
    Ok(_) => real_directory(&logs_dir)?,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(error.into()),
  }
  let path = logs_dir.join(format!("{stream}.log"));
  match regular_file(&path) {
    Ok(file) => {
      real_directory(&logs_dir)?;
      Ok(Some(file))
    }
    Err(ExpriError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error),
  }
}

fn regular_file(path: &Path) -> Result<File> {
  let initial = fs::symlink_metadata(path)?;
  if !initial.is_file() || initial.file_type().is_symlink() {
    return Err(message(format!("file must be regular: {}", path.display())));
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options.open(path)?;
  let opened = file.metadata()?;
  let current = fs::symlink_metadata(path)?;
  if !opened.is_file() || !current.is_file() || current.file_type().is_symlink() {
    return Err(message(format!("file must be regular: {}", path.display())));
  }
  // Run state is published by atomic replacement. Reading the previous regular
  // descriptor is safe, and must not become a spurious error during publication.
  Ok(file)
}

fn real_directory(path: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  if metadata.is_dir() && !metadata.file_type().is_symlink() {
    Ok(())
  } else {
    Err(message(format!(
      "directory must be real: {}",
      path.display()
    )))
  }
}

fn message(value: impl Into<String>) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests;
