use super::*;
use crate::service::browser_assets::DashboardAssets;
use crate::service::store::{Store, tests::MockStorage};
use serde_json::{Value, json};

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
fn experiment_routes_require_session_same_origin_and_primary_writes() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let auth = BrowserAuth::new("https://expri.example.com", b"dashboard-password").unwrap();
  let cookie = auth
    .login(&request("POST", "/login", b"", None), b"dashboard-password")
    .unwrap();
  let cookie = cookie.split(';').next().unwrap();
  let body = serde_json::to_vec(&json!({"project_id":"project","experiment_id":null,"expected_revision":null,"create_request_id":"create-one","name":"Experiment","description":"","members":[],"lineage":[]})).unwrap();
  assert_eq!(
    handle(
      &store,
      &auth,
      true,
      &request("GET", "/api/experiments?project_id=project", b"", None)
    )
    .status(),
    401
  );
  for path in ["/api/experiments/save", "/api/experiments/delete"] {
    assert_eq!(
      handle(&store, &auth, true, &request("POST", path, &body, None)).status(),
      401
    );
    assert_eq!(
      handle(
        &store,
        &auth,
        false,
        &request("POST", path, &body, Some(cookie))
      )
      .status(),
      403
    );
    let mut foreign = request("POST", path, &body, Some(cookie));
    foreign
      .headers_mut()
      .insert("Origin", "https://outside.example".parse().unwrap());
    assert_eq!(handle(&store, &auth, true, &foreign).status(), 403);
    foreign.headers_mut().remove("Origin");
    assert_eq!(handle(&store, &auth, true, &foreign).status(), 403);
    let mut wrong_type = request("POST", path, &body, Some(cookie));
    wrong_type
      .headers_mut()
      .insert("Content-Type", "text/plain".parse().unwrap());
    assert_eq!(handle(&store, &auth, true, &wrong_type).status(), 415);
  }
  let saved = handle(
    &store,
    &auth,
    true,
    &request("POST", "/api/experiments/save", &body, Some(cookie)),
  );
  assert_eq!(saved.status(), 200);
  let experiment: Value = serde_json::from_slice(saved.body()).unwrap();
  let readonly = handle(
    &store,
    &auth,
    false,
    &request(
      "GET",
      "/api/experiments?project_id=project",
      b"",
      Some(cookie),
    ),
  );
  assert_eq!(readonly.status(), 200);
  assert_eq!(
    serde_json::from_slice::<Value>(readonly.body()).unwrap()["experiments"][0],
    experiment
  );
  let catalog = handle(
    &store,
    &auth,
    false,
    &request("GET", "/api/projects", b"", Some(cookie)),
  );
  let catalog: Value = serde_json::from_slice(catalog.body()).unwrap();
  assert_eq!(catalog["experiments_supported"], true);
  assert_eq!(catalog["experiment_management_enabled"], false);
  let delete = serde_json::to_vec(&json!({"project_id":"project","experiment_id":experiment["experiment_id"],"expected_revision":experiment["revision"]})).unwrap();
  assert_eq!(
    handle(
      &store,
      &auth,
      true,
      &request("POST", "/api/experiments/delete", &delete, Some(cookie))
    )
    .status(),
    200
  );
}

#[test]
fn selections_and_mutation_envelopes_are_strict_and_bounded() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let auth = BrowserAuth::new("https://expri.example.com", b"dashboard-password").unwrap();
  let cookie = auth
    .login(&request("POST", "/login", b"", None), b"dashboard-password")
    .unwrap();
  let cookie = cookie.split(';').next().unwrap();
  for path in [
    "/api/experiments",
    "/api/experiments?project_id=project&project_id=other",
    "/api/experiments?project_id=../outside",
    "/api/experiments?project_id=project&extra=x",
  ] {
    assert_eq!(
      handle(
        &store,
        &auth,
        true,
        &request("GET", path, b"", Some(cookie))
      )
      .status(),
      400
    );
  }
  assert_eq!(
    handle(
      &store,
      &auth,
      true,
      &request(
        "POST",
        "/api/experiments/save?project_id=project",
        b"{}",
        Some(cookie)
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
      &request(
        "POST",
        "/api/experiments/save",
        &vec![b' '; 1024 * 1024 + 1],
        Some(cookie)
      )
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
        "/api/experiments/delete",
        &vec![b' '; 4097],
        Some(cookie)
      )
    )
    .status(),
    413
  );
}

