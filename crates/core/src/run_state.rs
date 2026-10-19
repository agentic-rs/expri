//! Run phases and serialized local state updates shared by tasks and supervisors.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::error::{ExpriError, Result};
use crate::lock::{self, LockAttempt};

pub const STATUSES: &[&str] = &[
  "pending",
  "preparing",
  "running",
  "waiting",
  "paused",
  "completed",
  "failed",
  "cancelled",
  "lost",
  "unknown",
];
pub const TASK_STATUSES: &[&str] = &["running", "waiting"];
pub const MAX_STATE_BYTES: usize = 256 * 1024;

pub fn is_active_status(status: &str) -> bool {
  matches!(status, "preparing" | "running" | "waiting" | "paused")
}

pub fn is_terminal_status(status: &str) -> bool {
  matches!(status, "completed" | "failed" | "cancelled" | "lost")
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RunStatusUpdate {
  pub run_id: String,
  pub previous_status: String,
  pub status: String,
  pub changed: bool,
}

/// Tasks may report a phase only while the supervisor owns the run lease.
pub fn set_task_status(run_dir: &Path, status: &str) -> Result<RunStatusUpdate> {
  if !TASK_STATUSES.contains(&status) {
    return Err(message("task status must be running or waiting"));
  }
  let run_dir = if run_dir.is_absolute() {
    run_dir.to_owned()
  } else {
    std::env::current_dir()?.join(run_dir)
  };
  update_state(&run_dir.join("run-state.json"), |state| {
    let run_id = state["run_id"]
      .as_str()
      .ok_or_else(|| message("run-state.json has no valid run_id"))?
      .to_owned();
    crate::identifiers::validate_component(&run_id)?;
    if !run_id
      .bytes()
      .next()
      .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
      return Err(message("run-state.json has an invalid run_id"));
    }
    if run_dir.file_name().and_then(|name| name.to_str()) != Some(run_id.as_str()) {
      return Err(message(
        "run-state.json identity does not match its run directory",
      ));
    }
    if state["schema_version"] != 1 {
      return Err(message("run-state.json has an unsupported schema_version"));
    }
    if state["started_at"]
      .as_str()
      .is_none_or(|start| chrono::DateTime::parse_from_rfc3339(start).is_err())
    {
      return Err(message("run-state.json has invalid or missing started_at"));
    }
    let previous_status = state["status"]
      .as_str()
      .filter(|phase| TASK_STATUSES.contains(phase))
      .ok_or_else(|| message("only running or waiting tasks can change status"))?
      .to_owned();
    if !state["finished_at"].is_null() {
      return Err(message("a finished run cannot change task status"));
    }
    if state
      .get("detached")
      .is_some_and(|value| !value.is_boolean())
    {
      return Err(message("run-state.json has an invalid detached field"));
    }
    for (field, child) in [("code_dir", "code"), ("output_dir", "outputs")] {
      if state.get(field).is_none() && state["detached"] != true {
        continue;
      }
      let expected = run_dir.join(child);
      if state[field].as_str() != expected.to_str() {
        return Err(message("run-state.json has a directory outside its run"));
      }
      let metadata = fs::symlink_metadata(&expected)?;
      if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(message(
          "run-state.json requires real code and output directories",
        ));
      }
    }
    if !matches!(
      lock::try_lock_file_readonly(&run_dir.join(".run.lock"))?,
      LockAttempt::Busy
    ) {
      return Err(message(
        "run supervisor is not active; task status was not changed",
      ));
    }
    let attempt_changed = crate::task_attempts::set_current_phase(state, status)?;
    let changed = previous_status != status || attempt_changed;
    state["status"] = Value::String(status.into());
    Ok(RunStatusUpdate {
      run_id,
      previous_status,
      status: status.into(),
      changed,
    })
  })
}

