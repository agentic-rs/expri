use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::*;

pub(super) type Step = Box<dyn FnOnce(&str) -> Vec<u8> + Send>;

pub(super) struct Mock {
  pub(super) url: String,
  requests: Arc<Mutex<Vec<String>>>,
  worker: Option<JoinHandle<()>>,
}

impl Mock {
  pub(super) fn start(steps: Vec<Step>) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let received = requests.clone();
    let worker = std::thread::spawn(move || {
      for step in steps {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
          match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
              assert!(Instant::now() < deadline, "mock request timed out");
              std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("mock accept: {error}"),
          }
        };
        // macOS can inherit the listener's nonblocking mode on accepted sockets.
        stream.set_nonblocking(false).unwrap();
        let request = read_request(&mut stream);
        received.lock().unwrap().push(request.clone());
        stream.write_all(&step(&request)).unwrap();
      }
    });
    Self {
      url,
      requests,
      worker: Some(worker),
    }
  }

  pub(super) fn finish(&mut self) -> Vec<String> {
    self.worker.take().unwrap().join().unwrap();
    self.requests.lock().unwrap().clone()
  }
}

impl Drop for Mock {
  fn drop(&mut self) {
    if !std::thread::panicking()
      && let Some(worker) = self.worker.take()
    {
      worker.join().unwrap();
    }
  }
}

fn read_request(stream: &mut TcpStream) -> String {
  stream
    .set_read_timeout(Some(Duration::from_secs(5)))
    .unwrap();
  let mut request = Vec::new();
  let mut byte = [0u8; 1];
  while !request.ends_with(b"\r\n\r\n") {
    stream.read_exact(&mut byte).unwrap();
    request.push(byte[0]);
    assert!(request.len() < 16 * 1024);
  }
  let headers = String::from_utf8(request).unwrap();
  let length = headers
    .lines()
    .find_map(|line| {
      line
        .to_ascii_lowercase()
        .strip_prefix("content-length: ")
        .map(|value| value.parse::<usize>().unwrap())
    })
    .unwrap_or(0);
  assert!(length < 16 * 1024);
  let mut body = vec![0u8; length];
  stream.read_exact(&mut body).unwrap();
  format!("{headers}{}", String::from_utf8(body).unwrap())
}

pub(super) fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
  let mut result = format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n").into_bytes();
  result.extend_from_slice(body);
  result
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let repo = temporary.path().canonicalize().unwrap();
  (temporary, repo)
}

fn descriptor(source: Source, bytes: &[u8]) -> Descriptor {
  Descriptor {
    version: 2,
    source,
    size: bytes.len() as u64,
    sha256: hex(&Sha256::digest(bytes)),
    availability: None,
    producer: None,
  }
}

fn url_source(mock: &Mock) -> Source {
  Source::Url {
    url: format!("{}/file", mock.url),
  }
}

fn json_response(value: serde_json::Value) -> Vec<u8> {
  let body = serde_json::to_vec(&value).unwrap();
  response(
    "200 OK",
    &format!(
      "Content-Length: {}\r\nContent-Type: application/json\r\n",
      body.len()
    ),
    &body,
  )
}

fn private_steps(bytes: &'static [u8]) -> Vec<Step> {
  let digest = hex(&Sha256::digest(bytes));
  vec![
    Box::new(move |request| {
      assert!(request.contains("authorization: Bearer "));
      assert!(request.contains("\"action\":\"get_file\""));
      json_response(serde_json::json!({"kind":"file","file":{
        "target":{"kind":"input","project_id":"example-project","input_id":"input"},
        "size":bytes.len(),"sha256":digest,"storage":"object"}}))
    }),
    Box::new(|request| {
      assert!(request.contains("authorization: Bearer "));
      assert!(request.contains("\"action\":\"download_url\""));
      let host = request
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap();
      json_response(
        serde_json::json!({"kind":"url","url":format!("http://{host}/object?signature=temporary-private-secret")}),
      )
    }),
    Box::new(move |request| {
      assert!(request.starts_with("GET /object?signature=temporary-private-secret HTTP/1.1"));
      assert!(!request.contains("authorization:"));
      assert!(request.contains(&format!("range: bytes=0-{}\r\n", bytes.len() - 1)));
      response(
        "206 Partial Content",
        &format!(
          "Content-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\n",
          bytes.len(),
          bytes.len() - 1,
          bytes.len()
        ),
        bytes,
      )
    }),
  ]
}

