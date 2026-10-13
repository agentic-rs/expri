use std::collections::BTreeMap;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use expri_core::storage::StoragePaths;
use expri_core::system_profile::{
  CpuInventory, MetricKind, MetricReading, SystemInventory, SystemSample,
};
use expri_system::store::LocalStore;
use serde_json::Value;

fn command(root: &Path) -> Command {
  let mut command = Command::new(env!("CARGO_BIN_EXE_expri"));
  command
    .arg("system")
    .arg("--json")
    .arg("--storage-dir")
    .arg(root);
  command
}

fn json(output: Output) -> Value {
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  serde_json::from_slice(&output.stdout).unwrap()
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
  let directory = tempfile::tempdir().unwrap();
  let root = std::fs::canonicalize(directory.path())
    .unwrap()
    .join("global expri");
  (directory, root)
}

#[test]
fn missing_system_status_is_a_human_table_without_creating_storage() {
  let (_directory, root) = fixture();
  let output = Command::new(env!("CARGO_BIN_EXE_expri"))
    .args(["system", "--storage-dir"])
    .arg(&root)
    .arg("status")
    .output()
    .unwrap();
  assert!(output.status.success());
  let text = String::from_utf8(output.stdout).unwrap();
  assert!(text.contains("FIELD"));
  assert!(text.contains("Collector running"));
  assert!(serde_json::from_str::<Value>(&text).is_err());
  assert!(!root.exists());
}

fn seed(root: &Path) -> LocalStore {
  let paths = StoragePaths::from_root(root.into()).unwrap();
  paths.ensure_private().unwrap();
  let mut store = LocalStore::open(&paths.profile_database()).unwrap();
  let now = chrono::Utc::now().timestamp_millis();
  store
    .record(
      SystemInventory {
        observed_at_ms: now,
        hostname: "fixture".into(),
        os_name: Some("test".into()),
        os_version: None,
        kernel_version: None,
        architecture: "test".into(),
        cpu: CpuInventory {
          brand: "fixture CPU".into(),
          vendor: "test".into(),
          logical_cpu_count: 2,
          physical_core_count: Some(1),
        },
        memory_total_bytes: 8 * 1024_u64.pow(3),
        measurement_scope: "fixture".into(),
        devices: Vec::new(),
        warnings: Vec::new(),
      },
      SystemSample {
        sequence: 0,
        session_id: "session-fixture".into(),
        observed_at_ms: now,
        interval_ms: 10_000,
        collection_duration_ms: 1,
        inventory_revision: 0,
        metrics: BTreeMap::from([(
          "cpu/utilization_percent".into(),
          MetricReading {
            kind: MetricKind::Gauge,
            value: 12.5,
          },
        )]),
        counters: BTreeMap::from([("network/fixture/received_bytes".into(), u64::MAX)]),
        warnings: Vec::new(),
      },
    )
    .unwrap();
  store
}

#[test]
fn status_and_history_need_no_project_and_do_not_initialize_empty_storage() {
  let (directory, root) = fixture();
  std::fs::write(
    directory.path().join("expri.toml"),
    "not valid project TOML",
  )
  .unwrap();
  let status = json(
    command(&root)
      .arg("status")
      .current_dir(directory.path())
      .output()
      .unwrap(),
  );
  assert_eq!(status["collector_running"], false);
  assert!(status["local"].is_null());
  assert!(!root.exists());
  let history = command(&root).arg("history").output().unwrap();
  assert!(!history.status.success());
  assert!(!root.exists());
}

#[test]
fn cli_reads_durable_detail_and_hourly_history() {
  let (_directory, root) = fixture();
  let store = seed(&root);
  let id = store.machine_id().to_string();
  drop(store);
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let lock = root.join("collector.lock");
    std::fs::write(&lock, "").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o400)).unwrap();
  }
  let detail = json(command(&root).arg("history").output().unwrap());
  assert_eq!(detail["machine_id"], id);
  assert_eq!(
    detail["samples"][0]["metrics"]["cpu/utilization_percent"]["value"],
    12.5
  );
  assert_eq!(
    detail["samples"][0]["counters"]["network/fixture/received_bytes"],
    u64::MAX.to_string()
  );
  let hourly = json(
    command(&root)
      .args(["history", "--resolution", "hourly"])
      .output()
      .unwrap(),
  );
  let metrics = &hourly["hours"][0]["metrics"]["cpu/utilization_percent"];
  assert_eq!(
    metrics["sum"].as_f64().unwrap() / metrics["coverage_ms"].as_f64().unwrap(),
    12.5
  );
  let status = json(command(&root).arg("status").output().unwrap());
  assert_eq!(status["local"]["sample_sequence"], 1);
  assert_eq!(status["machine"]["inventory"]["hostname"], "fixture");
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

