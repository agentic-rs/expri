#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const OWNER: &str = "queue-integration-owner-token-with-24-characters";
const TASK: &str = r#"#!/bin/sh
set -eu
printf '%s\n' "$EXPRI_RUN_ID" >> "$2"
cat marker.txt > "$EXPRI_OUTPUT_DIR/frozen.txt"
printf '%s\n' "${CUDA_VISIBLE_DEVICES-}" > "$EXPRI_OUTPUT_DIR/gpus.txt"
while [ ! -f "$1" ]; do sleep 0.05; done
"#;

// A dependency-free native stand-in for uv keeps these lifecycle tests offline.
// The real Rust run supervisor still prepares, launches, and observes the task.
const UV_SOURCE: &str = r#"
use std::{env, fs, path::PathBuf, process::Command};
use std::os::unix::process::CommandExt;
fn string(value: &str) -> String {
  format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
}
fn field(text: &str, name: &str) -> String {
  let key = format!("\"{name}\"");
  let tail = text.split_once(&key).unwrap().1.split_once(':').unwrap().1.trim_start();
  let mut chars = tail.strip_prefix('"').unwrap().chars();
  let mut result = String::new();
  while let Some(ch) = chars.next() {
    match ch {
      '"' => return result,
      '\\' => result.push(match chars.next().unwrap() { 'n' => '\n', 'r' => '\r', 't' => '\t', other => other }),
      other => result.push(other),
    }
  }
  panic!("unterminated JSON field")
}
fn main() {
  let args: Vec<String> = env::args().skip(1).collect();
  if args.starts_with(&["run".into(), "--isolated".into()]) {
    let root = PathBuf::from(field(args.last().unwrap(), "state_dir"));
    let venv = root.join("environment/.venv");
    fs::create_dir_all(&venv).unwrap();
    let manifest = root.join("environment/environment-state.json");
    fs::write(&manifest, "{}").unwrap();
    println!("{{\"environment_path\":{},\"python\":\"unused\",\"manifest_path\":{},\"run_env\":{{\"PATH\":{}}}}}",
      string(&venv.to_string_lossy()), string(&manifest.to_string_lossy()), string(&env::var("PATH").unwrap()));
  } else if args.starts_with(&["run".into(), "--no-sync".into(), "--no-env-file".into(), "--".into()]) {
    panic!("{}", Command::new(&args[4]).args(&args[5..]).exec());
  } else {
    panic!("unexpected uv arguments")
  }
}
"#;

struct NativeUv {
  _directory: tempfile::TempDir,
  bin: PathBuf,
}

fn native_uv() -> &'static NativeUv {
  static UV: OnceLock<NativeUv> = OnceLock::new();
  UV.get_or_init(|| {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("uv.rs");
    let bin = directory.path().canonicalize().unwrap();
    fs::write(&source, UV_SOURCE).unwrap();
    let output = Command::new("rustc")
      .args(["--edition=2024", "--crate-name", "queue_fixture_uv"])
      .arg(&source)
      .arg("-o")
      .arg(bin.join("uv"))
      .output()
      .unwrap();
    assert_success(&output);
    NativeUv {
      _directory: directory,
      bin,
    }
  })
}

struct ChildGuard(Child);

impl ChildGuard {
  fn stop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

impl Drop for ChildGuard {
  fn drop(&mut self) {
    self.stop();
  }
}

struct HttpMessage {
  method: String,
  target: String,
  range: Option<(usize, usize)>,
  body: Vec<u8>,
}

fn read_http(stream: &mut TcpStream) -> HttpMessage {
  // Accepted sockets inherit listener flags on some Unix platforms. Poll the
  // listener without making an in-progress request fail between TCP packets.
  stream.set_nonblocking(false).unwrap();
  stream
    .set_read_timeout(Some(Duration::from_secs(30)))
    .unwrap();
  stream
    .set_write_timeout(Some(Duration::from_secs(30)))
    .unwrap();
  let mut bytes = Vec::new();
  let header_end = loop {
    if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
      break index + 4;
    }
    let mut chunk = [0_u8; 4096];
    let count = stream.read(&mut chunk).unwrap();
    assert!(count > 0, "HTTP peer closed before its headers");
    bytes.extend_from_slice(&chunk[..count]);
    assert!(bytes.len() <= 64 * 1024, "fixture HTTP header is too large");
  };
  let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
  let mut first = headers.lines().next().unwrap().split_whitespace();
  let method = first.next().unwrap().to_owned();
  let target = first.next().unwrap().to_owned();
  let range = headers
    .lines()
    .filter_map(|line| line.split_once(':'))
    .find(|(key, _)| key.eq_ignore_ascii_case("range"))
    .map(|(_, value)| {
      let (start, end) = value
        .trim()
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
      (start.parse().unwrap(), end.parse().unwrap())
    });
  let size = headers
    .lines()
    .filter_map(|line| line.split_once(':'))
    .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
    .map(|(_, value)| value.trim().parse::<usize>().unwrap())
    .unwrap_or(0);
  assert!(size <= 16 * 1024 * 1024, "fixture HTTP body is too large");
  while bytes.len() - header_end < size {
    let mut chunk = [0_u8; 8192];
    let count = stream.read(&mut chunk).unwrap();
    assert!(count > 0, "HTTP peer closed before its body");
    bytes.extend_from_slice(&chunk[..count]);
  }
  HttpMessage {
    method,
    target,
    range,
    body: bytes[header_end..header_end + size].to_vec(),
  }
}

fn reply(
  stream: &mut TcpStream,
  status: u16,
  headers: &str,
  body: &[u8],
  size: usize,
) -> std::io::Result<()> {
  write!(
    stream,
    "HTTP/1.1 {status} fixture\r\nConnection: close\r\nContent-Length: {size}\r\n{headers}\r\n"
  )?;
  stream.write_all(body)?;
  let _ = stream.shutdown(Shutdown::Both);
  Ok(())
}

fn reply_object(
  stream: &mut TcpStream,
  method: &str,
  range: Option<(usize, usize)>,
  bytes: &[u8],
) -> std::io::Result<()> {
  let headers = "Content-Type: application/octet-stream\r\nETag: \"fixture-object\"\r\n";
  if method == "HEAD" {
    reply(stream, 200, headers, b"", bytes.len())
  } else if let Some((start, end)) = range {
    let headers = format!(
      "{headers}Content-Range: bytes {start}-{end}/{}\r\n",
      bytes.len()
    );
    let body = &bytes[start..=end];
    reply(stream, 206, &headers, body, body.len())
  } else {
    reply(stream, 200, headers, bytes, bytes.len())
  }
}

// Exercise the service's actual S3 adapter and source upload/download protocol,
// while accepting requests only on a private loopback listener.
struct S3Fixture {
  endpoint: String,
  objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
  block_downloads: Arc<AtomicBool>,
  download_entered: Arc<AtomicBool>,
  tamper_next_download: Arc<AtomicBool>,
  stop: Arc<AtomicBool>,
  thread: Option<thread::JoinHandle<()>>,
}

struct DownloadBlock<'a>(&'a S3Fixture);

impl DownloadBlock<'_> {
  fn await_download(&self) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !self.0.download_entered.load(Ordering::Acquire) {
      assert!(
        Instant::now() < deadline,
        "worker did not request its queued source"
      );
      thread::sleep(Duration::from_millis(25));
    }
  }
}

