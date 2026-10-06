use std::fs;

use tempfile::TempDir;

use super::*;
use crate::config::Config;
use crate::context::CommandContext;

const AUTHORITY: &str = "127.0.0.1:8765";

struct Fixture {
  _directory: TempDir,
  dashboard: Dashboard,
}

impl Fixture {
  fn new() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    let config_path = root.join("expri.toml");
    fs::write(
      &config_path,
      "[project]\nname = 'server-fixture'\n[environment]\n",
    )
    .unwrap();
    for (id, value) in [("run-one", 0.5), ("run-two", 0.25)] {
      let run = root.join(".expri/runs").join(id);
      fs::create_dir_all(run.join("outputs")).unwrap();
      fs::create_dir_all(run.join("logs")).unwrap();
      fs::write(
        run.join("run-state.json"),
        json!({
          "schema_version": 1, "run_id": id, "task": "train", "status": "completed",
          "started_at": "2026-10-03T01:00:00Z", "finished_at": "2026-10-03T01:01:00Z",
          "exit_code": 0, "command": ["python", "train.py"], "detached": false,
        })
        .to_string(),
      )
      .unwrap();
      fs::write(
        run.join("outputs/metrics.jsonl"),
        format!(
          "{}\n",
          json!({
            "schema_version": 1, "step": 1, "metrics": {"train/loss": value},
          })
        ),
      )
      .unwrap();
      fs::write(
        run.join("outputs/params.json"),
        json!({
          "schema_version": 1, "params": {"learning_rate": value},
        })
        .to_string(),
      )
      .unwrap();
      fs::write(run.join("logs/stdout.log"), "first line\nlast line\n").unwrap();
    }
    let context = CommandContext {
      config: Config::load(&config_path).unwrap(),
      repo_root: root,
      project_name: Some("server-fixture".to_string()),
    };
    Self {
      dashboard: Dashboard::new(context, None).unwrap(),
      _directory: directory,
    }
  }

  fn get(&self, path: &str) -> Reply {
    route(
      &self.dashboard,
      AUTHORITY,
      "GET",
      path,
      &[("Host", AUTHORITY)],
    )
  }
}

fn assert_error(reply: Reply, status: u16) {
  assert_eq!(reply.status, status);
  assert_eq!(reply.content_type, "application/json; charset=utf-8");
  let body: Value = serde_json::from_slice(&reply.body).unwrap();
  assert!(
    body["error"]
      .as_str()
      .is_some_and(|value| !value.is_empty())
  );
}

#[test]
fn enforces_exact_single_host_and_same_origin_browser_boundaries() {
  let fixture = Fixture::new();
  for headers in [
    vec![],
    vec![("Host", "localhost:8765")],
    vec![("Host", "127.0.0.1:8766")],
    vec![("Host", "attacker.example")],
    vec![("Host", AUTHORITY), ("Host", AUTHORITY)],
    vec![("Host", AUTHORITY), ("Origin", "null")],
    vec![("Host", AUTHORITY), ("Origin", "http://attacker.example")],
    vec![("Host", AUTHORITY), ("Origin", "http://127.0.0.1:8765/")],
    vec![
      ("Host", AUTHORITY),
      ("Origin", "http://127.0.0.1:8765"),
      ("Origin", "http://127.0.0.1:8765"),
    ],
    vec![("Host", AUTHORITY), ("Sec-Fetch-Site", "cross-site")],
    vec![
      ("Host", AUTHORITY),
      ("Sec-Fetch-Site", "same-origin"),
      ("Sec-Fetch-Site", "cross-site"),
    ],
  ] {
    assert_error(
      route(
        &fixture.dashboard,
        AUTHORITY,
        "GET",
        "/api/catalog",
        &headers,
      ),
      403,
    );
  }
  for site in ["same-origin", "same-site", "none"] {
    let reply = route(
      &fixture.dashboard,
      AUTHORITY,
      "GET",
      "/api/catalog",
      &[
        ("hOsT", AUTHORITY),
        ("oRiGiN", "http://127.0.0.1:8765"),
        ("Sec-Fetch-Site", site),
      ],
    );
    assert_eq!(reply.status, 200);
  }
}