#[test]
fn public_import_publishes_read_only_cache_and_verified_download_works_offline() {
  let (_temporary, repo) = fixture();
  let bytes = b"model data";
  let mut mock = Mock::start(vec![Box::new(|request| {
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    response(
      "200 OK",
      "Content-Length: 10\r\nETag: \"v1\"\r\n",
      b"model data",
    )
  })]);
  let source = url_source(&mock);
  let actual = resolve(&repo, &source, None, &mut || Ok(false)).unwrap();
  assert_eq!(actual, descriptor(source, bytes));
  mock.finish();
  let cached = ensure(&repo, &actual, None, &mut || Ok(false)).unwrap();
  assert_eq!(std::fs::read(&cached).unwrap(), bytes);
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
      std::fs::metadata(cached).unwrap().permissions().mode() & 0o777,
      0o400
    );
  }
}

#[test]
fn interrupted_public_download_resumes_only_with_matching_validator_and_range() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(vec![
    Box::new(|request| {
      assert!(!request.contains("range:"));
      response("200 OK", "Content-Length: 10\r\nETag: \"v1\"\r\n", b"0123")
    }),
    Box::new(|request| {
      assert!(request.contains("range: bytes=4-\r\n"), "{request}");
      assert!(request.contains("if-range: \"v1\"\r\n"), "{request}");
      response(
        "206 Partial Content",
        "Content-Length: 6\r\nContent-Range: bytes 4-9/10\r\nETag: \"v1\"\r\n",
        b"456789",
      )
    }),
  ]);
  let source = url_source(&mock);
  assert!(
    resolve(&repo, &source, None, &mut || Ok(false))
      .unwrap_err()
      .to_string()
      .contains("resume")
  );
  let staging = transfer_directory(&repo, &source, None).unwrap();
  let state: serde_json::Value =
    serde_json::from_slice(&std::fs::read(staging.join("progress.json")).unwrap()).unwrap();
  assert_eq!(state["offset"], 4);
  let actual = resolve(&repo, &source, None, &mut || Ok(false)).unwrap();
  assert_eq!(actual, descriptor(source, b"0123456789"));
  mock.finish();
  assert!(!staging.join("part").exists());
}

#[test]
fn a_public_source_without_strong_etag_restarts_partial_downloads() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(vec![
    Box::new(|_| {
      response(
        "200 OK",
        "Content-Length: 10\r\nETag: W/\"v1\"\r\n",
        b"old!",
      )
    }),
    Box::new(|request| {
      assert!(!request.contains("range:"));
      response("200 OK", "Content-Length: 10\r\n", b"new bytes!")
    }),
  ]);
  let source = url_source(&mock);
  assert!(resolve(&repo, &source, None, &mut || Ok(false)).is_err());
  // The second source is ten bytes; its changed prefix must replace the partial.
  let actual = resolve(&repo, &source, None, &mut || Ok(false)).unwrap();
  assert_eq!(actual, descriptor(source, b"new bytes!"));
  mock.finish();
}

#[test]
fn ignored_range_restarts_instead_of_joining_versions() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(vec![
    Box::new(|_| response("200 OK", "Content-Length: 10\r\nETag: \"old\"\r\n", b"old!")),
    Box::new(|request| {
      assert!(request.contains("range: bytes=4-\r\n"));
      response(
        "200 OK",
        "Content-Length: 10\r\nETag: \"new\"\r\n",
        b"new bytes!",
      )
    }),
  ]);
  let source = url_source(&mock);
  assert!(resolve(&repo, &source, None, &mut || Ok(false)).is_err());
  let actual = resolve(&repo, &source, None, &mut || Ok(false)).unwrap();
  assert_eq!(actual, descriptor(source, b"new bytes!"));
  mock.finish();
}

