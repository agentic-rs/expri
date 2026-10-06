use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use sha2::{Digest, Sha256};

use super::super::InputGetOptions;
use super::*;

struct HttpRequest {
  path: String,
  headers: String,
  body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> HttpRequest {
  stream.set_nonblocking(false).unwrap();
  stream
    .set_read_timeout(Some(Duration::from_secs(3)))
    .unwrap();
  let mut reader = BufReader::new(stream.try_clone().unwrap());
  let mut first = String::new();
  reader.read_line(&mut first).unwrap();
  let mut headers = String::new();
  let mut length = 0;
  loop {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "incomplete test HTTP request");
    if line == "\r\n" {
      break;
    }
    if let Some((key, value)) = line.split_once(':')
      && key.eq_ignore_ascii_case("content-length")
    {
      length = value.trim().parse::<usize>().unwrap();
    }
    headers.push_str(&line);
  }
  assert!(length <= MAX_REQUEST);
  let mut body = vec![0; length];
  reader.read_exact(&mut body).unwrap();
  HttpRequest {
    path: first.split_whitespace().nth(1).unwrap().to_string(),
    headers,
    body,
  }
}

fn mock(
  count: usize,
  mut handler: impl FnMut(HttpRequest, &str) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  let origin = url.clone();
  let task = thread::spawn(move || {
    for _ in 0..count {
      let deadline = Instant::now() + Duration::from_secs(5);
      let mut stream = loop {
        match listener.accept() {
          Ok((stream, _)) => break stream,
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            assert!(
              Instant::now() < deadline,
              "test HTTP client did not connect"
            );
            thread::sleep(Duration::from_millis(5));
          }
          Err(error) => panic!("test HTTP accept: {error}"),
        }
      };
      let request = read_request(&mut stream);
      let (status, headers, body) = handler(request, &origin);
      write!(stream, "HTTP/1.1 {status} Test\r\nConnection: close\r\n").unwrap();
      if !headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("content-length"))
      {
        write!(stream, "Content-Length: {}\r\n", body.len()).unwrap();
      }
      for (key, value) in headers {
        write!(stream, "{key}: {value}\r\n").unwrap();
      }
      stream.write_all(b"\r\n").unwrap();
      stream.write_all(&body).unwrap();
    }
  });
  (url, task)
}

fn config(root: &Path, url: &str) -> PathBuf {
  let path = root.join("client.toml");
  // Use an existing non-secret variable rather than mutating process environment.
  std::fs::write(&path, format!("url={url:?}\ntoken_env='PATH'\n")).unwrap();
  path
}

fn root() -> (tempfile::TempDir, PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let path = std::fs::canonicalize(temporary.path()).unwrap();
  (temporary, path)
}

#[test]
fn manual_and_automatic_diagnostics_redact_the_in_memory_service_token() {
  let (_temporary, root) = root();
  let api = Api::new(&config(&root, "http://example.invalid")).unwrap();
  let token = std::env::var("PATH").unwrap();
  assert_eq!(
    api.redact(&format!("rejected {token}\n retry")),
    "rejected [redacted] retry"
  );
}

