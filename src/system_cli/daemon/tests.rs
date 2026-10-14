use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use super::*;

#[derive(Clone)]
struct TestControl {
  paths: StoragePaths,
  stop: Arc<AtomicBool>,
  ready: Sender<(String, u64)>,
  statuses: Arc<Mutex<Vec<Value>>>,
}

impl TestControl {
  fn new(paths: StoragePaths) -> (Self, Receiver<(String, u64)>) {
    let (ready, receiver) = mpsc::channel();
    (
      Self {
        paths,
        stop: Arc::new(AtomicBool::new(false)),
        ready,
        statuses: Arc::new(Mutex::new(Vec::new())),
      },
      receiver,
    )
  }
}

impl CollectionControl for TestControl {
  fn stop_flag(&self) -> Arc<AtomicBool> {
    self.stop.clone()
  }
  fn is_stopping(&self) -> bool {
    self.stop.load(Ordering::Acquire)
  }
  fn wait(&self, duration: Duration) -> bool {
    thread::sleep(duration.min(Duration::from_millis(10)));
    self.is_stopping()
  }
  fn collection_ready(&self, machine_id: String, sample_sequence: u64) {
    let store = LocalStore::open_readonly(&self.paths.profile_database())
      .unwrap()
      .unwrap();
    let committed = store.status().unwrap();
    assert_eq!(committed.machine_id, machine_id);
    assert_eq!(committed.sample_sequence, sample_sequence);
    assert!(committed.inventory_revision > 0);
    assert!(store.profile().unwrap().is_some());
    self.ready.send((machine_id, sample_sequence)).unwrap();
  }
  fn collection_status(&self, detail: Value) {
    self.statuses.lock().unwrap().push(detail);
  }
  fn component_status(&self, detail: Value) {
    self.statuses.lock().unwrap().push(detail);
  }
}

fn paths(directory: &tempfile::TempDir) -> StoragePaths {
  StoragePaths::from_root(directory.path().canonicalize().unwrap()).unwrap()
}

fn start(
  paths: StoragePaths,
  interval: Duration,
) -> (TestControl, Receiver<(String, u64)>, JoinHandle<Result<()>>) {
  let (context, ready) = TestControl::new(paths.clone());
  let collecting = context.clone();
  let task = thread::spawn(move || {
    collect(
      paths,
      CollectionConfig {
        interval,
        publishing: PublishingMode::Disabled,
      },
      collecting,
    )
  });
  (context, ready, task)
}

fn start_publisher(
  paths: StoragePaths,
  publishing: PublishingMode,
) -> (TestControl, JoinHandle<Result<()>>) {
  let (context, _) = TestControl::new(paths.clone());
  let publishing_context = context.clone();
  let task = thread::spawn(move || publish(paths, publishing, publishing_context));
  (context, task)
}

#[test]
fn saved_publisher_errors_never_include_configuration_or_json_snippets() {
  let secret = "private-machine-token";
  let toml_error = toml::from_str::<Value>(&format!("token = '{secret}' garbage")).unwrap_err();
  let json_error = serde_json::from_str::<u64>(&format!("\"{secret}\"")).unwrap_err();
  for error in [ExpriError::Toml(toml_error), ExpriError::Json(json_error)] {
    let detail = safe_publishing_error(&error);
    assert!(!detail.contains(secret));
    assert!(detail.contains("invalid"));
  }
}

#[test]
fn readiness_is_a_committed_supported_observation_without_publishing_credentials() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  let (context, ready, task) = start(paths.clone(), Duration::from_secs(10));
  let (machine_id, sequence) = ready.recv_timeout(Duration::from_secs(10)).unwrap();
  assert_eq!(sequence, 1);
  assert!(matches!(
    lock::try_lock_file_readonly(&paths.root().join("collector.lock")).unwrap(),
    LockAttempt::Busy
  ));
  let run = paths.root().join("run-readiness");
  std::fs::create_dir_all(run.join("outputs")).unwrap();
  let mut store = LocalStore::open_existing(&paths.profile_database())
    .unwrap()
    .unwrap();
  let first = store.status().unwrap().first_sample_at_ms.unwrap();
  assert!(
    store
      .begin_run_capture(&run, "run-readiness", first)
      .is_ok(),
    "readiness includes the capture support marker"
  );
  assert_eq!(
    read_status(&paths.root().join("publishing.json")).unwrap(),
    None,
    "collection does not initialize another component"
  );
  let (publishing, publisher) = start_publisher(paths.clone(), PublishingMode::Unconfigured);
  publisher.join().unwrap().unwrap();
  assert!(!publishing.is_stopping());
  assert_eq!(
    read_status(&paths.root().join("publishing.json"))
      .unwrap()
      .unwrap()["status"],
    "unconfigured"
  );
  let stopped = Instant::now();
  context.stop.store(true, Ordering::Release);
  task.join().unwrap().unwrap();
  assert!(
    stopped.elapsed() < Duration::from_secs(2),
    "stopping does not wait for the ten-second interval"
  );
  assert_eq!(
    read_status(&paths.root().join("collector.json"))
      .unwrap()
      .unwrap()["status"],
    "stopped"
  );
  assert!(
    collector_lease(&paths).is_ok(),
    "the stop releases the collection lease"
  );
  let (restarted, ready, task) = start(paths.clone(), Duration::from_secs(10));
  let (next_machine, next_sequence) = ready.recv_timeout(Duration::from_secs(10)).unwrap();
  assert_eq!(next_machine, machine_id);
  assert!(next_sequence > sequence);
  restarted.stop.store(true, Ordering::Release);
  task.join().unwrap().unwrap();
  let (_, publisher) = start_publisher(paths.clone(), PublishingMode::Disabled);
  publisher.join().unwrap().unwrap();
  assert_eq!(
    read_status(&paths.root().join("publishing.json"))
      .unwrap()
      .unwrap()["status"],
    "disabled"
  );
}

