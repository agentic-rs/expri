use http::{Request as HttpRequest, Response as HttpResponse};

use super::{BrowserAuth, HostedDashboard, ObjectStorage, response};
use crate::service::store::run_control::ControlSubmission;
use crate::service::store::{ApiError, ApiResult};
use crate::service::types::{RunScope, validate_scope};

pub(super) fn read<S: ObjectStorage>(
  dashboard: &HostedDashboard<'_, S>,
  request: &HttpRequest<Vec<u8>>,
) -> ApiResult<HttpResponse<Vec<u8>>> {
  let query = request.uri().query().unwrap_or_default();
  if query.len() > 1024 {
    return Err(ApiError::new(400, "invalid run selection"));
  }
  let mut fields = std::collections::BTreeMap::new();
  for (name, value) in form_urlencoded::parse(query.as_bytes()) {
    if !matches!(name.as_ref(), "project_id" | "origin" | "run_id")
      || fields
        .insert(name.into_owned(), value.into_owned())
        .is_some()
    {
      return Err(ApiError::new(400, "invalid run selection"));
    }
  }
  if fields.len() != 3 {
    return Err(ApiError::new(
      400,
      "run control requires project_id, origin, and run_id",
    ));
  }
  let scope = RunScope {
    project_id: fields.remove("project_id").unwrap(),
    origin: fields.remove("origin").unwrap(),
    run_id: fields.remove("run_id").unwrap(),
  };
  validate_scope(&scope).map_err(|_| ApiError::new(400, "invalid run selection"))?;
  let body = serde_json::to_vec(&dashboard.run_control(&scope)?)
    .map_err(|_| ApiError::new(500, "cannot encode lifecycle state"))?;
  Ok(response(200, "application/json", body))
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
      "run lifecycle is disabled on this dashboard",
    ));
  }
  if request.uri().query().is_some() {
    return Err(ApiError::new(
      400,
      "run lifecycle does not accept query parameters",
    ));
  }
  let mut types = request.headers().get_all("content-type").iter();
  if !types
    .next()
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| {
      let mut parts = value.split(';');
      parts
        .next()
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        && parts.all(|parameter| parameter.trim().eq_ignore_ascii_case("charset=utf-8"))
    })
    || types.next().is_some()
  {
    return Err(ApiError::new(415, "run lifecycle requires JSON"));
  }
  if request.body().len() > 48 * 1024 {
    return Err(ApiError::new(413, "run lifecycle request exceeds 48 KiB"));
  }
  let selection: ControlSubmission = serde_json::from_slice(request.body())
    .map_err(|_| ApiError::new(400, "invalid run lifecycle request"))?;
  let body = serde_json::to_vec(&dashboard.submit_run_control(selection)?)
    .map_err(|_| ApiError::new(500, "cannot encode lifecycle state"))?;
  Ok(response(200, "application/json", body))
}

#[cfg(test)]
mod tests;
