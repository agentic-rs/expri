//! Exercise the CLI across two checkouts; the catalog fixture transfers payloads
//! only through its separate object endpoints.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Default)]
struct Catalog {
  file: Value,
  parts: Vec<Value>,
  complete: bool,
  bytes: Vec<u8>,
  object_uploads: usize,
  object_downloads: usize,
  requests: Vec<Value>,
}

struct Service {
  url: String,
  catalog: Arc<Mutex<Catalog>>,
  stopped: Arc<AtomicBool>,
  thread: Option<thread::JoinHandle<()>>,
}

impl Service {
  fn new() -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let catalog = Arc::new(Mutex::new(Catalog::default()));
    let stopped = Arc::new(AtomicBool::new(false));
    let (state, stop, endpoint) = (catalog.clone(), stopped.clone(), url.clone());
    let thread = thread::spawn(move || {
      while !stop.load(Ordering::Acquire) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            stream.set_nonblocking(false).unwrap();
            stream
              .set_read_timeout(Some(Duration::from_secs(5)))
              .unwrap();
            let (method, path, body) = request(&mut stream);
            let mut catalog = state.lock().unwrap();
            if method == "PUT" && path == "/part" {
              catalog.object_uploads += 1;
              catalog.bytes = body;
              respond(&mut stream, b"", "ETag: \"part-1\"\r\n");
            } else if method == "GET" && path == "/object" {
              catalog.object_downloads += 1;
              let headers = format!(
                "Content-Range: bytes 0-{}/{}\r\nETag: \"object-1\"\r\n",
                catalog.bytes.len() - 1,
                catalog.bytes.len()
              );
              respond_with_status(&mut stream, &catalog.bytes, &headers, "206 Partial Content");
            } else {
              assert_eq!((method.as_str(), path.as_str()), ("POST", "/v1/request"));
              let request: Value = serde_json::from_slice(&body).unwrap();
              catalog.requests.push(request.clone());
              let response = match request["action"].as_str().unwrap() {
                // This fixture models a legacy service without continuation.
                "capabilities" => json!({"kind": "capabilities", "features": []}),
                "begin_upload" => {
                  if catalog.file.is_null() {
                    catalog.file = json!({"target": request["target"], "size": request["size"], "sha256": request["sha256"], "storage": "object"});
                  }
                  assert_eq!(catalog.file["target"], request["target"]);
                  assert_eq!(catalog.file["sha256"], request["sha256"]);
                  json!({"kind": "upload", "upload": {"upload_id": request["upload_id"], "part_size": 8 * 1024 * 1024, "parts": catalog.parts, "complete": catalog.complete}})
                }
                "part_url" => json!({"kind": "url", "url": format!("{endpoint}/part")}),
                "record_part" => {
                  catalog.parts.push(request["part"].clone());
                  json!({"kind": "upload", "upload": {"upload_id": request["upload_id"], "part_size": 8 * 1024 * 1024, "parts": catalog.parts, "complete": false}})
                }
                "complete_upload" => {
                  assert_eq!(
                    catalog.bytes.len() as u64,
                    catalog.file["size"].as_u64().unwrap()
                  );
                  catalog.complete = true;
                  json!({"kind": "file", "file": catalog.file})
                }
                "get_file" => {
                  json!({"kind": "file", "file": catalog_file(&catalog, &request["target"])})
                }
                "reference_file" => {
                  assert_eq!(request["sha256"], catalog.file["sha256"]);
                  assert_eq!(request["size"], catalog.file["size"]);
                  json!({"kind": "file", "file": catalog_file(&catalog, &request["target"])})
                }
                "download_url" => json!({"kind": "url", "url": format!("{endpoint}/object")}),
                "import_output" => {
                  assert_eq!(
                    request["scope"],
                    json!({"project_id": "example-project", "origin": "worker-1", "run_id": "run-abc123"})
                  );
                  assert_eq!(request["path"], "outputs/best.pt");
                  assert_eq!(request["sha256"], catalog.file["sha256"]);
                  let mut file = catalog.file.clone();
                  file["target"] = json!({"kind": "input", "project_id": "example-project", "input_id": "asset-imported"});
                  file["provenance"] = provenance();
                  json!({"kind": "file", "file": file})
                }
                action => panic!("unexpected catalog request: {action}"),
              };
              respond(
                &mut stream,
                &serde_json::to_vec(&response).unwrap(),
                "Content-Type: application/json\r\n",
              );
            }
          }
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5))
          }
          Err(error) => panic!("catalog listener failed: {error}"),
        }
      }
    });
    Self {
      url,
      catalog,
      stopped,
      thread: Some(thread),
    }
  }

  fn stop(&mut self) {
    self.stopped.store(true, Ordering::Release);
    if let Some(thread) = self.thread.take() {
      let result = thread.join();
      if !std::thread::panicking() {
        result.unwrap();
      }
    }
  }
}

impl Drop for Service {
  fn drop(&mut self) {
    self.stop();
  }
}

