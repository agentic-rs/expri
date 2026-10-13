#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod support;

struct Fixture {
  directory: tempfile::TempDir,
  storage: PathBuf,
  stop: Arc<AtomicBool>,
  server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
  fn new(status: u16) -> Self {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path();
    let storage = base.join("worker-storage");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let server = thread::spawn(move || {
      while !stopping.load(Ordering::Acquire) {
        let Ok((mut socket, _)) = listener.accept() else {
          thread::sleep(Duration::from_millis(10));
          continue;
        };
        socket.set_nonblocking(false).unwrap();
        socket
          .set_read_timeout(Some(Duration::from_secs(1)))
          .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while let Ok(length) = socket.read(&mut buffer) {
          if length == 0 {
            break;
          }
          request.extend_from_slice(&buffer[..length]);
          let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
          };
          let header = String::from_utf8_lossy(&request[..end]);
          let length = header
            .lines()
            .find_map(|line| {
              let (name, value) = line.split_once(':')?;
              name
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
            })
            .unwrap_or(0);
          if request.len() >= end + 4 + length {
            break;
          }
        }
        let body = b"{\"error\":\"fixture failure\"}";
        let _ = write!(
          socket,
          "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        let _ = socket.write_all(body);
      }
    });
    fs::write(
      base.join("client.toml"),
      format!("url={url:?}\ntoken='private-fixture-token'\n"),
    )
    .unwrap();
    fs::create_dir_all(base.join("run-fixture/outputs")).unwrap();
    fs::write(base.join("run-fixture/snapshot.json"), b"{}").unwrap();
    fs::write(
      base.join("run-fixture/run-state.json"),
      serde_json::to_vec(&json!({"run_id": "run-fixture", "status": "completed"})).unwrap(),
    )
    .unwrap();
    Self {
      directory,
      storage,
      stop,
      server: Some(server),
    }
  }

  fn command(&self) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_expri"));
    command
      .current_dir(self.directory.path())
      .env("EXPRI_STORAGE_DIR", &self.storage);
    command
  }

  fn publish(&self) -> Command {
    let mut command = self.command();
    command.args([
      "service",
      "publish",
      "--json",
      "--config",
      "client.toml",
      "--run-dir",
      "run-fixture",
      "--project-id",
      "demo",
      "--origin",
      "worker",
      "--queue-dir",
      "queue",
    ]);
    command
  }

  fn status(&self) -> Value {
    json_report(
      self
        .command()
        .args(["service", "status", "--json"])
        .output()
        .unwrap(),
    )
  }
}

impl Drop for Fixture {
  fn drop(&mut self) {
    support::stop_worker_service(&self.storage);
    self.stop.store(true, Ordering::Release);
    if let Some(server) = self.server.take() {
      server.join().unwrap();
    }
  }
}

fn json_report(output: Output) -> Value {
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  let report: Value = serde_json::from_slice(&output.stdout).unwrap();
  assert!(!String::from_utf8_lossy(&output.stdout).contains("private-fixture-token"));
  report
}

#[test]
fn queued_publication_returns_offline_reuses_one_job_and_survives_observer_interrupt() {
  let fixture = Fixture::new(503);
  let before = Instant::now();
  let receipt = json_report(fixture.publish().output().unwrap());
  assert!(before.elapsed() < Duration::from_secs(3));
  assert_eq!(receipt["accepted"], true);
  assert_eq!(receipt["status"], "queued");
  let pid = receipt["service"]["pid"].clone();
  let repeated = json_report(fixture.publish().output().unwrap());
  assert_eq!(repeated["job_id"], receipt["job_id"]);
  assert_eq!(repeated["already_registered"], true);

  let mut observer = fixture
    .publish()
    .arg("--watch")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  thread::sleep(Duration::from_millis(300));
  assert!(observer.try_wait().unwrap().is_none());
  assert!(
    Command::new("kill")
      .args(["-INT", &observer.id().to_string()])
      .status()
      .unwrap()
      .success()
  );
  assert!(!observer.wait_with_output().unwrap().status.success());
  let observed = fixture.status();
  assert_eq!(observed["pid"], pid);
  assert_eq!(observed["running"], true);
  assert_eq!(observed["jobs_total"], 1);
  support::stop_worker_service(&fixture.storage);
  let stopped = fixture.status();
  assert_eq!(stopped["running"], false);
  assert_eq!(stopped["jobs_total"], 1);
  let restarted = json_report(
    fixture
      .command()
      .args(["service", "start", "--json"])
      .output()
      .unwrap(),
  );
  assert_ne!(restarted["pid"], pid);
  assert_eq!(fixture.status()["jobs_total"], 1);
}

#[test]
fn wait_reports_permanent_rejection_and_exits_unsuccessfully() {
  let fixture = Fixture::new(401);
  let output = fixture.publish().arg("--wait").output().unwrap();
  assert!(!output.status.success());
  let report: Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(report["status"], "error");
  assert!(!String::from_utf8_lossy(&output.stdout).contains("private-fixture-token"));
  assert!(String::from_utf8_lossy(&output.stderr).contains("publication has not completed"));
  assert_eq!(fixture.status()["running"], true);
}
