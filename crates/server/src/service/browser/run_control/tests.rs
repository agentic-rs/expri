use super::*;
use crate::service::browser_assets::DashboardAssets;
use crate::service::store::{Store, tests::MockStorage};
use expri_core::run_control::{ControlAction, RunControlRequest};
use serde_json::{Value, json};

const PASSWORD: &[u8] = b"a-dedicated-dashboard-password";
const PATH: &str = "/api/runs/control";

fn request(method: &str, path: &str, cookie: Option<&str>, body: Vec<u8>) -> HttpRequest<Vec<u8>> {
  let mut builder = HttpRequest::builder()
    .method(method)
    .uri(path)
    .header("Host", "expri.example.com")
    .header("Origin", "https://expri.example.com")
    .header("Content-Type", "application/json");
  if let Some(cookie) = cookie {
    builder = builder.header("Cookie", cookie);
  }
  builder.body(body).unwrap()
}

fn fixture() -> (tempfile::TempDir, Store<MockStorage>, BrowserAuth, String) {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  store
    .execute(crate::service::types::Request::RunControl {
      request: RunControlRequest::Poll {
        scope: RunScope {
          project_id: "project".into(),
          origin: "worker".into(),
          run_id: "run-1".into(),
        },
        binding_id: "binding-1".into(),
        attempt: 1,
        generation: 1,
        status: "running".into(),
        capabilities: vec![ControlAction::Pause],
        command: vec!["python".into(), "train.py".into()],
        checkpoints: vec![],
        lifecycle: None,
        requirements: None,
        resource_status: None,
        manual_paused: None,
        result: None,
      },
    })
    .unwrap();
  let auth = BrowserAuth::new("https://expri.example.com", PASSWORD).unwrap();
  let cookie = auth
    .login(&request("POST", "/login", None, vec![]), PASSWORD)
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

fn command() -> Vec<u8> {
  serde_json::to_vec(
    &json!({"project_id":"project","origin":"worker","run_id":"run-1",
    "request_id":"request-1","binding_id":"binding-1","expected_attempt":1,
    "expected_generation":1,"action":"pause","arguments":null,"checkpoint":null,
    "reason":"Requested from dashboard","grace_seconds":30}),
  )
  .unwrap()
}

#[test]
fn lifecycle_mutations_require_session_exact_origin_and_primary_dashboard() {
  let (_directory, store, auth, cookie) = fixture();
  assert_eq!(
    handle(&store, &auth, true, &request("POST", PATH, None, command())).status(),
    401
  );
  assert_eq!(
    handle(
      &store,
      &auth,
      false,
      &request("POST", PATH, Some(&cookie), command())
    )
    .status(),
    403
  );
  let mut hostile = request("POST", PATH, Some(&cookie), command());
  hostile
    .headers_mut()
    .insert("Origin", "https://other.example.com".parse().unwrap());
  assert_eq!(handle(&store, &auth, true, &hostile).status(), 403);
  let reply = handle(
    &store,
    &auth,
    true,
    &request("POST", PATH, Some(&cookie), command()),
  );
  assert_eq!(reply.status(), 200);
  let value: Value = serde_json::from_slice(reply.body()).unwrap();
  assert_eq!(value["status"], "running");
  assert_eq!(value["request"]["status"], "requested");
}

#[test]
fn lifecycle_reads_require_session_but_are_available_on_preview() {
  let (_directory, store, auth, cookie) = fixture();
  let path = format!("{PATH}?project_id=project&origin=worker&run_id=run-1");
  assert_eq!(
    handle(&store, &auth, false, &request("GET", &path, None, vec![])).status(),
    401
  );
  let reply = handle(
    &store,
    &auth,
    false,
    &request("GET", &path, Some(&cookie), vec![]),
  );
  assert_eq!(reply.status(), 200);
  let value: Value = serde_json::from_slice(reply.body()).unwrap();
  assert_eq!(value["supported"], true);
  assert_eq!(value["online"], true);
  assert_eq!(value["request"], Value::Null);
  for query in [
    "project_id=project&origin=worker",
    "project_id=project&origin=worker&run_id=run-1&run_id=run-2",
  ] {
    assert_eq!(
      handle(
        &store,
        &auth,
        false,
        &request("GET", &format!("{PATH}?{query}"), Some(&cookie), vec![])
      )
      .status(),
      400
    );
  }
}

#[test]
fn lifecycle_request_limits_reject_extra_fields_and_server_assigned_expiry() {
  let (_directory, store, auth, cookie) = fixture();
  let mut value: Value = serde_json::from_slice(&command()).unwrap();
  value["expires_at_ms"] = json!(i64::MAX);
  assert_eq!(
    handle(
      &store,
      &auth,
      true,
      &request(
        "POST",
        PATH,
        Some(&cookie),
        serde_json::to_vec(&value).unwrap()
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
      &request("POST", PATH, Some(&cookie), vec![b' '; 96 * 1024 + 1])
    )
    .status(),
    413
  );
  let mut duplicate_type = request("POST", PATH, Some(&cookie), command());
  duplicate_type
    .headers_mut()
    .append("content-type", "application/json".parse().unwrap());
  assert_eq!(handle(&store, &auth, true, &duplicate_type).status(), 415);
}
