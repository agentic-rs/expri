use http::{Request as HttpRequest, Response as HttpResponse};
use serde::Deserialize;

use super::{BrowserAuth, HostedDashboard, ObjectStorage, response};
use crate::service::store::{ApiError, ApiResult, experiments::ExperimentSave};
use crate::service::types::validate_component;

pub(super) fn read<S: ObjectStorage>(
  dashboard: &HostedDashboard<'_, S>,
  request: &HttpRequest<Vec<u8>>,
) -> ApiResult<HttpResponse<Vec<u8>>> {
  let query = request.uri().query().unwrap_or_default();
  if query.len() > 384 {
    return Err(ApiError::new(400, "invalid experiment project selection"));
  }
  let fields: Vec<_> = form_urlencoded::parse(query.as_bytes()).collect();
  if fields.len() != 1 || fields[0].0 != "project_id" {
    return Err(ApiError::new(400, "experiments require project_id"));
  }
  validate_component(&fields[0].1)
    .map_err(|_| ApiError::new(400, "invalid experiment project selection"))?;
  encode(&dashboard.experiments(&fields[0].1)?)
}

fn encode(value: &impl serde::Serialize) -> ApiResult<HttpResponse<Vec<u8>>> {
  Ok(response(
    200,
    "application/json",
    serde_json::to_vec(value)
      .map_err(|_| ApiError::new(500, "cannot encode experiment response"))?,
  ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteSelection {
  project_id: String,
  experiment_id: String,
  expected_revision: String,
}

pub(super) fn mutate<S: ObjectStorage>(
  dashboard: &HostedDashboard<'_, S>,
  auth: &BrowserAuth,
  enabled: bool,
  request: &HttpRequest<Vec<u8>>,
) -> ApiResult<HttpResponse<Vec<u8>>> {
  auth.authorized(request)?;
  auth.check_boundary(request, true)?;
  if !enabled {
    return Err(ApiError::new(
      403,
      "experiment management is disabled on this dashboard",
    ));
  }
  if request.uri().query().is_some() {
    return Err(ApiError::new(
      400,
      "experiment changes do not accept query parameters",
    ));
  }
  let mut content_types = request.headers().get_all("content-type").iter();
  if !content_types
    .next()
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| {
      let mut parts = value.split(';');
      parts
        .next()
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        && parts.all(|parameter| parameter.trim().eq_ignore_ascii_case("charset=utf-8"))
    })
    || content_types.next().is_some()
  {
    return Err(ApiError::new(415, "experiment changes require JSON"));
  }
  let limit = if request.uri().path() == "/api/experiments/save" {
    1024 * 1024
  } else {
    4096
  };
  if request.body().len() > limit {
    return Err(ApiError::new(
      413,
      "experiment request exceeds its size limit",
    ));
  }
  if request.uri().path() == "/api/experiments/save" {
    let selection: ExperimentSave = serde_json::from_slice(request.body())
      .map_err(|_| ApiError::new(400, "invalid experiment save request"))?;
    encode(&dashboard.save_experiment(selection)?)
  } else {
    let selection: DeleteSelection = serde_json::from_slice(request.body())
      .map_err(|_| ApiError::new(400, "invalid experiment delete request"))?;
    encode(&dashboard.delete_experiment(
      &selection.project_id,
      &selection.experiment_id,
      &selection.expected_revision,
    )?)
  }
}

#[cfg(test)]
mod tests;