impl Drop for DownloadBlock<'_> {
  fn drop(&mut self) {
    self.0.block_downloads.store(false, Ordering::Release);
  }
}

impl S3Fixture {
  fn new() -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let objects = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
    let thread_objects = Arc::clone(&objects);
    let block_downloads = Arc::new(AtomicBool::new(false));
    let thread_block_downloads = Arc::clone(&block_downloads);
    let download_entered = Arc::new(AtomicBool::new(false));
    let thread_download_entered = Arc::clone(&download_entered);
    let tamper_next_download = Arc::new(AtomicBool::new(false));
    let thread_tamper_next_download = Arc::clone(&tamper_next_download);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || {
      let mut parts = BTreeMap::new();
      let mut downloads = Vec::new();
      while !thread_stop.load(Ordering::Acquire) {
        let mut stream = match listener.accept() {
          Ok((stream, _)) => stream,
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
            continue;
          }
          Err(error) => panic!("fixture S3 accept failed: {error}"),
        };
        let request = read_http(&mut stream);
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let key = url.path().to_owned();
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        match request.method.as_str() {
          "POST" if query.contains_key("uploads") => {
            let xml = format!(
              "<InitiateMultipartUploadResult><Bucket>queue-fixture</Bucket><Key>{key}</Key><UploadId>fixture-upload</UploadId></InitiateMultipartUploadResult>"
            );
            reply(
              &mut stream,
              200,
              "Content-Type: application/xml\r\n",
              xml.as_bytes(),
              xml.len(),
            )
            .unwrap();
          }
          "PUT" if query.contains_key("partNumber") => {
            parts.insert(key, request.body);
            reply(&mut stream, 200, "ETag: \"fixture-part\"\r\n", b"", 0).unwrap();
          }
          "POST" if query.contains_key("uploadId") => {
            let bytes = parts
              .remove(&key)
              .expect("completion has no uploaded source part");
            thread_objects.lock().unwrap().insert(key.clone(), bytes);
            let xml = format!(
              "<CompleteMultipartUploadResult><Location>http://fixture{key}</Location><Bucket>queue-fixture</Bucket><Key>{key}</Key><ETag>\"fixture-object\"</ETag></CompleteMultipartUploadResult>"
            );
            reply(
              &mut stream,
              200,
              "Content-Type: application/xml\r\n",
              xml.as_bytes(),
              xml.len(),
            )
            .unwrap();
          }
          "HEAD" | "GET" => {
            let bytes = thread_objects.lock().unwrap().get(&key).cloned();
            if let Some(mut bytes) = bytes {
              if request.method == "GET"
                && thread_tamper_next_download.swap(false, Ordering::AcqRel)
              {
                *bytes.first_mut().expect("queued source must not be empty") ^= 1;
              }
              if request.method == "GET" && thread_block_downloads.load(Ordering::Acquire) {
                let blocked = Arc::clone(&thread_block_downloads);
                let stopped = Arc::clone(&thread_stop);
                let range = request.range;
                thread_download_entered.store(true, Ordering::Release);
                downloads.push(thread::spawn(move || {
                  while blocked.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(10));
                  }
                  // Service stop intentionally closes an in-flight download.
                  // Either its headers or body can observe that peer disconnect.
                  if let Err(error) = reply_object(&mut stream, "GET", range, &bytes)
                    && !matches!(
                      error.kind(),
                      std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    )
                  {
                    panic!("delayed fixture download failed: {error}");
                  }
                }));
              } else {
                reply_object(&mut stream, &request.method, request.range, &bytes).unwrap();
              }
            } else {
              reply(&mut stream, 404, "", b"", 0).unwrap();
            }
          }
          "DELETE" => {
            thread_objects.lock().unwrap().remove(&key);
            reply(&mut stream, 204, "", b"", 0).unwrap();
          }
          _ => panic!(
            "unexpected S3 request {} {}",
            request.method, request.target
          ),
        }
      }
      for download in downloads {
        download.join().unwrap();
      }
    });
    Self {
      endpoint,
      objects,
      block_downloads,
      download_entered,
      tamper_next_download,
      stop,
      thread: Some(thread),
    }
  }

  fn block_download(&self) -> DownloadBlock<'_> {
    self.download_entered.store(false, Ordering::Release);
    self.block_downloads.store(true, Ordering::Release);
    DownloadBlock(self)
  }

  fn tamper_next_download(&self) {
    self.tamper_next_download.store(true, Ordering::Release);
  }
}

impl Drop for S3Fixture {
  fn drop(&mut self) {
    self.block_downloads.store(false, Ordering::Release);
    self.stop.store(true, Ordering::Release);
    if let Some(thread) = self.thread.take() {
      let result = thread.join();
      if !thread::panicking() {
        result.unwrap();
      }
    }
  }
}

struct Fixture {
  _root: tempfile::TempDir,
  project: PathBuf,
  storage: PathBuf,
  worker_storage: PathBuf,
  endpoint: String,
  starts: PathBuf,
  server: ChildGuard,
  s3: S3Fixture,
}

impl Fixture {
  fn new() -> Self {
    let root = tempfile::Builder::new()
      .prefix("expri queue 'quoted' ")
      .tempdir()
      .unwrap();
    let base = root.path().canonicalize().unwrap();
    let project = base.join("project");
    let storage = base.join("submitter");
    let worker_storage = base.join("worker");
    for directory in [&project, &storage, &worker_storage] {
      fs::create_dir(directory).unwrap();
    }
    fs::create_dir(project.join(".expri")).unwrap();
    fs::write(project.join("train.sh"), TASK).unwrap();
    fs::set_permissions(project.join("train.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(project.join("marker.txt"), "submitted source\n").unwrap();
    fs::write(
      project.join("pyproject.toml"),
      "[project]\nname='queue-fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(project.join("uv.lock"), "version=1\n").unwrap();
    fs::write(
      project.join(".expri/checkout.manifest"),
      "train.sh\nmarker.txt\npyproject.toml\nuv.lock\n",
    )
    .unwrap();
    fs::write(
      project.join("expri.toml"),
      "[environment]\n[tasks]\ntrain=['/bin/sh', 'train.sh']\n",
    )
    .unwrap();
    let s3 = S3Fixture::new();
    let address = TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap();
    let endpoint = format!("http://{address}");
    let server_config = base.join("server.toml");
    fs::write(
      &server_config,
      format!(
        "owner_token_env='EXPRI_QUEUE_TEST_OWNER'\n[storage]\nendpoint={:?}\nbucket='queue-fixture'\nregion='us-east-1'\npath_style=true\n",
        s3.endpoint
      ),
    )
    .unwrap();
    let server_log = base.join("server.stderr");
    let mut server = ChildGuard(
      Command::new(env!("CARGO_BIN_EXE_expri"))
        .args(["service", "serve", "--config"])
        .arg(server_config)
        .arg("--listen")
        .arg(address.to_string())
        .arg("--data-dir")
        .arg(base.join("service"))
        .env("EXPRI_QUEUE_TEST_OWNER", OWNER)
        .env("AWS_ACCESS_KEY_ID", "queue-fixture-access")
        .env("AWS_SECRET_ACCESS_KEY", "queue-fixture-secret")
        .env_remove("AWS_SESSION_TOKEN")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdout(Stdio::null())
        .stderr(fs::File::create(&server_log).unwrap())
        .spawn()
        .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      if TcpStream::connect(address).is_ok() {
        break;
      }
      assert!(
        server.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
        "service failed to start: {}",
        fs::read_to_string(&server_log).unwrap()
      );
      thread::sleep(Duration::from_millis(10));
    }
    let connection = format!("url={endpoint:?}\ntoken={OWNER:?}\n");
    fs::write(
      storage.join("config.toml"),
      format!("[server]\n{connection}"),
    )
    .unwrap();
    Self {
      _root: root,
      project,
      storage,
      worker_storage,
      endpoint,
      starts: base.join("starts.txt"),
      server,
      s3,
    }
  }

  fn command(&self) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_expri"));
    command
      .current_dir(&self.project)
      .env("EXPRI_STORAGE_DIR", &self.storage)
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_SYSTEM", "/dev/null")
      .env_remove("GIT_DIR")
      .env_remove("GIT_WORK_TREE")
      .env_remove("GIT_INDEX_FILE")
      .env("NO_PROXY", "127.0.0.1,localhost");
    command
  }

  fn run(&self, args: &[&str]) -> Output {
    let output = self.command().args(args).output().unwrap();
    assert_success(&output);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(OWNER));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(OWNER));
    output
  }