#[test]
fn changed_validator_and_incorrect_content_range_discard_partial_bytes() {
  for (etag, range) in [("new", "bytes 4-9/10"), ("old", "bytes 3-8/10")] {
    let (_temporary, repo) = fixture();
    let headers = format!("Content-Length: 6\r\nETag: \"{etag}\"\r\nContent-Range: {range}\r\n");
    let mut mock = Mock::start(vec![
      Box::new(|_| response("200 OK", "Content-Length: 10\r\nETag: \"old\"\r\n", b"old!")),
      Box::new(move |_| response("206 Partial Content", &headers, b"bytes!")),
    ]);
    let source = url_source(&mock);
    assert!(resolve(&repo, &source, None, &mut || Ok(false)).is_err());
    let error = resolve(&repo, &source, None, &mut || Ok(false))
      .unwrap_err()
      .to_string();
    assert!(error.contains("inconsistent"));
    mock.finish();
    assert_eq!(
      std::fs::metadata(
        transfer_directory(&repo, &source, None)
          .unwrap()
          .join("part")
      )
      .unwrap()
      .len(),
      0
    );
  }
}

#[test]
fn frozen_digest_mismatch_never_replaces_cached_or_bound_run_bytes() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(vec![Box::new(|_| {
    response("200 OK", "Content-Length: 10\r\n", b"new bytes!")
  })]);
  let expected = descriptor(url_source(&mock), b"old bytes!");
  let unrelated = descriptor(expected.source.clone(), b"stable run");
  let cached = cache_file(&repo, &unrelated.sha256);
  directories(cached.parent().unwrap()).unwrap();
  std::fs::write(&cached, b"stable run").unwrap();
  let binding = repo.join("run-model");
  std::fs::hard_link(&cached, &binding).unwrap();
  let error = ensure(&repo, &expected, None, &mut || Ok(false))
    .unwrap_err()
    .to_string();
  assert!(error.contains("descriptor"));
  assert!(!cache_file(&repo, &expected.sha256).exists());
  assert_eq!(std::fs::read(binding).unwrap(), b"stable run");
  assert_eq!(std::fs::read(cached).unwrap(), b"stable run");
  mock.finish();
}

#[test]
fn corrupt_immutable_cache_is_reported_without_overwriting_a_run_link() {
  let (_temporary, repo) = fixture();
  let expected = descriptor(
    Source::Url {
      url: "http://127.0.0.1:1/file".into(),
    },
    b"good bytes",
  );
  let cached = cache_file(&repo, &expected.sha256);
  directories(cached.parent().unwrap()).unwrap();
  std::fs::write(&cached, b"bad! bytes").unwrap();
  let binding = repo.join("run-model");
  std::fs::hard_link(&cached, &binding).unwrap();
  assert!(
    ensure(&repo, &expected, None, &mut || Ok(false))
      .unwrap_err()
      .to_string()
      .contains("corrupt")
  );
  assert_eq!(std::fs::read(binding).unwrap(), b"bad! bytes");
}

#[test]
fn redirect_urls_are_not_persisted_and_error_bodies_are_not_exposed() {
  let (_temporary, repo) = fixture();
  let secret = "signed-value-must-never-be-saved";
  let mut mock = Mock::start(vec![
    Box::new(move |_| {
      response(
        "302 Found",
        &format!("Content-Length: 0\r\nLocation: /bytes?X-Amz-Signature={secret}\r\n"),
        b"",
      )
    }),
    Box::new(move |request| {
      assert!(request.contains(secret));
      assert!(!request.contains("authorization:"));
      response(
        "503 Unavailable",
        "Content-Length: 30\r\n",
        b"secret-response-do-not-print-it",
      )
    }),
  ]);
  let source = url_source(&mock);
  let error = resolve(&repo, &source, None, &mut || Ok(false))
    .unwrap_err()
    .to_string();
  assert!(error.contains("503"));
  assert!(!error.contains(secret));
  assert!(!error.contains("secret-response"));
  let staging = transfer_directory(&repo, &source, None).unwrap();
  for entry in std::fs::read_dir(staging).unwrap() {
    assert!(
      !String::from_utf8_lossy(&std::fs::read(entry.unwrap().path()).unwrap()).contains(secret)
    );
  }
  mock.finish();
}

