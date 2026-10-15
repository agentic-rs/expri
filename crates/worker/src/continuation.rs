//! Checkpoint continuations retain one run identity and record separate attempts.

use expri_core::error::{ExpriError, Result};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

/// Publishable per-attempt documents complement the authoritative run state.
/// Keep private request fingerprints, local environment paths and process
/// identities out of these documents.
pub(crate) fn sync_attempt_records(run_dir: &Path) -> Result<()> {
  let mut bytes = Vec::new();
  File::open(run_dir.join("run-state.json"))?
    .take(expri_core::run_state::MAX_STATE_BYTES as u64 + 1)
    .read_to_end(&mut bytes)?;
  if bytes.len() > expri_core::run_state::MAX_STATE_BYTES {
    return Err(ExpriError::Message(
      "run state exceeds its metadata limit".into(),
    ));
  }
  let state: Value = serde_json::from_slice(&bytes)?;
  let Some(attempts) = state["task_attempts"]["attempts"].as_array() else {
    return Ok(());
  };
  validate_metadata_directory(&run_dir.join("outputs"))?;
  let parent = run_dir.join("outputs/attempts");
  create_metadata_directory(&parent)?;
  for attempt in attempts {
    let Some(number) = attempt["attempt"].as_u64() else {
      continue;
    };
    let directory = parent.join(number.to_string());
    create_metadata_directory(&directory)?;
    let mut record = serde_json::Map::new();
    for field in [
      "attempt",
      "request_id",
      "reason",
      "task",
      "command",
      "status",
      "requested_at",
      "previous_worker_stopped_at",
      "started_at",
      "finished_at",
      "exit_code",
      "error",
      "worker_restarted",
      "metric_step_offset",
      "source",
      "system_capture_id",
      "machine_id",
      "system_capture_started_at",
      "system_capture_finished_at",
      "system_capture_status",
    ] {
      if let Some(value) = attempt.get(field) {
        record.insert(field.into(), value.clone());
      }
    }
    if let Some(checkpoint) = attempt["checkpoint"].as_object() {
      record.insert(
        "checkpoint".into(),
        Value::Object(
          checkpoint
            .iter()
            .filter(|(key, _)| ["path", "sha256", "size"].contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        ),
      );
    }
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    file.write_all(&serde_json::to_vec(&Value::Object(record))?)?;
    file.as_file().sync_all()?;
    file
      .persist(directory.join("attempt-state.json"))
      .map_err(|error| error.error)?;
    File::open(&directory)?.sync_all()?;
  }
  Ok(())
}

fn create_metadata_directory(path: &Path) -> Result<()> {
  match fs::create_dir(path) {
    Ok(()) => {}
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
    Err(error) => return Err(error.into()),
  }
  validate_metadata_directory(path)
}

fn validate_metadata_directory(path: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(ExpriError::Message(
      "attempt metadata directory must be a real directory".into(),
    ));
  }
  Ok(())
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{
  PreparedContinuation, complete_old_attempt, fail_after_stop, launch_replacement, mark_stopping,
  pending, reject_prepared,
};
#[cfg(target_os = "linux")]
pub use linux::{request_continue, worker};

#[cfg(not(target_os = "linux"))]
pub fn request_continue(
  _: &std::path::Path,
  _: &expri_core::protocol::ContinueRunRequest,
) -> expri_core::error::Result<serde_json::Value> {
  Err(expri_core::error::ExpriError::Message(
    "task continuation requires a native Linux detached worker".into(),
  ))
}

#[cfg(not(target_os = "linux"))]
pub fn worker(_: &std::path::Path, _: &str) -> expri_core::error::Result<()> {
  Err(expri_core::error::ExpriError::Message(
    "task continuation requires Linux".into(),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn attempt_documents_preserve_original_outputs_and_exclude_runtime_private_fields() {
    let root = tempfile::tempdir().unwrap();
    let run = root.path();
    fs::create_dir(run.join("outputs")).unwrap();
    fs::write(run.join("outputs/params.json"), b"original parameters").unwrap();
    let state = json!({"task_attempts":{"attempts":[{"attempt":2,"request_id":"extend","status":"running","metric_step_offset":"101","checkpoint":{"path":"outputs/state.bin","pinned_path":"/private/checkpoint","sha256":"digest","size":7},"request_fingerprint":"private","worker_pid":123,"code_dir":"/private/code","source":{"files_sha256":"code-digest"}}]}});
    fs::write(
      run.join("run-state.json"),
      serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    sync_attempt_records(run).unwrap();
    let record: Value =
      serde_json::from_slice(&fs::read(run.join("outputs/attempts/2/attempt-state.json")).unwrap())
        .unwrap();
    assert_eq!(record["request_id"], "extend");
    assert_eq!(record["source"]["files_sha256"], "code-digest");
    assert_eq!(record["checkpoint"]["path"], "outputs/state.bin");
    assert_eq!(record["checkpoint"]["size"], 7);
    for field in ["request_fingerprint", "worker_pid", "code_dir"] {
      assert!(record.get(field).is_none());
    }
    assert!(record["checkpoint"].get("pinned_path").is_none());
    assert_eq!(
      fs::read(run.join("outputs/params.json")).unwrap(),
      b"original parameters"
    );
  }

  #[test]
  #[cfg(unix)]
  fn attempt_documents_refuse_a_redirected_outputs_directory() {
    let run = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), run.path().join("outputs")).unwrap();
    fs::write(
      run.path().join("run-state.json"),
      br#"{"task_attempts":{"attempts":[{"attempt":2}]}}"#,
    )
    .unwrap();
    assert!(sync_attempt_records(run.path()).is_err());
    assert!(!elsewhere.path().join("attempts").exists());
  }
}