  fn json(&self, args: &[&str]) -> Value {
    serde_json::from_slice(&self.run(args).stdout).unwrap()
  }

  fn submit(&self, gate: &Path, resources: &[&str]) -> Value {
    let output = self
      .command()
      .args(["run", "--queue", "--json", "--project-id", "queue-project"])
      .args(resources)
      .args([
        "train",
        gate.to_str().unwrap(),
        self.starts.to_str().unwrap(),
      ])
      .output()
      .unwrap();
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
  }

  fn gate(&self, name: &str) -> PathBuf {
    let gate = self._root.path().join(format!("{name}.gate"));
    fs::write(gate.with_extension("gate.pending"), b"").unwrap();
    gate
  }

  fn show(&self, job_id: &str) -> Value {
    self.json(&["queue", "show", job_id, "--json"])
  }

  fn await_status(&self, job_id: &str, statuses: &[&str]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      let job = self.show(job_id);
      if statuses.contains(&job["status"].as_str().unwrap_or("")) {
        return job;
      }
      assert!(
        Instant::now() < deadline,
        "queue job failed to reach {statuses:?}: {job}"
      );
      thread::sleep(Duration::from_millis(25));
    }
  }

  fn starts(&self) -> Vec<String> {
    fs::read_to_string(&self.starts)
      .unwrap_or_default()
      .lines()
      .map(str::to_owned)
      .collect()
  }

  fn await_starts(&self, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while self.starts().len() < count {
      assert!(
        Instant::now() < deadline,
        "task failed to launch; jobs: {}",
        self.json(&["queue", "list", "--json"])
      );
      thread::sleep(Duration::from_millis(25));
    }
  }

  fn daemon(&self, cpus: u32, memory_bytes: u64, gpus: bool) -> ChildGuard {
    self.daemon_with_global(cpus, memory_bytes, gpus, None)
  }

  fn global_daemon(&self, cpus: u32, memory_bytes: u64, project_id: Option<&str>) -> ChildGuard {
    let mut connection = format!("url={:?}\ntoken={OWNER:?}\n", self.endpoint);
    if let Some(project_id) = project_id {
      connection.push_str(&format!("project_id={project_id:?}\n"));
    }
    self.daemon_with_global(cpus, memory_bytes, false, Some(&connection))
  }

  fn daemon_with_global(
    &self,
    cpus: u32,
    memory_bytes: u64,
    gpus: bool,
    connection: Option<&str>,
  ) -> ChildGuard {
    let mut config = connection
      .map(|connection| format!("[server]\n{connection}\n"))
      .unwrap_or_else(|| {
        format!(
          "[server]\nurl={:?}\ntoken={OWNER:?}\nproject_id='queue-project'\n",
          self.endpoint
        )
      });
    config.push_str(&format!(
      "[queue_worker]\nworker_id='worker-a'\nproject_ids=['queue-project']\n[queue_worker.capacity]\ncpus={cpus}\nmemory_bytes={memory_bytes}\n"
    ));
    if gpus {
      config.push_str("[[queue_worker.capacity.gpus]]\ndevice_id='GPU-fixture'\nmodel='fixture GPU'\nvendor='nvidia'\nselector='GPU-fixture'\nmemory_bytes=8589934592\n");
    } else {
      config.push_str("gpus=[]\n");
    }
    fs::write(self.worker_storage.join("config.toml"), config).unwrap();
    self.start_service()
  }

  fn start_service(&self) -> ChildGuard {
    let path = format!(
      "{}:{}",
      native_uv().bin.display(),
      std::env::var("PATH").unwrap()
    );
    let mut command = self.command();
    command.args(["service", "start", "--foreground", "--storage-dir"]);
    command.arg(&self.worker_storage);
    ChildGuard(
      command
        .env("EXPRI_STORAGE_DIR", &self.worker_storage)
        .env("PATH", path)
        .stdout(Stdio::null())
        .stderr(fs::File::create(self.worker_storage.join("daemon.stderr")).unwrap())
        .spawn()
        .unwrap(),
    )
  }

  fn output_path(&self, name: &str) -> PathBuf {
    let run_id = self.starts().into_iter().next().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let mut matches = Vec::new();
      find_files(&self.worker_storage, name, &mut matches);
      if let Some(path) = matches.into_iter().find(|path| {
        path
          .components()
          .any(|part| part.as_os_str() == run_id.as_str())
          && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0)
      }) {
        return path;
      }
      assert!(
        Instant::now() < deadline,
        "worker run output is missing: {name}"
      );
      thread::sleep(Duration::from_millis(25));
    }
  }

  fn service_report(&self, action: &str) -> Value {
    self.component_report(action, None)
  }

  fn component_report(&self, action: &str, component: Option<&str>) -> Value {
    let mut command = self.command();
    command
      .args(["service", action, "--json", "--storage-dir"])
      .arg(&self.worker_storage);
    if let Some(component) = component {
      command.args(["--component", component]);
    }
    let output = command.output().unwrap();
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
  }

  fn queue(&self, request: Value) -> Value {
    queue_request(&self.endpoint, request)
  }

  fn register(&self, worker_id: &str, session_id: &str, capacity: Value) -> Value {
    self.queue(json!({
      "operation": "register_worker",
      "worker_id": worker_id,
      "session_id": session_id,
      "project_ids": ["queue-project"],
      "capacity": capacity,
    }))
  }
}

impl Drop for Fixture {
  fn drop(&mut self) {
    for entry in fs::read_dir(self._root.path()).unwrap() {
      let path = entry.unwrap().path();
      if path
        .extension()
        .is_some_and(|extension| extension == "pending")
      {
        let _ = fs::write(path.with_extension(""), b"release");
      }
    }
    // Detached supervisors outlive the daemon. Let released shell tasks exit
    // before TempDir removes their gates, including after a failed assertion.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
      let mut states = Vec::new();
      find_files(&self.worker_storage, "run-state.json", &mut states);
      if states.iter().all(|path| {
        fs::read(path)
          .ok()
          .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
          .is_some_and(|state| {
            matches!(
              state["status"].as_str(),
              Some("completed" | "failed" | "cancelled" | "lost")
            )
          })
      }) || Instant::now() >= deadline
      {
        break;
      }
      thread::sleep(Duration::from_millis(25));
    }
    self.server.stop();
  }
}