/// Re-read and atomically update metadata under the shared short state lease.
/// Unchanged records are not rewritten, so idempotent requests emit no revision.
pub fn update_state<T>(path: &Path, update: impl FnOnce(&mut Value) -> Result<T>) -> Result<T> {
  let directory = state_directory(path)?;
  let _lease = lock::run_state_lock(directory)?;
  update_locked(path, update)
}

/// Attempt a short metadata update without delaying a live task's log pump.
/// A busy state lease returns None and never invokes the update callback.
pub fn try_update_state<T>(
  path: &Path,
  update: impl FnOnce(&mut Value) -> Result<T>,
) -> Result<Option<T>> {
  let directory = state_directory(path)?;
  let LockAttempt::Acquired(_lease) =
    lock::try_lock_file(&directory.join(".run-state.lock"), true)?
  else {
    return Ok(None);
  };
  update_locked(path, update).map(Some)
}

fn update_locked<T>(path: &Path, update: impl FnOnce(&mut Value) -> Result<T>) -> Result<T> {
  let mut state = read_state(path)?;
  let original = state.clone();
  let result = update(&mut state)?;
  if state != original {
    save_state(path, &state)?;
  }
  Ok(result)
}

/// Publish supervisor changes while retaining fields added since its snapshot.
/// A terminal record can never be restored to an active phase by a stale writer.
pub fn write_state(path: &Path, state: &Value) -> Result<()> {
  state_bytes(state)?;
  let directory = state_directory(path)?;
  let _lease = lock::run_state_lock(directory)?;
  let mut current = match read_state(path) {
    Ok(value) => value,
    Err(ExpriError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
      return save_state(path, state);
    }
    Err(error) => return Err(error),
  };
  if current["status"].as_str().is_some_and(is_terminal_status)
    && state["status"]
      .as_str()
      .is_some_and(|phase| !is_terminal_status(phase))
  {
    return Err(message(
      "a terminal run cannot be restored to an active phase",
    ));
  }
  let fields = current
    .as_object_mut()
    .expect("read_state validates an object");
  for (key, value) in state.as_object().expect("state_bytes validates an object") {
    fields.insert(key.clone(), value.clone());
  }
  save_state(path, &current)
}

fn state_directory(path: &Path) -> Result<&Path> {
  path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .ok_or_else(|| message("run-state.json requires a run directory"))
}

fn read_state(path: &Path) -> Result<Value> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_file() || metadata.file_type().is_symlink() {
    return Err(message("run-state.json must be a regular file"));
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
    return Err(message("run-state.json must be a regular file"));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if opened.dev() != current.dev() || opened.ino() != current.ino() {
      return Err(message("run-state.json changed while opening"));
    }
  }
  let mut bytes = Vec::new();
  file
    .take((MAX_STATE_BYTES + 1) as u64)
    .read_to_end(&mut bytes)?;
  if bytes.len() > MAX_STATE_BYTES {
    return Err(message(
      "run-state.json exceeds the 256 KiB metadata size limit",
    ));
  }
  let value: Value = serde_json::from_slice(&bytes)?;
  if !value.is_object() {
    return Err(message("run-state.json must contain an object"));
  }
  Ok(value)
}

fn state_bytes(state: &Value) -> Result<Vec<u8>> {
  if !state.is_object() {
    return Err(message("run-state.json must contain an object"));
  }
  let bytes = serde_json::to_vec_pretty(state)?;
  if bytes.len() > MAX_STATE_BYTES {
    return Err(message(
      "run-state.json exceeds the 256 KiB metadata size limit; reduce command or asset source metadata",
    ));
  }
  Ok(bytes)
}

fn save_state(path: &Path, state: &Value) -> Result<()> {
  let bytes = state_bytes(state)?;
  let directory = state_directory(path)?;
  let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
  temporary.write_all(&bytes)?;
  temporary.as_file().sync_all()?;
  temporary.persist(path).map_err(|error| error.error)?;
  #[cfg(unix)]
  File::open(directory)?.sync_all()?;
  Ok(())
}

fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests;
