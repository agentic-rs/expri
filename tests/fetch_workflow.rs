//! Exercise fetch report formatting against a local empty service catalog.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use expri_core::service::{Request, Response};

fn empty_catalog() -> (String, thread::JoinHandle<()>) {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let endpoint = format!("http://{}", listener.local_addr().unwrap());
  let server = thread::spawn(move || {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
      match listener.accept() {
        Ok((stream, _)) => break stream,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          assert!(
            Instant::now() < deadline,
            "fetch did not contact the catalog"
          );
          thread::sleep(Duration::from_millis(5));
        }
        Err(error) => panic!("{error}"),
      }
    };
    stream.set_nonblocking(false).unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "POST /v1/request HTTP/1.1\r\n");
    let mut content_length = None;
    let mut authenticated = false;
    loop {
      line.clear();
      assert!(reader.read_line(&mut line).unwrap() > 0);
      if line == "\r\n" {
        break;
      }
      let (name, value) = line.split_once(':').unwrap();
      if name.eq_ignore_ascii_case("content-length") {
        content_length = Some(value.trim().parse::<usize>().unwrap());
      }
      if name.eq_ignore_ascii_case("authorization") {
        assert_eq!(value.trim(), "Bearer fetch-fixture-token");
        authenticated = true;
      }
    }
    assert!(authenticated);
    let length = content_length.unwrap();
    assert!(length < 4096);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    assert!(matches!(
      serde_json::from_slice::<Request>(&body).unwrap(),
      Request::ListRuns { project_id, origin } if project_id == "example" && origin == "worker"
    ));
    let body = serde_json::to_vec(&Response::Runs { runs: Vec::new() }).unwrap();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
    stream.write_all(&body).unwrap();
  });
  (endpoint, server)
}

fn project(endpoint: &str) -> (tempfile::TempDir, PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  std::fs::write(
    root.join("client.toml"),
    format!("url='{endpoint}'\ntoken='fetch-fixture-token'\n"),
  )
  .unwrap();
  std::fs::write(
    root.join("expri.toml"),
    "[fetch]\nclient_config='client.toml'\nproject_id='example'\norigins=['worker']\n",
  )
  .unwrap();
  (temporary, root)
}

fn fetch(args: &[&str]) -> String {
  let (endpoint, server) = empty_catalog();
  let (_temporary, root) = project(&endpoint);
  let output = Command::new(env!("CARGO_BIN_EXE_expri"))
    .current_dir(&root)
    .args(["fetch", "--dry-run"])
    .args(args)
    .arg("--repo")
    .arg(&root)
    .output()
    .unwrap();
  server.join().unwrap();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  String::from_utf8(output.stdout).unwrap()
}

#[test]
fn fetch_defaults_to_a_table_and_json_overrides_quiet() {
  let table = fetch(&[]);
  assert!(table.contains("example"));
  assert!(table.contains("fetched"));
  assert!(serde_json::from_str::<serde_json::Value>(&table).is_err());
  assert!(fetch(&["--quiet"]).is_empty());
  let json = fetch(&["--quiet", "--json"]);
  let report: serde_json::Value = serde_json::from_str(&json).unwrap();
  assert_eq!(report["project_id"], "example");
  assert_eq!(report["dry_run"], true);
  assert_eq!(report["run_count"], 0);
}

#[test]
fn fetch_watch_json_writes_one_complete_object_per_line_even_when_quiet() {
  let (endpoint, server) = empty_catalog();
  let (_temporary, root) = project(&endpoint);
  let mut child = Command::new(env!("CARGO_BIN_EXE_expri"))
    .current_dir(&root)
    .args(["fetch", "--watch", "--quiet", "--json"])
    .arg("--repo")
    .arg(&root)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let stdout = child.stdout.take().unwrap();
  let (sender, receiver) = mpsc::channel();
  let reader = thread::spawn(move || {
    let mut line = String::new();
    let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
    let _ = sender.send(result);
  });
  let line = receiver.recv_timeout(Duration::from_secs(10));
  let _ = child.kill();
  let output = child.wait_with_output().unwrap();
  reader.join().unwrap();
  server.join().unwrap();
  let line = line.unwrap().unwrap();
  let report: serde_json::Value = serde_json::from_str(&line).unwrap_or_else(|error| {
    panic!(
      "watch summary is not a JSON line: {error}; stderr: {}",
      String::from_utf8_lossy(&output.stderr)
    )
  });
  assert!(line.ends_with('\n'));
  assert_eq!(line.lines().count(), 1);
  assert_eq!(report["project_id"], "example");
  assert_eq!(report["status"], "fetched");
  assert_eq!(report["run_count"], 0);
  assert!(report.get("runs").is_none());
}
