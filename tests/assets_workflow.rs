//! Exercise the user-facing asset workflow with a local HTTP origin and real Git.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

struct Origin {
  url: String,
  bytes: Arc<Mutex<Vec<u8>>>,
  requests: Arc<AtomicUsize>,
  stopped: Arc<AtomicBool>,
  thread: Option<thread::JoinHandle<()>>,
}

impl Origin {
  fn new(bytes: &[u8]) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let bytes = Arc::new(Mutex::new(bytes.to_vec()));
    let requests = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicBool::new(false));
    let (data, counter, stop) = (bytes.clone(), requests.clone(), stopped.clone());
    let handle = thread::spawn(move || {
      while !stop.load(Ordering::Acquire) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            stream.set_nonblocking(false).unwrap();
            stream
              .set_read_timeout(Some(Duration::from_secs(2)))
              .unwrap();
            if read_headers(&mut stream).is_none() {
              continue;
            }
            counter.fetch_add(1, Ordering::Relaxed);
            let bytes = data.lock().unwrap().clone();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"version-{}\"\r\nConnection: close\r\n\r\n", bytes.len(), bytes.len()).unwrap();
            let _ = stream.write_all(&bytes);
          }
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5))
          }
          Err(error) => panic!("mock origin failed: {error}"),
        }
      }
    });
    Self {
      url: format!("http://{address}/train.bin"),
      bytes,
      requests,
      stopped,
      thread: Some(handle),
    }
  }

  fn stop(&mut self) {
    self.stopped.store(true, Ordering::Release);
    if let Some(handle) = self.thread.take() {
      handle.join().unwrap();
    }
  }
}

impl Drop for Origin {
  fn drop(&mut self) {
    self.stop();
  }
}

fn read_headers(stream: &mut TcpStream) -> Option<()> {
  let mut headers = Vec::new();
  while !headers.ends_with(b"\r\n\r\n") {
    let mut byte = [0];
    if stream.read(&mut byte).ok()? == 0 || headers.len() > 16 * 1024 {
      return None;
    }
    headers.push(byte[0]);
  }
  Some(())
}

fn expri(root: &Path, args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_expri"))
    .current_dir(root)
    .args(args)
    .output()
    .unwrap()
}

fn succeeds(root: &Path, args: &[&str]) -> serde_json::Value {
  let output = expri(root, args);
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  serde_json::from_slice(&output.stdout).unwrap()
}

fn git(root: &Path, args: &[&str]) -> Output {
  Command::new("git")
    .current_dir(root)
    .args(args)
    .output()
    .unwrap()
}

#[test]
fn import_update_frozen_download_offline_cache_and_git_sidecars() {
  let root = tempfile::tempdir().unwrap();
  assert!(git(root.path(), &["init", "--quiet"]).status.success());
  let mut origin = Origin::new(b"first dataset");
  let imported = succeeds(
    root.path(),
    &["assets", "import", &origin.url, "data/train.bin", "--json"],
  );
  assert_eq!(imported["sidecar"], "data/train.bin.expri.toml");
  let original_sidecar = fs::read(root.path().join("data/train.bin.expri.toml")).unwrap();
  assert_eq!(
    fs::read(root.path().join("data/train.bin")).unwrap(),
    b"first dataset"
  );
  assert!(
    git(
      root.path(),
      &["check-ignore", "data/train.bin", ".expri/assets/"]
    )
    .status
    .success()
  );
  assert!(
    !git(root.path(), &["check-ignore", "data/train.bin.expri.toml"])
      .status
      .success()
  );
  assert!(git(root.path(), &["add", "."]).status.success());
  let tracked = git(root.path(), &["ls-files"]);
  let tracked = String::from_utf8(tracked.stdout).unwrap();
  assert!(tracked.contains("data/train.bin.expri.toml"));
  assert!(
    !tracked
      .lines()
      .any(|line| line == "data/train.bin" || line.starts_with(".expri/"))
  );

  // A fresh checkout must not silently accept newer bytes from a moving URL.
  *origin.bytes.lock().unwrap() = b"second larger dataset".to_vec();
  let clone = tempfile::tempdir().unwrap();
  fs::create_dir(clone.path().join("data")).unwrap();
  fs::write(
    clone.path().join("data/train.bin.expri.toml"),
    &original_sidecar,
  )
  .unwrap();
  let rejected = expri(
    clone.path(),
    &["assets", "download", "data/train.bin", "--json"],
  );
  assert!(!rejected.status.success());
  assert!(!clone.path().join("data/train.bin").exists());
  assert_eq!(
    fs::read(clone.path().join("data/train.bin.expri.toml")).unwrap(),
    original_sidecar
  );

  let updated = succeeds(
    root.path(),
    &["assets", "update", "data/train.bin", "--json"],
  );
  assert_eq!(updated["changed"], true);
  assert_ne!(updated["sha256"], imported["sha256"]);
  assert_eq!(
    fs::read(root.path().join("data/train.bin")).unwrap(),
    b"second larger dataset"
  );
  origin.stop();
  let requests = origin.requests.load(Ordering::Relaxed);
  fs::remove_file(root.path().join("data/train.bin")).unwrap();
  succeeds(root.path(), &["assets", "download", "--json"]);
  let status = succeeds(root.path(), &["assets", "status", "--json"]);
  assert_eq!(status["assets"][0]["status"], "ready");
  assert_eq!(origin.requests.load(Ordering::Relaxed), requests);
  assert_eq!(
    fs::read(root.path().join("data/train.bin")).unwrap(),
    b"second larger dataset"
  );
}

