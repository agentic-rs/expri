//! Durable launch-time grouping, independent of training and stream transfer.
use std::path::Path;

use expri_core::experiment_assignment::{
  EXPERIMENT_ASSIGNMENT_CAPABILITY, RunExperimentAssignment, RunExperimentIntent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Api, fs::message, queue::Queue};
use crate::error::{ExpriError, Result};
use crate::types::{Request, Response, RunScope, validate_component, validate_scope};

/// Check known errors before launch. An unavailable service leaves the saved
/// intent pending; the shared worker service will retry after connectivity returns.
pub fn preflight(config: &Path, project_id: &str, intent: &RunExperimentIntent) -> Result<()> {
  validate_component(project_id)?;
  intent.validate()?;
  let api = Api::new(config)?.with_short_control_timeout()?;
  match validate(&api, project_id, intent) {
    Err(ExpriError::ServiceUnavailable { .. }) => Ok(()),
    Err(ExpriError::ServiceRejected { status, .. })
      if status >= 500 || status == 408 || status == 429 =>
    {
      Ok(())
    }
    result => result,
  }
}

/// Queued submission validation belongs to the server transaction, after its
/// idempotent replay check. Only negotiate support before uploading its source.
pub fn check_support(config: &Path) -> Result<()> {
  capability(&Api::new(config)?.with_short_control_timeout()?)
}

fn capability(api: &Api) -> Result<()> {
  let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
    return Err(message(
      "service did not return its experiment capabilities",
    ));
  };
  if !features
    .iter()
    .any(|value| value == EXPERIMENT_ASSIGNMENT_CAPABILITY)
  {
    return Err(message(
      "experiment assignment is unavailable for this service or credential; upgrade the server and use owner publishing credentials from `expri login`",
    ));
  }
  Ok(())
}

fn validate(api: &Api, project_id: &str, intent: &RunExperimentIntent) -> Result<()> {
  capability(api)?;
  match api.request(&Request::ValidateExperimentSelection {
    project_id: project_id.into(),
    intent: intent.clone(),
  })? {
    Response::ExperimentSelectionValidated => Ok(()),
    _ => Err(message("service did not validate the experiment selection")),
  }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Acknowledgement {
  intent: RunExperimentIntent,
  assignment: RunExperimentAssignment,
}

pub(super) fn publish(api: &Api, queue: &mut Queue, scope: &RunScope, state: &Value) -> Result<()> {
  let Some(value) = state
    .get("experiment_intent")
    .filter(|value| !value.is_null())
  else {
    return Ok(());
  };
  let intent: RunExperimentIntent = serde_json::from_value(value.clone())?;
  intent.validate()?;
  if let Some(saved) = &queue.state.experiment_assignment {
    if saved.intent != intent || &saved.assignment.scope != scope {
      return Err(message(
        "saved experiment assignment belongs to another launch intent",
      ));
    }
    return Ok(());
  }
  let result = (|| {
    capability(api)?;
    let Response::RunExperimentAssigned { assignment } =
      api.request(&Request::AssignRunExperiment {
        scope: scope.clone(),
        intent: intent.clone(),
      })?
    else {
      return Err(message("service did not return an experiment assignment"));
    };
    validate_scope(&assignment.scope)?;
    validate_component(&assignment.experiment_id)?;
    if &assignment.scope != scope {
      return Err(message("service returned an assignment for another run"));
    }
    queue.state.experiment_assignment = Some(Acknowledgement { intent, assignment });
    queue.save()
  })();
  // A grouping rejection must not permanently disable this run's metrics
  // publisher. Subsequent slices keep uploading progress before retrying it.
  result.map_err(|error| message(format!("Experiment assignment is pending: {error}; inspect the selected experiment and publisher credentials")))
}

#[cfg(test)]
mod tests;
