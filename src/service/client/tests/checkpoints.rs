use std::collections::BTreeMap;
use std::sync::{
  Arc, Condvar, Mutex,
  atomic::{AtomicBool, Ordering},
  mpsc,
};

use super::*;
use crate::service::registrations;

type UploadReceipt = (FileTarget, u64, String, UploadState);

#[derive(Default)]
struct State {
  uploads: BTreeMap<String, UploadReceipt>,
  upload_ids: Vec<String>,
  object_puts: usize,
  metric_offsets: Vec<u64>,
  seals: usize,
}

struct Server {
  url: String,
  state: Arc<Mutex<State>>,
  stop: Arc<AtomicBool>,
  release: Arc<(Mutex<bool>, Condvar)>,
  started: mpsc::Receiver<()>,
  thread: Option<thread::JoinHandle<()>>,
}

impl Server {
  fn new(fail_completion: bool) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(State::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_send, started) = mpsc::channel();
    let server_url = url.clone();
    let server_state = state.clone();
    let server_stop = stop.clone();
    let server_release = release.clone();
    let fail_completion = Arc::new(AtomicBool::new(fail_completion));
    let thread = thread::spawn(move || {
      let mut handlers = Vec::new();
      while !server_stop.load(Ordering::Relaxed) {
        let (mut stream, _) = match listener.accept() {
          Ok(connection) => connection,
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(2));
            continue;
          }
          Err(error) => panic!("test server accept: {error}"),
        };
        let url = server_url.clone();
        let state = server_state.clone();
        let release = server_release.clone();
        let started = started_send.clone();
        let fail_completion = fail_completion.clone();
        handlers.push(thread::spawn(move || {
          let request = read_request(&mut stream);
          let mut headers = Vec::new();
          let (status, bytes) = if request.path == "/checkpoint" {
            started.send(()).unwrap();
            let (mutex, wake) = &*release;
            let (allowed, timeout) = wake
              .wait_timeout_while(mutex.lock().unwrap(), Duration::from_secs(5), |allowed| {
                !*allowed
              })
              .unwrap();
            assert!(
              *allowed && !timeout.timed_out(),
              "checkpoint test did not release its object request"
            );
            assert_eq!(request.body, b"completed-checkpoint");
            state.lock().unwrap().object_puts += 1;
            headers.push(("ETag", "checkpoint-part"));
            (200, Vec::new())
          } else {
            let request: Request = serde_json::from_slice(&request.body).unwrap();
            let mut state = state.lock().unwrap();
            let response = match request {
              Request::Capabilities => Response::Capabilities {
                features: vec!["tracking-v1".into()],
              },
              Request::PutDocument {
                revision,
                offset,
                total_size,
                data_base64,
                ..
              } => {
                let size =
                  base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_base64)
                    .unwrap()
                    .len() as u64;
                Response::DocumentAcknowledged {
                  revision,
                  offset: offset + size,
                  complete: offset + size == total_size,
                }
              }
              Request::AppendTracking {
                path,
                offset,
                data_base64,
                ..
              } => {
                let size =
                  base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_base64)
                    .unwrap()
                    .len() as u64;
                if path == "outputs/metrics.jsonl" {
                  state.metric_offsets.push(offset + size);
                }
                Response::Acknowledged {
                  offset: offset + size,
                }
              }
              Request::BeginUpload {
                upload_id,
                target,
                size,
                sha256,
              } => {
                state.upload_ids.push(upload_id.clone());
                let receipt = state.uploads.entry(upload_id.clone()).or_insert_with(|| {
                  (
                    target,
                    size,
                    sha256,
                    UploadState {
                      upload_id,
                      part_size: 8 * 1024 * 1024,
                      parts: Vec::new(),
                      complete: false,
                    },
                  )
                });
                Response::Upload {
                  upload: receipt.3.clone(),
                }
              }
              Request::PartUrl { .. } => Response::Url {
                url: format!("{url}/checkpoint"),
              },
              Request::RecordPart { upload_id, part } => {
                let receipt = state.uploads.get_mut(&upload_id).unwrap();
                receipt.3.parts.push(part);
                Response::Upload {
                  upload: receipt.3.clone(),
                }
              }
              Request::CompleteUpload { upload_id } => {
                if fail_completion.swap(false, Ordering::SeqCst) {
                  drop(state);
                  write!(
                    stream,
                    "HTTP/1.1 503 Test\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                  )
                  .unwrap();
                  return;
                }
                let (target, size, sha256, upload) = state.uploads.get_mut(&upload_id).unwrap();
                assert_eq!(upload.parts.len(), 1);
                upload.complete = true;
                Response::File {
                  file: FileRecord {
                    target: target.clone(),
                    size: *size,
                    sha256: Some(sha256.clone()),
                    storage: FileStorage::Object,
                  },
                }
              }
              Request::SealRun { .. } => {
                state.seals += 1;
                Response::Archive {
                  archive: ArchiveRecord {
                    status: "pending".into(),
                    incomplete: false,
                    file: None,
                    last_error: None,
                  },
                }
              }
              _ => panic!("unexpected checkpoint test request"),
            };
            (200, serde_json::to_vec(&response).unwrap())
          };
          write!(
            stream,
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
            bytes.len()
          )
          .unwrap();
          for (key, value) in headers {
            write!(stream, "{key}: {value}\r\n").unwrap();
          }
          stream.write_all(b"\r\n").unwrap();
          stream.write_all(&bytes).unwrap();
        }));
      }
      for handler in handlers {
        handler.join().unwrap();
      }
    });
    Self {
      url,
      state,
      stop,
      release,
      started,
      thread: Some(thread),
    }
  }

  fn release(&self) {
    *self.release.0.lock().unwrap() = true;
    self.release.1.notify_all();
  }
}