#[test]
fn automatic_status_stays_small_when_manual_queue_has_long_artifact_names() {
  let (_temporary, root) = root();
  let run_dir = root.join("run-progress");
  fs::directories(&run_dir).unwrap();
  std::fs::write(
    run_dir.join("run-state.json"),
    br#"{"run_id":"run-progress","status":"running"}"#,
  )
  .unwrap();
  let options = PushOptions {
    config: config(&root, "http://example.invalid"),
    run_dir,
    project_id: "project".into(),
    origin: "worker".into(),
    artifacts: Vec::new(),
    watch: true,
    queue_dir: root.join("queue"),
  };
  let mut publisher = Publisher::new(&options).unwrap();
  for index in 0..64 {
    let path = format!("outputs/{}/{index}.bin", vec!["a".repeat(90); 10].join("/"));
    validate_run_path(&path).unwrap();
    publisher.queue.state.files.insert(
      path.clone(),
      SavedFile {
        target: run_target(&publisher.scope, &path),
        size: 0,
        sha256: "0".repeat(64),
        snapshot: None,
        upload: UploadState {
          upload_id: format!("finished-{index}"),
          part_size: 8 * 1024 * 1024,
          parts: Vec::new(),
          complete: true,
        },
      },
    );
  }
  publisher
    .queue
    .state
    .streams
    .insert("outputs/metrics.jsonl".into(), 100);
  let progress = publisher.progress(false);
  assert_eq!(progress["files_completed"], 64);
  assert_eq!(progress["stream_offsets"]["outputs/metrics.jsonl"], 100);
  assert!(serde_json::to_vec_pretty(&progress).unwrap().len() < 1024);
  assert_eq!(
    publisher.report(false)["files"].as_array().unwrap().len(),
    64,
    "manual report lost its selected file list"
  );
}

