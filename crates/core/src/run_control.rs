//! Worker-acknowledged lifecycle commands for directly launched runs.

use serde::{Deserialize, Serialize};

use crate::resource_requirements::{ResourceRequirements, ResourceStatus};
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
  SetRequirements,
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
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub requirements: Option<ResourceRequirements>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requirements: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resource_status: Option<Box<ResourceStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manual_paused: Option<bool>,
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

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn legacy_control_polls_remain_unchanged_without_requirement_fields() {
    let legacy = json!({
      "action": "poll", "scope": {"project_id":"vision", "origin":"worker", "run_id":"run-1"},
      "binding_id":"binding-1", "attempt":1, "generation":1, "status":"running",
      "capabilities":["pause", "resume", "stop"], "command":["python", "train.py"],
      "checkpoints":[], "result":null,
    });
    let request: RunControlRequest = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), legacy);
    let mut upgraded = legacy.clone();
    upgraded["requirements"] = json!({"disk":[], "recovery_seconds":30});
    upgraded["manual_paused"] = json!(false);
    upgraded["capabilities"] = json!(["set_requirements"]);
    let request: RunControlRequest = serde_json::from_value(upgraded.clone()).unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), upgraded);
    let mut invalid = legacy;
    invalid["manual_paused"] = json!("false");
    assert!(serde_json::from_value::<RunControlRequest>(invalid).is_err());
  }

  #[test]
  fn legacy_commands_do_not_gain_a_requirements_field() {
    let legacy = json!({
      "request_id":"request-1", "binding_id":"binding-1", "expected_attempt":1,
      "expected_generation":1, "action":"pause", "arguments":null, "checkpoint":null,
      "reason":"manual pause", "grace_seconds":30, "expires_at_ms":42,
    });
    let command: ControlCommand = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(command).unwrap(), legacy);
  }
}
