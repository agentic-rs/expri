use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::Dashboard;
use super::server::{Reply, artifact_download, respond, route};
use crate::config::Config;
use crate::context::CommandContext;

const AUTHORITY: &str = "127.0.0.1:8765";
const QUERY_LIMIT: usize = 8 * 1024;

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
fn local_project_probe_returns_capability_fallback_and_rejects_machine_filters() {
  let fixture = Fixture::new();
  assert_error(fixture.get("/api/projects"), 404);
  assert_error(fixture.get("/api/storage?project_id=demo&kind=input"), 404);
  assert_error(fixture.get("/api/storage?project_id=demo&kind=output"), 404);
  assert_error(
    fixture.get("/api/storage?project_id=demo&kind=unknown"),
    400,
  );
  assert_error(
    fixture.get("/api/storage?project_id=demo&kind=input&limit=101"),
    400,
  );
  assert_error(
    fixture.get("/api/storage?project_id=demo&kind=input&offset=0&offset=1"),
    400,
  );
  assert_error(fixture.get("/api/projects?source=local"), 400);
  assert_error(fixture.get("/api/runs?source=local&origin=worker"), 400);
  assert_error(fixture.get("/api/runs?source=local&origin=a&origin=b"), 400);
}

#[test]
fn run_table_routes_discover_keys_validate_selection_and_sort_before_paging() {
  let fixture = Fixture::new();
  let columns = fixture.get("/api/run-columns?source=local");
  assert_eq!(columns.status, 200);
  let columns: Value = serde_json::from_slice(&columns.body).unwrap();
  assert_eq!(
    columns["available_columns"]["params"],
    json!([{"key":"/learning_rate","label":"learning_rate","run_count":2}])
  );
  assert_eq!(columns["available_columns"]["sampled_run_count"], 2);
  let page = fixture.get("/api/runs?source=local&param=%2Flearning_rate&metric=train%2Floss&reduction=last&sort=metric%3Atrain%2Floss&direction=asc&limit=1&offset=1");
  assert_eq!(page.status, 200);
  let page: Value = serde_json::from_slice(&page.body).unwrap();
  assert_eq!(page["runs"][0]["run_id"], "run-one");
  assert_eq!(
    page["runs"][0]["table_values"]["metrics"]["train/loss"],
    0.5
  );
  for query in [
    "param=/x&param=/x",
    "param=/bad~2pointer",
    "sort=metric:unselected",
    "sort=duration",
    "direction=up",
    "reduction=mean",
    "metric=loss&metric=loss",
    "param=/x&limit=101",
    "direction=asc&direction=desc",
  ] {
    assert_error(fixture.get(&format!("/api/runs?{query}")), 400);
  }
  assert_error(fixture.get("/api/run-columns?source=local&param=/x"), 400);
}

#[test]
fn artifacts_route_is_bounded_and_download_queries_are_strict() {
  let fixture = Fixture::new();
  let reply = fixture.get("/api/artifacts?source=local&run_id=run-one");
  assert_eq!(reply.status, 200);
  let catalog: Value = serde_json::from_slice(&reply.body).unwrap();
  assert_eq!(catalog["files"].as_array().unwrap().len(), 2);
  for url in [
    "/api/artifact?run_id=run-one&path=outputs/params.json",
    "/api/artifact?source=local&run_id=run-one&path=outputs/params.json&path=outputs/metrics.jsonl",
    "/api/artifact?source=local&run_id=run-one&path=outputs/params.json&token=private",
    "/api/artifact?source=local&run_id=run-one&run_id=run-two&path=outputs/params.json",
    "/api/artifact?source=local&run_id=run-one&path=outputs%2F..%2Frun-state.json",
  ] {
    let Err(reply) = artifact_download(&fixture.dashboard, url) else {
      panic!("unsafe download query accepted")
    };
    assert_error(reply, 400);
  }
  let Err(reply) = artifact_download(
    &fixture.dashboard,
    "/api/artifact?source=local&run_id=run-one&path=outputs/missing.pt",
  ) else {
    panic!("missing file accepted")
  };
  assert_error(reply, 404);
}