impl Drop for Server {
  fn drop(&mut self) {
    self.release();
    self.stop.store(true, Ordering::Relaxed);
    self.thread.take().unwrap().join().unwrap();
  }
}

fn run(root: &Path, server: &Server) -> PushOptions {
  let run = root.join("run-1");
  fs::directories(&run.join("outputs")).unwrap();
  fs::atomic_json(
    &run.join("run-state.json"),
    &json!({"run_id":"run-1","status":"running"}),
  )
  .unwrap();
  std::fs::write(run.join("snapshot.json"), "{}").unwrap();
  std::fs::write(run.join("outputs/checkpoint.pt"), "completed-checkpoint").unwrap();
  std::fs::write(run.join("outputs/metrics.jsonl"), "{\"step\":1}\n").unwrap();
  registrations::register_with_labels(&run, "outputs/checkpoint.pt", &["latest".into()]).unwrap();
  PushOptions {
    config: config(root, &server.url),
    run_dir: run,
    project_id: "project".into(),
    origin: "worker".into(),
    artifacts: Vec::new(),
    watch: true,
    queue_dir: root.join("queue"),
  }
}

fn wait_for_task(publisher: &Publisher) {
  let deadline = Instant::now() + Duration::from_secs(4);
  loop {
    let status = registrations::list(&publisher.run_dir).unwrap()["files"][0]["sync_status"]
      .as_str()
      .unwrap()
      .to_string();
    if status != "uploading" {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "checkpoint transfer did not finish"
    );
    thread::sleep(Duration::from_millis(5));
  }
}

