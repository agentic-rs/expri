//! Exercise credential persistence and revocation through real CLI processes.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use expri_core::global_config::{GlobalConfig, PublishingConfig, SystemProfileConfig};
use expri_core::service::{ClientConfig, Request, Response};
use expri_core::service_auth::{CLI_TOKEN_PREFIX, LoginRequest, LoginResponse};
use expri_core::service_catalog::{RunListPage, SERVICE_CATALOG_CAPABILITY};
use serde::Serialize;
use serde_json::Value;

const PASSWORD: &str = "fixture dashboard password 123";
const STATIC_TOKEN: &str = "static-fixture-credential-never-print";

fn token(marker: char) -> String {
  format!("{CLI_TOKEN_PREFIX}{}", marker.to_string().repeat(43))
}

struct Fixture {
  _directory: tempfile::TempDir,
  workspace: PathBuf,
  root: PathBuf,
}

impl Fixture {
  fn new() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let root = workspace.join("global");
    fs::create_dir(&root).unwrap();
    fs::write(workspace.join("expri.toml"), "invalid local project TOML").unwrap();
    Self {
      _directory: directory,
      workspace,
      root,
    }
  }

  fn config(&self) -> PathBuf {
    self.root.join("config.toml")
  }

  fn saved(&self) -> GlobalConfig {
    GlobalConfig::load(&self.config()).unwrap()
  }

  fn run(&self, args: &[&str], password: Option<&str>) -> Output {
    self.run_output(args, password, true)
  }

  fn run_output(&self, args: &[&str], password: Option<&str>, json: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_expri"));
    command
      .args(args)
      .env("EXPRI_STORAGE_DIR", &self.root)
      .env("NO_PROXY", "127.0.0.1,localhost")
      .current_dir(&self.workspace)
      .stdin(if password.is_some() {
        Stdio::piped()
      } else {
        Stdio::null()
      })
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());
    if json {
      command.arg("--json");
    }
    let mut child = command.spawn().unwrap();
    if let Some(password) = password {
      let mut input = child.stdin.take().unwrap();
      input.write_all(password.as_bytes()).unwrap();
      input.write_all(b"\n").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      if child.try_wait().unwrap().is_some() {
        break;
      }
      if Instant::now() >= deadline {
        let _ = child.kill();
        let _ = child.wait();
        panic!("fixture CLI did not finish within 20 seconds");
      }
      thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_secret_safe(&output);
    output
  }
}

fn write_config(root: &Path, url: &str, credential: Option<String>, interval: u64) -> Vec<u8> {
  let config = GlobalConfig {
    server: Some(ClientConfig {
      url: url.into(),
      token: credential,
      token_env: None,
      project_id: Some("vision".into()),
    }),
    system_profile: SystemProfileConfig {
      interval_seconds: interval,
      publish: Some(PublishingConfig {
        client_config: root.parent().unwrap().join("machine.toml"),
      }),
    },
  };
  config.validate().unwrap();
  let bytes = toml::to_string(&config).unwrap().into_bytes();
  fs::write(root.join("config.toml"), &bytes).unwrap();
  bytes
}

fn assert_secret_safe(output: &Output) {
  let text = format!(
    "{}{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  for secret in [
    PASSWORD.to_owned(),
    STATIC_TOKEN.to_owned(),
    token('A'),
    token('B'),
    token('C'),
  ] {
    assert!(
      !text.contains(&secret),
      "CLI output must not contain fixture credentials"
    );
  }
}

fn success(output: Output) -> Value {
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  serde_json::from_slice(&output.stdout).unwrap()
}

struct HttpRequest {
  path: String,
  headers: BTreeMap<String, String>,
  body: Vec<u8>,
}

fn request(stream: &mut TcpStream) -> HttpRequest {
  stream.set_nonblocking(false).unwrap();
  stream
    .set_read_timeout(Some(Duration::from_secs(5)))
    .unwrap();
  stream
    .set_write_timeout(Some(Duration::from_secs(5)))
    .unwrap();
  let mut bytes = Vec::new();
  let (header_end, length, path, headers) = loop {
    let mut chunk = [0; 4096];
    let count = stream.read(&mut chunk).unwrap();
    assert!(count > 0, "CLI closed before sending headers");
    bytes.extend_from_slice(&chunk[..count]);
    if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
      assert!(offset <= 16 * 1024);
      let head = std::str::from_utf8(&bytes[..offset]).unwrap();
      let mut lines = head.lines();
      let mut first = lines.next().unwrap().split_whitespace();
      assert_eq!(first.next(), Some("POST"));
      let path = first.next().unwrap().to_string();
      let headers: BTreeMap<_, _> = lines
        .map(|line| {
          let (name, value) = line.split_once(':').unwrap();
          (name.to_ascii_lowercase(), value.trim().to_string())
        })
        .collect();
      let length = headers
        .get("content-length")
        .map_or(0, |value| value.parse::<usize>().unwrap());
      assert!(length <= 4096);
      break (offset + 4, length, path, headers);
    }
    assert!(bytes.len() < 16 * 1024);
  };
  while bytes.len() < header_end + length {
    let mut chunk = [0; 4096];
    let count = stream.read(&mut chunk).unwrap();
    assert!(count > 0, "CLI closed before sending its body");
    bytes.extend_from_slice(&chunk[..count]);
  }
  HttpRequest {
    path,
    headers,
    body: bytes[header_end..header_end + length].to_vec(),
  }
}