fn find_files(directory: &Path, name: &str, matches: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(directory).unwrap() {
    let entry = entry.unwrap();
    let path = entry.path();
    if entry.file_type().unwrap().is_dir() {
      find_files(&path, name, matches);
    } else if entry.file_name() == name {
      matches.push(path);
    }
  }
}

fn assert_success(output: &Output) {
  assert!(
    output.status.success(),
    "command failed: {}\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

fn queue_request(endpoint: &str, request: Value) -> Value {
  let (status, value) = queue_http(endpoint, request);
  assert_eq!(status, 200, "queue API failed: {value}");
  assert_eq!(value["kind"], "queue");
  value["response"].clone()
}

fn queue_http(endpoint: &str, request: Value) -> (u16, Value) {
  let address = url::Url::parse(endpoint)
    .unwrap()
    .socket_addrs(|| None)
    .unwrap()[0];
  let mut stream = TcpStream::connect(address).unwrap();
  let body = serde_json::to_vec(&json!({"action": "queue", "request": request})).unwrap();
  write!(
    stream,
    "POST /v1/request HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {OWNER}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    body.len()
  )
  .unwrap();
  stream.write_all(&body).unwrap();
  let response = read_http(&mut stream);
  let value: Value = serde_json::from_slice(&response.body).unwrap();
  (response.target.parse().unwrap(), value)
}

fn capacity(cpus: u32, memory_bytes: u64, gpu_id: Option<&str>) -> Value {
  let gpus = gpu_id
    .map(|gpu_id| {
      vec![json!({
        "device_id": gpu_id,
        "model": "fixture GPU",
        "memory_bytes": 8_u64 * 1024 * 1024 * 1024,
        "vendor": "nvidia",
        "selector": gpu_id,
      })]
    })
    .unwrap_or_default();
  json!({"cpus": cpus, "memory_bytes": memory_bytes, "gpus": gpus})
}

fn claim(worker_id: &str, session_id: &str, claim_id: &str) -> Value {
  json!({
    "operation": "claim",
    "worker_id": worker_id,
    "session_id": session_id,
    "claim_id": claim_id,
  })
}

fn identity(attempt: &Value) -> Value {
  json!({
    "attempt_id": attempt["attempt_id"],
    "worker_id": attempt["worker_id"],
    "session_id": attempt["session_id"],
    "fencing_token": attempt["fencing_token"],
  })
}

#[test]
fn worker_with_no_capacity_section_registers_detected_budget_and_executes_queued_work() {
  let fixture = Fixture::new();
  let expected = expri_system::detect_worker_capacity().unwrap();
  fs::write(fixture.worker_storage.join("config.toml"), format!(
    "[server]\nurl={:?}\ntoken={OWNER:?}\n[queue_worker]\nworker_id='worker-a'\nproject_ids=['queue-project']\n",
    fixture.endpoint,
  )).unwrap();
  let gate = fixture.gate("inferred-worker");
  let job = fixture.submit(&gate, &["--cpus", "1", "--memory", "1MiB"]);
  let _service = fixture.start_service();
  fixture.await_status(job["job_id"].as_str().unwrap(), &["running"]);
  fixture.await_starts(1);
  let workers = fixture.queue(json!({"operation": "list_workers", "cursor": null, "limit": 20}));
  assert_eq!(
    workers["workers"][0]["capacity"],
    serde_json::to_value(expected).unwrap()
  );
  assert_eq!(workers["workers"][0]["reserved"]["cpus"], 1);
  fs::write(gate, b"release").unwrap();
  fixture.await_status(job["job_id"].as_str().unwrap(), &["completed"]);
}

#[test]
fn shared_service_reports_queue_capacity_and_stop_preserves_training_for_restart() {
  let fixture = Fixture::new();
  let gate = fixture.gate("shared-service");
  let job = fixture.submit(&gate, &["--cpus", "2", "--memory", "2GiB"]);
  let job_id = job["job_id"].as_str().unwrap();
  let mut service = fixture.daemon(2, 2 << 30, false);
  fixture.await_status(job_id, &["running"]);
  fixture.await_starts(1);
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    let status = fixture.service_report("status");
    if status["ready"] == true && status["execution"]["outstanding_attempts"] == 1 {
      assert_eq!(status["execution"]["worker_id"], "worker-a");
      assert_eq!(status["execution"]["capacity"]["cpus"], 2);
      assert_eq!(status["execution"]["available"]["cpus"], 0);
      assert_eq!(status["execution"]["reserved"]["cpus"], 2);
      break;
    }
    assert!(
      Instant::now() < deadline,
      "executor status was not reported: {status}"
    );
    thread::sleep(Duration::from_millis(25));
  }
  fixture.service_report("stop");
  let deadline = Instant::now() + Duration::from_secs(10);
  while service.0.try_wait().unwrap().is_none() {
    assert!(Instant::now() < deadline, "shared service did not stop");
    thread::sleep(Duration::from_millis(25));
  }
  assert_eq!(fixture.show(job_id)["status"], "running");
  assert_eq!(fixture.starts().len(), 1);
  let workers = fixture.queue(json!({"operation": "list_workers", "cursor": null, "limit": 20}));
  assert_eq!(workers["workers"][0]["reserved"]["cpus"], 2);
  fs::write(&gate, b"release").unwrap();
  let _restarted = fixture.daemon(2, 2 << 30, false);
  fixture.await_status(job_id, &["completed"]);
  assert_eq!(fixture.starts().len(), 1);
  let workers = fixture.queue(json!({"operation": "list_workers", "cursor": null, "limit": 20}));
  assert_eq!(workers["workers"][0]["available"]["cpus"], 2);
}

#[test]
fn queue_execution_can_pause_and_resume_while_training_and_other_components_continue() {
  let fixture = Fixture::new();
  let gate = fixture.gate("component-active");
  let job = fixture.submit(&gate, &["--cpus", "2", "--memory", "2GiB"]);
  let job_id = job["job_id"].as_str().unwrap();
  let _service = fixture.daemon(2, 2 << 30, false);
  fixture.await_status(job_id, &["running"]);
  fixture.await_starts(1);
  let initial = fixture.service_report("status");
  fixture.component_report("stop", Some("queue-execution"));
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    let status = fixture.service_report("status");
    if status["components"]["queue_execution"]["phase"] == "stopped" {
      assert_eq!(status["pid"], initial["pid"]);
      assert_eq!(status["components"]["collection"]["phase"], "running");
      assert_eq!(status["components"]["publishing"]["phase"], "running");
      assert_eq!(
        status["components"]["queue_execution"]["desired_running"],
        false
      );
      break;
    }
    assert!(
      Instant::now() < deadline,
      "executor did not pause: {status}"
    );
    thread::sleep(Duration::from_millis(25));
  }
  assert_eq!(fixture.show(job_id)["status"], "running");
  assert_eq!(fixture.starts().len(), 1);
  let workers = fixture.queue(json!({"operation": "list_workers", "cursor": null, "limit": 20}));
  assert_eq!(workers["workers"][0]["reserved"]["cpus"], 2);
  fs::write(&gate, b"release").unwrap();
  fixture.component_report("start", Some("queue-execution"));
  fixture.await_status(job_id, &["completed"]);
  assert_eq!(fixture.service_report("status")["pid"], initial["pid"]);
  assert_eq!(fixture.starts().len(), 1);
  let workers = fixture.queue(json!({"operation": "list_workers", "cursor": null, "limit": 20}));
  assert_eq!(workers["workers"][0]["available"]["cpus"], 2);
}