#[test]
fn supports_only_read_methods_and_fixed_paths() {
  let fixture = Fixture::new();
  for method in [
    "POST", "PUT", "DELETE", "OPTIONS", "PATCH", "CONNECT", "TRACE",
  ] {
    assert_error(
      route(
        &fixture.dashboard,
        AUTHORITY,
        method,
        "/api/catalog",
        &[("Host", AUTHORITY)],
      ),
      405,
    );
  }
  for path in [
    "http://127.0.0.1:8765/",
    "//attacker.example/",
    "*",
    "/#fragment",
  ] {
    assert_error(fixture.get(path), 400);
  }
  for path in [
    "/unknown",
    "/api/cancel",
    "/api/run/run-one",
    "/../expri.toml",
    "/%2e%2e/expri.toml",
    "/.expri/runs/run-one/run-state.json",
  ] {
    assert_error(fixture.get(path), 404);
  }
  let get = fixture.get("/");
  let head = route(
    &fixture.dashboard,
    AUTHORITY,
    "HEAD",
    "/",
    &[("Host", AUTHORITY)],
  );
  assert_eq!(head.status, 200);
  // HEAD uses the same representation; the transport suppresses its body.
  assert_eq!(head.body, get.body);
}

#[test]
fn rejects_unknown_duplicate_oversized_and_invalid_query_fields() {
  let fixture = Fixture::new();
  for path in [
    "/?source=local",
    "/api/catalog?limit=1",
    "/styles.css?version=1",
    "/api/runs?source=local&source=local",
    "/api/runs?limit=1&%6cimit=2",
    "/api/run?run_id=run-one&run_id=run-two",
    "/api/log?run_id=run-one&run_id=run-two",
    "/api/run?id=run-one",
    "/api/runs?limit=0",
    "/api/runs?limit=1001",
    "/api/runs?limit=no",
    "/api/runs?limit=-1",
    "/api/runs?offset=-1",
    "/api/runs?offset=no",
    "/api/runs?offset=184467440737095516160",
    "/api/log?run_id=run-one&stream=combined",
    "/api/log?run_id=run-one&tail=1001",
    "/api/compare?run_id=run-one",
    "/api/compare?run_id=run-one&run_id=run-one",
    "/api/compare?run_id=run-one&run_id=run-two&reduction=mean",
    "/api/chart",
    "/api/chart?run_id=run-one&reduction=last",
    "/api/run?run_id=",
    "/api/log",
    "/api/updates?limit=1",
    "/api/updates?source=local&source=local",
    "/api/updates?run_id=run-one",
    "/api/updates?source=&run_id=run-one",
    "/api/updates?source=local&run_id=run-one&run_id=run-one",
    "/api/updates?source=local&run_id=",
    "/api/updates?source=local&run_id=..%2Foutside",
  ] {
    assert_error(fixture.get(path), 400);
  }
  let over_limit = format!("/api/runs?search={}", "x".repeat(QUERY_LIMIT));
  assert_error(fixture.get(&over_limit), 400);
  let too_many = format!(
    "/api/chart?{}",
    (0..9)
      .map(|index| format!("run_id=run-{index}"))
      .collect::<Vec<_>>()
      .join("&")
  );
  assert_error(fixture.get(&too_many), 400);
  assert_error(
    fixture.get(&too_many.replace("/api/chart?", "/api/updates?source=local&")),
    400,
  );
  assert_error(
    fixture.get(&format!(
      "/api/updates?source=local&run_id={}",
      "a".repeat(257)
    )),
    400,
  );
}

#[test]
fn update_routes_return_bounded_selected_hints_and_preserve_read_boundaries() {
  let fixture = Fixture::new();
  for path in [
    "/api/updates",
    "/api/updates?source=",
    "/api/updates?source=local",
  ] {
    let reply = fixture.get(path);
    assert_eq!(reply.status, 200);
    let value: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(value["runs"], json!([]));
    assert_eq!(value["source_revision"], Value::Null);
  }
  let path = "/api/updates?source=local&run_id=run-one&run_id=removed";
  let reply = fixture.get(path);
  assert_eq!(reply.status, 200);
  assert!(reply.body.len() < 4096);
  let value: Value = serde_json::from_slice(&reply.body).unwrap();
  assert_eq!(value["runs"][0]["run_id"], "run-one");
  assert_eq!(value["runs"][0]["missing"], false);
  assert_eq!(value["runs"][1]["missing"], true);
  for (method, headers, expected) in [
    ("GET", vec![("Host", AUTHORITY), ("Origin", "null")], 403),
    (
      "GET",
      vec![("Host", AUTHORITY), ("Sec-Fetch-Site", "cross-site")],
      403,
    ),
    ("POST", vec![("Host", AUTHORITY)], 405),
  ] {
    assert_error(
      route(&fixture.dashboard, AUTHORITY, method, path, &headers),
      expected,
    );
  }
}

