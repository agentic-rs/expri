#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

const CACHED_SOURCE: &str = "cached:gpu rental";
const CACHED_QUERY: &str = "cached%3Agpu%20rental";

struct Fixture {
  _root: tempfile::TempDir,
  repo: PathBuf,
  config: PathBuf,
}

impl Fixture {
  fn new(populated: bool) -> Self {
    let root = tempfile::Builder::new()
      .prefix("expri dashboard 'quoted' ")
      .tempdir()
      .unwrap();
    let repo = fs::canonicalize(root.path())
      .unwrap()
      .join("experiment repo");
    fs::create_dir(&repo).unwrap();
    let config = repo.join("expri.toml");
    let transport = repo.join("must not run transport");
    fs::write(
      &transport,
      "#!/bin/sh\ntouch transport-was-launched\nexit 91\n",
    )
    .unwrap();
    fs::set_permissions(&transport, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(&config, format!(
      "[project]\nname='Dashboard experiment'\n[download]\nresults_dir='results cache'\n[target.unused]\nhost='offline-rental'\nremote_dir='/unavailable/remote'\ntransport='ctl'\nctl_bin={}\n",
      serde_json::to_string(&transport.to_string_lossy()).unwrap(),
    )).unwrap();
    let fixture = Self {
      _root: root,
      repo,
      config,
    };
    if populated {
      fixture.add_run(
        "local",
        "run.shared",
        "train vision",
        "completed",
        Some(0.25),
      );
      fixture.add_run("local", "run.second", "evaluate", "failed", None);
      fixture.add_run(
        CACHED_SOURCE,
        "run.shared",
        "train vision",
        "completed",
        Some(8.0),
      );
      fixture.add_run(
        CACHED_SOURCE,
        "run.second",
        "train vision",
        "completed",
        Some(9.0),
      );
    }
    fixture
  }

  fn runs_dir(&self, source: &str) -> PathBuf {
    if source == "local" {
      self.repo.join(".expri/runs")
    } else {
      self.repo.join("results cache/gpu rental/runs")
    }
  }

  fn add_run(
    &self,
    source: &str,
    id: &str,
    task: &str,
    status: &str,
    loss: Option<f64>,
  ) -> PathBuf {
    let run = self.runs_dir(source).join(id);
    fs::create_dir_all(&run).unwrap();
    fs::write(
      run.join("run-state.json"),
      serde_json::to_vec(&json!({
        "schema_version":1, "run_id":id, "task":task, "status":status,
        "started_at":"2026-10-03T00:00:00Z", "finished_at":"2026-10-03T00:01:00Z",
        "exit_code":if status == "failed" {7} else {0},
        "code_dir":run.join("code"), "output_dir":run.join("outputs"),
      }))
      .unwrap(),
    )
    .unwrap();
    if let Some(loss) = loss {
      fs::create_dir_all(run.join("outputs")).unwrap();
      fs::create_dir_all(run.join("logs")).unwrap();
      fs::write(
        run.join("outputs/metrics.jsonl"),
        format!(
          "{}\n{}\n",
          json!({"schema_version":1,"step":0,"metrics":{"train/loss":10.0}}),
          json!({"schema_version":1,"step":1,"metrics":{"train/loss":loss}}),
        ),
      )
      .unwrap();
      fs::write(
        run.join("outputs/params.json"),
        serde_json::to_vec(&json!({
          "schema_version":1,"params":{"learning_rate":loss,"model":"small <vision>"},
        }))
        .unwrap(),
      )
      .unwrap();
      fs::write(run.join("logs/stdout.log"), b"first\nsecond\nlast line\n").unwrap();
    }
    if source != "local" {
      fs::write(
        run.join("pull-state.json"),
        serde_json::to_vec(&json!({
          "schema_version":1,"target_name":"gpu rental","run_id":id,
          "remote_run_dir":format!("/remote/.expri/runs/{id}"),
          "pulled_at":"2026-10-03T01:00:00Z",
          "selected_files":["run-state.json","outputs/metrics.jsonl","outputs/params.json"],
        }))
        .unwrap(),
      )
      .unwrap();
    }
    run
  }
}

struct Server {
  child: Child,
  address: SocketAddr,
}

impl Server {
  fn start(fixture: &Fixture, target: Option<&str>) -> Self {
    let mut command = Command::new(env!("CARGO_BIN_EXE_expri"));
    command.current_dir(&fixture.repo);
    if let Some(target) = target {
      command.args(["-T", target]);
    }
    let mut child = command
      .args([
        "dashboard",
        "--config",
        fixture.config.to_str().unwrap(),
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--port",
        "0",
      ])
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
      let mut line = String::new();
      let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
      let _ = send.send(result);
    });
    let line = match receive.recv_timeout(Duration::from_secs(10)) {
      Ok(Ok(line)) if line.contains("http://") => line,
      result => {
        let _ = child.kill();
        let _ = child.wait();
        let mut stderr = String::new();
        child
          .stderr
          .take()
          .unwrap()
          .read_to_string(&mut stderr)
          .unwrap();
        panic!("dashboard failed to advertise its URL: {result:?}\n{stderr}");
      }
    };
    let url = line
      .split_whitespace()
      .find(|part| part.starts_with("http://"))
      .unwrap();
    let address: SocketAddr = url
      .trim_start_matches("http://")
      .trim_end_matches('/')
      .parse()
      .unwrap();
    assert_eq!(
      address.ip(),
      std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    Self { child, address }
  }

