use http::{Request as HttpRequest, Response as HttpResponse};
use serde::{Deserialize, Deserializer};

use super::{BrowserAuth, HostedDashboard, ObjectStorage, response};
use crate::service::store::{ApiError, ApiResult};
use crate::service::types::{RunScope, validate_scope};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunSelection {
  project_id: String,
  origin: String,
  run_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnnotationSelection {
  project_id: String,
  origin: String,
  run_id: String,
  // Unlike a defaulted Option, the replacement must explicitly clear or set it.
  #[serde(deserialize_with = "required_nickname")]
  nickname: Option<String>,
  tags: Vec<String>,
}

fn required_nickname<'de, D: Deserializer<'de>>(
  deserializer: D,
) -> Result<Option<String>, D::Error> {
  Option::<String>::deserialize(deserializer)
}

pub(super) fn mutate<S: ObjectStorage>(
  dashboard: &HostedDashboard<'_, S>,
  auth: &BrowserAuth,
  allow_run_management: bool,
  request: &HttpRequest<Vec<u8>>,
) -> ApiResult<HttpResponse<Vec<u8>>> {
  auth.authorized(request)?;
  auth.check_boundary(request, true)?;
  if !allow_run_management {
    return Err(ApiError::new(
      403,
      "run management is disabled on this dashboard",
    ));
  }
  if request.uri().query().is_some() {
    return Err(ApiError::new(
      400,
      "run management does not accept query parameters",
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
    return Err(ApiError::new(415, "run management requires JSON"));
  }
  let annotations = request.uri().path() == "/api/runs/annotations";
  let body_limit = if annotations { 16 * 1024 } else { 4096 };
  if request.body().len() > body_limit {
    return Err(ApiError::new(
      413,
      if annotations {
        "run annotations request exceeds 16 KiB"
      } else {
        "run management request exceeds 4 KiB"
      },
    ));
  }
  if annotations {
    let selection: AnnotationSelection = serde_json::from_slice(request.body())
      .map_err(|_| ApiError::new(400, "invalid run annotations request"))?;
    let scope = RunScope {
      project_id: selection.project_id,
      origin: selection.origin,
      run_id: selection.run_id,
    };
    validate_scope(&scope).map_err(|_| ApiError::new(400, "invalid run selection"))?;
    let annotations = dashboard.set_run_annotations(&scope, selection.nickname, selection.tags)?;
    let body = serde_json::to_vec(&annotations)
      .map_err(|_| ApiError::new(500, "cannot encode run annotations"))?;
    return Ok(response(200, "application/json", body));
  }
  let selection: RunSelection = serde_json::from_slice(request.body())
    .map_err(|_| ApiError::new(400, "invalid run management request"))?;
  let scope = RunScope {
    project_id: selection.project_id,
    origin: selection.origin,
    run_id: selection.run_id,
  };
  validate_scope(&scope).map_err(|_| ApiError::new(400, "invalid run selection"))?;
  let archival = match request.uri().path() {
    "/api/runs/archive" => dashboard.archive_run(&scope)?,
    "/api/runs/restore" => dashboard.restore_run(&scope)?,
    _ => return Err(ApiError::new(404, "run management endpoint is missing")),
  };
  let body = serde_json::to_vec(&archival)
    .map_err(|_| ApiError::new(500, "cannot encode run archival status"))?;
  Ok(response(200, "application/json", body))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::service::browser_assets::DashboardAssets;
  use crate::service::store::{Store, tests::MockStorage};
  use crate::service::types::Request;
  use base64::Engine;
  use serde_json::{Value, json};

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

  fn scope() -> RunScope {
    RunScope {
      project_id: "vision".into(),
      origin: "gpu-1".into(),
      run_id: "run-1".into(),
    }
  }

  fn fixture() -> (tempfile::TempDir, Store<MockStorage>, BrowserAuth, String) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let bytes = br#"{"status":"completed"}"#;
    store
      .execute(Request::PutDocument {
        scope: scope(),
        path: "run-state.json".into(),
        revision: 1,
        offset: 0,
        total_size: bytes.len() as u64,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
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
    (directory, store, auth, cookie)
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
      false,
      enabled,
      request,
    )
  }

  #[test]
  fn archival_requires_session_same_origin_and_primary_capability() {
    let (_directory, store, auth, cookie) = fixture();
    let body = serde_json::to_vec(&scope()).unwrap();
    for path in [
      "/api/runs/archive",
      "/api/runs/restore",
      "/api/runs/annotations",
    ] {
      assert_eq!(
        handle(&store, &auth, true, &request("POST", path, &body, None)).status(),
        401
      );
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
      for origin in [None, Some("https://outside.example")] {
        let mut write = request("POST", path, &body, Some(&cookie));
        write.headers_mut().remove("Origin");
        if let Some(origin) = origin {
          write
            .headers_mut()
            .insert("Origin", origin.parse().unwrap());
        }
        assert_eq!(handle(&store, &auth, true, &write).status(), 403);
      }
    }
    assert_eq!(store.run_archival(&scope()).unwrap().status, "active");
    let archived = handle(
      &store,
      &auth,
      true,
      &request("POST", "/api/runs/archive", &body, Some(&cookie)),
    );
    assert_eq!(archived.status(), 200);
    let archived: Value = serde_json::from_slice(archived.body()).unwrap();
    assert_eq!(archived["scope"], json!(scope()));
    assert_eq!(archived["status"], "archived");
    assert!(archived["delete_after"].as_str().is_some());
    let restored = handle(
      &store,
      &auth,
      true,
      &request("POST", "/api/runs/restore", &body, Some(&cookie)),
    );
    assert_eq!(restored.status(), 200);
    let restored: Value = serde_json::from_slice(restored.body()).unwrap();
    assert_eq!(restored["status"], "active");
    assert!(restored["delete_after"].is_null());
  }

  fn annotations_body(nickname: Value, tags: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
      "project_id": "vision", "origin": "gpu-1", "run_id": "run-1", "nickname": nickname, "tags": tags,
    }))
    .unwrap()
  }

  #[test]
  fn annotations_replace_and_clear_the_scoped_metadata() {
    let (_directory, store, auth, cookie) = fixture();
    let body = annotations_body(json!("  Baseline  "), json!([" train ", "train", "gold"]));
    let reply = handle(
      &store,
      &auth,
      true,
      &request("POST", "/api/runs/annotations", &body, Some(&cookie)),
    );
    assert_eq!(reply.status(), 200);
    let value: Value = serde_json::from_slice(reply.body()).unwrap();
    assert_eq!(
      value,
      json!({"scope":scope(),"nickname":"Baseline","tags":["train","gold"]})
    );
    assert_eq!(
      store.run_annotations(&scope()).unwrap().nickname.as_deref(),
      Some("Baseline")
    );

    let body = annotations_body(Value::Null, json!([]));
    let reply = handle(
      &store,
      &auth,
      true,
      &request("POST", "/api/runs/annotations", &body, Some(&cookie)),
    );
    assert_eq!(reply.status(), 200);
    assert_eq!(
      serde_json::from_slice::<Value>(reply.body()).unwrap(),
      json!({"scope":scope(),"nickname":null,"tags":[]})
    );
  }

  #[test]
  fn annotations_require_explicit_strict_fields_and_valid_values() {
    let (_directory, store, auth, cookie) = fixture();
    let path = "/api/runs/annotations";
    let mut invalid = vec![
      br#"{"project_id":"vision","origin":"gpu-1","run_id":"run-1","tags":[]}"#.to_vec(),
      br#"{"project_id":"vision","origin":"gpu-1","run_id":"run-1","nickname":null}"#.to_vec(),
      br#"{"project_id":"vision","origin":"gpu-1","run_id":"run-1","nickname":null,"tags":[],"extra":"secret"}"#.to_vec(),
      br#"{"project_id":"vision","origin":"gpu-1","run_id":"run-1","nickname":null,"tags":[],"tags":[]}"#.to_vec(),
      br#"{"project_id":"vision","origin":"gpu-1","run_id":"run-1","nickname":null,"tags":[],"nickname":"secret"}"#.to_vec(),
      br#"{"project_id":"vision","origin":"gpu-1","run_id":"../run-1","nickname":null,"tags":[]}"#.to_vec(),
      b"invalid-json-secret".to_vec(),
    ];
    for (nickname, tags) in [
      (json!(42), json!([])),
      (Value::Null, Value::Null),
      (Value::Null, json!([42])),
      (Value::Null, json!([" "])),
      (Value::Null, json!(["line\nbreak"])),
      (json!("line\nbreak"), json!([])),
      (json!("x".repeat(129)), json!([])),
      (Value::Null, json!(["x".repeat(49)])),
      (Value::Null, json!(vec!["tag"; 17])),
    ] {
      invalid.push(annotations_body(nickname, tags));
    }
    for body in invalid {
      let reply = handle(
        &store,
        &auth,
        true,
        &request("POST", path, &body, Some(&cookie)),
      );
      assert_eq!(
        reply.status(),
        400,
        "body: {}",
        String::from_utf8_lossy(&body)
      );
      assert!(!String::from_utf8_lossy(reply.body()).contains("secret"));
    }
    assert_eq!(
      store.run_annotations(&scope()).unwrap().tags,
      Vec::<String>::new()
    );
  }

  #[test]
  fn annotations_bound_the_wire_body_without_rejecting_escaped_valid_values() {
    let (_directory, store, auth, cookie) = fixture();
    let path = "/api/runs/annotations";
    let tags: Vec<_> = (b'a'..=b'p')
      .map(|suffix| format!("{}{}", "😀".repeat(47), suffix as char))
      .collect();
    let body = String::from_utf8(annotations_body(json!("😀".repeat(128)), json!(tags)))
      .unwrap()
      .replace('😀', "\\ud83d\\ude00")
      .into_bytes();
    assert!(body.len() > 4096 && body.len() <= 16 * 1024);
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request("POST", path, &body, Some(&cookie))
      )
      .status(),
      200
    );
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request("POST", path, &vec![b' '; 16 * 1024 + 1], Some(&cookie))
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
          &format!("{path}?run_id=run-1"),
          &body,
          Some(&cookie)
        )
      )
      .status(),
      400
    );
    for content_type in ["text/plain", "application/json; boundary=unexpected"] {
      let mut wrong_type = request("POST", path, &body, Some(&cookie));
      wrong_type
        .headers_mut()
        .insert("Content-Type", content_type.parse().unwrap());
      assert_eq!(handle(&store, &auth, true, &wrong_type).status(), 415);
    }
    let mut duplicate = request("POST", path, &body, Some(&cookie));
    duplicate
      .headers_mut()
      .append("Content-Type", "application/json".parse().unwrap());
    assert_eq!(handle(&store, &auth, true, &duplicate).status(), 415);
  }

  #[test]
  fn annotations_reject_missing_and_unavailable_runs() {
    let (directory, store, auth, cookie) = fixture();
    let path = "/api/runs/annotations";
    let body = annotations_body(json!("Baseline"), json!(["gold"]));
    let mut missing: Value = serde_json::from_slice(&body).unwrap();
    missing["run_id"] = json!("missing");
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request(
          "POST",
          path,
          &serde_json::to_vec(&missing).unwrap(),
          Some(&cookie)
        )
      )
      .status(),
      404
    );
    store.archive_run(&scope()).unwrap();
    rusqlite::Connection::open(directory.path().join("metadata.sqlite3"))
      .unwrap()
      .execute("UPDATE run_retention SET delete_after=0", [])
      .unwrap();
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request("POST", path, &body, Some(&cookie))
      )
      .status(),
      410
    );
  }

  #[test]
  fn archival_json_selection_is_strict_and_bounded() {
    let (_directory, store, auth, cookie) = fixture();
    let path = "/api/runs/archive";
    for body in [
      br#"{"project_id":"vision","origin":"gpu-1","run_id":"run-1","run_id":"other"}"#.as_slice(),
      br#"{"project_id":"vision","origin":"gpu-1","run_id":"run-1","extra":"secret"}"#.as_slice(),
      br#"{"project_id":"vision","run_id":"run-1"}"#.as_slice(),
      br#"{"project_id":"../vision","origin":"gpu-1","run_id":"run-1"}"#.as_slice(),
      b"invalid-json-secret".as_slice(),
    ] {
      let reply = handle(
        &store,
        &auth,
        true,
        &request("POST", path, body, Some(&cookie)),
      );
      assert_eq!(reply.status(), 400);
      assert!(
        !std::str::from_utf8(reply.body())
          .unwrap()
          .contains("secret")
      );
    }
    let body = serde_json::to_vec(&scope()).unwrap();
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request(
          "POST",
          &format!("{path}?run_id=run-1"),
          &body,
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
        &request("POST", path, &vec![b' '; 4097], Some(&cookie))
      )
      .status(),
      413
    );
    let mut wrong_type = request("POST", path, &body, Some(&cookie));
    wrong_type
      .headers_mut()
      .insert("Content-Type", "text/plain".parse().unwrap());
    assert_eq!(handle(&store, &auth, true, &wrong_type).status(), 415);
    let mut duplicate = request("POST", path, &body, Some(&cookie));
    duplicate
      .headers_mut()
      .append("Content-Type", "application/json".parse().unwrap());
    assert_eq!(handle(&store, &auth, true, &duplicate).status(), 415);
    assert_eq!(store.run_archival(&scope()).unwrap().status, "active");
  }

  #[test]
  fn primary_and_preview_catalogs_report_run_management_capabilities() {
    let (_directory, store, auth, cookie) = fixture();
    for path in ["/api/catalog", "/api/projects"] {
      for enabled in [true, false] {
        let reply = handle(
          &store,
          &auth,
          enabled,
          &request("GET", path, b"", Some(&cookie)),
        );
        assert_eq!(reply.status(), 200);
        let value: Value = serde_json::from_slice(reply.body()).unwrap();
        assert_eq!(value["run_management_enabled"], enabled);
      }
    }
  }
}