fn serve(
  count: usize,
  mut handler: impl FnMut(usize, HttpRequest, &str) -> (u16, Vec<u8>) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  let endpoint = url.clone();
  let server = thread::spawn(move || {
    for index in 0..count {
      let deadline = Instant::now() + Duration::from_secs(20);
      let mut stream = loop {
        match listener.accept() {
          Ok((stream, _)) => break stream,
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            assert!(
              Instant::now() < deadline,
              "CLI did not finish expected fixture requests"
            );
            thread::sleep(Duration::from_millis(5));
          }
          Err(error) => panic!("{error}"),
        }
      };
      let (status, body) = handler(index, request(&mut stream), &endpoint);
      write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
      stream.write_all(&body).unwrap();
    }
  });
  (url, server)
}

fn json(value: &impl Serialize) -> Vec<u8> {
  serde_json::to_vec(value).unwrap()
}

fn login_request(request: &HttpRequest) {
  assert_eq!(request.path, "/v1/login");
  assert!(!request.headers.contains_key("authorization"));
  assert!(!request.headers.contains_key("cookie"));
  let body: LoginRequest = serde_json::from_slice(&request.body).unwrap();
  assert_eq!(body.password, PASSWORD);
  body.validate().unwrap();
}

fn bearer(request: &HttpRequest, marker: char) {
  assert_eq!(
    request.headers["authorization"],
    format!("Bearer {}", token(marker))
  );
  assert!(!request.headers.contains_key("cookie"));
}

fn issued(marker: char) -> Vec<u8> {
  json(&LoginResponse {
    token: token(marker),
    expires_at_ms: None,
  })
}

#[test]
fn login_persists_private_credentials_for_queries_and_logout_retains_the_default_url() {
  let fixture = Fixture::new();
  let (url, server) = serve(7, |index, request, _| match index {
    0 => {
      login_request(&request);
      (200, issued('A'))
    }
    1 => {
      bearer(&request, 'A');
      assert_eq!(request.path, "/v1/request");
      assert!(matches!(
        serde_json::from_slice::<Request>(&request.body).unwrap(),
        Request::Capabilities
      ));
      (
        200,
        json(&Response::Capabilities {
          features: vec![SERVICE_CATALOG_CAPABILITY.into()],
        }),
      )
    }
    2 => {
      bearer(&request, 'A');
      let Request::BrowseRuns { query } = serde_json::from_slice(&request.body).unwrap() else {
        panic!("expected run query")
      };
      assert!(query.project_id.is_none() && query.origin.is_none());
      (
        200,
        json(&Response::RunPage {
          page: RunListPage {
            runs: Vec::new(),
            next_cursor: None,
          },
        }),
      )
    }
    3 => {
      assert_eq!(request.path, "/v1/logout");
      bearer(&request, 'A');
      assert!(request.body.is_empty());
      (204, Vec::new())
    }
    4 => {
      login_request(&request);
      (200, issued('B'))
    }
    5 => {
      login_request(&request);
      (200, issued('C'))
    }
    6 => {
      assert_eq!(request.path, "/v1/logout");
      bearer(&request, 'B');
      assert!(request.body.is_empty());
      (204, Vec::new())
    }
    _ => unreachable!(),
  });
  write_config(&fixture.root, &url, None, 30);
  let logged_in = success(fixture.run(
    &["login", "--url", &url, "--password-stdin"],
    Some(PASSWORD),
  ));
  assert_eq!(logged_in["logged_in"], true);
  assert!(logged_in["expires_at_ms"].is_null());
  let saved = fixture.saved();
  let connection = saved.server.unwrap();
  assert_eq!(connection.token.as_deref(), Some(token('A').as_str()));
  assert_eq!(connection.project_id.as_deref(), Some("vision"));
  assert_eq!(saved.system_profile.interval_seconds, 30);
  assert_eq!(
    saved.system_profile.publish.unwrap().client_config,
    fixture.workspace.join("machine.toml")
  );
  assert!(
    !fs::read_to_string(fixture.config())
      .unwrap()
      .contains(PASSWORD)
  );
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
      fs::metadata(fixture.config()).unwrap().permissions().mode() & 0o777,
      0o600
    );
    assert_eq!(
      fs::metadata(&fixture.root).unwrap().permissions().mode() & 0o777,
      0o700
    );
  }
  let before_query = fs::read(fixture.config()).unwrap();
  assert_eq!(
    success(fixture.run(&["query", "runs"], None))["runs"],
    serde_json::json!([])
  );
  assert_eq!(fs::read(fixture.config()).unwrap(), before_query);
  assert_eq!(success(fixture.run(&["logout"], None))["logged_in"], false);
  let connection = fixture.saved().server.unwrap();
  assert_eq!(connection.url, url);
  assert!(connection.token.is_none() && connection.token_env.is_none());
  assert_eq!(connection.project_id.as_deref(), Some("vision"));
  let shown = success(fixture.run(&["config", "server", "show"], None));
  assert_eq!(shown["server"]["token_configured"], false);
  assert!(shown["server"]["token_source"].is_null());
  let unauthenticated = fixture.run(&["query", "runs"], None);
  assert!(!unauthenticated.status.success());
  assert!(String::from_utf8_lossy(&unauthenticated.stderr).contains("expri login"));
  assert_eq!(
    success(fixture.run(&["login", "--password-stdin"], Some(PASSWORD)))["url"],
    url
  );
  assert_eq!(
    fixture.saved().server.unwrap().token.as_deref(),
    Some(token('B').as_str())
  );
  // Refreshing a login first persists the new token, then revokes its predecessor.
  assert_eq!(
    success(fixture.run(&["login", "--password-stdin"], Some(PASSWORD)))["logged_in"],
    true
  );
  assert_eq!(
    fixture.saved().server.unwrap().token.as_deref(),
    Some(token('C').as_str())
  );
  server.join().unwrap();
}