#[test]
fn local_download_socket_keeps_host_and_origin_boundaries() {
  use std::net::TcpListener;
  let fixture = Fixture::new();
  for (host, origin, expected) in [
    ("attacker.example", None, 403),
    (AUTHORITY, Some("https://attacker.example"), 403),
    (AUTHORITY, None, 200),
  ] {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    std::thread::scope(|scope| {
      scope.spawn(|| {
        let (stream, _) = listener.accept().unwrap();
        respond(&fixture.dashboard, AUTHORITY, stream);
      });
      let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
      client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
      write!(client, "HEAD /api/artifact?source=local&run_id=run-one&path=outputs/params.json HTTP/1.1\r\nHost: {host}\r\n").unwrap();
      if let Some(origin) = origin {
        write!(client, "Origin: {origin}\r\n").unwrap();
      }
      client.write_all(b"\r\n").unwrap();
      let mut response = String::new();
      client.read_to_string(&mut response).unwrap();
      assert!(response.starts_with(&format!("HTTP/1.1 {expected} ")));
      assert!(response.ends_with("\r\n\r\n"));
    });
  }
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
    "/api/runs?source=local&archival=all",
    "/api/runs?source=local&archival=archived",
    "/api/runs?source=local&archival=active&archival=archived",
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
    "/api/chart?run_id=run-one&x_axis=",
    "/api/chart?run_id=run-one&x_axis=time",
    "/api/chart?run_id=run-one&x_axis=STEP",
    "/api/chart?run_id=run-one&x_axis=step&x_axis=elapsed",
    "/api/chart?run_id=run-one&x_axis=step&%78_axis=step",
    "/api/compare?run_id=run-one&run_id=run-two&x_axis=elapsed",
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
fn charts_select_timestamp_axes_without_changing_filtered_elapsed_origin() {
  let fixture = Fixture::new();
  fs::write(
    fixture
      .dashboard
      .repo_root
      .join(".expri/runs/run-one/outputs/metrics.jsonl"),
    [
      json!({"step":5,"timestamp":"2026-10-03T01:00:00Z","metrics":{"setup":1}}),
      json!({"step":8,"timestamp":"2026-10-03T01:00:02.000000001Z","metrics":{"train/loss":0.5}}),
      json!({"step":3,"metrics":{"train/loss":0.25}}),
      json!({"step":0,"timestamp":"2026-10-03T01:00:05.000000002Z","metrics":{"train/loss":0.125}}),
    ]
    .map(|row| row.to_string())
    .join("\n")
      + "\n",
  )
  .unwrap();
  let base = "/api/chart?run_id=run-one&metric=train%2Floss";
  let default = fixture.get(base);
  let explicit = fixture.get(&format!("{base}&x_axis=step"));
  assert_eq!(default.status, 200);
  assert_eq!(default.body, explicit.body);
  let step = String::from_utf8(default.body).unwrap();
  assert!(step.contains("data-x-axis=\"step\""));
  assert_eq!(step.matches("<circle class=point ").count(), 3);
  let elapsed = fixture.get(&format!("{base}&x_axis=elapsed"));
  assert_eq!(elapsed.status, 200);
  assert!(elapsed.chart);
  let html = String::from_utf8(elapsed.body).unwrap();
  assert!(html.contains("data-x-axis=\"elapsed\""));
  assert!(html.contains("data-x-value=\"2000000001\""));
  assert!(html.contains("data-x-value=\"5000000002\""));
  assert!(html.contains("1 of 3 samples omitted"));
  assert_eq!(html.matches("<circle class=point ").count(), 2);
  let wall_clock = fixture.get(&format!("{base}&x_axis=wall_clock"));
  assert_eq!(wall_clock.status, 200);
  let html = String::from_utf8(wall_clock.body).unwrap();
  assert!(html.contains("data-x-axis=\"wall_clock\""));
  assert!(html.contains("Wall-clock time (UTC)"));
  assert!(html.contains("data-timestamp=\"2026-10-03T01:00:02.000000001Z\""));
}

#[test]
fn time_charts_without_timestamps_report_omissions_and_serve_a_valid_empty_view() {
  let fixture = Fixture::new();
  for x_axis in ["elapsed", "wall_clock"] {
    let reply = fixture.get(&format!(
      "/api/chart?run_id=run-one&metric=train%2Floss&x_axis={x_axis}"
    ));
    assert_eq!(reply.status, 200);
    assert!(reply.chart);
    let html = String::from_utf8(reply.body).unwrap();
    assert!(html.contains("1 of 1 samples omitted"));
    assert!(html.contains("No timestamped samples are available to plot"));
    assert!(!html.contains("<circle class=point "));
  }
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
