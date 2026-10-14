#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
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

  fn control(&self, action: &str, component: Option<&str>) -> Value {
    let mut command = self.command();
    command.args(["service", action, "--json"]);
    if let Some(component) = component {
      command.args(["--component", component]);
    }
    json_report(command.output().unwrap())
  }

  fn await_component(&self, component: &str, phase: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let report = self.status();
      if report["components"][component]["phase"] == phase {
        return report;
      }
      assert!(
        Instant::now() < deadline,
        "{component} did not reach {phase}: {report}"
      );
      thread::sleep(Duration::from_millis(25));
    }
  }

  fn await_collection_ready(&self) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let report = self.status();
      if report["collection_ready"] == true {
        return report;
      }
      assert!(
        Instant::now() < deadline,
        "collection did not become ready: {report}"
      );
      thread::sleep(Duration::from_millis(25));
    }
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

#[test]
fn collection_can_pause_and_restart_without_restarting_the_service_or_publishers() {
  let fixture = Fixture::new(503);
  fs::create_dir_all(&fixture.storage).unwrap();
  fs::write(
    fixture.storage.join("config.toml"),
    "[system_profile]\ninterval_seconds=1\n",
  )
  .unwrap();
  fixture.control("start", None);
  let initial = fixture.await_collection_ready();
  let pid = initial["pid"].clone();
  let generation = initial["components"]["collection"]["generation"]
    .as_u64()
    .unwrap();
  fixture.await_component("publishing", "running");

  fixture.control("stop", Some("collection"));
  let stopped = fixture.await_component("collection", "stopped");
  assert_eq!(stopped["pid"], pid);
  assert_eq!(stopped["collection_ready"], false);
  assert_eq!(
    stopped["components"]["collection"]["desired_running"],
    false
  );
  assert_eq!(stopped["components"]["publishing"]["phase"], "running");
  let sequence = stopped["collection"]["sample_sequence"].clone();
  thread::sleep(Duration::from_millis(1200));
  assert_eq!(fixture.status()["collection"]["sample_sequence"], sequence);

  fixture.control("start", Some("collection"));
  let resumed = fixture.await_collection_ready();
  assert_eq!(resumed["pid"], pid);
  assert!(
    resumed["components"]["collection"]["generation"]
      .as_u64()
      .unwrap()
      > generation
  );
  assert_eq!(resumed["components"]["collection"]["desired_running"], true);
}

#[test]
fn paused_publication_keeps_jobs_and_saved_choices_survive_process_restart() {
  let fixture = Fixture::new(503);
  // Pausing an offline component saves the choice without starting a process.
  let paused = fixture.control("stop", Some("publishing"));
  assert_eq!(paused["running"], false);
  let choices: Value =
    serde_json::from_slice(&fs::read(fixture.storage.join("service.json")).unwrap()).unwrap();
  assert_eq!(choices, json!({"components": {"publishing": "disable"}}));
  assert!(
    !fixture
      .storage
      .join("worker-service/components.json")
      .exists()
  );
  let receipt = json_report(fixture.publish().output().unwrap());
  assert_eq!(receipt["accepted"], true);
  let initial = fixture.await_component("publishing", "stopped");
  let pid = initial["pid"].clone();
  assert_eq!(
    initial["components"]["publishing"]["desired_running"],
    false
  );
  assert_eq!(initial["jobs_total"], 1);

  let mut observer = fixture
    .publish()
    .arg("--wait")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let deadline = Instant::now() + Duration::from_secs(3);
  while observer.try_wait().unwrap().is_none() {
    if Instant::now() >= deadline {
      observer.kill().unwrap();
      let _ = observer.wait();
      panic!("waiting for paused publication did not return promptly");
    }
    thread::sleep(Duration::from_millis(25));
  }
  let output = observer.wait_with_output().unwrap();
  assert!(!output.status.success());
  let report: Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(report["status"], "paused");
  assert_eq!(fixture.status()["pid"], pid);

  fixture.control("stop", Some("collection"));
  fixture.await_component("collection", "stopped");
  let saved_choices = fs::read(fixture.storage.join("service.json")).unwrap();
  fixture.control("restart", None);
  let restarted = fixture.await_component("publishing", "stopped");
  assert_ne!(restarted["pid"], pid);
  assert_eq!(
    restarted["components"]["collection"]["desired_running"],
    false
  );
  assert_eq!(
    restarted["components"]["publishing"]["desired_running"],
    false
  );
  assert_eq!(restarted["jobs_total"], 1);
  assert_eq!(
    fs::read(fixture.storage.join("service.json")).unwrap(),
    saved_choices
  );
  fixture.control("restart", Some("publishing"));
  let resumed = fixture.await_component("publishing", "running");
  assert_eq!(resumed["pid"], restarted["pid"]);
  assert_eq!(resumed["components"]["collection"]["phase"], "stopped");
  assert_eq!(
    resumed["components"]["collection"]["desired_running"],
    false
  );
}