#[test]
fn rejected_authentication_keeps_pending_queue_and_redacts_the_response_before_bounding() {
  let (_temporary, root) = root();
  let token = std::env::var("PATH").unwrap();
  let echoed = token.clone();
  let (url, task) = mock(1, move |request, _| {
    assert!(matches!(
      serde_json::from_slice::<Request>(&request.body).unwrap(),
      Request::BeginUpload { .. }
    ));
    (
      401,
      Vec::new(),
      serde_json::to_vec(&json!({"error": format!("{} {echoed}", "x".repeat(480))})).unwrap(),
    )
  });
  let run_dir = root.join("run-auth");
  fs::directories(&run_dir).unwrap();
  std::fs::write(run_dir.join("snapshot.json"), br#"{"run_id":"run-auth"}"#).unwrap();
  std::fs::write(
    run_dir.join("run-state.json"),
    br#"{"run_id":"run-auth","status":"running"}"#,
  )
  .unwrap();
  let options = PushOptions {
    config: config(&root, &url),
    run_dir,
    project_id: "project".into(),
    origin: "worker".into(),
    artifacts: Vec::new(),
    watch: true,
    queue_dir: root.join("queue"),
  };
  let mut publisher = Publisher::new(&options).unwrap();
  let failure = publisher.cycle(&mut |_| Ok(())).unwrap_err();
  assert!(Publisher::authentication_rejected(&failure));
  let detail = publisher.error_text(&failure);
  assert!(!detail.contains(&token));
  assert!(!detail.contains(token.chars().take(20).collect::<String>().as_str()));
  let saved = &publisher.queue.state.files["snapshot.json"];
  assert!(!saved.upload.complete);
  assert!(
    publisher
      .queue
      .directory
      .join(saved.snapshot.as_ref().unwrap())
      .is_file()
  );
  assert!(publisher.queue.directory.join("queue.json").is_file());
  task.join().unwrap();
}

#[test]
fn failed_begin_preserves_id_and_immutable_metadata_snapshot_for_retry() {
  let (_temporary, root) = root();
  let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
  let received = seen.clone();
  let (url, task) = mock(2, move |request, _| {
    assert_eq!(request.path, "/v1/request");
    let Request::BeginUpload {
      upload_id,
      target,
      size,
      sha256,
    } = serde_json::from_slice(&request.body).unwrap()
    else {
      panic!("expected begin");
    };
    received
      .lock()
      .unwrap()
      .push((upload_id, target, size, sha256));
    (503, Vec::new(), br#"{"error":"fixture offline"}"#.to_vec())
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let scope = RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-1".into(),
  };
  let path = root.join("run-state.json");
  let original = br#"{"run_id":"run-1","status":"running"}"#;
  std::fs::write(&path, original).unwrap();
  let directory = root.join("queue");
  let owner = json!({"endpoint": api.endpoint, "scope": scope});
  let mut queue = Queue::new(directory.clone(), owner.clone()).unwrap();
  assert!(
    sync_file(
      &api,
      &mut queue,
      "run-state.json",
      run_target(&scope, "run-state.json"),
      &path,
      true
    )
    .is_err()
  );
  let saved = &queue.state.files["run-state.json"];
  let id = saved.upload.upload_id.clone();
  let snapshot = directory.join(saved.snapshot.as_ref().unwrap());
  assert_eq!(std::fs::read(&snapshot).unwrap(), original);
  assert!(
    Queue::new(directory.clone(), owner.clone()).is_err(),
    "concurrent uploader acquired the queue"
  );
  drop(queue);
  std::fs::write(&path, br#"{"run_id":"run-1","status":"completed"}"#).unwrap();
  let mut queue = Queue::new(directory, owner).unwrap();
  assert_eq!(queue.state.files["run-state.json"].upload.upload_id, id);
  assert!(
    sync_file(
      &api,
      &mut queue,
      "run-state.json",
      run_target(&scope, "run-state.json"),
      &path,
      true
    )
    .is_err()
  );
  task.join().unwrap();
  let received = seen.lock().unwrap();
  assert_eq!(received.len(), 2);
  assert_eq!(received[0], received[1]);
  assert_eq!(std::fs::read(snapshot).unwrap(), original);
}

#[test]
fn source_mutation_discards_acknowledged_parts_before_retrying_original_bytes() {
  for snapshot in [false, true] {
    let (_temporary, root) = root();
    let original = b"original-artifact";
    let changed = b"modified-artifact";
    let path = root.join("artifact.bin");
    std::fs::write(&path, original).unwrap();
    let queue_dir = root.join("queue");
    let target = FileTarget::Input {
      project_id: "project".into(),
      input_id: "artifact-v1".into(),
    };
    let expected = target.clone();
    let source = path.clone();
    let saved_queue = queue_dir.clone();
    let observed = std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), Vec::new())));
    let received = observed.clone();
    let mut current_id = String::new();
    let mut acknowledged = std::collections::BTreeMap::<String, Vec<CompletedPart>>::new();
    let (url, task) = mock(9, move |request, origin| {
      if request.path != "/v1/request" {
        received.lock().unwrap().1.push(request.body);
        return (
          200,
          vec![("ETag".into(), "part-receipt".into())],
          Vec::new(),
        );
      }
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::BeginUpload {
          upload_id,
          target,
          size,
          sha256,
        } => {
          assert_eq!(target, expected);
          assert_eq!(size, original.len() as u64);
          assert_eq!(sha256, fs::hex(&Sha256::digest(original)));
          let mut seen = received.lock().unwrap();
          if let Some(previous) = seen.0.first() {
            assert_ne!(
              &upload_id, previous,
              "unsafe acknowledged parts were reused"
            );
          }
          seen.0.push(upload_id.clone());
          current_id = upload_id.clone();
          Response::Upload {
            upload: UploadState {
              upload_id: upload_id.clone(),
              part_size: 8 * 1024 * 1024,
              parts: acknowledged.entry(upload_id).or_default().clone(),
              complete: false,
            },
          }
        }
        Request::PartUrl {
          upload_id,
          part_number,
        } => {
          assert_eq!(upload_id, current_id);
          assert_eq!(part_number, 1);
          if received.lock().unwrap().0.len() == 1 {
            let queued: Value =
              serde_json::from_slice(&std::fs::read(saved_queue.join("queue.json")).unwrap())
                .unwrap();
            let uploaded_source = if snapshot {
              saved_queue.join(queued["files"]["artifact"]["snapshot"].as_str().unwrap())
            } else {
              source.clone()
            };
            std::fs::write(uploaded_source, changed).unwrap();
          }
          Response::Url {
            url: format!("{origin}/part"),
          }
        }
        Request::RecordPart { upload_id, part } => {
          assert_eq!(upload_id, current_id);
          acknowledged.get_mut(&upload_id).unwrap().push(part);
          Response::Upload {
            upload: UploadState {
              upload_id: upload_id.clone(),
              part_size: 8 * 1024 * 1024,
              parts: acknowledged[&upload_id].clone(),
              complete: false,
            },
          }
        }
        Request::CompleteUpload { upload_id } => {
          assert_eq!(upload_id, current_id);
          assert_eq!(received.lock().unwrap().0.len(), 2);
          Response::File {
            file: FileRecord {
              target: expected.clone(),
              size: original.len() as u64,
              sha256: Some(fs::hex(&Sha256::digest(original))),
              storage: FileStorage::Object,
            },
          }
        }
        _ => panic!("unexpected test request"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    });
    let api = Api::new(&config(&root, &url)).unwrap();
    let owner = json!({"endpoint": api.endpoint, "target": target});
    let mut queue = Queue::new(queue_dir.clone(), owner.clone()).unwrap();
    let error = sync_file(
      &api,
      &mut queue,
      "artifact",
      target.clone(),
      &path,
      snapshot,
    )
    .unwrap_err();
    assert!(error.to_string().contains("receipts were discarded"));
    assert!(!queue.state.files.contains_key("artifact"));
    assert_eq!(
      std::fs::read(&path).unwrap(),
      if snapshot { original } else { changed }
    );
    assert!(std::fs::read_dir(&queue_dir).unwrap().all(|entry| {
      !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("upload-")
    }));
    drop(queue);
    // Restoring the exact queued SHA must still start a fresh multipart upload.
    std::fs::write(&path, original).unwrap();
    let mut queue = Queue::new(queue_dir, owner).unwrap();
    assert!(!queue.state.files.contains_key("artifact"));
    sync_file(&api, &mut queue, "artifact", target, &path, snapshot).unwrap();
    task.join().unwrap();
    let seen = observed.lock().unwrap();
    assert_eq!(seen.0.len(), 2);
    assert_eq!(seen.1, vec![changed.to_vec(), original.to_vec()]);
    assert!(queue.state.files["artifact"].upload.complete);
  }
}

#[test]
fn private_input_digest_failure_preserves_old_file_and_signed_requests_have_no_bearer_token() {
  let (_temporary, root) = root();
  let target = FileTarget::Input {
    project_id: "project".into(),
    input_id: "dataset-v1".into(),
  };
  let body = b"new input bytes".to_vec();
  let record = FileRecord {
    target: target.clone(),
    size: body.len() as u64,
    sha256: Some("a".repeat(64)),
    storage: FileStorage::Object,
  };
  let (url, task) = mock(3, move |request, origin| {
    if request.path == "/v1/request" {
      assert!(
        request
          .headers
          .to_ascii_lowercase()
          .contains("authorization: bearer ")
      );
      let request: Request = serde_json::from_slice(&request.body).unwrap();
      let response = match request {
        Request::GetFile { .. } => Response::File {
          file: record.clone(),
        },
        Request::DownloadUrl { .. } => Response::Url {
          url: format!("{origin}/object?X-Amz-Signature=private-signature"),
        },
        _ => panic!("unexpected test request"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      assert!(
        !request
          .headers
          .to_ascii_lowercase()
          .contains("authorization:"),
        "service token leaked to signed object request"
      );
      assert!(
        request
          .headers
          .to_ascii_lowercase()
          .contains("range: bytes=0-14")
      );
      (
        206,
        vec![("Content-Range".into(), format!("bytes 0-14/{}", body.len()))],
        body.clone(),
      )
    }
  });
  let destination = root.join("dataset.bin");
  std::fs::write(&destination, b"existing verified input").unwrap();
  let before = std::fs::metadata(&destination).unwrap().modified().unwrap();
  let error = input_get(InputGetOptions {
    config: config(&root, &url),
    project_id: "project".into(),
    input_id: "dataset-v1".into(),
    destination: destination.clone(),
  })
  .unwrap_err();
  task.join().unwrap();
  assert!(error.to_string().contains("SHA256"));
  assert!(!error.to_string().contains("private-signature"));
  assert_eq!(
    std::fs::read(&destination).unwrap(),
    b"existing verified input"
  );
  assert_eq!(
    std::fs::metadata(destination).unwrap().modified().unwrap(),
    before
  );
}

#[test]
fn truncated_object_body_does_not_leak_signed_url_or_replace_previous_input() {
  let (_temporary, root) = root();
  let record = FileRecord {
    target: FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset-v1".into(),
    },
    size: 15,
    sha256: Some("a".repeat(64)),
    storage: FileStorage::Object,
  };
  // One catalog request, then three fresh signed URLs and failed Range requests.
  let (url, task) = mock(7, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::GetFile { .. } => Response::File {
          file: record.clone(),
        },
        Request::DownloadUrl { .. } => Response::Url {
          url: format!("{origin}/object?X-Amz-Signature=private-signature"),
        },
        _ => panic!("unexpected test request"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      assert_eq!(request.path, "/object?X-Amz-Signature=private-signature");
      assert!(
        request
          .headers
          .to_ascii_lowercase()
          .contains("range: bytes=0-14")
      );
      assert!(
        !request
          .headers
          .to_ascii_lowercase()
          .contains("authorization:")
      );
      (
        206,
        vec![
          ("Content-Range".into(), "bytes 0-14/15".into()),
          ("Content-Length".into(), "15".into()),
        ],
        b"cut".to_vec(),
      )
    }
  });
  let destination = root.join("dataset.bin");
  std::fs::write(&destination, b"existing verified input").unwrap();
  let error = input_get(InputGetOptions {
    config: config(&root, &url),
    project_id: "project".into(),
    input_id: "dataset-v1".into(),
    destination: destination.clone(),
  })
  .unwrap_err();
  task.join().unwrap();
  let error = error.to_string();
  assert!(
    error.contains("object download failed while reading or saving"),
    "{error}"
  );
  assert!(!error.contains("private-signature"));
  assert!(!error.contains("X-Amz"));
  assert!(!error.contains(&url));
  assert_eq!(
    std::fs::read(destination).unwrap(),
    b"existing verified input"
  );
}

#[test]
fn complete_metric_prefix_crosses_batches_and_does_not_publish_a_partial_row() {
  let (_temporary, root) = root();
  let path = root.join("metrics.jsonl");
  let mut bytes = vec![b'x'; STREAM_BATCH + 17];
  bytes[3] = b'\n';
  bytes.extend_from_slice(b"\npartial row");
  std::fs::write(&path, &bytes).unwrap();
  let mut file = fs::open(&path).unwrap();
  let first = upload::closed_prefix(&mut file, bytes.len() as u64).unwrap();
  assert_eq!(first, (STREAM_BATCH + 18) as u64);
  bytes.push(b'\n');
  std::fs::write(&path, &bytes).unwrap();
  let mut file = fs::open(&path).unwrap();
  assert_eq!(
    upload::closed_prefix(&mut file, bytes.len() as u64).unwrap(),
    bytes.len() as u64
  );
  std::fs::write(&path, vec![b'x'; STREAM_BATCH + 1]).unwrap();
  let mut file = fs::open(&path).unwrap();
  assert_eq!(
    upload::closed_prefix(&mut file, (STREAM_BATCH + 1) as u64).unwrap(),
    0
  );
}

#[test]
fn empty_files_have_a_verified_digest_and_unsafe_parents_are_refused() {
  let (_temporary, root) = root();
  let path = root.join("empty.log");
  let mut file = File::create(&path).unwrap();
  assert_eq!(
    fs::digest(&mut file, 0).unwrap(),
    fs::hex(&Sha256::digest([]))
  );
  fs::atomic_json(&root.join("owner.json"), &json!({"origin":"worker"})).unwrap();
  #[cfg(unix)]
  {
    std::os::unix::fs::symlink(&root, root.join("linked")).unwrap();
    assert!(fs::open(&root.join("linked/empty.log")).is_err());
    assert!(fs::directories(&root.join("linked/queue")).is_err());
  }
}

#[test]
fn watching_a_future_artifact_still_forwards_live_complete_metric_rows() {
  let (_temporary, root) = root();
  let row = b"{\"step\":0,\"metrics\":{\"loss\":1}}\n";
  let (url, task) = mock(1, move |request, _| {
    let Request::AppendStream {
      scope,
      path,
      offset,
      data_base64,
    } = serde_json::from_slice(&request.body).unwrap()
    else {
      panic!("expected live metric stream");
    };
    assert_eq!(scope.run_id, "run-1");
    assert_eq!(path, "outputs/metrics.jsonl");
    assert_eq!(offset, 0);
    assert_eq!(
      base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_base64).unwrap(),
      row
    );
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::Acknowledged {
        offset: row.len() as u64,
      })
      .unwrap(),
    )
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let run_dir = root.join("run-1");
  fs::directories(&run_dir.join("outputs")).unwrap();
  std::fs::write(run_dir.join("snapshot.json"), br#"{"run_id":"run-1"}"#).unwrap();
  std::fs::write(
    run_dir.join("run-state.json"),
    br#"{"run_id":"run-1","status":"running"}"#,
  )
  .unwrap();
  let mut metrics = row.to_vec();
  metrics.extend_from_slice(b"{\"partial\":");
  std::fs::write(run_dir.join("outputs/metrics.jsonl"), metrics).unwrap();
  let scope = RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-1".into(),
  };
  let mut queue = Queue::new(
    root.join("queue"),
    json!({"endpoint": api.endpoint, "scope": scope}),
  )
  .unwrap();
  // This cycle follows already synchronized metadata; only the live stream needs networking.
  for path in ["snapshot.json", "run-state.json"] {
    let mut file = fs::open(&run_dir.join(path)).unwrap();
    let size = file.metadata().unwrap().len();
    queue.state.files.insert(
      path.into(),
      SavedFile {
        target: run_target(&scope, path),
        size,
        sha256: fs::digest(&mut file, size).unwrap(),
        snapshot: None,
        upload: UploadState {
          upload_id: format!("finished-{path}"),
          part_size: 8 * 1024 * 1024,
          parts: Vec::new(),
          complete: true,
        },
      },
    );
  }
  let selected = BTreeSet::from(["outputs/checkpoint.pt".into()]);
  assert!(
    !push_cycle(
      &api,
      &mut queue,
      &scope,
      &run_dir,
      &selected,
      true,
      &mut |_| Ok(())
    )
    .unwrap()
  );
  assert_eq!(
    queue.state.streams["outputs/metrics.jsonl"],
    row.len() as u64
  );
  assert!(!queue.state.files.contains_key("outputs/checkpoint.pt"));
  task.join().unwrap();
}

