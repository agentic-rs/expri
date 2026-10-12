use std::io::Write;

use super::*;
use crate::service::store::tests::MockStorage;

const MAIN: &str = "expri.example.test";
const PREVIEW: &str = "preview.example.test";
const PASSWORD: &[u8] = b"dedicated-dashboard-test-password";
const OWNER: &str = "owner-token-with-at-least-24-characters";
const WORKER: &str = "worker-token-with-at-least-24-characters";
const MACHINE: &str = "machine-token-with-at-least-24-characters";

fn authentication(password: &[u8]) -> Auth {
  Auth {
    owner: OWNER.as_bytes().to_vec(),
    workers: vec![(
      WORKER.as_bytes().to_vec(),
      WorkerAuth {
        project_id: "project".into(),
        origin: "worker".into(),
        token_env: "unused".into(),
      },
    )],
    machines: vec![(
      MACHINE.as_bytes().to_vec(),
      MachineAuth {
        machine_id: "machine-test".into(),
        token_env: "unused".into(),
      },
    )],
    sites: [(MAIN, false), (PREVIEW, true)]
      .into_iter()
      .map(|(host, preview)| DashboardSite {
        auth: BrowserAuth::new(&format!("https://{host}"), password).unwrap(),
        assets: DashboardAssets::embedded(),
        preview,
        allow_project_deletion: false,
      })
      .collect(),
    login_limiter: LoginLimiter::default(),
  }
}

fn login_request(password: &[u8]) -> HttpRequest<Vec<u8>> {
  HttpRequest::builder()
    .method("POST")
    .uri("/v1/login")
    .header("Host", MAIN)
    .header("Content-Type", "application/json")
    .body(
      serde_json::to_vec(&LoginRequest {
        password: String::from_utf8(password.to_vec()).unwrap(),
      })
      .unwrap(),
    )
    .unwrap()
}

fn api_request(token: &str, operation: &Request) -> HttpRequest<Vec<u8>> {
  HttpRequest::builder()
    .method("POST")
    .uri("/v1/request")
    .header("Host", MAIN)
    .header("Content-Type", "application/json")
    .header("Authorization", format!("Bearer {token}"))
    .body(serde_json::to_vec(operation).unwrap())
    .unwrap()
}

fn logout_request(token: &str) -> HttpRequest<Vec<u8>> {
  HttpRequest::builder()
    .method("POST")
    .uri("/v1/logout")
    .header("Host", MAIN)
    .header("Authorization", format!("Bearer {token}"))
    .body(Vec::new())
    .unwrap()
}

fn sign_in(store: &Store<MockStorage>, auth: &Auth) -> LoginResponse {
  let reply = dispatch(store, auth, None, login_request(PASSWORD));
  assert_eq!(
    reply.status(),
    200,
    "{}",
    String::from_utf8_lossy(reply.body())
  );
  assert_eq!(reply.headers()["cache-control"], "no-store");
  assert!(!reply.headers().contains_key("set-cookie"));
  let login: LoginResponse = serde_json::from_slice(reply.body()).unwrap();
  login.validate(now_ms()).unwrap();
  assert_eq!(login.expires_at_ms, None);
  login
}