#[test]
fn unsafe_redirect_schemes_and_embedded_credentials_are_rejected() {
  for location in ["file:///etc/passwd", "http://name:secret@127.0.0.1/file"] {
    let (_temporary, repo) = fixture();
    let headers = format!("Content-Length: 0\r\nLocation: {location}\r\n");
    let mut mock = Mock::start(vec![Box::new(move |_| {
      response("302 Found", &headers, b"")
    })]);
    assert!(resolve(&repo, &url_source(&mock), None, &mut || Ok(false)).is_err());
    mock.finish();
  }
}

#[test]
fn missing_private_project_fails_before_credentials_or_network_are_used() {
  let (_temporary, repo) = fixture();
  let path = repo.join("client.toml");
  std::fs::write(
    &path,
    "url = 'https://wrong.example'\ntoken_env = 'MISSING_TOKEN'\n",
  )
  .unwrap();
  let source = Source::Expri {
    input_id: "input".into(),
  };
  let error = resolve(&repo, &source, Some(&path), &mut || Ok(false))
    .unwrap_err()
    .to_string();
  assert!(error.contains("project_id"));
  assert!(!error.contains("MISSING_TOKEN"));
  let expected = descriptor(source, b"private bytes");
  let cache = cache_file(&repo, &expected.sha256);
  directories(cache.parent().unwrap()).unwrap();
  std::fs::write(&cache, b"private bytes").unwrap();
  assert_eq!(
    ensure(&repo, &expected, None, &mut || Ok(false)).unwrap(),
    cache
  );
}

#[cfg(unix)]
#[test]
fn explicitly_selected_client_configuration_accepts_read_only_symlink_parents() {
  let (_temporary, repo) = fixture();
  let actual = repo.join("actual-config");
  std::fs::create_dir(&actual).unwrap();
  let config = actual.join("client.toml");
  std::fs::write(
    &config,
    "url='https://private.example.net'\nproject_id='example-project'\ntoken_env='MISSING_TOKEN'\n",
  )
  .unwrap();
  let alias = repo.join("config-alias");
  std::os::unix::fs::symlink(&actual, &alias).unwrap();
  assert_eq!(
    matching_client(Some(&alias.join("client.toml"))).unwrap(),
    (
      config,
      "example-project".into(),
      "https://private.example.net".into()
    )
  );
}

#[test]
fn registered_private_input_uses_existing_service_transfer_and_only_keeps_one_full_copy() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(private_steps(b"private v1"));
  let config = repo.join("client.toml");
  // Use the same harmless, already-present test token convention as service tests.
  std::fs::write(
    &config,
    format!(
      "url={:?}\ntoken_env='PATH'\nproject_id='example-project'\n",
      mock.url
    ),
  )
  .unwrap();
  let source = Source::Expri {
    input_id: "input".into(),
  };
  let actual = resolve(&repo, &source, Some(&config), &mut || Ok(false)).unwrap();
  assert_eq!(actual, descriptor(source.clone(), b"private v1"));
  mock.finish();
  let staging =
    transfer_directory_scoped(&repo, &source, None, Some((&mock.url, "example-project"))).unwrap();
  assert!(!staging.join("file").exists());
  let record = staging.join(".expri-input-downloads/file/input-record.json");
  assert!(
    !std::fs::read_to_string(record)
      .unwrap()
      .contains("temporary-private-secret")
  );
  assert_eq!(
    std::fs::read(ensure(&repo, &actual, None, &mut || Ok(false)).unwrap()).unwrap(),
    b"private v1"
  );
}

#[test]
fn frozen_private_input_rejects_changed_server_record_before_asset_cache_publication() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(private_steps(b"private v2"));
  let config = repo.join("client.toml");
  std::fs::write(
    &config,
    format!(
      "url={:?}\ntoken_env='PATH'\nproject_id='example-project'\n",
      mock.url
    ),
  )
  .unwrap();
  let source = Source::Expri {
    input_id: "input".into(),
  };
  let old = descriptor(source.clone(), b"private v1");
  let new = descriptor(source, b"private v2");
  let error = ensure(&repo, &old, Some(&config), &mut || Ok(false))
    .unwrap_err()
    .to_string();
  assert!(error.contains("private asset changed"));
  assert!(!cache_file(&repo, &old.sha256).exists());
  assert!(!cache_file(&repo, &new.sha256).exists());
  mock.finish();
}

