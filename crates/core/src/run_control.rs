//! Worker-acknowledged lifecycle commands for directly launched runs.

use serde::{Deserialize, Serialize};

use crate::service::RunScope;

pub const MAX_CONTROL_POLLS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlLifecycle {
  pub started_at: String,
  pub finished_at: Option<String>,
  pub exit_code: Option<i32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
  Pause,
  Resume,
  Stop,
  Continue,
  Restart,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlCommand {
  pub request_id: String,
  pub binding_id: String,
  pub expected_attempt: u64,
  pub expected_generation: u64,
  pub action: ControlAction,
  pub arguments: Option<Vec<String>>,
  pub checkpoint: Option<String>,
  pub reason: String,
  pub grace_seconds: u32,
  pub expires_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlResult {
  pub request_id: String,
  pub status: String,
  pub error: Option<String>,
  pub new_run_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunControlRequest {
  Poll {
    scope: RunScope,
    binding_id: String,
    attempt: u64,
    generation: u64,
    status: String,
    capabilities: Vec<ControlAction>,
    command: Vec<String>,
    checkpoints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lifecycle: Option<ControlLifecycle>,
    result: Option<ControlResult>,
  },
}

impl RunControlRequest {
  pub fn scope(&self) -> &RunScope {
    match self {
      Self::Poll { scope, .. } => scope,
    }
  }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunControlResponse {
  pub command: Option<ControlCommand>,
}

/// Batch entries fail independently after every child scope has been authorized.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlPollResult {
  pub response: Option<RunControlResponse>,
  pub error: Option<String>,
}