#[test]
fn queued_runs_can_execute_with_collection_paused_without_claiming_a_complete_capture() {
  let fixture = Fixture::new();
  let _service = fixture.daemon(2, 2 << 30, false);
  // This also handles a pause racing with initial collection startup.
  fixture.component_report("stop", Some("collection"));
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    let status = fixture.service_report("status");
    if status["components"]["collection"]["phase"] == "stopped" {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "collection did not pause: {status}"
    );
    thread::sleep(Duration::from_millis(25));
  }
  let gate = fixture.gate("paused-collection");
  let job = fixture.submit(&gate, &["--cpus", "1", "--memory", "1GiB"]);
  let job_id = job["job_id"].as_str().unwrap();
  fixture.await_status(job_id, &["running"]);
  fixture.await_starts(1);
  let output = fixture.output_path("frozen.txt");
  let run = output.parent().unwrap().parent().unwrap();
  let state: Value =
    serde_json::from_slice(&fs::read(run.join("run-state.json")).unwrap()).unwrap();
  assert!(state["system_capture_id"].is_null(), "{state}");
  assert!(state["system_capture_error"].as_str().is_some(), "{state}");
  assert_eq!(
    fixture.service_report("status")["components"]["collection"]["desired_running"],
    false
  );
  fs::write(&gate, b"release").unwrap();
  fixture.await_status(job_id, &["completed"]);
}

#[test]
fn shared_service_stop_interrupts_source_download_without_cancelling_running_training() {
  let fixture = Fixture::new();
  let first_gate = fixture.gate("shared-active");
  let first = fixture.submit(&first_gate, &["--cpus", "1", "--memory", "1GiB"]);
  let mut service = fixture.daemon(2, 2 << 30, false);
  fixture.await_status(first["job_id"].as_str().unwrap(), &["running"]);
  fixture.await_starts(1);
  let blocked = fixture.s3.block_download();
  let second_gate = fixture.gate("shared-blocked");
  let second = fixture.submit(&second_gate, &["--cpus", "1", "--memory", "1GiB"]);
  blocked.await_download();
  assert_eq!(fixture.service_report("status")["ready"], true);
  fixture.service_report("stop");
  let deadline = Instant::now() + Duration::from_secs(10);
  while service.0.try_wait().unwrap().is_none() {
    assert!(
      Instant::now() < deadline,
      "source download prevented service stop"
    );
    thread::sleep(Duration::from_millis(25));
  }
  assert_eq!(
    fixture.show(first["job_id"].as_str().unwrap())["status"],
    "running"
  );
  assert_eq!(
    fixture.show(second["job_id"].as_str().unwrap())["status"],
    "starting"
  );
  assert_eq!(fixture.starts().len(), 1);
  let workers = fixture.queue(json!({"operation": "list_workers", "cursor": null, "limit": 20}));
  assert_eq!(workers["workers"][0]["reserved"]["cpus"], 2);
  drop(blocked);
  fs::write(&first_gate, b"release").unwrap();
}

#[test]
fn queued_submission_freezes_uploaded_source_and_daemon_restart_does_not_duplicate_it() {
  let fixture = Fixture::new();
  fs::remove_file(fixture.project.join(".expri/checkout.manifest")).unwrap();
  fs::write(fixture.project.join(".gitignore"), "marker.txt\n").unwrap();
  let config_path = fixture.project.join("expri.toml");
  let mut config = fs::read_to_string(&config_path).unwrap();
  config.push_str("\n[push]\ninclude_ignored=['marker.txt']\n");
  fs::write(config_path, config).unwrap();
  let initialized = Command::new("git")
    .args(["init", "--quiet"])
    .current_dir(&fixture.project)
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env_remove("GIT_DIR")
    .env_remove("GIT_WORK_TREE")
    .env_remove("GIT_INDEX_FILE")
    .output()
    .unwrap();
  assert_success(&initialized);
  let gate = fixture.gate("frozen");
  let receipt = fixture.submit(&gate, &["--cpus", "1", "--memory", "16MiB"]);
  let job_id = receipt["job_id"].as_str().unwrap();
  assert_eq!(receipt["status"], "queued");
  assert!(
    !fixture.s3.objects.lock().unwrap().is_empty(),
    "submission did not upload source"
  );
  assert!(
    fixture.starts().is_empty(),
    "submission launched the job on the client"
  );
  assert!(!fixture.project.join(".expri/runs").exists());

  fs::write(
    fixture.project.join("marker.txt"),
    "changed after submission\n",
  )
  .unwrap();
  fs::write(fixture.project.join("train.sh"), "exit 91\n").unwrap();
  let mut daemon = fixture.daemon(1, 16 * 1024 * 1024, false);
  fixture.await_starts(1);
  let running = fixture.await_status(job_id, &["running"]);
  let attempt_id = running["attempt"]["attempt_id"].clone();
  assert_eq!(
    fs::read_to_string(fixture.output_path("frozen.txt")).unwrap(),
    "submitted source\n"
  );

  let last_seen = fixture.json(&["queue", "workers", "--json"])["workers"][0]["last_seen_at_ms"]
    .as_i64()
    .unwrap();
  daemon.stop();
  let mut restarted = fixture.daemon(1, 16 * 1024 * 1024, false);
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    assert!(
      restarted.0.try_wait().unwrap().is_none(),
      "restarted daemon exited"
    );
    let workers = fixture.json(&["queue", "workers", "--json"]);
    if workers["workers"][0]["last_seen_at_ms"].as_i64().unwrap() > last_seen {
      let recovered = fixture.show(job_id);
      assert_eq!(recovered["attempt"]["attempt_id"], attempt_id);
      break;
    }
    assert!(
      Instant::now() < deadline,
      "restarted daemon failed to renew its worker lease: {workers}"
    );
    thread::sleep(Duration::from_millis(25));
  }
  fs::write(&gate, b"release").unwrap();
  let completed = fixture.await_status(job_id, &["completed"]);
  assert_eq!(completed["attempt"]["attempt_id"], attempt_id);
  assert_eq!(completed["attempt"]["exit_code"], 0);
  assert_eq!(
    fixture.starts().len(),
    1,
    "daemon recovery launched the frozen job twice"
  );
}