#[test]
fn edited_files_existing_code_and_unsafe_paths_are_preserved() {
  let root = tempfile::tempdir().unwrap();
  let origin = Origin::new(b"immutable asset");
  fs::write(root.path().join("train.py"), "print('source')").unwrap();
  assert!(
    !expri(root.path(), &["assets", "import", &origin.url, "train.py"])
      .status
      .success()
  );
  assert_eq!(
    fs::read_to_string(root.path().join("train.py")).unwrap(),
    "print('source')"
  );
  for path in [
    "../outside.bin",
    ".git/config",
    "uv.lock",
    "data/../train.bin",
  ] {
    assert!(
      !expri(root.path(), &["assets", "import", &origin.url, path])
        .status
        .success()
    );
  }
  assert_eq!(origin.requests.load(Ordering::Relaxed), 0);
  let imported = succeeds(
    root.path(),
    &["assets", "import", &origin.url, "data/train.bin", "--json"],
  );
  // Replace the workspace path rather than changing the shared read-only cache inode.
  fs::remove_file(root.path().join("data/train.bin")).unwrap();
  fs::write(root.path().join("data/train.bin"), b"local edits").unwrap();
  let status = succeeds(
    root.path(),
    &["assets", "status", "data/train.bin", "--json"],
  );
  assert_eq!(status["assets"][0]["status"], "modified");
  assert!(
    !expri(root.path(), &["assets", "download", "data/train.bin"])
      .status
      .success()
  );
  assert!(
    !expri(root.path(), &["assets", "update", "data/train.bin"])
      .status
      .success()
  );
  assert_eq!(
    fs::read(root.path().join("data/train.bin")).unwrap(),
    b"local edits"
  );
  let downloaded = succeeds(
    root.path(),
    &["assets", "download", "data/train.bin", "--force", "--json"],
  );
  assert_eq!(downloaded["assets"][0]["sha256"], imported["sha256"]);
  assert_eq!(
    fs::read(root.path().join("data/train.bin")).unwrap(),
    b"immutable asset"
  );
}

#[test]
fn tracked_data_is_rejected_before_download_even_when_missing_on_disk() {
  let root = tempfile::tempdir().unwrap();
  let origin = Origin::new(b"asset");
  assert!(git(root.path(), &["init", "--quiet"]).status.success());
  fs::write(root.path().join("dataset.bin"), b"old").unwrap();
  assert!(git(root.path(), &["add", "dataset.bin"]).status.success());
  fs::remove_file(root.path().join("dataset.bin")).unwrap();
  let result = expri(
    root.path(),
    &["assets", "import", &origin.url, "dataset.bin"],
  );
  assert!(!result.status.success());
  assert!(String::from_utf8_lossy(&result.stderr).contains("tracked by Git"));
  assert_eq!(origin.requests.load(Ordering::Relaxed), 0);
}

#[test]
fn ignored_data_directories_keep_bytes_ignored_and_descriptors_trackable() {
  let root = tempfile::tempdir().unwrap();
  assert!(git(root.path(), &["init", "--quiet"]).status.success());
  fs::write(root.path().join(".gitignore"), "data/\n*.toml\n").unwrap();
  let origin = Origin::new(b"asset");
  succeeds(
    root.path(),
    &[
      "assets",
      "import",
      &origin.url,
      "data/nested/train.bin",
      "--json",
    ],
  );
  fs::write(root.path().join("data/other.bin"), b"unrelated").unwrap();
  fs::write(root.path().join("data/nested/other.bin"), b"unrelated").unwrap();
  fs::create_dir(root.path().join("data/nested/.expri-asset-crash")).unwrap();
  fs::write(
    root.path().join("data/nested/.expri-asset-crash/binding"),
    b"leftover",
  )
  .unwrap();
  for path in [
    "data/nested/train.bin",
    "data/other.bin",
    "data/nested/other.bin",
    "data/nested/.expri-asset-crash/binding",
  ] {
    assert!(
      git(root.path(), &["check-ignore", "--quiet", path])
        .status
        .success(),
      "{path}"
    );
  }
  assert!(
    !git(
      root.path(),
      &[
        "check-ignore",
        "--quiet",
        "data/nested/train.bin.expri.toml"
      ]
    )
    .status
    .success()
  );
  assert!(git(root.path(), &["add", "."]).status.success());
  let tracked = String::from_utf8(git(root.path(), &["ls-files"]).stdout).unwrap();
  assert!(
    tracked
      .lines()
      .any(|path| path == "data/nested/train.bin.expri.toml")
  );
  assert!(!tracked.lines().any(|path| path.ends_with(".bin")));
}
