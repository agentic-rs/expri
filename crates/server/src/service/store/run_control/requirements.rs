use super::*;

pub(super) fn validate_facts(
  requirements: Option<&ResourceRequirements>,
  status: Option<&ResourceStatus>,
  capabilities: &[ControlAction],
) -> ApiResult<()> {
  let Some(requirements) = requirements else {
    if status.is_some() || capabilities.contains(&ControlAction::SetRequirements) {
      return Err(ApiError::new(
        400,
        "resource control requires a requirements policy",
      ));
    }
    return Ok(());
  };
  requirements.validate().map_err(bad)?;
  if let Some(status) = status {
    status.validate(requirements).map_err(bad)?;
  }
  Ok(())
}