#[test]
fn large_valid_groups_remain_editable_and_project_budget_failures_roll_back_creation() {
  use crate::service::types::{Request, RunScope};
  use base64::Engine;

  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let auth = BrowserAuth::new("https://expri.example.com", b"dashboard-password").unwrap();
  let cookie = auth
    .login(&request("POST", "/login", b"", None), b"dashboard-password")
    .unwrap();
  let cookie = cookie.split(';').next().unwrap();
  let members = |origin: &str| {
    (0..24)
      .map(|n| RunScope {
        project_id: "project".into(),
        origin: origin.into(),
        run_id: format!("run-{n}"),
      })
      .collect::<Vec<_>>()
  };
  let first_members = members("worker-a");
  let second_members = members("worker-b");
  for scope in first_members.iter().chain(&second_members) {
    let bytes = br#"{"status":"completed"}"#;
    store
      .execute(Request::PutDocument {
        scope: scope.clone(),
        path: "run-state.json".into(),
        revision: 1,
        offset: 0,
        total_size: bytes.len() as u64,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
      })
      .unwrap();
  }
  let lineage = |members: &[RunScope], count: usize| {
    let checkpoint = "🧪".repeat(1024);
    let mut edges = Vec::new();
    for (parent_index, parent) in members.iter().enumerate() {
      for child in members.iter().skip(parent_index + 1) {
        edges.push(json!({"parent":parent,"child":child,"kind":"split","checkpoint":checkpoint,"step":"18446744073709551615"}));
      }
    }
    edges.truncate(count);
    edges
  };
  let mut first = json!({"project_id":"project","experiment_id":null,"expected_revision":null,"create_request_id":"large-first","name":"Large group","description":"","members":first_members,"lineage":lineage(&first_members,190)});
  let body = serde_json::to_vec(&first).unwrap();
  assert!(body.len() > 512 * 1024 && body.len() < 1024 * 1024);
  let saved = handle(
    &store,
    &auth,
    true,
    &request("POST", "/api/experiments/save", &body, Some(cookie)),
  );
  assert_eq!(
    saved.status(),
    200,
    "{}",
    String::from_utf8_lossy(saved.body())
  );
  let saved: Value = serde_json::from_slice(saved.body()).unwrap();
  first["experiment_id"] = saved["experiment_id"].clone();
  first["expected_revision"] = saved["revision"].clone();
  first["create_request_id"] = Value::Null;
  first["name"] = json!("Edited large group");
  let edited = handle(
    &store,
    &auth,
    true,
    &request(
      "POST",
      "/api/experiments/save",
      &serde_json::to_vec(&first).unwrap(),
      Some(cookie),
    ),
  );
  assert_eq!(
    edited.status(),
    200,
    "{}",
    String::from_utf8_lossy(edited.body())
  );
  let before = store.experiments("project").unwrap();
  let project_revision = store.preview_project_delete("project").unwrap().revision;
  let mut second = json!({"project_id":"project","experiment_id":null,"expected_revision":null,"create_request_id":"large-second","name":"Second group","description":"","members":second_members,"lineage":lineage(&second_members,80)});
  let body = serde_json::to_vec(&second).unwrap();
  assert!(body.len() < 1024 * 1024);
  let rejected = handle(
    &store,
    &auth,
    true,
    &request("POST", "/api/experiments/save", &body, Some(cookie)),
  );
  assert_eq!(
    rejected.status(),
    409,
    "{}",
    String::from_utf8_lossy(rejected.body())
  );
  assert_eq!(store.experiments("project").unwrap(), before);
  assert_eq!(
    store.preview_project_delete("project").unwrap().revision,
    project_revision
  );
  // A failed transaction neither assigns its runs nor reserves its request ID.
  second["lineage"] = json!([]);
  let retry = handle(
    &store,
    &auth,
    true,
    &request(
      "POST",
      "/api/experiments/save",
      &serde_json::to_vec(&second).unwrap(),
      Some(cookie),
    ),
  );
  assert_eq!(
    retry.status(),
    200,
    "{}",
    String::from_utf8_lossy(retry.body())
  );
  assert_eq!(
    store.experiments("project").unwrap()["experiments"]
      .as_array()
      .unwrap()
      .len(),
    2
  );
}