#[test]
fn password_exchange_issues_separate_admin_credentials_without_browser_sessions() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let auth = authentication(PASSWORD);
  let first = sign_in(&store, &auth);
  let second = sign_in(&store, &auth);
  assert_ne!(first.token, second.token);
  assert_ne!(first.token, OWNER);
  assert!(!first.token.contains(std::str::from_utf8(PASSWORD).unwrap()));
  let query = Request::BrowseRuns {
    query: expri_core::service_catalog::RunListQuery {
      project_id: None,
      origin: None,
      archival: expri_core::service_catalog::RunArchiveFilter::Active,
      limit: 100,
      cursor: None,
    },
  };
  let page = dispatch(&store, &auth, None, api_request(&first.token, &query));
  assert_eq!(page.status(), 200);
  assert!(matches!(
    serde_json::from_slice::<types::Response>(page.body()).unwrap(),
    types::Response::RunPage { .. }
  ));
  let scope = types::RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-test".into(),
  };
  store
    .execute(Request::AppendStream {
      scope: scope.clone(),
      path: "logs/stdout.log".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"started\n"),
    })
    .unwrap();
  let set = Request::SetRunAnnotations {
    scope: scope.clone(),
    nickname: Some("reviewed".into()),
    tags: Vec::new(),
  };
  assert_eq!(
    dispatch(&store, &auth, None, api_request(&first.token, &set)).status(),
    200
  );
  assert_eq!(
    dispatch(&store, &auth, None, api_request(WORKER, &set)).status(),
    403
  );
  assert_eq!(
    dispatch(&store, &auth, None, api_request(MACHINE, &query)).status(),
    403
  );
  assert_eq!(
    dispatch(
      &store,
      &auth,
      None,
      api_request(
        std::str::from_utf8(PASSWORD).unwrap(),
        &Request::Capabilities
      )
    )
    .status(),
    401
  );
  let dashboard = HostedDashboard::new(&store).unwrap();
  let browser = HttpRequest::builder()
    .uri("/api/catalog")
    .header("Host", MAIN)
    .header("Authorization", format!("Bearer {}", first.token))
    .body(Vec::new())
    .unwrap();
  assert_eq!(
    dispatch(&store, &auth, Some(&dashboard), browser).status(),
    401
  );
}

#[test]
fn logout_revokes_only_the_presented_issued_credential_and_leaves_static_roles_intact() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let auth = authentication(PASSWORD);
  let first = sign_in(&store, &auth);
  let second = sign_in(&store, &auth);
  let revoked = dispatch(&store, &auth, None, logout_request(&first.token));
  assert_eq!(revoked.status(), 204);
  assert!(revoked.body().is_empty());
  assert_eq!(revoked.headers()["cache-control"], "no-store");
  assert!(!revoked.headers().contains_key("set-cookie"));
  assert_eq!(
    dispatch(
      &store,
      &auth,
      None,
      api_request(&first.token, &Request::Capabilities)
    )
    .status(),
    401
  );
  assert_eq!(
    dispatch(
      &store,
      &auth,
      None,
      api_request(&second.token, &Request::Capabilities)
    )
    .status(),
    200
  );
  for token in [OWNER, WORKER, MACHINE] {
    assert_eq!(
      dispatch(&store, &auth, None, logout_request(token)).status(),
      409
    );
    assert_eq!(
      dispatch(
        &store,
        &auth,
        None,
        api_request(token, &Request::Capabilities)
      )
      .status(),
      200
    );
  }
  assert_eq!(
    dispatch(&store, &auth, None, logout_request("unknown-credential")).status(),
    401
  );
}

#[test]
fn a_static_credential_in_the_issued_namespace_never_reports_revocation_success() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let token = format!("{CLI_TOKEN_PREFIX}{}", "a".repeat(43));
  let mut auth = authentication(PASSWORD);
  auth.owner = token.as_bytes().to_vec();
  assert_eq!(
    dispatch(
      &store,
      &auth,
      None,
      api_request(&token, &Request::Capabilities)
    )
    .status(),
    200
  );
  let response = dispatch(&store, &auth, None, logout_request(&token));
  assert_eq!(response.status(), 409);
  assert_eq!(
    serde_json::from_slice::<serde_json::Value>(response.body()).unwrap()["error"],
    "static credentials cannot be revoked through CLI logout"
  );
  assert_eq!(
    dispatch(
      &store,
      &auth,
      None,
      api_request(&token, &Request::Capabilities)
    )
    .status(),
    200
  );
}