#[test]
fn queued_receipts_and_catalog_are_human_by_default_and_cancel_before_launch_is_terminal() {
  let fixture = Fixture::new();
  let gate = fixture.gate("cancelled");
  let output = fixture
    .command()
    .args([
      "run",
      "--queue",
      "--project-id",
      "queue-project",
      "--cpus",
      "1",
      "--memory",
      "16MiB",
      "train",
    ])
    .args([gate.to_str().unwrap(), fixture.starts.to_str().unwrap()])
    .output()
    .unwrap();
  assert_success(&output);
  let text = String::from_utf8(output.stdout).unwrap();
  assert!(text.contains("FIELD"));
  assert!(text.contains("queued"));
  assert!(serde_json::from_str::<Value>(&text).is_err());
  let list = fixture.json(&["queue", "list", "--project-id", "queue-project", "--json"]);
  let jobs = list["jobs"].as_array().unwrap();
  assert_eq!(jobs.len(), 1);
  let job_id = jobs[0]["job_id"].as_str().unwrap();
  assert!(text.contains(job_id));
  let human = String::from_utf8(fixture.run(&["queue", "list"]).stdout).unwrap();
  assert!(human.contains(job_id));
  assert!(human.to_ascii_lowercase().contains("status"));
  assert!(serde_json::from_str::<Value>(&human).is_err());
  let cancelled = fixture.json(&["queue", "cancel", job_id, "--json"]);
  assert_eq!(cancelled["status"], "cancelled");
  assert_eq!(
    fixture.json(&["queue", "cancel", job_id, "--json"])["status"],
    "cancelled"
  );
  let _daemon = fixture.daemon(1, 16 * 1024 * 1024, false);
  let worker_deadline = Instant::now() + Duration::from_secs(10);
  while fixture.json(&["queue", "workers", "--json"])["workers"]
    .as_array()
    .unwrap()
    .is_empty()
  {
    assert!(Instant::now() < worker_deadline, "daemon did not register");
    thread::sleep(Duration::from_millis(25));
  }
  assert!(
    fixture.queue(claim(
      "worker-a",
      fixture.json(&["queue", "workers", "--json"])["workers"][0]["session_id"]
        .as_str()
        .unwrap(),
      "cancelled-claim"
    ))["assignment"]
      .is_null()
  );
  assert!(
    fixture.starts().is_empty(),
    "a cancelled queued job was launched"
  );
}

#[test]
fn queue_rejects_missing_memory_and_invalid_resources_before_uploading_source() {
  let fixture = Fixture::new();
  for resources in [
    vec![],
    vec!["--cpus", "1"],
    vec!["--cpus", "0", "--memory", "16MiB"],
    vec!["--cpus", "1", "--memory", "0"],
    vec!["--memory", "0MiB"],
    vec!["--memory", "18446744073709551615GiB"],
    vec![
      "--memory",
      "16MiB",
      "--gpus",
      "2",
      "--gpu-id",
      "GPU-fixture",
    ],
    vec![
      "--memory",
      "16MiB",
      "--gpus",
      "0",
      "--gpu-model",
      "fixture GPU",
    ],
  ] {
    let output = fixture
      .command()
      .args(["run", "--queue", "--project-id", "queue-project"])
      .args(&resources)
      .arg("train")
      .output()
      .unwrap();
    assert!(
      !output.status.success(),
      "accepted incomplete or invalid resources: {resources:?}"
    );
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(OWNER));
    assert!(
      fixture.s3.objects.lock().unwrap().is_empty(),
      "invalid resource request uploaded source"
    );
  }
  assert_eq!(
    fixture.json(&["queue", "list", "--json"])["jobs"],
    json!([])
  );
  assert!(!fixture.project.join(".expri/runs").exists());
  let valid = fixture.submit(&fixture.gate("default-cpu"), &["--memory", "16MiB"]);
  assert_eq!(valid["resources"]["cpus"], 1);
  assert_eq!(valid["resources"]["memory_bytes"], 16 * 1024 * 1024);
  assert_eq!(valid["status"], "queued");
}

#[test]
fn whole_gpu_cpu_and_memory_reservations_delay_the_second_job_until_first_finishes() {
  for (worker_cpus, worker_memory, gpu, requested) in [
    (
      1,
      32 * 1024 * 1024,
      false,
      vec!["--cpus", "1", "--memory", "16MiB"],
    ),
    (
      2,
      16 * 1024 * 1024,
      false,
      vec!["--cpus", "1", "--memory", "16MiB"],
    ),
    (
      2,
      32 * 1024 * 1024,
      true,
      vec!["--cpus", "1", "--memory", "16MiB", "--gpus", "1"],
    ),
  ] {
    let fixture = Fixture::new();
    let first_gate = fixture.gate("first");
    let second_gate = fixture.gate("second");
    let first = fixture.submit(&first_gate, &requested);
    let second = fixture.submit(&second_gate, &requested);
    let first_id = first["job_id"].as_str().unwrap();
    let second_id = second["job_id"].as_str().unwrap();
    let _daemon = fixture.daemon(worker_cpus, worker_memory, gpu);
    fixture.await_starts(1);
    let first_running = fixture.await_status(first_id, &["running"]);
    for _ in 0..20 {
      let second_pending = fixture.show(second_id);
      assert_eq!(second_pending["status"], "queued");
      assert!(second_pending["attempt"].is_null());
      assert_eq!(
        fixture.starts().len(),
        1,
        "both jobs launched while capacity was reserved"
      );
      thread::sleep(Duration::from_millis(50));
    }
    let worker = &fixture.json(&["queue", "workers", "--json"])["workers"][0];
    assert_eq!(worker["outstanding_attempts"], 1);
    if gpu {
      assert_eq!(
        first_running["attempt"]["allocation"]["gpus"][0]["device_id"],
        "GPU-fixture"
      );
      assert_eq!(
        fs::read_to_string(fixture.output_path("gpus.txt")).unwrap(),
        "GPU-fixture\n"
      );
    }
    if worker_cpus == 1 {
      assert_eq!(
        fixture.json(&["queue", "cancel", first_id, "--json"])["status"],
        "cancel_requested"
      );
      fixture.await_status(first_id, &["cancelled"]);
    } else {
      fs::write(&first_gate, b"release").unwrap();
      fixture.await_status(first_id, &["completed"]);
    }
    fixture.await_starts(2);
    let second_running = fixture.await_status(second_id, &["running"]);
    assert_ne!(
      second_running["attempt"]["attempt_id"],
      first_running["attempt"]["attempt_id"]
    );
    fs::write(&second_gate, b"release").unwrap();
    fixture.await_status(second_id, &["completed"]);
    assert_eq!(fixture.starts().len(), 2);
  }
}