#[test]
fn private_response_details_are_redacted_at_the_asset_boundary() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(vec![Box::new(|_| {
    let body = br#"{"error":"signed-private-response-secret"}"#;
    response(
      "403 Forbidden",
      &format!("Content-Length: {}\r\n", body.len()),
      body,
    )
  })]);
  let config = repo.join("client.toml");
  std::fs::write(
    &config,
    format!(
      "url={:?}\ntoken_env='PATH'\nproject_id='example-project'\n",
      mock.url
    ),
  )
  .unwrap();
  let source = Source::Expri {
    input_id: "input".into(),
  };
  let error = resolve(&repo, &source, Some(&config), &mut || Ok(false))
    .unwrap_err()
    .to_string();
  assert!(error.contains("403"));
  assert!(!error.contains("signed-private-response-secret"));
  mock.finish();
}

#[test]
fn concurrent_ensure_reuses_cache_prepared_while_waiting_for_transfer_lease() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(vec![Box::new(|_| {
    response("200 OK", "Content-Length: 10\r\n", b"model data")
  })]);
  let expected = descriptor(url_source(&mock), b"model data");
  let staging = transfer_directory(&repo, &expected.source, Some(&expected)).unwrap();
  directories(&staging).unwrap();
  let held = lease(&staging.join(".asset.lock"), &mut || Ok(false)).unwrap();
  let workers = (0..2)
    .map(|_| {
      let repo = repo.clone();
      let expected = expected.clone();
      std::thread::spawn(move || ensure(&repo, &expected, None, &mut || Ok(false)))
    })
    .collect::<Vec<_>>();
  std::thread::sleep(Duration::from_millis(50));
  drop(held);
  for worker in workers {
    assert_eq!(
      worker.join().unwrap().unwrap(),
      cache_file(&repo, &expected.sha256)
    );
  }
  assert_eq!(mock.finish().len(), 1);
}

#[test]
fn cancelled_transfers_release_the_lease_and_keep_only_safe_progress() {
  let (_temporary, repo) = fixture();
  let source = Source::Url {
    url: "http://127.0.0.1:1/file".into(),
  };
  let staging = transfer_directory(&repo, &source, None).unwrap();
  directories(&staging).unwrap();
  let held = lease(&staging.join(".asset.lock"), &mut || Ok(false)).unwrap();
  let mut calls = 0;
  let mut cancelled = || {
    calls += 1;
    Ok(calls >= 3)
  };
  assert!(resolve(&repo, &source, None, &mut cancelled).is_err());
  drop(held);
  let lease = lease(&staging.join(".asset.lock"), &mut || Ok(false)).unwrap();
  drop(lease);
}

fn private_config(repo: &Path, url: &str) -> PathBuf {
  let config = repo.join("client.toml");
  std::fs::write(
    &config,
    format!("url={url:?}\nproject_id='example-project'\ntoken_env='PATH'\n",),
  )
  .unwrap();
  config
}

#[test]
fn local_adoption_copies_into_immutable_cache_without_changing_the_original() {
  let (_temporary, repo) = fixture();
  let source = repo.join("local.bin");
  std::fs::write(&source, b"original bytes").unwrap();
  let before = std::fs::metadata(&source).unwrap();
  let actual = adopt(&repo, &source, &mut || Ok(false)).unwrap();
  assert_eq!(actual, descriptor(Source::Local, b"original bytes"));
  let cache = ensure(&repo, &actual, None, &mut || Ok(false)).unwrap();
  assert_eq!(std::fs::read(&cache).unwrap(), b"original bytes");
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let after = std::fs::metadata(&source).unwrap();
    let cached = std::fs::metadata(cache).unwrap();
    assert_eq!((before.ino(), before.mode()), (after.ino(), after.mode()));
    assert_ne!(after.ino(), cached.ino());
    assert_eq!(cached.mode() & 0o777, 0o400);
  }
  std::fs::write(&source, b"edited workspace").unwrap();
  assert_eq!(
    std::fs::read(cache_file(&repo, &actual.sha256)).unwrap(),
    b"original bytes"
  );
}