#[test]
fn invalid_publisher_configuration_is_explicitly_paused_without_blocking_readiness() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  let config = paths.root().join("missing-machine-client.toml");
  let (context, ready, task) = start(paths.clone(), Duration::from_secs(1));
  ready.recv_timeout(Duration::from_secs(10)).unwrap();
  let (publishing, publisher) = start_publisher(paths.clone(), PublishingMode::Configured(config));
  publisher.join().unwrap().unwrap();
  let status = read_status(&paths.root().join("publishing.json"))
    .unwrap()
    .unwrap();
  assert_eq!(status["status"], "paused");
  assert!(status["last_error"].is_string());
  assert!(
    publishing
      .statuses
      .lock()
      .unwrap()
      .iter()
      .any(|detail| detail["publishing"]["status"] == "paused")
  );
  assert!(!context.is_stopping());
  assert!(!publishing.is_stopping());
  wait_for_sequence(&paths, 2);
  context.stop.store(true, Ordering::Release);
  task.join().unwrap().unwrap();
  assert!(collector_lease(&paths).is_ok());
}

#[test]
fn collection_continues_during_publish_failure_and_retries_keep_the_original_auth_snapshot() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let endpoint = format!("http://{}", listener.local_addr().unwrap());
  let config = paths.root().join("machine-client.toml");
  std::fs::write(
    &config,
    format!("url='{endpoint}'\ntoken='fixture-machine-owner-key'\n"),
  )
  .unwrap();
  let (requests, received) = mpsc::channel();
  let server = thread::spawn(move || {
    for _ in 0..2 {
      let deadline = Instant::now() + Duration::from_secs(10);
      let mut stream = loop {
        match listener.accept() {
          Ok((stream, _)) => break stream,
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            assert!(Instant::now() < deadline, "publisher did not retry");
            thread::sleep(Duration::from_millis(10));
          }
          Err(error) => panic!("fixture accept: {error}"),
        }
      };
      stream.set_nonblocking(false).unwrap();
      stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
      let mut reader = BufReader::new(stream.try_clone().unwrap());
      let mut headers = String::new();
      let mut length = 0;
      loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
          break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
          length = value.trim().parse::<usize>().unwrap();
        }
        headers.push_str(&line);
      }
      assert!(length <= 1024 * 1024);
      let mut body = vec![0; length];
      reader.read_exact(&mut body).unwrap();
      requests.send(headers).unwrap();
      let body = br#"{"error":"fixture-machine-owner-key"}"#;
      write!(
        stream,
        "HTTP/1.1 503 Unavailable\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
      )
      .unwrap();
      stream.write_all(body).unwrap();
    }
  });
  let (context, ready, task) = start(paths.clone(), Duration::from_secs(1));
  ready.recv_timeout(Duration::from_secs(10)).unwrap();
  let (publishing, publisher) =
    start_publisher(paths.clone(), PublishingMode::Configured(config.clone()));
  assert!(
    received
      .recv_timeout(Duration::from_secs(10))
      .unwrap()
      .contains("Bearer fixture-machine-owner-key")
  );
  std::fs::write(
    config,
    format!("url='{endpoint}'\ntoken='fixture-other-run-key'\n"),
  )
  .unwrap();
  assert!(
    received
      .recv_timeout(Duration::from_secs(10))
      .unwrap()
      .contains("Bearer fixture-machine-owner-key")
  );
  wait_for_sequence(&paths, 3);
  let statuses = serde_json::to_string(&*publishing.statuses.lock().unwrap()).unwrap();
  assert!(statuses.contains("retrying"));
  assert!(!statuses.contains("fixture-machine-owner-key"));
  assert!(!statuses.contains("fixture-other-run-key"));
  publishing.stop.store(true, Ordering::Release);
  publisher.join().unwrap().unwrap();
  assert!(!context.is_stopping());
  let sequence = LocalStore::open_readonly(&paths.profile_database())
    .unwrap()
    .unwrap()
    .status()
    .unwrap()
    .sample_sequence;
  wait_for_sequence(&paths, sequence + 1);
  context.stop.store(true, Ordering::Release);
  task.join().unwrap().unwrap();
  server.join().unwrap();
  assert_eq!(
    read_status(&paths.root().join("publishing.json"))
      .unwrap()
      .unwrap()["status"],
    "stopped"
  );
  assert!(collector_lease(&paths).is_ok());
}