#[test]
fn opaque_persistence_survives_restart_but_password_api_secret_and_host_rotations_revoke_it() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let login = sign_in(&store, &authentication(PASSWORD));
  drop(store);
  for name in ["metadata.sqlite3", "metadata.sqlite3-wal"] {
    if let Ok(bytes) = std::fs::read(directory.path().join(name)) {
      for secret in [login.token.as_bytes(), PASSWORD, OWNER.as_bytes()] {
        assert!(!bytes.windows(secret.len()).any(|window| window == secret));
      }
    }
  }
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert_eq!(
    dispatch(
      &store,
      &authentication(PASSWORD),
      None,
      api_request(&login.token, &Request::Capabilities)
    )
    .status(),
    200
  );
  let changed_password = authentication(b"another-dedicated-dashboard-password");
  assert_eq!(
    dispatch(
      &store,
      &changed_password,
      None,
      api_request(&login.token, &Request::Capabilities)
    )
    .status(),
    401
  );
  let mut changed_key = authentication(PASSWORD);
  changed_key.owner = b"another-owner-token-with-at-least-24-characters".to_vec();
  assert_eq!(
    dispatch(
      &store,
      &changed_key,
      None,
      api_request(&login.token, &Request::Capabilities)
    )
    .status(),
    401
  );
  let mut changed_host = authentication(PASSWORD);
  changed_host.sites[0].auth = BrowserAuth::new("https://another.example.test", PASSWORD).unwrap();
  let mut request = api_request(&login.token, &Request::Capabilities);
  request
    .headers_mut()
    .insert("Host", "another.example.test".parse().unwrap());
  assert_eq!(dispatch(&store, &changed_host, None, request).status(), 401);
  let mut disabled = authentication(PASSWORD);
  disabled.sites.clear();
  assert_eq!(
    dispatch(
      &store,
      &disabled,
      None,
      api_request(&login.token, &Request::Capabilities)
    )
    .status(),
    401
  );
}