#[test]
fn local_asset_on_a_new_worker_requires_upload_and_then_uses_private_availability() {
  let (_temporary, repo) = fixture();
  let mut expected = descriptor(Source::Local, b"private v1");
  let error = ensure(&repo, &expected, None, &mut || Ok(false))
    .unwrap_err()
    .to_string();
  assert!(error.contains("upload"));
  assert!(!repo.join(".expri").exists());
  expected.availability = Some(crate::ExpriAvailability {
    input_id: "input".into(),
  });
  let mut mock = Mock::start(private_steps(b"private v1"));
  let config = private_config(&repo, &mock.url);
  let cache = ensure(&repo, &expected, Some(&config), &mut || Ok(false)).unwrap();
  assert_eq!(std::fs::read(cache).unwrap(), b"private v1");
  assert_eq!(mock.finish().len(), 3);
  assert_eq!(expected.source, Source::Local);
}

#[test]
fn supplied_public_identity_is_locked_offline_without_creating_transfer_state() {
  let (_temporary, repo) = fixture();
  let expected = descriptor(
    Source::Url {
      url: "https://example.net/model.bin".into(),
    },
    b"model data",
  );
  let identity = Identity {
    size: expected.size,
    sha256: expected.sha256.clone(),
  };
  assert_eq!(
    resolve_metadata(&repo, &expected.source, Some(&identity), None, &mut || Ok(
      false
    ))
    .unwrap(),
    expected
  );
  assert!(!repo.join(".expri").exists());
}

#[test]
fn public_metadata_only_add_uses_head_and_never_downloads_or_creates_a_cache() {
  let (_temporary, repo) = fixture();
  let digest = hex(&Sha256::digest(b"model data"));
  let mut mock = Mock::start(vec![Box::new(move |request| {
    assert!(request.starts_with("HEAD /file HTTP/1.1"));
    response(
      "200 OK",
      &format!("Content-Length: 10\r\nX-Checksum-Sha256: {digest}\r\n"),
      b"",
    )
  })]);
  let source = url_source(&mock);
  assert_eq!(
    resolve_metadata(&repo, &source, None, None, &mut || Ok(false)).unwrap(),
    descriptor(source, b"model data")
  );
  assert_eq!(mock.finish().len(), 1);
  assert!(!repo.join(".expri").exists());
}

#[test]
fn metadata_only_add_rejects_unknown_sha256_instead_of_fetching_payload() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(vec![Box::new(|request| {
    assert!(request.starts_with("HEAD "));
    response("200 OK", "Content-Length: 10\r\nETag: \"version\"\r\n", b"")
  })]);
  let error = resolve_metadata(&repo, &url_source(&mock), None, None, &mut || Ok(false))
    .unwrap_err()
    .to_string();
  assert!(error.contains("known SHA256"));
  assert_eq!(mock.finish().len(), 1);
  assert!(!repo.join(".expri").exists());
}

#[test]
fn private_metadata_only_add_retains_short_producer_without_fetching_payload() {
  let (_temporary, repo) = fixture();
  let digest = hex(&Sha256::digest(b"private v1"));
  let mut mock = Mock::start(vec![Box::new(move |request| {
    assert!(request.contains("\"action\":\"get_file\""));
    assert!(request.contains("\"project_id\":\"example-project\""));
    json_response(serde_json::json!({"kind":"file","file":{
      "target":{"kind":"input","project_id":"example-project","input_id":"input"},
      "size":10,"sha256":digest,"storage":"object",
      "provenance": {"producer":{"origin":"worker-1","run_id":"run-abc123","path":"outputs/best.pt"},
        "captured_at":"2026-10-11T00:00:00Z", "state":{"private":"captured-state"},"snapshot":{},"environment":{}}
    }}))
  })]);
  let config = private_config(&repo, &mock.url);
  let source = Source::Expri {
    input_id: "input".into(),
  };
  let actual = resolve_metadata(&repo, &source, None, Some(&config), &mut || Ok(false)).unwrap();
  assert_eq!(actual.producer.as_ref().unwrap().origin, "worker-1");
  let serialized = toml::to_string(&actual).unwrap();
  assert!(!serialized.contains("example-project"));
  assert!(!serialized.contains(&mock.url));
  assert!(!serialized.contains("captured-state"));
  assert_eq!(mock.finish().len(), 1);
  assert!(!repo.join(".expri").exists());
}