#[test]
fn serves_embedded_assets_and_read_apis_with_json_error_statuses() {
  let fixture = Fixture::new();
  for (path, content_type) in [
    ("/", "text/html; charset=utf-8"),
    ("/index.html", "text/html; charset=utf-8"),
    ("/styles.css", "text/css; charset=utf-8"),
    ("/app.js", "text/javascript; charset=utf-8"),
  ] {
    let reply = fixture.get(path);
    assert_eq!(reply.status, 200, "{path}");
    assert_eq!(reply.content_type, content_type);
    assert!(!reply.body.is_empty());
    assert!(!reply.chart);
  }
  for path in [
    "/api/catalog",
    "/api/runs",
    "/api/runs?source=local&search=train&task=train&status=completed&limit=1",
    "/api/run?run_id=run-one",
    "/api/log?run_id=run-one&tail=1",
    "/api/log?run_id=run-one&tail=0",
    "/api/compare?run_id=run-one&run_id=run-two&metric=train%2Floss&metric=train%2Floss&reduction=min",
  ] {
    let reply = fixture.get(path);
    assert_eq!(
      reply.status,
      200,
      "{path}: {}",
      String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.content_type, "application/json; charset=utf-8");
    assert!(
      serde_json::from_slice::<Value>(&reply.body)
        .unwrap()
        .is_object()
    );
  }
  let chart = fixture.get("/api/chart?run_id=run-one&metric=train%2Floss");
  assert_eq!(chart.status, 200);
  assert_eq!(chart.content_type, "text/html; charset=utf-8");
  assert!(chart.chart);
  assert!(String::from_utf8_lossy(&chart.body).contains("<svg"));
  assert_error(fixture.get("/api/run?run_id=missing-run"), 404);
  assert_error(fixture.get("/api/runs?source=missing"), 404);
  assert_error(fixture.get("/api/run?run_id=..%2Foutside"), 400);
  assert_error(
    fixture.get("/api/compare?run_id=run-one&run_id=run-two&metric=unknown+source"),
    400,
  );
  assert_error(fixture.get("/api/run?run_id=invalid+run+is+missing"), 400);
  assert_error(
    service_error(std::io::Error::other("read failure").into()),
    500,
  );
}

#[test]
fn query_decoding_preserves_repeated_metric_and_run_order() {
  let query =
    Query::parse("run_id=run-two&metric=train%2Floss&run_id=run-one&metric=eval+accuracy")
      .unwrap_or_else(|_| panic!("valid query"));
  assert_eq!(
    query.runs(2).unwrap_or_else(|_| panic!("valid IDs")),
    ["run-two", "run-one"]
  );
  assert_eq!(query.many("metric"), ["train/loss", "eval accuracy"]);
  assert_eq!(
    query
      .number("limit", 100, 1, 1000)
      .unwrap_or_else(|_| panic!("default limit")),
    100
  );
}

#[test]
fn run_pages_accept_offsets_without_expanding_the_response_limit() {
  let fixture = Fixture::new();
  let first = fixture.get("/api/runs?limit=1&offset=0");
  assert_eq!(first.status, 200);
  let first: Value = serde_json::from_slice(&first.body).unwrap();
  assert_eq!(first["runs"].as_array().unwrap().len(), 1);
  assert_eq!(first["offset"], 0);
  assert_eq!(first["next_offset"], 1);
  let second = fixture.get("/api/runs?limit=1&offset=1");
  assert_eq!(second.status, 200);
  let second: Value = serde_json::from_slice(&second.body).unwrap();
  assert_eq!(second["runs"].as_array().unwrap().len(), 1);
  assert_eq!(second["offset"], 1);
  assert!(second["next_offset"].is_null());
  assert_ne!(first["runs"][0]["run_id"], second["runs"][0]["run_id"]);
  let beyond = fixture.get(&format!("/api/runs?limit=1&offset={}", usize::MAX));
  assert_eq!(beyond.status, 200);
  let beyond: Value = serde_json::from_slice(&beyond.body).unwrap();
  assert!(beyond["runs"].as_array().unwrap().is_empty());
  assert!(beyond["next_offset"].is_null());
}