#[test]
fn concurrent_repeated_claims_reserve_one_attempt_and_cannot_overallocate_the_worker() {
  let fixture = Fixture::new();
  let first = fixture.submit(
    &fixture.gate("first"),
    &["--cpus", "1", "--memory", "16MiB", "--gpus", "1"],
  );
  let second = fixture.submit(
    &fixture.gate("second"),
    &["--cpus", "1", "--memory", "16MiB", "--gpus", "1"],
  );
  fixture.register(
    "worker-a",
    "session-a",
    capacity(1, 16 * 1024 * 1024, Some("GPU-fixture")),
  );
  let claims: Vec<_> = (0..8)
    .map(|_| {
      let endpoint = fixture.endpoint.clone();
      thread::spawn(move || {
        queue_request(&endpoint, claim("worker-a", "session-a", "repeat-claim"))
      })
    })
    .collect();
  let replies: Vec<_> = claims
    .into_iter()
    .map(|thread| thread.join().unwrap())
    .collect();
  let assignment = &replies[0]["assignment"];
  assert_eq!(assignment["job"]["job_id"], first["job_id"]);
  for reply in &replies {
    assert_eq!(
      reply["assignment"]["attempt"]["attempt_id"],
      assignment["attempt"]["attempt_id"]
    );
  }
  assert!(fixture.queue(claim("worker-a", "session-a", "next-claim"))["assignment"].is_null());
  assert_eq!(
    fixture.show(second["job_id"].as_str().unwrap())["status"],
    "queued"
  );
  let repeated_submission =
    fixture.queue(json!({"operation": "submit", "submission": assignment["submission"]}));
  assert_eq!(repeated_submission["job"]["job_id"], first["job_id"]);
  assert_eq!(
    fixture.json(&["queue", "list", "--json"])["jobs"]
      .as_array()
      .unwrap()
      .len(),
    2
  );
  fixture.queue(json!({
    "operation": "finish", "attempt": identity(&assignment["attempt"]),
    "status": "completed", "exit_code": 0, "message": null,
  }));
  let next = fixture.queue(claim("worker-a", "session-a", "after-finish"));
  assert_eq!(next["assignment"]["job"]["job_id"], second["job_id"]);
  assert_eq!(
    next["assignment"]["attempt"]["allocation"]["gpus"]
      .as_array()
      .unwrap()
      .len(),
    1
  );
}

#[test]
fn automatic_placement_claims_available_workers_and_a_worker_pin_waits_for_its_worker() {
  let fixture = Fixture::new();
  let automatic = fixture.submit(
    &fixture.gate("automatic"),
    &["--cpus", "1", "--memory", "16MiB"],
  );
  let pinned = fixture.submit(
    &fixture.gate("pinned"),
    &["--cpus", "1", "--memory", "16MiB", "--worker", "worker-b"],
  );
  assert!(automatic["worker_id"].is_null());
  assert_eq!(pinned["worker_id"], "worker-b");
  fixture.register("worker-a", "session-a", capacity(1, 16 * 1024 * 1024, None));
  let placed = fixture.queue(claim("worker-a", "session-a", "automatic-claim"));
  assert_eq!(placed["assignment"]["job"]["job_id"], automatic["job_id"]);
  assert_eq!(placed["assignment"]["attempt"]["worker_id"], "worker-a");
  fixture.queue(json!({
    "operation": "finish", "attempt": identity(&placed["assignment"]["attempt"]),
    "status": "completed", "exit_code": 0, "message": null,
  }));
  assert!(
    fixture.queue(claim("worker-a", "session-a", "wrong-worker-claim"))["assignment"].is_null()
  );
  let waiting = fixture.show(pinned["job_id"].as_str().unwrap());
  assert_eq!(waiting["status"], "queued");
  assert!(waiting["attempt"].is_null());
  fixture.register("worker-b", "session-b", capacity(1, 16 * 1024 * 1024, None));
  let placed = fixture.queue(claim("worker-b", "session-b", "pinned-claim"));
  assert_eq!(placed["assignment"]["job"]["job_id"], pinned["job_id"]);
  assert_eq!(placed["assignment"]["attempt"]["worker_id"], "worker-b");
}

#[test]
fn configured_task_reservations_are_used_when_queue_flags_are_omitted() {
  let fixture = Fixture::new();
  fs::write(
    fixture.project.join("expri.toml"),
    "[environment]\n[tasks.train]\ncommand=['/bin/sh', 'train.sh']\n[tasks.train.resources]\ncpus=2\nmemory='32MiB'\ngpus=0\n",
  )
  .unwrap();
  let receipt = fixture.submit(&fixture.gate("configured"), &[]);
  assert_eq!(receipt["status"], "queued");
  assert_eq!(receipt["resources"]["cpus"], 2);
  assert_eq!(receipt["resources"]["memory_bytes"], 32 * 1024 * 1024);
  assert_eq!(receipt["resources"]["gpus"], 0);
  assert!(fixture.starts().is_empty());
}

#[test]
fn uncertain_worker_holds_its_reservation_until_stopped_is_explicitly_confirmed() {
  let fixture = Fixture::new();
  let first = fixture.submit(
    &fixture.gate("uncertain"),
    &["--cpus", "1", "--memory", "16MiB", "--gpus", "1"],
  );
  let second = fixture.submit(
    &fixture.gate("waiting"),
    &["--cpus", "1", "--memory", "16MiB", "--gpus", "1"],
  );
  fixture.register(
    "worker-a",
    "session-a",
    capacity(1, 16 * 1024 * 1024, Some("GPU-fixture")),
  );
  let claimed = fixture.queue(claim("worker-a", "session-a", "uncertain-claim"));
  let attempt = &claimed["assignment"]["attempt"];
  let uncertain = fixture.queue(json!({
    "operation": "report_uncertain", "attempt": identity(attempt),
    "message": "fixture supervisor identity cannot be confirmed",
  }));
  assert_eq!(uncertain["attempt"]["status"], "unknown");
  assert_eq!(
    fixture.show(first["job_id"].as_str().unwrap())["status"],
    "unknown"
  );
  let workers = fixture.json(&["queue", "workers", "--json"]);
  assert_eq!(workers["workers"][0]["quarantined"], true);
  assert_eq!(workers["workers"][0]["outstanding_attempts"], 1);
  let (status, rejected) = queue_http(
    &fixture.endpoint,
    claim("worker-a", "session-a", "unsafe-reuse"),
  );
  assert_eq!(status, 409);
  assert!(
    rejected["error"]
      .as_str()
      .unwrap()
      .contains("reconciliation")
  );
  assert_eq!(
    fixture.show(second["job_id"].as_str().unwrap())["status"],
    "queued"
  );

  let (status, rejected) = queue_http(
    &fixture.endpoint,
    json!({
      "operation": "recover", "attempt_id": attempt["attempt_id"], "confirmed_stopped": false,
    }),
  );
  assert_eq!(status, 400);
  assert!(rejected["error"].as_str().unwrap().contains("confirmation"));
  assert_eq!(
    fixture.show(first["job_id"].as_str().unwrap())["status"],
    "unknown"
  );
  assert_eq!(
    fixture.json(&["queue", "workers", "--json"])["workers"][0]["outstanding_attempts"],
    1
  );

  let recovered = fixture.queue(json!({
    "operation": "recover", "attempt_id": attempt["attempt_id"], "confirmed_stopped": true,
  }));
  assert_eq!(recovered["attempt"]["status"], "lost");
  assert_eq!(
    fixture.show(first["job_id"].as_str().unwrap())["status"],
    "lost"
  );
  let next = fixture.queue(claim("worker-a", "session-a", "confirmed-safe-reuse"));
  assert_eq!(next["assignment"]["job"]["job_id"], second["job_id"]);
  assert_eq!(
    next["assignment"]["attempt"]["allocation"]["gpus"][0]["device_id"],
    "GPU-fixture"
  );
}