#[test]
fn live_checkpoint_transfer_does_not_block_metrics_and_terminal_seal_waits_for_it() {
  let (_temporary, root) = root();
  let server = Server::new(false);
  let options = run(&root, &server);
  let mut publisher = Publisher::new(&options).unwrap();
  assert!(!publisher.cycle(&mut |_| Ok(())).unwrap());
  server.started.recv_timeout(Duration::from_secs(3)).unwrap();
  let initial_offset = server.state.lock().unwrap().metric_offsets[0];
  let mut metrics = std::fs::OpenOptions::new()
    .append(true)
    .open(options.run_dir.join("outputs/metrics.jsonl"))
    .unwrap();
  metrics.write_all(b"{\"step\":2}\n").unwrap();
  let started = Instant::now();
  assert!(!publisher.cycle(&mut |_| Ok(())).unwrap());
  assert!(started.elapsed() < Duration::from_secs(1));
  assert!(server.state.lock().unwrap().metric_offsets.last().unwrap() > &initial_offset);
  fs::atomic_json(
    &options.run_dir.join("run-state.json"),
    &json!({"run_id":"run-1","status":"completed"}),
  )
  .unwrap();
  assert!(!publisher.cycle(&mut |_| Ok(())).unwrap());
  assert_eq!(server.state.lock().unwrap().seals, 0);
  assert_eq!(
    registrations::list(&options.run_dir).unwrap()["files"][0]["sync_status"],
    "uploading"
  );
  server.release();
  wait_for_task(&publisher);
  let deadline = Instant::now() + Duration::from_secs(3);
  while !publisher.cycle(&mut |_| Ok(())).unwrap() {
    assert!(Instant::now() < deadline);
    thread::sleep(Duration::from_millis(5));
  }
  assert_eq!(
    registrations::list(&options.run_dir).unwrap()["files"][0]["sync_status"],
    "cloud"
  );
  let inventory: Value = serde_json::from_slice(
    &std::fs::read(options.run_dir.join(crate::run_artifacts::INVENTORY_PATH)).unwrap(),
  )
  .unwrap();
  let checkpoint = inventory["files"]
    .as_array()
    .unwrap()
    .iter()
    .find(|file| file["path"] == "outputs/checkpoint.pt")
    .unwrap();
  assert_eq!(checkpoint["sync_status"], "cloud");
  assert_eq!(checkpoint["labels"], json!(["latest"]));
  assert_eq!(
    checkpoint["sha256"],
    fs::hex(&Sha256::digest(b"completed-checkpoint"))
  );
  assert!(server.state.lock().unwrap().seals > 0);
}

#[test]
fn completed_parts_and_readiness_survive_publisher_restart_after_failure() {
  let (_temporary, root) = root();
  let server = Server::new(true);
  server.release();
  let options = run(&root, &server);
  let mut publisher = Publisher::new(&options).unwrap();
  assert!(!publisher.cycle(&mut |_| Ok(())).unwrap());
  server.started.recv_timeout(Duration::from_secs(3)).unwrap();
  wait_for_task(&publisher);
  assert_eq!(
    registrations::list(&options.run_dir).unwrap()["files"][0]["sync_status"],
    "registered"
  );
  let receipt: Value = serde_json::from_slice(
    &std::fs::read(publisher.queue.directory.join("checkpoints/queue.json")).unwrap(),
  )
  .unwrap();
  let queued = &receipt["files"]["outputs/checkpoint.pt"];
  assert_eq!(queued["upload"]["parts"].as_array().unwrap().len(), 1);
  let id = queued["upload"]["upload_id"].as_str().unwrap().to_string();
  drop(publisher);
  fs::atomic_json(
    &options.run_dir.join("run-state.json"),
    &json!({"run_id":"run-1","status":"completed"}),
  )
  .unwrap();
  let mut publisher = Publisher::new(&options).unwrap();
  let deadline = Instant::now() + Duration::from_secs(3);
  while !publisher.cycle(&mut |_| Ok(())).unwrap() {
    assert!(Instant::now() < deadline);
    thread::sleep(Duration::from_millis(5));
  }
  let state = server.state.lock().unwrap();
  assert_eq!(
    state.object_puts, 1,
    "acknowledged checkpoint bytes were retransmitted"
  );
  assert!(state.upload_ids.iter().all(|previous| previous == &id));
}

#[test]
fn changed_checkpoint_needs_attention_without_stopping_metadata_sync() {
  let (_temporary, root) = root();
  let server = Server::new(false);
  let options = run(&root, &server);
  std::fs::write(
    options.run_dir.join("outputs/checkpoint.pt"),
    "different-checkpoint",
  )
  .unwrap();
  let mut publisher = Publisher::new(&options).unwrap();
  assert!(!publisher.cycle(&mut |_| Ok(())).unwrap());
  let deadline = Instant::now() + Duration::from_secs(3);
  while registrations::list(&options.run_dir).unwrap()["files"][0]["sync_status"]
    != "needs_attention"
  {
    assert!(Instant::now() < deadline);
    thread::sleep(Duration::from_millis(5));
  }
  assert!(!publisher.cycle(&mut |_| Ok(())).unwrap());
  assert!(!server.state.lock().unwrap().metric_offsets.is_empty());
  assert_eq!(server.state.lock().unwrap().object_puts, 0);
  assert!(server.state.lock().unwrap().upload_ids.is_empty());
}
