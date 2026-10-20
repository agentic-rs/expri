use expri_core::resource_requirements::{ResourceRequirements, ResourceStatus};
use serde_json::{Value, json};

/// Project only the bounded, public policy and observations from untrusted state.
pub fn resource_metadata(state: &Value) -> (Value, Vec<&'static str>) {
  let mut output = json!({});
  let mut invalid = Vec::new();
  let requirements = state
    .get("requirements")
    .filter(|value| !value.is_null())
    .and_then(
      |value| match serde_json::from_value::<ResourceRequirements>(value.clone()) {
        Ok(requirements) if requirements.validate().is_ok() => {
          output["requirements"] = json!(requirements);
          Some(requirements)
        }
        _ => {
          invalid.push("requirements");
          None
        }
      },
    );
  if let Some(value) = state
    .get("resource_status")
    .filter(|value| !value.is_null())
  {
    match serde_json::from_value::<ResourceStatus>(value.clone()) {
      Ok(status)
        if requirements
          .as_ref()
          .is_some_and(|policy| status.validate(policy).is_ok()) =>
      {
        output["resource_status"] = json!(status);
      }
      _ => invalid.push("resource_status"),
    }
  }
  if let Some(value) = state.get("manual_paused") {
    if let Some(paused) = value.as_bool() {
      output["manual_paused"] = json!(paused);
    } else {
      invalid.push("manual_paused");
    }
  }
  (output, invalid)
}