#[test]
fn running_task_cancels_while_another_source_download_is_blocked() {
  let fixture = Fixture::new();
  let first_gate = fixture.gate("running");
  let second_gate = fixture.gate("downloading");
  let first = fixture.submit(&first_gate, &["--cpus", "1", "--memory", "16MiB"]);
  let first_id = first["job_id"].as_str().unwrap();
  let _daemon = fixture.daemon(2, 32 * 1024 * 1024, false);
  fixture.await_starts(1);
  fixture.await_status(first_id, &["running"]);
  let first_state = fixture.output_path("run-state.json");

  let blocked = fixture.s3.block_download();
  let second = fixture.submit(&second_gate, &["--cpus", "1", "--memory", "16MiB"]);
  blocked.await_download();
  assert_eq!(
    fixture.json(&["queue", "cancel", first_id, "--json"])["status"],
    "cancel_requested"
  );
  let deadline = Instant::now() + Duration::from_secs(20);
  loop {
    let state: Value = serde_json::from_slice(&fs::read(&first_state).unwrap()).unwrap();
    if state["status"] == "cancelled" {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "running task cancellation waited for the blocked source download: {state}"
    );
    thread::sleep(Duration::from_millis(25));
  }
  assert!(fixture.s3.block_downloads.load(Ordering::Acquire));
  assert_eq!(fixture.starts().len(), 1, "blocked source launched a task");
  drop(blocked);
  fixture.await_status(first_id, &["cancelled"]);
  fixture.await_starts(2);
  fs::write(&second_gate, b"release").unwrap();
  fixture.await_status(second["job_id"].as_str().unwrap(), &["completed"]);
}

#[test]
fn tampered_source_is_rejected_before_launch_and_its_reservations_are_released() {
  let fixture = Fixture::new();
  let first = fixture.submit(
    &fixture.gate("tampered"),
    &["--cpus", "1", "--memory", "16MiB"],
  );
  let valid_gate = fixture.gate("valid");
  let second = fixture.submit(&valid_gate, &["--cpus", "1", "--memory", "16MiB"]);
  fixture.s3.tamper_next_download();
  let _daemon = fixture.daemon(1, 16 * 1024 * 1024, false);
  let failed = fixture.await_status(first["job_id"].as_str().unwrap(), &["failed"]);
  assert!(
    failed["attempt"]["run_id"].is_null(),
    "corrupt source allocated a native run"
  );
  assert!(
    failed["attempt"]["message"]
      .as_str()
      .unwrap()
      .contains("integrity")
  );
  fixture.await_starts(1);
  let running = fixture.await_status(second["job_id"].as_str().unwrap(), &["running"]);
  assert_eq!(
    fixture.starts(),
    vec![running["attempt"]["run_id"].as_str().unwrap()]
  );
  assert_eq!(
    fixture.json(&["queue", "workers", "--json"])["workers"][0]["outstanding_attempts"],
    1
  );
  fs::write(&valid_gate, b"release").unwrap();
  fixture.await_status(second["job_id"].as_str().unwrap(), &["completed"]);
}

#[test]
fn queued_private_inputs_use_the_jobs_project_without_enabling_publishing() {
  for global_project in [None, Some("different-project")] {
    let fixture = Fixture::new();
    let payload = fixture._root.path().join("private-input.txt");
    fs::write(&payload, b"private dataset from the queued project\n").unwrap();
    let payload = fs::canonicalize(payload).unwrap();
    let uploaded = fixture.json(&[
      "service",
      "--json",
      "input",
      "upload",
      "--project-id",
      "queue-project",
      "--input-id",
      "dataset-fixture",
      "--file",
      payload.to_str().unwrap(),
    ]);
    let descriptor = expri_assets::Descriptor {
      version: expri_assets::DESCRIPTOR_VERSION,
      source: expri_assets::Source::Expri {
        input_id: "dataset-fixture".into(),
      },
      size: uploaded["size"].as_u64().unwrap(),
      sha256: uploaded["sha256"].as_str().unwrap().into(),
      availability: None,
      producer: None,
    };
    fs::create_dir(fixture.project.join("data")).unwrap();
    expri_assets::save(
      &fixture.project.join("data/private.txt.expri.toml"),
      &descriptor,
    )
    .unwrap();
    let manifest = fixture.project.join(".expri/checkout.manifest");
    let mut source_paths = fs::read_to_string(&manifest).unwrap();
    source_paths.push_str("data/private.txt.expri.toml\n");
    fs::write(manifest, source_paths).unwrap();
    fs::write(
      fixture.project.join("train.sh"),
      TASK.replace(
        "cat marker.txt > \"$EXPRI_OUTPUT_DIR/frozen.txt\"",
        "cat marker.txt > \"$EXPRI_OUTPUT_DIR/frozen.txt\"\ncat data/private.txt > \"$EXPRI_OUTPUT_DIR/private.txt\"",
      ),
    )
    .unwrap();
    assert!(!fixture.project.join("data/private.txt").exists());
    let gate = fixture.gate("private-input");
    fs::write(&gate, b"release").unwrap();
    let submitted = fixture.submit(&gate, &["--memory", "16MiB", "--no-publish"]);
    let _daemon = fixture.global_daemon(1, 16 * 1024 * 1024, global_project);
    let completed = fixture.await_status(submitted["job_id"].as_str().unwrap(), &["completed"]);
    assert_eq!(completed["attempt"]["exit_code"], 0);
    assert_eq!(
      fs::read(fixture.output_path("private.txt")).unwrap(),
      fs::read(&payload).unwrap()
    );
    let run = fixture
      .output_path("run-state.json")
      .parent()
      .unwrap()
      .to_path_buf();
    assert!(!run.join("publishing-request.json").exists());
    assert_eq!(
      fixture.json(&["query", "runs", "--project-id", "queue-project", "--json"])["runs"],
      json!([])
    );
  }
}

#[test]
fn source_upload_receipts_are_separate_when_the_saved_server_connection_changes() {
  let first_server = Fixture::new();
  let second_server = Fixture::new();
  let gate = first_server.gate("shared-source");
  let first = first_server.submit(&gate, &["--memory", "16MiB"]);
  fs::write(
    first_server.storage.join("config.toml"),
    format!(
      "[server]\nurl={:?}\ntoken={OWNER:?}\n",
      second_server.endpoint
    ),
  )
  .unwrap();
  let second = first_server.submit(&gate, &["--memory", "16MiB"]);
  assert_eq!(
    first_server.queue(json!({"operation": "get_job", "job_id": first["job_id"]}))["job"]["job_id"],
    first["job_id"]
  );
  assert_eq!(
    second_server.queue(json!({"operation": "get_job", "job_id": second["job_id"]}))["job"]["job_id"],
    second["job_id"]
  );
  for server in [&first_server, &second_server] {
    assert!(
      !server.s3.objects.lock().unwrap().is_empty(),
      "source was not uploaded to the selected server"
    );
    server.register("worker-a", "session-a", capacity(1, 16 * 1024 * 1024, None));
  }
  let first_assignment = first_server.queue(claim("worker-a", "session-a", "first-server-claim"));
  let second_assignment =
    second_server.queue(claim("worker-a", "session-a", "second-server-claim"));
  assert_eq!(
    first_assignment["assignment"]["job"]["job_id"],
    first["job_id"]
  );
  assert_eq!(
    second_assignment["assignment"]["job"]["job_id"],
    second["job_id"]
  );
  assert_eq!(
    first_assignment["assignment"]["submission"]["source"],
    second_assignment["assignment"]["submission"]["source"]
  );
}