#[test]
fn uploaded_public_asset_prefers_private_mirror_and_falls_back_to_its_source() {
  let (_temporary, repo) = fixture();
  let mut private = Mock::start(private_steps(b"private v1"));
  let config = private_config(&repo, &private.url);
  let mut expected = descriptor(
    Source::Url {
      url: "https://example.net/unavailable.bin".into(),
    },
    b"private v1",
  );
  expected.availability = Some(crate::ExpriAvailability {
    input_id: "input".into(),
  });
  assert!(
    ensure(&repo, &expected, Some(&config), &mut || Ok(false))
      .unwrap()
      .exists()
  );
  assert_eq!(private.finish().len(), 3);

  let (_temporary, repo) = fixture();
  let mut public = Mock::start(vec![Box::new(|request| {
    assert!(request.starts_with("GET /file "));
    response("200 OK", "Content-Length: 10\r\n", b"model data")
  })]);
  let mut expected = descriptor(url_source(&public), b"model data");
  expected.availability = Some(crate::ExpriAvailability {
    input_id: "input".into(),
  });
  let config = private_config(&repo, "http://127.0.0.1:1");
  assert!(
    ensure(&repo, &expected, Some(&config), &mut || Ok(false))
      .unwrap()
      .exists()
  );
  assert_eq!(public.finish().len(), 1);

  let (_temporary, repo) = fixture();
  let mut public = Mock::start(vec![Box::new(|request| {
    assert!(request.starts_with("GET /file "));
    response("200 OK", "Content-Length: 10\r\n", b"model data")
  })]);
  let mut expected = descriptor(url_source(&public), b"model data");
  expected.availability = Some(crate::ExpriAvailability {
    input_id: "input".into(),
  });
  assert!(
    ensure(&repo, &expected, None, &mut || Ok(false))
      .unwrap()
      .exists()
  );
  assert_eq!(public.finish().len(), 1);
}

#[test]
fn uploaded_private_asset_uses_the_new_availability_without_losing_original_source() {
  let (_temporary, repo) = fixture();
  let mut mock = Mock::start(private_steps(b"private v1"));
  let config = private_config(&repo, &mock.url);
  let mut expected = descriptor(
    Source::Expri {
      input_id: "original-input".into(),
    },
    b"private v1",
  );
  expected.availability = Some(crate::ExpriAvailability {
    input_id: "input".into(),
  });
  assert!(
    ensure(&repo, &expected, Some(&config), &mut || Ok(false))
      .unwrap()
      .exists()
  );
  assert_eq!(mock.finish().len(), 3);
  assert_eq!(
    expected.source,
    Source::Expri {
      input_id: "original-input".into()
    }
  );
}

#[test]
fn private_transfer_staging_is_scoped_by_configuration_without_affecting_content_identity() {
  let (_temporary, repo) = fixture();
  let source = Source::Expri {
    input_id: "input".into(),
  };
  let expected = descriptor(source.clone(), b"bytes");
  let first = transfer_directory_scoped(
    &repo,
    &source,
    Some(&expected),
    Some(("https://first.example.net", "example-project")),
  )
  .unwrap();
  let second = transfer_directory_scoped(
    &repo,
    &source,
    Some(&expected),
    Some(("https://second.example.net", "example-project")),
  )
  .unwrap();
  let third = transfer_directory_scoped(
    &repo,
    &source,
    Some(&expected),
    Some(("https://first.example.net", "other-project")),
  )
  .unwrap();
  assert_ne!(first, second);
  assert_ne!(first, third);
  assert_eq!(
    cache_file(&repo, &expected.sha256),
    repo
      .join(".expri/assets")
      .join(&expected.sha256)
      .join("file")
  );
}