fn provenance() -> Value {
  json!({"producer": {"origin": "worker-1", "run_id": "run-abc123", "path": "outputs/best.pt"}, "captured_at": "2026-01-01T00:00:00Z", "state": {"status": "succeeded"}, "snapshot": {}, "environment": {}})
}

fn catalog_file(catalog: &Catalog, target: &Value) -> Value {
  let mut file = catalog.file.clone();
  file["target"] = target.clone();
  if matches!(
    target["input_id"].as_str(),
    Some("asset-imported" | "asset-alias" | "asset-conflicting")
  ) {
    file["provenance"] = provenance();
    if target["input_id"] == "asset-conflicting" {
      file["provenance"]["producer"]["run_id"] = json!("run-other");
    }
  }
  file
}

fn request(stream: &mut TcpStream) -> (String, String, Vec<u8>) {
  let mut bytes = Vec::new();
  while !bytes.ends_with(b"\r\n\r\n") {
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
    bytes.push(byte[0]);
    assert!(bytes.len() <= 32 * 1024);
  }
  let headers = String::from_utf8(bytes).unwrap();
  let mut first = headers.lines().next().unwrap().split_whitespace();
  let (method, path) = (first.next().unwrap().into(), first.next().unwrap().into());
  let size = headers
    .lines()
    .find_map(|line| {
      let (name, value) = line.split_once(':')?;
      name
        .eq_ignore_ascii_case("content-length")
        .then(|| value.trim().parse::<usize>().unwrap())
    })
    .unwrap_or(0);
  let mut body = vec![0; size];
  stream.read_exact(&mut body).unwrap();
  (method, path, body)
}

fn respond(stream: &mut TcpStream, body: &[u8], headers: &str) {
  respond_with_status(stream, body, headers, "200 OK");
}

fn respond_with_status(stream: &mut TcpStream, body: &[u8], headers: &str, status: &str) {
  write!(
    stream,
    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n",
    body.len()
  )
  .unwrap();
  stream.write_all(body).unwrap();
}

fn expri(root: &Path, args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_expri"))
    .current_dir(root)
    .args(args)
    .env("EXPRI_ASSET_TEST_TOKEN", "local-test-token")
    .output()
    .unwrap()
}

fn succeeds(root: &Path, args: &[&str]) -> Value {
  let output = expri(root, args);
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  serde_json::from_slice(&output.stdout).unwrap()
}

fn configure(root: &Path, service: &Service) {
  fs::create_dir_all(root.join(".expri")).unwrap();
  fs::write(
    root.join(".expri/config.toml"),
    format!(
      "url = '{}'\nproject_id = 'example-project'\ntoken_env = 'EXPRI_ASSET_TEST_TOKEN'\n",
      service.url
    ),
  )
  .unwrap();
}