  fn request(&self, method: &str, path: &str) -> Response {
    let request = format!(
      "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
      self.address
    );
    self
      .raw(request.as_bytes())
      .expect("HTTP response to a valid request")
  }

  fn raw(&self, request: &[u8]) -> Option<Response> {
    let mut connection = TcpStream::connect(self.address).unwrap();
    connection
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();
    connection
      .set_write_timeout(Some(Duration::from_secs(5)))
      .unwrap();
    connection.write_all(request).unwrap();
    connection.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    if let Err(error) = connection.read_to_end(&mut response) {
      if response.is_empty()
        && matches!(
          error.kind(),
          std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof
        )
      {
        return None;
      }
      panic!("read HTTP response: {error}");
    }
    if response.is_empty() {
      return None;
    }
    let boundary = response
      .windows(4)
      .position(|part| part == b"\r\n\r\n")
      .expect("HTTP header separator");
    let header = std::str::from_utf8(&response[..boundary]).unwrap();
    let mut lines = header.split("\r\n");
    let status = lines
      .next()
      .unwrap()
      .split_whitespace()
      .nth(1)
      .unwrap()
      .parse()
      .unwrap();
    let headers = lines
      .map(|line| {
        let (name, value) = line.split_once(':').unwrap();
        (name.to_ascii_lowercase(), value.trim().to_string())
      })
      .collect();
    Some(Response {
      status,
      headers,
      body: response[boundary + 4..].to_vec(),
    })
  }

  fn json(&self, path: &str) -> Value {
    let response = self.request("GET", path);
    assert_eq!(
      response.status,
      200,
      "{}",
      String::from_utf8_lossy(&response.body)
    );
    assert!(response.headers["content-type"].contains("application/json"));
    serde_json::from_slice(&response.body).unwrap()
  }
}

