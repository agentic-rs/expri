use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{atomic::Ordering, mpsc};
use std::thread;
use std::time::Instant;

use super::*;
use crate::client::tests::mock;
use crate::error::ExpriError;

fn source(bytes: &[u8]) -> SourceRef {
  let sha256: String = Sha256::digest(bytes)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  SourceRef {
    input_id: format!("queue-source-{sha256}"),
    size: bytes.len() as u64,
    sha256,
  }
}

fn attempt() -> AttemptIdentity {
  AttemptIdentity {
    attempt_id: "attempt-one".into(),
    worker_id: "worker-one".into(),
    session_id: "session-one".into(),
    fencing_token: "fence-one".into(),
  }
}

fn config(root: &Path, url: &str) -> PathBuf {
  let path = root.join("client.toml");
  std::fs::write(&path, format!("url='{url}'\ntoken='bound-token'\n")).unwrap();
  path
}

fn control(request: Request, origin: &str, source: &SourceRef) -> Response {
  match request {
    Request::Capabilities => Response::Capabilities {
      features: vec![RESOURCE_QUEUE_CAPABILITY.into()],
    },
    Request::Queue {
      request: QueueRequest::SourceUrl { attempt: identity },
    } => {
      assert_eq!(identity, attempt());
      Response::Queue {
        response: QueueResponse::SourceUrl {
          url: format!("{origin}/source?signature=must-not-leak"),
          size: source.size,
          sha256: source.sha256.clone(),
        },
      }
    }
    other => panic!("unexpected queue source request: {other:?}"),
  }
}

#[test]
fn legacy_queue_servers_are_refused_before_submission_or_worker_registration() {
  let (url, server) = mock(1, |request, _| {
    assert!(matches!(
      serde_json::from_slice::<Request>(&request.body).unwrap(),
      Request::Capabilities
    ));
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::Capabilities {
        features: vec!["resource-queue-v1".into()],
      })
      .unwrap(),
    )
  });
  let root = tempfile::tempdir().unwrap();
  let error = ResourceQueueClient::new(&config(root.path(), &url))
    .err()
    .unwrap()
    .to_string();
  assert!(error.contains("upgrade the server"));
  assert!(error.contains("resource-queue-v2"));
  server.join().unwrap();
}

#[test]
fn pinned_sources_keep_integrity_and_private_permissions_in_both_transports() {
  for cooperative in [false, true] {
    let bytes = b"pinned source bytes";
    let expected = source(bytes);
    let server_source = expected.clone();
    let (url, server) = mock(3, move |request, origin| {
      if request.path.starts_with("/source?") {
        assert!(
          !request
            .headers
            .to_ascii_lowercase()
            .contains("authorization:")
        );
        return (200, Vec::new(), bytes.to_vec());
      }
      assert!(request.headers.contains("Bearer bound-token"));
      let response = control(
        serde_json::from_slice(&request.body).unwrap(),
        origin,
        &server_source,
      );
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    });
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root, &url);
    let stop = Arc::new(AtomicBool::new(false));
    let client = if cooperative {
      ResourceQueueClient::with_stop(&config, Arc::clone(&stop)).unwrap()
    } else {
      ResourceQueueClient::new(&config).unwrap()
    };
    let cloned = client.clone();
    if cooperative {
      assert!(cloned.api.belongs_to(&stop));
    }
    let destination = root.join("source.zip");
    cloned
      .download_source(&attempt(), &expected, &destination)
      .unwrap();
    assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(
        std::fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o600
      );
    }
    server.join().unwrap();
  }
}

#[test]
fn service_stop_cancels_source_waiting_for_headers() {
  cancel_stalled_source(false);
}

#[test]
fn service_stop_cancels_source_waiting_for_the_next_chunk() {
  cancel_stalled_source(true);
}

fn cancel_stalled_source(send_headers: bool) {
  let bytes = b"source bytes";
  let expected = source(bytes);
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let object_origin = format!("http://{}", listener.local_addr().unwrap());
  let (connected, connection) = mpsc::channel();
  let (release, released) = mpsc::channel();
  let object_server = thread::spawn(move || {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
      match listener.accept() {
        Ok((stream, _)) => break stream,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          assert!(Instant::now() < deadline, "source client did not connect");
          thread::sleep(Duration::from_millis(5));
        }
        Err(error) => panic!("source test accept: {error}"),
      }
    };
    stream.set_nonblocking(false).unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(3)))
      .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    loop {
      let mut line = String::new();
      reader.read_line(&mut line).unwrap();
      assert!(!line.is_empty(), "incomplete source request");
      assert!(!line.to_ascii_lowercase().starts_with("authorization:"));
      if line == "\r\n" {
        break;
      }
    }
    if send_headers {
      write!(
        stream,
        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
      )
      .unwrap();
      stream.write_all(&bytes[..3]).unwrap();
      stream.flush().unwrap();
    }
    connected.send(()).unwrap();
    released.recv_timeout(Duration::from_secs(5)).unwrap();
  });
  let server_source = expected.clone();
  let (url, control_server) = mock(2, move |request, _| {
    let response = control(
      serde_json::from_slice(&request.body).unwrap(),
      &object_origin,
      &server_source,
    );
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let config = config(&root, &url);
  let stop = Arc::new(AtomicBool::new(false));
  let client = ResourceQueueClient::with_stop(&config, Arc::clone(&stop)).unwrap();
  let destination = root.join("source.zip");
  let task_destination = destination.clone();
  let (finished, result) = mpsc::channel();
  let task = thread::spawn(move || {
    finished
      .send(client.download_source(&attempt(), &expected, &task_destination))
      .unwrap();
  });
  connection.recv_timeout(Duration::from_secs(3)).unwrap();
  let partial_saved = if send_headers {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
      if std::fs::read_dir(&root).unwrap().any(|entry| {
        entry
          .unwrap()
          .metadata()
          .is_ok_and(|metadata| metadata.len() == 3)
      }) {
        break true;
      }
      if Instant::now() >= deadline {
        break false;
      }
      thread::sleep(Duration::from_millis(5));
    }
  } else {
    true
  };
  stop.store(true, Ordering::Release);
  let result = result.recv_timeout(Duration::from_secs(1));
  release.send(()).unwrap();
  task.join().unwrap();
  object_server.join().unwrap();
  control_server.join().unwrap();
  assert!(
    partial_saved,
    "source client did not consume the first chunk"
  );
  let error = result
    .expect("queue source cancellation was not prompt")
    .unwrap_err();
  assert!(matches!(error, ExpriError::DownloadCancelled));
  assert!(!destination.exists());
  assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
}

#[test]
fn cooperative_download_rejects_tampering_without_overwriting_existing_source() {
  let expected = source(b"submitted source");
  let server_source = expected.clone();
  let (url, server) = mock(3, move |request, origin| {
    if request.path.starts_with("/source?") {
      return (200, Vec::new(), b"tampered! source".to_vec());
    }
    let response = control(
      serde_json::from_slice(&request.body).unwrap(),
      origin,
      &server_source,
    );
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let config = config(&root, &url);
  let client = ResourceQueueClient::with_stop(&config, Arc::new(AtomicBool::new(false))).unwrap();
  let destination = root.join("source.zip");
  std::fs::write(&destination, b"existing verified source").unwrap();
  let error = client
    .download_source(&attempt(), &expected, &destination)
    .unwrap_err();
  assert!(error.to_string().contains("integrity verification"));
  assert!(!error.to_string().contains("must-not-leak"));
  assert_eq!(
    std::fs::read(destination).unwrap(),
    b"existing verified source"
  );
  server.join().unwrap();
}