#[test]
fn response_writer_does_not_extend_its_deadline_after_a_partial_write() {
  let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
  let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
  let (mut stream, _) = listener.accept().unwrap();
  let mut writer = DeadlineWriter {
    stream: &mut stream,
    deadline: Instant::now() + SOCKET_TIMEOUT,
  };
  writer.write_all(b"first").unwrap();
  writer.deadline = Instant::now();
  let error = writer.write_all(b"second").unwrap_err();
  assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn http_parser_and_body_validation_do_not_allocate_declared_content_lengths() {
  for length in ["1073741824", "18446744073709551615", "not-a-number"] {
    let wire =
      format!("GET /api/catalog HTTP/1.1\r\nHost: {AUTHORITY}\r\nContent-Length: {length}\r\n\r\n");
    let request = parse_http_request(wire.as_bytes())
      .unwrap_or_else(|_| panic!("parsed headers"))
      .expect("complete headers");
    let headers = request
      .headers()
      .iter()
      .map(|(name, value)| (name.as_str(), value.to_str().unwrap()))
      .collect::<Vec<_>>();
    assert_error(validate_body(&headers).expect_err("body rejected"), 400);
  }
  assert!(validate_body(&[]).is_ok());
  assert!(validate_body(&[("Content-Length", "0")]).is_ok());
  for headers in [
    vec![("Content-Length", "0"), ("Content-Length", "0")],
    vec![("Transfer-Encoding", "chunked")],
  ] {
    assert_error(validate_body(&headers).expect_err("body rejected"), 400);
  }
  assert!(matches!(
    parse_http_request(b"GET / HTTP/1.1\r\n"),
    Ok(None)
  ));
  assert_error(
    parse_http_request(b"GET / HTTP/1.1\r\nBroken-Header\r\n\r\n").expect_err("malformed header"),
    400,
  );
  let oversized = format!(
    "GET / HTTP/1.1\r\nX-Large: {}\r\n\r\n",
    "x".repeat(HEADER_LIMIT)
  );
  assert_error(
    parse_http_request(oversized.as_bytes()).expect_err("header limit"),
    431,
  );
  let repeated = format!("GET / HTTP/1.1\r\n{}\r\n", "X-Value: x\r\n".repeat(65));
  assert_error(
    parse_http_request(repeated.as_bytes()).expect_err("header count limit"),
    431,
  );
  let duplicate_host = format!("GET / HTTP/1.1\r\nHost: {AUTHORITY}\r\nHost: {AUTHORITY}\r\n\r\n");
  let request = parse_http_request(duplicate_host.as_bytes())
    .unwrap_or_else(|_| panic!("parsed duplicate headers"))
    .unwrap();
  let headers = request
    .headers()
    .iter()
    .map(|(name, value)| (name.as_str(), value.to_str().unwrap()))
    .collect::<Vec<_>>();
  assert_error(
    validate_boundary(AUTHORITY, "GET", "/", &headers).expect_err("duplicate Host rejected"),
    403,
  );
}

#[test]
fn http_response_headers_are_bounded_and_head_omits_the_body() {
  let mut wire = Vec::new();
  write_response(&mut wire, Reply::json(json!({"ok": true})), true).unwrap();
  let text = String::from_utf8(wire).unwrap();
  assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
  assert!(text.contains("content-length: 11\r\n"));
  assert!(text.contains("connection: close\r\n"));
  assert!(text.contains("cache-control: no-store\r\n"));
  assert!(text.contains("x-content-type-options: nosniff\r\n"));
  assert!(text.contains("frame-ancestors 'none'"));
  assert!(text.ends_with("\r\n\r\n"));
  let mut wire = Vec::new();
  let mut chart = Reply::bytes("text/html; charset=utf-8", "<svg></svg>");
  chart.chart = true;
  write_response(&mut wire, chart, false).unwrap();
  let text = String::from_utf8(wire).unwrap();
  assert!(text.contains("frame-ancestors 'self'"));
  assert!(text.contains("style-src 'unsafe-inline'"));
  assert!(text.ends_with("<svg></svg>"));
  let mut wire = Vec::new();
  write_response(&mut wire, Reply::error(405, "method rejected"), false).unwrap();
  assert!(
    String::from_utf8(wire)
      .unwrap()
      .contains("allow: GET, HEAD\r\n")
  );
  assert_error(
    Reply::json(json!({"oversized": "x".repeat(JSON_LIMIT)})),
    413,
  );
  assert_error(
    Reply::bytes("text/html; charset=utf-8", vec![b'x'; HTML_LIMIT + 1]),
    413,
  );
  let mut writer = LimitedWriter(Vec::new());
  writer.write_all(&vec![b'x'; JSON_LIMIT]).unwrap();
  assert!(writer.write_all(b"x").is_err());
  assert_eq!(writer.0.len(), JSON_LIMIT);
}
