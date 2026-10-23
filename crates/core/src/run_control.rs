//! Worker-acknowledged lifecycle commands for directly launched runs.

use serde::{Deserialize, Serialize};

use crate::experiment_assignment::ExperimentSelection;
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

/// State eligibility shared by worker reporting and server presentation. Runtime
/// support (leases, platform, checkpoints) is intersected by the worker.
pub fn available_actions(
  status: &str,
  manual_paused: Option<bool>,
  resource_blocked: bool,
) -> Vec<ControlAction> {
  use crate::run_state::RunPhase;
  use ControlAction::*;
  let mut actions = match RunPhase::parse(status) {
    Some(RunPhase::Running | RunPhase::Waiting) => vec![Pause, Stop, Continue, Restart],
    Some(RunPhase::Paused) => {
      let mut actions = Vec::new();
      if manual_paused == Some(false) && resource_blocked {
        actions.push(Pause);
      }
      actions.extend([Resume, Stop, Continue, Restart]);
      actions
    }
    Some(RunPhase::Pending | RunPhase::Preparing) => vec![Stop],
    Some(RunPhase::Completed | RunPhase::Failed | RunPhase::Cancelled | RunPhase::Lost) => {
      vec![Continue, Restart]
    }
    _ => Vec::new(),
  };
  actions.push(SetRequirements);
  actions
}

/// Validated internal command payload. The flattened wire envelope remains
/// compatible with existing workers; impossible field combinations stop here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlPayload {
  Pause,
  Resume,
  Stop,
  Continue {
    checkpoint: String,
    arguments: Option<Vec<String>>,
  },
  Restart {
    arguments: Option<Vec<String>>,
    experiment: Option<ExperimentSelection>,
  },
  SetRequirements(ResourceRequirements),
}

impl ControlPayload {
  pub fn from_fields(
    action: ControlAction,
    arguments: Option<Vec<String>>,
    checkpoint: Option<String>,
    requirements: Option<ResourceRequirements>,
    experiment: Option<ExperimentSelection>,
  ) -> crate::error::Result<Self> {
    use crate::error::ExpriError;
    let invalid = |text: &str| ExpriError::Message(text.into());
    if !matches!(action, ControlAction::Continue | ControlAction::Restart)
      && (arguments.is_some() || checkpoint.is_some())
    {
      return Err(invalid("only continue or restart accepts task arguments"));
    }
    if action != ControlAction::Restart && experiment.is_some() {
      return Err(invalid("only restart accepts an experiment selection"));
    }
    if action != ControlAction::SetRequirements && requirements.is_some() {
      return Err(invalid("only set_requirements accepts requirements"));
    }
    if let Some(selection) = &experiment {
      selection.validate()?;
    }
    Ok(match action {
      ControlAction::Pause => Self::Pause,
      ControlAction::Resume => Self::Resume,
      ControlAction::Stop => Self::Stop,
      ControlAction::Continue => Self::Continue {
        checkpoint: checkpoint.ok_or_else(|| invalid("continue requires a recorded checkpoint"))?,
        arguments,
      },
      ControlAction::Restart => {
        if checkpoint.is_some() {
          return Err(invalid("restart does not accept a checkpoint"));
        }
        Self::Restart {
          arguments,
          experiment,
        }
      }
      ControlAction::SetRequirements => {
        let policy = requirements
          .ok_or_else(|| invalid("set_requirements requires a disk requirements policy"))?;
        policy.validate()?;
        Self::SetRequirements(policy)
      }
    })
  }
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
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub experiment: Option<ExperimentSelection>,
  pub reason: String,
  pub grace_seconds: u32,
  pub expires_at_ms: i64,
}

impl ControlCommand {
  pub fn payload(&self) -> crate::error::Result<ControlPayload> {
    ControlPayload::from_fields(
      self.action,
      self.arguments.clone(),
      self.checkpoint.clone(),
      self.requirements.clone(),
      self.experiment.clone(),
    )
  }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    experiment_assignment_supported: Option<bool>,
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
  fn typed_payloads_reject_cross_action_fields_before_execution() {
    for action in [
      ControlAction::Pause,
      ControlAction::Resume,
      ControlAction::Stop,
    ] {
      assert!(ControlPayload::from_fields(action, None, None, None, None).is_ok());
      assert!(ControlPayload::from_fields(action, Some(vec![]), None, None, None).is_err());
      assert!(
        ControlPayload::from_fields(action, None, Some("checkpoint.pt".into()), None, None)
          .is_err()
      );
      assert!(
        ControlPayload::from_fields(
          action,
          None,
          None,
          Some(ResourceRequirements::default()),
          None
        )
        .is_err()
      );
    }
    assert!(ControlPayload::from_fields(ControlAction::Continue, None, None, None, None).is_err());
    assert!(matches!(
      ControlPayload::from_fields(
        ControlAction::Continue,
        Some(vec!["--step".into(), "100".into()]),
        Some("checkpoint.pt".into()),
        None,
        None
      )
      .unwrap(),
      ControlPayload::Continue { .. }
    ));
    assert!(
      ControlPayload::from_fields(
        ControlAction::Restart,
        None,
        Some("checkpoint.pt".into()),
        None,
        None
      )
      .is_err()
    );
    assert!(
      ControlPayload::from_fields(ControlAction::SetRequirements, None, None, None, None).is_err()
    );
    assert!(matches!(
      ControlPayload::from_fields(
        ControlAction::SetRequirements,
        None,
        None,
        Some(ResourceRequirements::default()),
        None
      )
      .unwrap(),
      ControlPayload::SetRequirements(_)
    ));
  }

  #[test]
  fn action_policy_distinguishes_task_waiting_and_composable_holds() {
    use ControlAction::*;
    assert_eq!(
      available_actions("waiting", Some(false), false),
      vec![Pause, Stop, Continue, Restart, SetRequirements]
    );
    assert_eq!(
      available_actions("paused", Some(false), true),
      vec![Pause, Resume, Stop, Continue, Restart, SetRequirements]
    );
    assert_eq!(
      available_actions("paused", Some(true), true),
      vec![Resume, Stop, Continue, Restart, SetRequirements]
    );
    assert_eq!(
      available_actions("pending", Some(false), true),
      vec![Stop, SetRequirements]
    );
    assert_eq!(
      available_actions("completed", None, false),
      vec![Continue, Restart, SetRequirements]
    );
  }

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
