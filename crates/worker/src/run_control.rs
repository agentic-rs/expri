//! Durable website lifecycle requests executed by the run's owning supervisor.
//! Exact request results are retained with the run for its lifetime.

#[cfg(not(target_os = "linux"))]
use expri_core::error::{ExpriError, Result};
#[cfg(not(target_os = "linux"))]
use expri_core::run_control::{ControlCommand, ControlResult};
#[cfg(not(target_os = "linux"))]
use serde_json::Value;
#[cfg(not(target_os = "linux"))]
use std::path::Path;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub use linux::{execute, inspect};

#[cfg(not(target_os = "linux"))]
pub fn inspect(run_dir: &Path) -> Result<Value> {
  let state: Value = serde_json::from_slice(&std::fs::read(run_dir.join("run-state.json"))?)?;
  Ok(
    serde_json::json!({"supported":false,"protocol_version":0,"attempt":state["task_attempts"]["current_attempt"].as_u64().unwrap_or(1),"generation":state["supervisor"]["generation"].as_u64().unwrap_or(0),"status":state["status"],"capabilities":[],"command":state["command"],"checkpoints":[]}),
  )
}

#[cfg(not(target_os = "linux"))]
pub fn execute(_: &Path, _: &ControlCommand) -> Result<ControlResult> {
  Err(ExpriError::Message(
    "website lifecycle controls require a native Linux detached run".into(),
  ))
}

#[cfg(target_os = "linux")]
pub(crate) fn recorded_source(
  run_dir: &std::path::Path,
  state: &serde_json::Value,
) -> expri_core::error::Result<(std::path::PathBuf, std::path::PathBuf)> {
  use expri_core::error::ExpriError;
  let current = state["task_attempts"]["current_attempt"]
    .as_u64()
    .unwrap_or(1);
  let parent = if current == 1 {
    run_dir.to_owned()
  } else {
    run_dir.join("attempts").join(current.to_string())
  };
  let code = parent.join("code");
  if state
    .get("current_code_dir")
    .is_some_and(|value| value.as_str() != code.to_str())
  {
    return Err(ExpriError::Message(
      "recorded attempt source identity changed".into(),
    ));
  }
  let mut cursor = run_dir.to_owned();
  for component in parent
    .strip_prefix(run_dir)
    .map_err(|_| ExpriError::Message("invalid recorded source".into()))?
    .components()
  {
    cursor.push(component);
    let metadata = std::fs::symlink_metadata(&cursor)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
      return Err(ExpriError::Message(
        "recorded source parents must be real directories".into(),
      ));
    }
  }
  Ok((code, parent.join("snapshot.json")))
}