fn wait_for_sequence(paths: &StoragePaths, expected: u64) {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    let status = LocalStore::open_readonly(&paths.profile_database())
      .unwrap()
      .unwrap()
      .status()
      .unwrap();
    if status.sample_sequence >= expected {
      return;
    }
    assert!(Instant::now() < deadline, "local observations stopped");
    thread::sleep(Duration::from_millis(20));
  }
}

#[test]
fn capture_export_can_stop_and_resume_while_collection_keeps_its_durable_outbox() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  let (collecting, ready, collection) = start(paths.clone(), Duration::from_secs(1));
  ready.recv_timeout(Duration::from_secs(10)).unwrap();
  let mut store = LocalStore::open_existing(&paths.profile_database())
    .unwrap()
    .unwrap();
  let first = store.status().unwrap().first_sample_at_ms.unwrap();
  let run = paths.root().join("run-export-resume");
  std::fs::create_dir_all(run.join("outputs")).unwrap();
  store
    .begin_run_capture(&run, "run-export-resume", first)
    .unwrap();
  drop(store);
  let output = run.join(expri_core::run_system::RUN_SYSTEM_PATH);
  let (exporting, _) = TestControl::new(paths.clone());
  let export_paths = paths.clone();
  let export_control = exporting.clone();
  let exporter = thread::spawn(move || materialize_loop(export_paths, export_control));
  wait_for_sequence(&paths, 2);
  let deadline = Instant::now() + Duration::from_secs(3);
  while !std::fs::read_to_string(&output)
    .unwrap()
    .contains("\"sequence\":2")
  {
    assert!(
      Instant::now() < deadline,
      "exporter did not flush the sample"
    );
    thread::sleep(Duration::from_millis(20));
  }
  exporting.stop.store(true, Ordering::Release);
  exporter.join().unwrap();
  let paused = std::fs::read(&output).unwrap();
  assert!(!collecting.is_stopping());
  let sequence = LocalStore::open_readonly(&paths.profile_database())
    .unwrap()
    .unwrap()
    .status()
    .unwrap()
    .sample_sequence;
  wait_for_sequence(&paths, sequence + 1);
  assert_eq!(std::fs::read(&output).unwrap(), paused);

  let (restarted, _) = TestControl::new(paths.clone());
  let export_paths = paths.clone();
  let export_control = restarted.clone();
  let exporter = thread::spawn(move || materialize_loop(export_paths, export_control));
  let deadline = Instant::now() + Duration::from_secs(3);
  while std::fs::read(&output).unwrap() == paused {
    assert!(
      Instant::now() < deadline,
      "restart did not flush the outbox"
    );
    thread::sleep(Duration::from_millis(20));
  }
  restarted.stop.store(true, Ordering::Release);
  exporter.join().unwrap();
  collecting.stop.store(true, Ordering::Release);
  collection.join().unwrap().unwrap();
  assert!(collector_lease(&paths).is_ok());
}

#[test]
fn status_files_are_atomic_private_and_reject_nonregular_or_oversized_reads() {
  let directory = tempfile::tempdir().unwrap();
  write_status(
    directory.path(),
    "collector.json",
    &json!({"status": "stopped"}),
  )
  .unwrap();
  let status = directory.path().join("collector.json");
  assert_eq!(read_status(&status).unwrap().unwrap()["status"], "stopped");
  std::fs::write(&status, vec![b' '; 16 * 1024 + 1]).unwrap();
  assert!(read_status(&status).is_err());
  assert!(read_status(directory.path()).is_err());
  #[cfg(unix)]
  {
    use std::os::unix::fs::{PermissionsExt, symlink};
    assert_eq!(
      std::fs::metadata(&status).unwrap().permissions().mode() & 0o777,
      0o600
    );
    let linked = directory.path().join("linked.json");
    symlink(&status, &linked).unwrap();
    assert!(read_status(&linked).is_err());
  }
}

#[test]
fn a_collector_failure_does_not_cancel_other_component_controls() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  let lease = collector_lease(&paths).unwrap();
  let (collector, _) = TestControl::new(paths.clone());
  let (exporter, _) = TestControl::new(paths.clone());
  let outcome = collect(
    paths,
    CollectionConfig {
      interval: Duration::from_secs(1),
      publishing: PublishingMode::Disabled,
    },
    collector.clone(),
  );
  assert!(outcome.is_err());
  assert!(!collector.is_stopping());
  assert!(!exporter.is_stopping());
  drop(lease);
}