impl Drop for Server {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

struct Response {
  status: u16,
  headers: BTreeMap<String, String>,
  body: Vec<u8>,
}

#[derive(Debug, PartialEq)]
enum Entry {
  Directory(u32, std::time::SystemTime),
  File(u32, std::time::SystemTime, Vec<u8>),
  Symlink(PathBuf),
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Entry> {
  fn visit(root: &Path, directory: &Path, output: &mut BTreeMap<PathBuf, Entry>) {
    for entry in fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      let metadata = fs::symlink_metadata(&path).unwrap();
      let relative = path.strip_prefix(root).unwrap().to_path_buf();
      let value = if metadata.file_type().is_symlink() {
        Entry::Symlink(fs::read_link(&path).unwrap())
      } else if metadata.is_dir() {
        visit(root, &path, output);
        Entry::Directory(metadata.permissions().mode(), metadata.modified().unwrap())
      } else {
        Entry::File(
          metadata.permissions().mode(),
          metadata.modified().unwrap(),
          fs::read(&path).unwrap(),
        )
      };
      output.insert(relative, value);
    }
  }
  let mut output = BTreeMap::new();
  let metadata = fs::metadata(root).unwrap();
  output.insert(
    PathBuf::new(),
    Entry::Directory(metadata.permissions().mode(), metadata.modified().unwrap()),
  );
  visit(root, root, &mut output);
  output
}

#[test]
fn dashboard_reviews_local_and_cached_runs_refreshes_data_and_never_writes_to_the_project() {
  let fixture = Fixture::new(true);
  let initial = tree(&fixture.repo);
  let server = Server::start(&fixture, None);
  let catalog = server.json("/api/catalog");
  assert_eq!(catalog["project_name"], "Dashboard experiment");
  assert_eq!(catalog["initial_source"], "local");
  assert!(
    catalog["sources"]
      .as_array()
      .unwrap()
      .iter()
      .any(|source| source["source_id"] == CACHED_SOURCE)
  );
  let local = server.json("/api/runs?source=local");
  assert_eq!(local["runs"].as_array().unwrap().len(), 2);
  assert_eq!(local["offset"], 0);
  assert_eq!(local["next_offset"], Value::Null);
  let first_page = server.json("/api/runs?source=local&limit=1&offset=0");
  assert_eq!(first_page["total_count"], 2);
  assert_eq!(first_page["runs"].as_array().unwrap().len(), 1);
  assert_eq!(first_page["offset"], 0);
  assert_eq!(first_page["next_offset"], 1);
  let second_page = server.json("/api/runs?source=local&limit=1&offset=1");
  assert_eq!(second_page["total_count"], 2);
  assert_eq!(second_page["runs"].as_array().unwrap().len(), 1);
  assert_eq!(second_page["offset"], 1);
  assert_eq!(second_page["next_offset"], Value::Null);
  assert_ne!(
    first_page["runs"][0]["run_id"],
    second_page["runs"][0]["run_id"]
  );
  let exhausted = server.json("/api/runs?source=local&limit=1&offset=2");
  assert_eq!(exhausted["runs"], json!([]));
  assert_eq!(exhausted["total_count"], 2);
  assert_eq!(exhausted["next_offset"], Value::Null);
  let filtered = server
    .json("/api/runs?source=local&task=train%20vision&status=completed&search=shared&limit=1");
  assert_eq!(filtered["runs"][0]["run_id"], "run.shared");
  assert_eq!(filtered["total_count"], 1);
  assert_eq!(filtered["next_offset"], Value::Null);
  let detail = server.json("/api/run?source=local&run_id=run%2Eshared");
  assert_eq!(detail["metrics"]["train/loss"]["last"]["value"], 0.25);
  assert_eq!(detail["params"]["learning_rate"], 0.25);
  assert_eq!(detail["cache"], Value::Null);
  let cached = server.json(&format!("/api/run?source={CACHED_QUERY}&run_id=run.shared"));
  assert_eq!(cached["metrics"]["train/loss"]["last"]["value"], 8.0);
  assert_eq!(cached["cache"]["pulled_at"], "2026-10-03T01:00:00Z");
  let missing = server.json("/api/run?source=local&run_id=run.second");
  assert_eq!(missing["metrics"], json!({}));
  assert!(!missing["warnings"].as_array().unwrap().is_empty());
  let log = server.json("/api/log?source=local&run_id=run.shared&stream=stdout&tail=1");
  assert_eq!(log["content"], "last line\n");
  let no_log = server.json("/api/log?source=local&run_id=run.second&stream=stderr&tail=100");
  assert_eq!(no_log["missing"], true);
  assert_eq!(no_log["content"], "");
  let comparison = server.json("/api/compare?source=local&run_id=run.shared&run_id=run.second&metric=train%2Floss&reduction=min");
  assert_eq!(
    comparison["comparison"]["runs"][0]["values"]["train/loss"]["value"],
    0.25
  );
  assert_eq!(
    comparison["comparison"]["runs"][1]["values"]["train/loss"],
    Value::Null
  );
  let chart = server.request(
    "GET",
    "/api/chart?source=local&run_id=run.shared&metric=train%2Floss",
  );
  assert_eq!(chart.status, 200);
  assert!(chart.headers["content-security-policy"].contains("frame-ancestors 'self'"));
  let html = std::str::from_utf8(&chart.body).unwrap();
  assert!(html.contains("<svg") && html.contains("train/loss"));
  assert!(!html.contains("small <vision>"));
  for (path, content_type) in [
    ("/", "text/html"),
    ("/app.js", "javascript"),
    ("/styles.css", "text/css"),
  ] {
    let response = server.request("GET", path);
    assert_eq!(response.status, 200);
    assert!(response.headers["content-type"].contains(content_type));
    assert!(!response.body.is_empty());
    let asset = String::from_utf8_lossy(&response.body);
    assert!(!asset.contains("https://cdn") && !asset.contains("https://unpkg"));
    assert_eq!(
      response
        .headers
        .get("x-content-type-options")
        .map(String::as_str),
      Some("nosniff")
    );
    assert!(response.headers["content-security-policy"].contains("frame-ancestors 'none'"));
  }
  let head = server.request("HEAD", "/");
  assert_eq!(head.status, 200);
  assert!(head.body.is_empty());
  assert_eq!(tree(&fixture.repo), initial);

  let run = fixture.runs_dir("local").join("run.shared");
  let mut metrics = fs::OpenOptions::new()
    .append(true)
    .open(run.join("outputs/metrics.jsonl"))
    .unwrap();
  writeln!(
    metrics,
    "{}",
    json!({"schema_version":1,"step":2,"metrics":{"train/loss":0.125}})
  )
  .unwrap();
  fixture.add_run("local", "run-new", "train vision", "completed", Some(0.75));
  let changed = tree(&fixture.repo);
  assert_eq!(
    server.json("/api/run?source=local&run_id=run.shared")["metrics"]["train/loss"]["last"]["value"],
    0.125
  );
  assert_eq!(server.json("/api/runs?source=local")["total_count"], 3);
  assert_eq!(tree(&fixture.repo), changed);
}

#[test]
fn cached_initial_source_and_empty_projects_need_no_environment_or_target_credentials() {
  let fixture = Fixture::new(true);
  let before = tree(&fixture.repo);
  let server = Server::start(&fixture, Some("gpu rental"));
  assert_eq!(server.json("/api/catalog")["initial_source"], CACHED_SOURCE);
  assert_eq!(
    server.json(&format!("/api/runs?source={CACHED_QUERY}"))["runs"]
      .as_array()
      .unwrap()
      .len(),
    2
  );
  assert_eq!(tree(&fixture.repo), before);
  drop(server);
  let empty = Fixture::new(false);
  let before = tree(&empty.repo);
  let server = Server::start(&empty, None);
  assert_eq!(server.json("/api/runs?source=local")["runs"], json!([]));
  assert!(!empty.repo.join(".expri").exists());
  assert_eq!(tree(&empty.repo), before);
}

#[test]
fn dashboard_rejects_unsafe_paths_mutations_and_malformed_requests_and_bounds_logs() {
  let fixture = Fixture::new(true);
  let run = fixture.runs_dir("local").join("run.shared");
  // NUL bytes expand sixfold when encoded as JSON; the HTTP payload must fit
  // even when the log tail contains the worst-case escaped text.
  fs::write(run.join("logs/stdout.log"), vec![0; 2 * 1024 * 1024]).unwrap();
  let secret = fixture.repo.join("private metric data");
  fs::write(
    &secret,
    b"{\"schema_version\":1,\"step\":1,\"metrics\":{\"DO_NOT_EXPOSE_PRIVATE_CONTENT\":999}}\n",
  )
  .unwrap();
  fs::remove_file(run.join("outputs/metrics.jsonl")).unwrap();
  symlink(&secret, run.join("outputs/metrics.jsonl")).unwrap();
  symlink(&secret, run.join("logs/stderr.log")).unwrap();
  symlink(&run, fixture.runs_dir("local").join("run.link")).unwrap();
  symlink(
    fixture.repo.join(".expri"),
    fixture.repo.join("results cache/linked"),
  )
  .unwrap();
  let before = tree(&fixture.repo);
  let server = Server::start(&fixture, None);
  for path in [
    "/api/run?source=local&run_id=..%2Frun.shared",
    "/api/run?source=local&run_id=%2Fetc%2Fpasswd",
    "/api/run?source=local&run_id=run.link",
    "/api/run?source=cached%3A..%2Foutside&run_id=run.shared",
    "/api/run?source=cached%3Alinked&run_id=run.shared",
    "/api/runs?source=unknown",
    "/api/runs?source=local&limit=1001",
    "/api/runs?source=local&offset=-1",
    "/api/runs?source=local&offset=184467440737095516160",
    "/api/runs?source=local&offset=0&offset=1",
    "/api/catalog?unexpected=1",
    "/api/run?source=local&run_id=run.shared&path=expri.toml",
    "/api/log?source=local&run_id=run.shared&stream=combined",
    "/api/log?source=local&run_id=run.shared&stream=stderr",
    "/api/log?source=local&run_id=run.shared&tail=1001",
    "/api/run?source=local&run_id=%GG",
    "/api/compare?source=local&run_id=run.shared&run_id=run.shared",
    "/api/compare?source=local&run_id=run.shared",
    "/api/compare?source=local&run_id=run.shared&run_id=run.second&reduction=mean",
    "/../../expri.toml",
  ] {
    let response = server.request("GET", path);
    assert!(
      response.status >= 400,
      "{path}: {}",
      String::from_utf8_lossy(&response.body)
    );
    assert!(!String::from_utf8_lossy(&response.body).contains("DO_NOT_EXPOSE_PRIVATE_CONTENT"));
  }
  assert!(
    server.json("/api/catalog")["sources"]
      .as_array()
      .unwrap()
      .iter()
      .all(|source| source["source_id"] != "cached:linked")
  );
  let unsafe_metric = server.json("/api/run?source=local&run_id=run.shared");
  assert_eq!(unsafe_metric["metrics"], json!({}));
  assert!(
    !serde_json::to_string(&unsafe_metric)
      .unwrap()
      .contains("DO_NOT_EXPOSE_PRIVATE_CONTENT")
  );
  assert!(!unsafe_metric["warnings"].as_array().unwrap().is_empty());
  let oversized_response = server.request("GET", "/api/log?source=local&run_id=run.shared&tail=1");
  assert_eq!(oversized_response.status, 200);
  assert!(oversized_response.body.len() <= 512 * 1024);
  let oversized: Value = serde_json::from_slice(&oversized_response.body).unwrap();
  assert_eq!(oversized["truncated"], true);
  assert!(oversized["content"].as_str().unwrap().len() <= 64 * 1024);
  let denied = server.request("POST", "/api/run?source=local&run_id=run.shared");
  assert_eq!(denied.status, 405);
  let malformed =
    server.raw(b"GET / HTTP/1.1\r\nHost: invalid.example\r\nConnection: close\r\n\r\n");
  assert!(malformed.is_none_or(|response| response.status >= 400));
  let malformed = server.raw(b"GET / HTTP/1.1\r\nBroken-Header\r\n\r\n");
  assert!(malformed.is_none_or(|response| response.status >= 400));
  let oversized_request = format!(
    "POST /api/run HTTP/1.1\r\nHost: {}\r\nContent-Length: 1073741824\r\nConnection: close\r\n\r\n",
    server.address
  );
  let refused = server.raw(oversized_request.as_bytes());
  assert!(refused.is_none_or(|response| response.status >= 400));
  for framing in [
    "Content-Length: 1073741824\r\n",
    "Content-Length: invalid\r\n",
    "Transfer-Encoding: chunked\r\n",
  ] {
    let request = format!(
      "GET /api/catalog HTTP/1.1\r\nHost: {}\r\n{framing}Connection: close\r\n\r\n",
      server.address
    );
    assert!(
      server
        .raw(request.as_bytes())
        .is_none_or(|response| response.status >= 400)
    );
  }
  assert_eq!(server.json("/api/catalog")["initial_source"], "local");
  assert_eq!(tree(&fixture.repo), before);
}

#[test]
fn dashboard_previews_bound_large_records_and_exact_metric_filters_keep_full_summaries() {
  let fixture = Fixture::new(true);
  let run = fixture.runs_dir("local").join("run.shared");
  let mut output =
    std::io::BufWriter::new(fs::File::create(run.join("outputs/metrics.jsonl")).unwrap());
  // The complete saved metric set exceeds the curve reader's point limit.
  // Summary responses and explicitly selected curves must still be usable.
  for step in 0..17_000 {
    let metrics: serde_json::Map<String, Value> = (0..60)
      .map(|index| (format!("metric_{index:02}"), json!(step + index)))
      .collect();
    writeln!(
      output,
      "{}",
      json!({"schema_version":1,"step":step,"metrics":metrics})
    )
    .unwrap();
  }
  output.flush().unwrap();
  let params: serde_json::Map<String, Value> = (0..30)
    .map(|index| (format!("parameter_{index:02}"), json!("x".repeat(30_000))))
    .collect();
  fs::write(
    run.join("outputs/params.json"),
    serde_json::to_vec(&json!({"schema_version":1,"params":params})).unwrap(),
  )
  .unwrap();
  fs::write(
    run.join("snapshot.json"),
    serde_json::to_vec(&json!({
      "schema_version":1,"source":{"kind":"git","git_head":"abc123"},
      "files":["DO_NOT_SEND_THE_SOURCE_INVENTORY".repeat(15_000)],
    }))
    .unwrap(),
  )
  .unwrap();
  fs::create_dir_all(run.join("environment")).unwrap();
  fs::write(
    run.join("environment/environment-state.json"),
    serde_json::to_vec(&json!({
      "schema_version":1,"python":"/remote/conda/bin/python",
      "combined_manifest":{
        "torch":{"version":"2.10.0+cu128"},
        "packages":{"torch":{"files":["DO_NOT_SEND_THE_PACKAGE_INVENTORY".repeat(15_000)]}},
      },
    }))
    .unwrap(),
  )
  .unwrap();
  let before = tree(&fixture.repo);
  let server = Server::start(&fixture, None);
  let response = server.request("GET", "/api/run?source=local&run_id=run.shared");
  assert_eq!(
    response.status,
    200,
    "{}",
    String::from_utf8_lossy(&response.body)
  );
  assert!(response.body.len() <= 512 * 1024);
  assert!(!String::from_utf8_lossy(&response.body).contains("DO_NOT_SEND_THE_SOURCE_INVENTORY"));
  assert!(!String::from_utf8_lossy(&response.body).contains("DO_NOT_SEND_THE_PACKAGE_INVENTORY"));
  let detail: Value = serde_json::from_slice(&response.body).unwrap();
  assert_eq!(detail["metrics"].as_object().unwrap().len(), 50);
  assert_eq!(detail["metric_count"], 60);
  assert_eq!(detail["metrics_truncated"], true);
  assert_eq!(detail["params_truncated"], true);
  assert_eq!(detail["snapshot"]["source"]["git_head"], "abc123");
  assert_eq!(detail["snapshot"]["file_count"], 1);
  assert_eq!(
    detail["environment"]["combined_manifest"]["torch"]["version"],
    "2.10.0+cu128"
  );
  assert_eq!(
    detail["environment"]["combined_manifest"]["package_count"],
    1
  );
  let comparison = server.json(
    "/api/compare?source=local&run_id=run.shared&run_id=run.second&metric=metric_59&reduction=last",
  );
  assert_eq!(
    comparison["comparison"]["metric_names"],
    json!(["metric_59"])
  );
  assert_eq!(
    comparison["comparison"]["runs"][0]["values"]["metric_59"]["value"],
    17058.0
  );
  for (query, plots) in [("", 4), ("&metric=metric_59", 1)] {
    let chart = server.request(
      "GET",
      &format!("/api/chart?source=local&run_id=run.shared{query}"),
    );
    assert_eq!(
      chart.status,
      200,
      "{}",
      String::from_utf8_lossy(&chart.body)
    );
    assert!(chart.body.len() <= 2 * 1024 * 1024);
    let html = std::str::from_utf8(&chart.body).unwrap();
    assert_eq!(html.matches("<svg ").count(), plots);
    assert!(html.matches("<circle ").count() <= plots * 600);
    assert!(html.contains("at most 600 points per run"));
    assert!(html.contains("Dashboard parameter preview"));
    assert!(!html.contains("parameter_24"));
    if query.is_empty() {
      assert!(html.contains("Showing the first 4 of 60 metrics"));
    } else {
      assert!(html.contains("metric_59"));
      assert!(html.contains("<td>17000</td><td>16999</td>"));
    }
  }
  assert_eq!(tree(&fixture.repo), before);
}