#[test]
fn login_and_logout_enforce_main_host_browser_boundaries_and_bounded_json_without_echoing_secrets()
{
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let auth = authentication(PASSWORD);
  for host in [PREVIEW, "unknown.example.test"] {
    let mut request = login_request(PASSWORD);
    request.headers_mut().insert("Host", host.parse().unwrap());
    assert_eq!(dispatch(&store, &auth, None, request).status(), 403);
  }
  let mut missing_host = login_request(PASSWORD);
  missing_host.headers_mut().remove("Host");
  assert_eq!(dispatch(&store, &auth, None, missing_host).status(), 403);
  let mut duplicate_host = login_request(PASSWORD);
  duplicate_host
    .headers_mut()
    .append("Host", MAIN.parse().unwrap());
  assert_eq!(dispatch(&store, &auth, None, duplicate_host).status(), 403);
  for (header, value) in [
    ("Origin", "https://expri.example.test"),
    ("Cookie", "__Host-expri_session=browser-session"),
    ("Referer", "https://expri.example.test/"),
    ("Sec-Fetch-Site", "same-origin"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Dest", "empty"),
    ("Authorization", "Bearer known-static-token"),
  ] {
    let mut request = login_request(PASSWORD);
    request.headers_mut().insert(
      HeaderName::from_bytes(header.as_bytes()).unwrap(),
      value.parse().unwrap(),
    );
    assert_eq!(
      dispatch(&store, &auth, None, request).status(),
      403,
      "{header}"
    );
  }
  for content_type in [
    None,
    Some("text/plain"),
    Some("application/x-www-form-urlencoded"),
  ] {
    let mut request = login_request(PASSWORD);
    request.headers_mut().remove("Content-Type");
    if let Some(value) = content_type {
      request
        .headers_mut()
        .insert("Content-Type", value.parse().unwrap());
    }
    assert_eq!(dispatch(&store, &auth, None, request).status(), 415);
  }
  let mut duplicate_type = login_request(PASSWORD);
  duplicate_type
    .headers_mut()
    .append("Content-Type", "application/json".parse().unwrap());
  assert_eq!(dispatch(&store, &auth, None, duplicate_type).status(), 415);
  let mut oversized = login_request(PASSWORD);
  *oversized.body_mut() = vec![b'x'; MAX_AUTH_REQUEST_BYTES + 1];
  assert_eq!(dispatch(&store, &auth, None, oversized).status(), 413);
  for (body, status) in [
    (b"malformed-private-password".to_vec(), 400),
    (
      serde_json::to_vec(&LoginRequest {
        password: "wrong-private-password-with-valid-length".into(),
      })
      .unwrap(),
      401,
    ),
  ] {
    let mut request = login_request(PASSWORD);
    *request.body_mut() = body;
    let reply = dispatch(&store, &auth, None, request);
    assert_eq!(reply.status(), status);
    assert!(!String::from_utf8_lossy(reply.body()).contains("private-password"));
    assert_eq!(reply.headers()["cache-control"], "no-store");
  }
  let mut get = login_request(PASSWORD);
  *get.method_mut() = http::Method::GET;
  assert_eq!(dispatch(&store, &auth, None, get).status(), 405);
  let mut query = login_request(PASSWORD);
  *query.uri_mut() = "/v1/login?password=private-password".parse().unwrap();
  assert_eq!(dispatch(&store, &auth, None, query).status(), 400);
  let mut disabled = auth;
  disabled.sites.clear();
  assert_eq!(
    dispatch(&store, &disabled, None, login_request(PASSWORD)).status(),
    404
  );
  let active = authentication(PASSWORD);
  for (header, value) in [
    ("Origin", "https://expri.example.test"),
    ("Cookie", "browser-session"),
    ("Sec-Fetch-Site", "none"),
  ] {
    let mut request = logout_request(OWNER);
    request.headers_mut().insert(
      HeaderName::from_bytes(header.as_bytes()).unwrap(),
      value.parse().unwrap(),
    );
    assert_eq!(dispatch(&store, &active, None, request).status(), 403);
  }
  let mut logout = logout_request(OWNER);
  *logout.body_mut() = b"{}".to_vec();
  assert_eq!(dispatch(&store, &active, None, logout).status(), 400);
}

#[test]
fn login_attempts_are_bounded_per_server_and_reopen_after_the_window() {
  let limiter = LoginLimiter::default();
  let start = Instant::now();
  for _ in 0..LOGIN_ATTEMPTS {
    limiter.check(start).unwrap();
  }
  assert_eq!(limiter.check(start).unwrap_err().status, 429);
  assert_eq!(limiter.attempts.lock().unwrap().len(), LOGIN_ATTEMPTS);
  limiter.check(start + LOGIN_WINDOW).unwrap();
  assert_eq!(limiter.attempts.lock().unwrap().len(), 1);
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let auth = authentication(PASSWORD);
  for _ in 0..LOGIN_ATTEMPTS {
    assert_eq!(
      dispatch(
        &store,
        &auth,
        None,
        login_request(b"wrong-password-with-valid-length")
      )
      .status(),
      401
    );
  }
  let blocked = dispatch(&store, &auth, None, login_request(PASSWORD));
  assert_eq!(blocked.status(), 429);
  assert_eq!(blocked.headers()["retry-after"], "60");
}

#[test]
fn socket_reader_rejects_oversized_auth_bodies_before_waiting_for_their_bytes() {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let address = listener.local_addr().unwrap();
  let worker = std::thread::spawn(move || {
    let (mut stream, _) = listener.accept().unwrap();
    assert_eq!(read_request(&mut stream).unwrap_err().status, 413);
  });
  let mut stream = TcpStream::connect(address).unwrap();
  stream.write_all(format!("POST /v1/login HTTP/1.1\r\nHost: {MAIN}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n", MAX_AUTH_REQUEST_BYTES+1).as_bytes()).unwrap();
  worker.join().unwrap();
}
