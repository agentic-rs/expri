//! Explicit live supervisor replacement. Shared service restarts do not use it.

#[cfg(not(target_os = "linux"))]
use std::path::Path;

#[cfg(not(target_os = "linux"))]
use expri_core::{
  error::{ExpriError, Result},
  protocol::SupervisorRestartRequest,
};
#[cfg(not(target_os = "linux"))]
use serde_json::Value;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{TaskOutput, task};
#[cfg(target_os = "linux")]
pub use linux::{request_restart, resume_from_fd};

#[cfg(not(target_os = "linux"))]
pub fn request_restart(_: &Path, _: &SupervisorRestartRequest) -> Result<Value> {
  Err(ExpriError::Message("live supervisor restart requires a newly launched native Linux detached run; this run was not changed".into()))
}

#[cfg(not(target_os = "linux"))]
pub fn resume_from_fd(_: i32) -> Result<()> {
  Err(ExpriError::Message(
    "live supervisor restart requires Linux".into(),
  ))
}