#[test]
fn terminal_metric_flush_preserves_the_unterminated_final_row() {
  let (_temporary, root) = root();
  let row = b"{\"step\":0,\"metrics\":{\"loss\":1}}\n";
  let tail = b"{\"step\":1,";
  let (url, task) = mock(1, move |request, _| {
    let Request::AppendStream {
      offset,
      data_base64,
      ..
    } = serde_json::from_slice(&request.body).unwrap()
    else {
      panic!("expected final metric tail");
    };
    assert_eq!(offset, row.len() as u64);
    assert_eq!(
      base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_base64).unwrap(),
      tail
    );
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::Acknowledged {
        offset: (row.len() + tail.len()) as u64,
      })
      .unwrap(),
    )
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let source = root.join("metrics.jsonl");
  let mut bytes = row.to_vec();
  bytes.extend_from_slice(tail);
  std::fs::write(&source, &bytes).unwrap();
  let scope = RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-1".into(),
  };
  let mut queue = Queue::new(
    root.join("queue"),
    json!({"endpoint": api.endpoint, "scope": scope}),
  )
  .unwrap();
  queue
    .state
    .streams
    .insert("outputs/metrics.jsonl".into(), row.len() as u64);
  sync_stream(
    &api,
    &mut queue,
    &scope,
    "outputs/metrics.jsonl",
    &source,
    false,
  )
  .unwrap();
  assert_eq!(
    queue.state.streams["outputs/metrics.jsonl"],
    row.len() as u64
  );
  sync_stream(
    &api,
    &mut queue,
    &scope,
    "outputs/metrics.jsonl",
    &source,
    true,
  )
  .unwrap();
  assert_eq!(
    queue.state.streams["outputs/metrics.jsonl"],
    bytes.len() as u64
  );
  task.join().unwrap();
}