#[test]
fn local_upload_deferred_add_output_import_and_worker_download_have_distinct_transfers() {
  let laptop = tempfile::tempdir().unwrap();
  let worker = tempfile::tempdir().unwrap();
  let mut service = Service::new();
  assert!(
    Command::new("git")
      .args(["init", "--quiet"])
      .current_dir(laptop.path())
      .status()
      .unwrap()
      .success()
  );
  configure(laptop.path(), &service);
  configure(worker.path(), &service);
  fs::create_dir(laptop.path().join("data")).unwrap();
  fs::write(
    laptop.path().join("data/train.bin"),
    b"reusable training bytes",
  )
  .unwrap();
  let added = succeeds(
    laptop.path(),
    &["assets", "add", "data/train.bin", "--json"],
  );
  assert_eq!(added["source"]["kind"], "local");
  assert!(service.catalog.lock().unwrap().requests.is_empty());
  succeeds(
    laptop.path(),
    &[
      "assets",
      "upload",
      "data/train.bin",
      "--input-id",
      "dataset-v1",
      "--json",
    ],
  );
  assert_eq!(
    service.catalog.lock().unwrap().bytes,
    b"reusable training bytes"
  );
  assert_eq!(service.catalog.lock().unwrap().object_uploads, 1);
  // The protected repository config cannot accidentally enter `git add .`.
  let ignored = Command::new("git")
    .args(["check-ignore", ".expri/config.toml"])
    .current_dir(laptop.path())
    .output()
    .unwrap();
  assert!(ignored.status.success());
  fs::create_dir(worker.path().join("data")).unwrap();
  fs::copy(
    laptop.path().join("data/train.bin.expri.toml"),
    worker.path().join("data/train.bin.expri.toml"),
  )
  .unwrap();
  succeeds(
    worker.path(),
    &["assets", "download", "data/train.bin", "--json"],
  );
  assert_eq!(
    fs::read(worker.path().join("data/train.bin")).unwrap(),
    b"reusable training bytes"
  );
  assert_eq!(service.catalog.lock().unwrap().object_downloads, 1);
  let declared = succeeds(
    laptop.path(),
    &[
      "assets",
      "add",
      "expri://./inputs/dataset-v1",
      "data/declared.bin",
      "--no-download",
      "--json",
    ],
  );
  assert_eq!(declared["sha256"], added["sha256"]);
  assert!(!laptop.path().join("data/declared.bin").exists());
  assert_eq!(service.catalog.lock().unwrap().object_downloads, 1);
  let source = "expri://./worker-1/run-abc123/outputs/best.pt";
  let imported = succeeds(
    laptop.path(),
    &["assets", "import", source, "models/base.pt", "--json"],
  );
  assert_eq!(imported["source"]["input_id"], "asset-imported");
  assert_eq!(imported["producer"]["run_id"], "run-abc123");
  assert!(!laptop.path().join("models/base.pt").exists());
  assert_eq!(service.catalog.lock().unwrap().object_uploads, 1);
  assert_eq!(service.catalog.lock().unwrap().object_downloads, 1);
  {
    let catalog = service.catalog.lock().unwrap();
    assert!(
      catalog
        .requests
        .iter()
        .any(|request| request["action"] == "capabilities")
    );
    assert!(catalog.requests.iter().all(|request| !matches!(
      request["action"].as_str(),
      Some("run_publication" | "at_run_epoch")
    )));
  }
  let producing_run_queries = service
    .catalog
    .lock()
    .unwrap()
    .requests
    .iter()
    .filter(|request| request["action"] == "get_file" && request["target"]["kind"] == "run")
    .count();
  assert_eq!(
    succeeds(
      laptop.path(),
      &["assets", "import", source, "models/base.pt", "--json"]
    )["reused"],
    true
  );
  assert_eq!(
    service
      .catalog
      .lock()
      .unwrap()
      .requests
      .iter()
      .filter(|request| request["action"] == "get_file" && request["target"]["kind"] == "run")
      .count(),
    producing_run_queries
  );
  let rejected = expri(
    laptop.path(),
    &[
      "assets",
      "import",
      source,
      "models/base.pt",
      "--input-id",
      "another-id",
    ],
  );
  assert!(!rejected.status.success());
  assert_eq!(
    service
      .catalog
      .lock()
      .unwrap()
      .requests
      .iter()
      .filter(|request| request["action"] == "import_output")
      .count(),
    1
  );
  let aliased = succeeds(
    laptop.path(),
    &[
      "assets",
      "upload",
      "models/base.pt",
      "--input-id",
      "asset-alias",
      "--json",
    ],
  );
  assert_eq!(
    aliased["assets"][0]["availability"]["input_id"],
    "asset-alias"
  );
  let restored = succeeds(
    laptop.path(),
    &[
      "assets",
      "upload",
      "models/base.pt",
      "--input-id",
      "asset-imported",
      "--json",
    ],
  );
  assert!(restored["assets"][0]["availability"].is_null());
  let sidecar = laptop.path().join("models/base.pt.expri.toml");
  let before = fs::read(&sidecar).unwrap();
  let conflicting = expri(
    laptop.path(),
    &[
      "assets",
      "upload",
      "models/base.pt",
      "--input-id",
      "asset-conflicting",
    ],
  );
  assert!(!conflicting.status.success());
  assert!(String::from_utf8_lossy(&conflicting.stderr).contains("provenance"));
  assert_eq!(fs::read(&sidecar).unwrap(), before);
  succeeds(laptop.path(), &["assets", "upload", "--json"]);
  assert_eq!(service.catalog.lock().unwrap().object_uploads, 1);
  assert_eq!(service.catalog.lock().unwrap().object_downloads, 1);
  for relative in [
    "data/train.bin.expri.toml",
    "data/declared.bin.expri.toml",
    "models/base.pt.expri.toml",
  ] {
    let descriptor = fs::read_to_string(laptop.path().join(relative)).unwrap();
    for forbidden in [
      &service.url,
      "example-project",
      "EXPRI_ASSET_TEST_TOKEN",
      "local-test-token",
      laptop.path().to_str().unwrap(),
    ] {
      assert!(
        !descriptor.contains(forbidden),
        "private context leaked into {relative}"
      );
    }
  }
  fs::create_dir(worker.path().join("models")).unwrap();
  fs::copy(
    laptop.path().join("models/base.pt.expri.toml"),
    worker.path().join("models/base.pt.expri.toml"),
  )
  .unwrap();
  // This worker already cached the same identity under the local asset.
  succeeds(
    worker.path(),
    &["assets", "download", "models/base.pt", "--json"],
  );
  assert_eq!(service.catalog.lock().unwrap().object_downloads, 1);
  service.stop();
  fs::remove_file(worker.path().join("models/base.pt")).unwrap();
  succeeds(
    worker.path(),
    &["assets", "download", "models/base.pt", "--json"],
  );
  assert_eq!(
    fs::read(worker.path().join("models/base.pt")).unwrap(),
    b"reusable training bytes"
  );
}
