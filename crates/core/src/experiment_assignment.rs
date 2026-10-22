//! Explicit, additive experiment membership requested when creating a run.
use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};
use crate::service::{RunScope, validate_component};

pub const EXPERIMENT_ASSIGNMENT_CAPABILITY: &str = "run-experiment-assignment-v1";

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExperimentSelection {
  Existing {
    experiment_id: String,
  },
  New {
    name: String,
  },
  JoinRun {
    run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
  },
}

impl ExperimentSelection {
  pub fn validate(&self) -> Result<()> {
    match self {
      Self::Existing { experiment_id } => validate_component(experiment_id),
      Self::JoinRun { run_id, origin } => {
        validate_component(run_id)?;
        if let Some(origin) = origin {
          validate_component(origin)?;
        }
        Ok(())
      }
      Self::New { name } => {
        let name = name.trim();
        if name.is_empty()
          || name.len() > 512
          || name.chars().count() > 128
          || name.chars().any(char::is_control)
        {
          return Err(ExpriError::Message("experiment name must contain 1 to 128 characters without control characters (at most 512 bytes)".into()));
        }
        Ok(())
      }
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunExperimentIntent {
  pub request_id: String,
  pub selection: ExperimentSelection,
}

impl RunExperimentIntent {
  pub fn validate(&self) -> Result<()> {
    validate_component(&self.request_id)?;
    self.selection.validate()
  }
}

/// Durable receipt for one assignment. Replaying it never undoes later UI edits.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunExperimentAssignment {
  pub scope: RunScope,
  pub experiment_id: String,
  pub name: String,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn selections_are_strict_and_names_remain_within_dashboard_limits() {
    for name in ["", "  ", "bad\nname", &"x".repeat(129), &"界".repeat(129)] {
      assert!(
        ExperimentSelection::New { name: name.into() }
          .validate()
          .is_err()
      );
    }
    ExperimentSelection::New {
      name: "  Training v2  ".into(),
    }
    .validate()
    .unwrap();
    assert!(
      serde_json::from_str::<ExperimentSelection>(
        r#"{"kind":"new","name":"Valid","project_id":"other"}"#
      )
      .is_err()
    );
    assert!(
      ExperimentSelection::JoinRun {
        run_id: "run-id".into(),
        origin: Some("../other".into())
      }
      .validate()
      .is_err()
    );
    assert!(
      RunExperimentIntent {
        request_id: "".into(),
        selection: ExperimentSelection::Existing {
          experiment_id: "exp-one".into()
        }
      }
      .validate()
      .is_err()
    );
  }
}