#[test]
fn restarting_a_healthy_component_reloads_configuration_and_keeps_paused_peers() {
  let fixture = Fixture::new(503);
  fs::create_dir_all(&fixture.storage).unwrap();
  let config = fixture.storage.join("config.toml");
  fs::write(&config, "[system_profile]\ninterval_seconds=1\n").unwrap();
  fixture.control("stop", Some("publishing"));
  fixture.control("restart", Some("collection"));
  let initial = fixture.await_collection_ready();
  let generation = initial["components"]["collection"]["generation"]
    .as_u64()
    .unwrap();
  assert_eq!(
    generation, 1,
    "offline restart must not cancel its freshly started component"
  );
  fs::write(&config, "[system_profile]\ninterval_seconds=2\n").unwrap();

  // Idempotent start leaves healthy threads alone; restart reloads settings.
  let unchanged = fixture.control("start", Some("collection"));
  assert_eq!(
    unchanged["components"]["collection"]["generation"],
    generation
  );
  fixture.control("restart", Some("collection"));
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    let status = fixture.status();
    if status["collection_ready"] == true
      && status["components"]["collection"]["generation"]
        .as_u64()
        .unwrap()
        > generation
    {
      assert_eq!(status["pid"], initial["pid"]);
      assert_eq!(status["collection"]["interval_seconds"], 2);
      assert_eq!(status["components"]["publishing"]["desired_running"], false);
      assert_eq!(status["components"]["publishing"]["phase"], "stopped");
      break;
    }
    assert!(
      Instant::now() < deadline,
      "component did not restart: {status}"
    );
    thread::sleep(Duration::from_millis(25));
  }
  let choices: Value =
    serde_json::from_slice(&fs::read(fixture.storage.join("service.json")).unwrap()).unwrap();
  assert_eq!(choices["components"]["collection"], "enable");
  assert_eq!(choices["components"]["publishing"], "disable");
}

#[test]
fn a_manual_service_file_is_read_without_starting_and_applied_by_whole_restart() {
  let fixture = Fixture::new(503);
  fs::create_dir_all(&fixture.storage).unwrap();
  let path = fixture.storage.join("service.json");
  let contents = br#"{"components":{"publishing":"disable"}}"#;
  fs::write(&path, contents).unwrap();
  fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
  let status = fixture.status();
  assert_eq!(status["running"], false);
  assert_eq!(status["components"]["publishing"]["desired_running"], false);
  assert_eq!(status["components"]["collection"]["desired_running"], true);
  assert!(!fixture.storage.join("worker-service").exists());
  assert_eq!(fs::read(&path).unwrap(), contents);
  assert_eq!(
    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
    0o644
  );

  fixture.control("restart", None);
  let restarted = fixture.await_component("publishing", "stopped");
  assert_eq!(restarted["running"], true);
  assert_eq!(
    restarted["components"]["publishing"]["desired_running"],
    false
  );
  assert_eq!(fs::read(path).unwrap(), contents);
}