#[test]
fn rejected_login_keeps_saved_bytes_and_never_repeats_the_server_error_body() {
  let fixture = Fixture::new();
  let (url, server) = serve(1, |_, request, _| {
    login_request(&request);
    (
      401,
      json(&serde_json::json!({"error":format!("{PASSWORD} {}", token('B'))})),
    )
  });
  let before = write_config(&fixture.root, &url, Some(token('A')), 30);
  let output = fixture.run(&["login", "--password-stdin"], Some(PASSWORD));
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("401 Unauthorized"));
  assert!(String::from_utf8_lossy(&output.stderr).contains("login password was rejected"));
  assert_eq!(fs::read(fixture.config()).unwrap(), before);
  assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 1);
  server.join().unwrap();
}

#[test]
fn nonauthentication_logout_failures_preserve_the_credential_for_retry() {
  for status in [409, 503] {
    let fixture = Fixture::new();
    let (url, server) = serve(1, move |_, request, _| {
      assert_eq!(request.path, "/v1/logout");
      bearer(&request, 'A');
      assert!(request.body.is_empty());
      (
        status,
        json(&serde_json::json!({"error":format!("{PASSWORD} {}", token('A'))})),
      )
    });
    let before = write_config(&fixture.root, &url, Some(token('A')), 30);
    let output = fixture.run(&["logout"], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(&status.to_string()));
    assert_eq!(fs::read(fixture.config()).unwrap(), before);
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 1);
    server.join().unwrap();
  }
}

#[test]
fn already_revoked_logout_clears_only_credentials_and_keeps_the_connection() {
  let fixture = Fixture::new();
  let (url, server) = serve(1, |_, request, _| {
    assert_eq!(request.path, "/v1/logout");
    bearer(&request, 'A');
    (401, json(&serde_json::json!({"error":token('A')})))
  });
  write_config(&fixture.root, &url, Some(token('A')), 20);
  let output = success(fixture.run(&["logout"], None));
  assert_eq!(output["logged_in"], false);
  assert_eq!(output["warnings"].as_array().unwrap().len(), 1);
  let saved = fixture.saved();
  assert_eq!(saved.system_profile.interval_seconds, 20);
  let connection = saved.server.unwrap();
  assert_eq!(connection.url, url);
  assert!(connection.token.is_none() && connection.token_env.is_none());
  server.join().unwrap();
}