#[test]
fn publisher_reopens_offline_queue_and_observes_only_acknowledged_terminal_progress() {
  let (_temporary, root) = root();
  let began = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
  let requests = began.clone();
  let mut uploads =
    std::collections::BTreeMap::<String, (FileTarget, u64, String, UploadState)>::new();
  let mut offline = true;
  let (url, task) = mock(11, move |request, origin| {
    if request.path.starts_with("/objects/") {
      assert!(
        !request
          .headers
          .to_ascii_lowercase()
          .contains("authorization:")
      );
      return (
        200,
        vec![("ETag".into(), "\"fixture-etag\"".into())],
        Vec::new(),
      );
    }
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::BeginUpload {
        upload_id,
        target,
        size,
        sha256,
      } => {
        requests.lock().unwrap().push(upload_id.clone());
        if offline {
          offline = false;
          return (
            503,
            Vec::new(),
            br#"{"error":"fixture unavailable"}"#.to_vec(),
          );
        }
        let entry = uploads.entry(upload_id.clone()).or_insert_with(|| {
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
          upload: entry.3.clone(),
        }
      }
      Request::PartUrl {
        upload_id,
        part_number,
      } => {
        assert_eq!(part_number, 1);
        Response::Url {
          url: format!("{origin}/objects/{upload_id}"),
        }
      }
      Request::RecordPart { upload_id, part } => {
        let upload = &mut uploads.get_mut(&upload_id).unwrap().3;
        upload.parts.push(part);
        Response::Upload {
          upload: upload.clone(),
        }
      }
      Request::CompleteUpload { upload_id } => {
        let (target, size, sha256, upload) = uploads.get_mut(&upload_id).unwrap();
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
      _ => panic!("unexpected publishing request"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let run_dir = root.join("run-1");
  fs::directories(&run_dir).unwrap();
  std::fs::write(run_dir.join("snapshot.json"), br#"{"run_id":"run-1"}"#).unwrap();
  std::fs::write(
    run_dir.join("run-state.json"),
    br#"{"run_id":"run-1","status":"lost"}"#,
  )
  .unwrap();
  let options = PushOptions {
    config: config(&root, &url),
    run_dir,
    project_id: "project".into(),
    origin: "worker".into(),
    artifacts: Vec::new(),
    watch: true,
    queue_dir: root.join("queue"),
  };
  let mut publisher = Publisher::new(&options).unwrap();
  let mut progress = Vec::new();
  assert!(
    publisher
      .cycle(&mut |value| {
        progress.push(value);
        Ok(())
      })
      .is_err()
  );
  assert!(
    progress.is_empty(),
    "failed work must not be reported as acknowledged"
  );
  let queue_path = publisher.queue.directory.clone();
  assert!(queue_path.join("queue.json").is_file());
  drop(publisher);
  let mut publisher = Publisher::new(&options).unwrap();
  assert!(
    publisher
      .cycle(&mut |value| {
        progress.push(value);
        Ok(())
      })
      .unwrap()
  );
  let report = publisher.report(true);
  assert_eq!(report["terminal"], true);
  assert_eq!(report["files"], json!(["run-state.json", "snapshot.json"]));
  assert_eq!(progress.len(), 2);
  assert!(
    publisher
      .queue
      .state
      .files
      .values()
      .all(|saved| saved.upload.complete && saved.snapshot.is_none())
  );
  let seen = began.lock().unwrap();
  assert_eq!(
    seen[0], seen[1],
    "restart changed the durable pending upload identity"
  );
  task.join().unwrap();
}
