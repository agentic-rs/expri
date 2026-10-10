use http::{Request as HttpRequest, Response as HttpResponse};
use serde::Deserialize;
use serde_json::json;

use super::{BrowserAuth, HostedDashboard, ObjectStorage, response};
use crate::service::store::{ApiError, ApiResult};
use crate::service::types::validate_component;

fn project_selection(request: &HttpRequest<Vec<u8>>) -> ApiResult<String> {
  let query = request.uri().query().unwrap_or_default();
  if query.len() > 384 || !request.body().is_empty() {
    return Err(ApiError::new(400, "invalid project selection"));
  }
  let fields: Vec<_> = form_urlencoded::parse(query.as_bytes()).collect();
  if fields.len() != 1 || fields[0].0 != "project_id" {
    return Err(ApiError::new(400, "project selection requires project_id"));
  }
  validate_component(&fields[0].1).map_err(|_| ApiError::new(400, "invalid project selection"))?;
  Ok(fields[0].1.to_string())
}

pub(super) fn read<S: ObjectStorage>(
  dashboard: &HostedDashboard<'_, S>,
  allow_project_deletion: bool,
  request: &HttpRequest<Vec<u8>>,
) -> ApiResult<Option<HttpResponse<Vec<u8>>>> {
  let path = request.uri().path();
  if !matches!(
    path,
    "/api/storage/stats" | "/api/projects/delete-preview" | "/api/projects/deletion"
  ) {
    return Ok(None);
  }
  let project_id = project_selection(request)?;
  let value = match path {
    "/api/storage/stats" => json!({
      "stats": dashboard.project_storage(&project_id)?,
      "delete_enabled": allow_project_deletion,
    }),
    "/api/projects/delete-preview" => json!(dashboard.project_delete_preview(&project_id)?),
    _ => json!(dashboard.project_deletion(&project_id)?),
  };
  Ok(Some(response(
    200,
    "application/json",
    serde_json::to_vec(&value).map_err(|_| ApiError::new(500, "cannot encode project response"))?,
  )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteSelection {
  project_id: String,
  revision: String,
  confirmation: String,
  password: String,
}

pub(super) fn delete<S: ObjectStorage>(
  dashboard: &HostedDashboard<'_, S>,
  auth: &BrowserAuth,
  allow_project_deletion: bool,
  request: &HttpRequest<Vec<u8>>,
) -> ApiResult<HttpResponse<Vec<u8>>> {
  auth.authorized(request)?;
  auth.check_boundary(request, true)?;
  if !allow_project_deletion {
    return Err(ApiError::new(
      403,
      "project deletion is disabled on this dashboard",
    ));
  }
  if request.uri().query().is_some() {
    return Err(ApiError::new(
      400,
      "project deletion does not accept query parameters",
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
    return Err(ApiError::new(415, "project deletion requires JSON"));
  }
  if request.body().len() > 4096 {
    return Err(ApiError::new(413, "project deletion request exceeds 4 KiB"));
  }
  let selection: DeleteSelection = serde_json::from_slice(request.body())
    .map_err(|_| ApiError::new(400, "invalid project deletion request"))?;
  validate_component(&selection.project_id)
    .map_err(|_| ApiError::new(400, "invalid project selection"))?;
  if selection.confirmation != selection.project_id
    || selection.revision.is_empty()
    || selection.revision.len() > 128
    || !selection
      .revision
      .bytes()
      .all(|byte| byte.is_ascii_graphic())
    || selection.password.len() > 256
  {
    return Err(ApiError::new(400, "invalid project deletion confirmation"));
  }
  auth.reauthenticate(request, selection.password.as_bytes())?;
  let deletion = dashboard.delete_project(
    &selection.project_id,
    &selection.revision,
    &selection.confirmation,
  )?;
  let body = serde_json::to_vec(&deletion)
    .map_err(|_| ApiError::new(500, "cannot encode project deletion"))?;
  Ok(response(202, "application/json", body))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::service::browser_assets::DashboardAssets;
  use crate::service::store::{Store, tests::MockStorage};
  use crate::service::types::{CompletedPart, FileTarget, Request};

  const PASSWORD: &[u8] = b"a-dedicated-dashboard-password";

  fn request(method: &str, path: &str, body: &[u8], cookie: Option<&str>) -> HttpRequest<Vec<u8>> {
    let mut builder = HttpRequest::builder()
      .method(method)
      .uri(path)
      .header("Host", "expri.example.com")
      .header("Origin", "https://expri.example.com")
      .header("Content-Type", "application/json");
    if let Some(cookie) = cookie {
      builder = builder.header("Cookie", cookie);
    }
    builder.body(body.to_vec()).unwrap()
  }

  fn fixture() -> (tempfile::TempDir, Store<MockStorage>, BrowserAuth, String) {
    let temporary = tempfile::tempdir().unwrap();
    let storage = MockStorage::default();
    let store = Store::open(temporary.path(), storage.clone()).unwrap();
    store
      .execute(Request::BeginUpload {
        upload_id: "project-private-input".into(),
        target: FileTarget::Input {
          project_id: "project".into(),
          input_id: "dataset".into(),
        },
        size: 8,
        sha256: "a".repeat(64),
      })
      .unwrap();
    storage.stage("project-private-input", 8);
    store
      .execute(Request::RecordPart {
        upload_id: "project-private-input".into(),
        part: CompletedPart {
          part_number: 1,
          etag: "part".into(),
        },
      })
      .unwrap();
    store
      .execute(Request::CompleteUpload {
        upload_id: "project-private-input".into(),
      })
      .unwrap();
    let auth = BrowserAuth::new("https://expri.example.com", PASSWORD).unwrap();
    let cookie = auth
      .login(&request("POST", "/login", b"", None), PASSWORD)
      .unwrap()
      .split(';')
      .next()
      .unwrap()
      .to_string();
    (temporary, store, auth, cookie)
  }

  fn handle(
    store: &Store<MockStorage>,
    auth: &BrowserAuth,
    enabled: bool,
    request: &HttpRequest<Vec<u8>>,
  ) -> HttpResponse<Vec<u8>> {
    crate::service::browser::handle(
      &HostedDashboard::new(store).unwrap(),
      auth,
      &DashboardAssets::embedded(),
      enabled,
      true,
      request,
    )
  }

  fn payload(revision: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"project_id":"project","revision":revision,"confirmation":"project","password":std::str::from_utf8(PASSWORD).unwrap()})).unwrap()
  }

  #[test]
  fn project_reads_are_scoped_authenticated_and_do_not_reveal_object_keys() {
    let (_temporary, store, auth, cookie) = fixture();
    for path in [
      "/api/storage/stats?project_id=project",
      "/api/projects/delete-preview?project_id=project",
      "/api/projects/deletion?project_id=project",
    ] {
      assert_eq!(
        handle(&store, &auth, false, &request("GET", path, b"", None)).status(),
        401
      );
      let response = handle(
        &store,
        &auth,
        false,
        &request("GET", path, b"", Some(&cookie)),
      );
      assert_eq!(
        response.status(),
        if path.starts_with("/api/projects/deletion?") {
          404
        } else {
          200
        },
        "{path}"
      );
      let body = std::str::from_utf8(response.body()).unwrap();
      assert!(body.contains("project"));
      assert!(!body.contains("objects/") && !body.contains("storage.invalid"));
      let repeated = format!("{path}&project_id=other");
      assert_eq!(
        handle(
          &store,
          &auth,
          false,
          &request("GET", &repeated, b"", Some(&cookie))
        )
        .status(),
        400
      );
      let mut hostile = request("GET", path, b"", Some(&cookie));
      hostile
        .headers_mut()
        .insert("Origin", "https://evil.example".parse().unwrap());
      assert_eq!(handle(&store, &auth, false, &hostile).status(), 403);
    }
    let response = handle(
      &store,
      &auth,
      false,
      &request(
        "GET",
        "/api/storage/stats?project_id=project",
        b"",
        Some(&cookie),
      ),
    );
    let value: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(value["delete_enabled"], false);
    assert_eq!(value["stats"]["project_id"], "project");
  }

  #[test]
  fn project_delete_requires_session_origin_password_exact_name_and_current_preview() {
    let (_temporary, store, auth, cookie) = fixture();
    let preview = store.preview_project_delete("project").unwrap();
    let body = payload(&preview.revision);
    let path = "/api/projects/delete";
    let mut bearer = request("POST", path, &body, None);
    bearer.headers_mut().insert(
      "Authorization",
      "Bearer owner-token-with-at-least-24-characters"
        .parse()
        .unwrap(),
    );
    assert_eq!(handle(&store, &auth, true, &bearer).status(), 401);
    assert_eq!(
      handle(
        &store,
        &auth,
        false,
        &request("POST", path, &body, Some(&cookie))
      )
      .status(),
      403
    );
    let mut absent_origin = request("POST", path, &body, Some(&cookie));
    absent_origin.headers_mut().remove("Origin");
    assert_eq!(handle(&store, &auth, true, &absent_origin).status(), 403);
    let mut hostile = request("POST", path, &body, Some(&cookie));
    hostile
      .headers_mut()
      .insert("Origin", "https://evil.example".parse().unwrap());
    assert_eq!(handle(&store, &auth, true, &hostile).status(), 403);
    let mut value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    value["password"] = json!("wrong-password");
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request(
          "POST",
          path,
          &serde_json::to_vec(&value).unwrap(),
          Some(&cookie)
        )
      )
      .status(),
      403
    );
    value["password"] = json!(std::str::from_utf8(PASSWORD).unwrap());
    value["confirmation"] = json!("other");
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request(
          "POST",
          path,
          &serde_json::to_vec(&value).unwrap(),
          Some(&cookie)
        )
      )
      .status(),
      400
    );
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request("POST", path, &payload("stale"), Some(&cookie))
      )
      .status(),
      409
    );
    let accepted = handle(
      &store,
      &auth,
      true,
      &request("POST", path, &body, Some(&cookie)),
    );
    assert_eq!(accepted.status(), 202);
    assert!(!accepted.headers().contains_key("Set-Cookie"));
    let deletion: serde_json::Value = serde_json::from_slice(accepted.body()).unwrap();
    assert_eq!(deletion["project_id"], "project");
  }

  #[test]
  fn project_delete_json_is_strict_and_bounded_without_echoing_passwords() {
    let (_temporary, store, auth, cookie) = fixture();
    let body = payload(&store.preview_project_delete("project").unwrap().revision);
    let path = "/api/projects/delete";
    for invalid in [
      br#"{"project_id":"project","project_id":"other","revision":"1","confirmation":"project","password":"secret"}"#.to_vec(),
      br#"{"project_id":"project","revision":"1","confirmation":"project","password":"secret","extra":"unsafe"}"#.to_vec(),
      b"not-json-secret".to_vec(),
    ] {
      let response = handle(&store, &auth, true, &request("POST", path, &invalid, Some(&cookie)));
      assert_eq!(response.status(), 400);
      assert!(!std::str::from_utf8(response.body()).unwrap().contains("secret"));
    }
    let mut form = request("POST", path, &body, Some(&cookie));
    form.headers_mut().insert(
      "Content-Type",
      "application/x-www-form-urlencoded".parse().unwrap(),
    );
    assert_eq!(handle(&store, &auth, true, &form).status(), 415);
    let mut duplicate = request("POST", path, &body, Some(&cookie));
    duplicate
      .headers_mut()
      .append("Content-Type", "application/json".parse().unwrap());
    assert_eq!(handle(&store, &auth, true, &duplicate).status(), 415);
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request("POST", path, &vec![b' '; 4097], Some(&cookie))
      )
      .status(),
      413
    );
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request(
          "POST",
          &format!("{path}?project_id=project"),
          &body,
          Some(&cookie)
        )
      )
      .status(),
      400
    );
    assert!(store.project_storage("project").is_ok());
  }
}