#[test]
fn static_credentials_are_not_sent_for_cli_revocation_or_removed_locally() {
  let fixture = Fixture::new();
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  let before = write_config(&fixture.root, &url, Some(STATIC_TOKEN.into()), 20);
  let output = fixture.run(&["logout"], None);
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("config server unset"));
  assert_eq!(fs::read(fixture.config()).unwrap(), before);
  assert_eq!(
    listener.accept().unwrap_err().kind(),
    std::io::ErrorKind::WouldBlock
  );
}

#[test]
fn changed_configuration_revokes_the_new_token_and_preserves_the_concurrent_edit() {
  let fixture = Fixture::new();
  let root = fixture.root.clone();
  let (url, server) = serve(2, move |index, request, endpoint| match index {
    0 => {
      login_request(&request);
      // The CLI already captured token A. Commit a different connection before
      // releasing the response so its conditional save must reject the snapshot.
      write_config(&root, endpoint, Some(token('C')), 40);
      (200, issued('B'))
    }
    1 => {
      assert_eq!(request.path, "/v1/logout");
      bearer(&request, 'B');
      assert!(request.body.is_empty());
      (204, Vec::new())
    }
    _ => unreachable!(),
  });
  write_config(&fixture.root, &url, Some(token('A')), 30);
  let output = fixture.run(&["login", "--password-stdin"], Some(PASSWORD));
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("configuration changed during"));
  let saved = fixture.saved();
  assert_eq!(
    saved.server.unwrap().token.as_deref(),
    Some(token('C').as_str())
  );
  assert_eq!(saved.system_profile.interval_seconds, 40);
  assert!(
    !fs::read_to_string(fixture.config())
      .unwrap()
      .contains(&token('B'))
  );
  assert!(fs::read_dir(&fixture.root).unwrap().all(|entry| {
    !entry
      .unwrap()
      .file_name()
      .to_string_lossy()
      .starts_with(".config-")
  }));
  server.join().unwrap();
}

#[test]
fn failed_persistence_revokes_the_unsaved_token_and_keeps_original_configuration() {
  let fixture = Fixture::new();
  let root = fixture.root.clone();
  let (url, server) = serve(2, move |index, request, _| match index {
    0 => {
      login_request(&request);
      // A directory cannot be used as the writer's managed lock file. This
      // reliably fails persistence without relying on the test process's UID.
      fs::create_dir(root.join("config.lock")).unwrap();
      (200, issued('B'))
    }
    1 => {
      assert_eq!(request.path, "/v1/logout");
      bearer(&request, 'B');
      assert!(request.body.is_empty());
      (204, Vec::new())
    }
    _ => unreachable!(),
  });
  let before = write_config(&fixture.root, &url, None, 30);
  let output = fixture.run(&["login", "--password-stdin"], Some(PASSWORD));
  assert!(!output.status.success());
  assert!(!output.stderr.is_empty());
  assert_eq!(fs::read(fixture.config()).unwrap(), before);
  assert!(fixture.root.join("config.lock").is_dir());
  assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 2);
  server.join().unwrap();
}

#[test]
fn authentication_and_configuration_default_to_private_human_tables() {
  let fixture = Fixture::new();
  let (url, server) = serve(4, |index, request, _| match index {
    0 => {
      login_request(&request);
      (200, issued('A'))
    }
    1 => {
      assert_eq!(request.path, "/v1/logout");
      bearer(&request, 'A');
      (204, Vec::new())
    }
    2 => {
      login_request(&request);
      (200, issued('B'))
    }
    3 => {
      assert_eq!(request.path, "/v1/logout");
      bearer(&request, 'B');
      (204, Vec::new())
    }
    _ => unreachable!(),
  });
  write_config(&fixture.root, &url, None, 30);
  let logged_in = fixture.run_output(&["login", "--password-stdin"], Some(PASSWORD), false);
  assert!(logged_in.status.success());
  let text = String::from_utf8(logged_in.stdout).unwrap();
  for field in ["FIELD", "VALUE", "Logged in", "Until revoked", &url] {
    assert!(text.contains(field), "{text}");
  }
  assert!(serde_json::from_str::<Value>(&text).is_err());
  let shown = fixture.run_output(&["config", "server", "show"], None, false);
  assert!(shown.status.success());
  assert!(String::from_utf8_lossy(&shown.stdout).contains("Server / url"));
  let logged_out = fixture.run_output(&["logout"], None, false);
  assert!(logged_out.status.success());
  assert!(String::from_utf8_lossy(&logged_out.stdout).contains("Logged out"));
  assert_eq!(
    success(fixture.run(&["login", "--password-stdin", "-q"], Some(PASSWORD)))["logged_in"],
    true
  );
  let quiet = fixture.run_output(&["logout", "-q"], None, false);
  assert!(quiet.status.success());
  assert!(quiet.stdout.is_empty());
  server.join().unwrap();
}