#[test]
fn foreground_daemon_is_singleton_and_keeps_sampling_when_publishing_fails() {
  let (_directory, root) = fixture();
  let client = root.parent().unwrap().join("machine.toml");
  std::fs::write(
    &client,
    "url='http://127.0.0.1:9'\ntoken_env='EXPRI_TEST_SYSTEM_ABSENT_TOKEN_03A76'\n",
  )
  .unwrap();
  let mut daemon = ChildGuard(
    command(&root)
      .args(["daemon", "--interval-seconds", "1", "--client-config"])
      .arg(client)
      .env_remove("EXPRI_TEST_SYSTEM_ABSENT_TOKEN_03A76")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .unwrap(),
  );
  let started = Instant::now();
  let status = loop {
    assert!(
      daemon.0.try_wait().unwrap().is_none(),
      "collector exited before inspection"
    );
    let output = command(&root).arg("status").output().unwrap();
    if output.status.success() {
      let status: Value = serde_json::from_slice(&output.stdout).unwrap();
      if status["local"]["sample_sequence"].as_u64().unwrap_or(0) >= 2
        && status["publishing"]["status"] == "paused"
      {
        break status;
      }
    }
    assert!(
      started.elapsed() < Duration::from_secs(15),
      "collector failed to produce samples"
    );
    thread::sleep(Duration::from_millis(100));
  };
  assert_eq!(status["collector_running"], true);
  assert!(
    status["publishing"]["last_error"]
      .as_str()
      .unwrap()
      .contains("environment variable")
  );
  let duplicate = command(&root)
    .args(["daemon", "--no-publish"])
    .output()
    .unwrap();
  assert!(!duplicate.status.success());
  assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already running"));
  let concurrent_sample = command(&root).arg("sample").output().unwrap();
  assert!(!concurrent_sample.status.success());
  assert!(String::from_utf8_lossy(&concurrent_sample.stderr).contains("already running"));
  drop(daemon);
  let status = json(command(&root).arg("status").output().unwrap());
  assert_eq!(status["collector_running"], false);
  assert!(status["local"]["sample_sequence"].as_u64().unwrap() >= 2);
}

#[test]
fn a_stalled_service_request_does_not_block_collection() {
  let (_directory, root) = fixture();
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let endpoint = format!("http://{}", listener.local_addr().unwrap());
  listener.set_nonblocking(true).unwrap();
  let (started_tx, started_rx) = mpsc::channel();
  let (release_tx, release_rx) = mpsc::channel();
  let server = thread::spawn(move || {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut stream = loop {
      match listener.accept() {
        Ok((stream, _)) => break stream,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          if Instant::now() >= deadline {
            return;
          }
          thread::sleep(Duration::from_millis(10));
        }
        Err(error) => panic!("{error}"),
      }
    };
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
      let read = stream.read(&mut chunk).unwrap();
      if read == 0 {
        return;
      }
      request.extend_from_slice(&chunk[..read]);
      assert!(request.len() < 16 * 1024);
    }
    started_tx.send(()).unwrap();
    // Hold capabilities open: collection must progress before any HTTP response.
    let _ = release_rx.recv_timeout(Duration::from_secs(15));
  });
  let client = root.parent().unwrap().join("machine.toml");
  std::fs::write(
    &client,
    format!("url='{endpoint}'\ntoken_env='EXPRI_TEST_SYSTEM_TOKEN'\n"),
  )
  .unwrap();
  let mut daemon = ChildGuard(
    command(&root)
      .args(["daemon", "--interval-seconds", "1", "--client-config"])
      .arg(client)
      .env("EXPRI_TEST_SYSTEM_TOKEN", "fixture-machine-token")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .unwrap(),
  );
  started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
  let started = Instant::now();
  loop {
    assert!(daemon.0.try_wait().unwrap().is_none());
    let output = command(&root).arg("status").output().unwrap();
    if output.status.success() {
      let status: Value = serde_json::from_slice(&output.stdout).unwrap();
      if status["local"]["sample_sequence"].as_u64().unwrap_or(0) >= 3 {
        assert_eq!(status["collector_running"], true);
        break;
      }
    }
    assert!(
      started.elapsed() < Duration::from_secs(8),
      "sampling waited for the HTTP response"
    );
    thread::sleep(Duration::from_millis(100));
  }
  drop(daemon);
  release_tx.send(()).unwrap();
  server.join().unwrap();
}
