use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use expri_core::service::{FileStorage, Request, Response, RunScope};
use expri_core::service_catalog::{
  ReceivedRunFile, ReportedRunFile, RunArchiveFilter, RunFileAvailability, RunFileEntry,
  RunFilesPage, RunListEntry, RunListPage, SERVICE_CATALOG_CAPABILITY, encode_run_cursor,
};
use serde_json::Value;

fn request(stream: &mut TcpStream) -> Request {
  stream
    .set_read_timeout(Some(Duration::from_secs(5)))
    .unwrap();
  let mut bytes = Vec::new();
  let (header_end, length) = loop {
    let mut chunk = [0; 4096];
    let length = stream.read(&mut chunk).unwrap();
    assert!(length > 0, "client closed before sending HTTP headers");
    bytes.extend_from_slice(&chunk[..length]);
    if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
      let headers = String::from_utf8(bytes[..offset].to_vec())
        .unwrap()
        .to_ascii_lowercase();
      assert!(headers.starts_with("post /v1/request http/1.1\r\n"));
      assert!(
        headers
          .lines()
          .any(|line| line == "authorization: bearer catalog-fixture-token")
      );
      let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
      break (offset + 4, length);
    }
    assert!(bytes.len() < 16 * 1024);
  };
  while bytes.len() < header_end + length {
    let mut chunk = [0; 4096];
    let length = stream.read(&mut chunk).unwrap();
    assert!(length > 0, "client closed before sending the request body");
    bytes.extend_from_slice(&chunk[..length]);
  }
  serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

fn mock_catalog(
  pages: Vec<Response>,
  inspect: impl Fn(Request, usize) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let endpoint = format!("http://{}", listener.local_addr().unwrap());
  let server = thread::spawn(move || {
    let deadline = Instant::now() + Duration::from_secs(20);
    for (index, page) in pages.into_iter().enumerate() {
      for capability in [true, false] {
        let mut stream = loop {
          match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
              assert!(
                Instant::now() < deadline,
                "CLI did not finish catalog requests"
              );
              thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
          }
        };
        let request = request(&mut stream);
        let response = if capability {
          assert!(matches!(request, Request::Capabilities));
          Response::Capabilities {
            features: vec![SERVICE_CATALOG_CAPABILITY.into()],
          }
        } else {
          inspect(request, index);
          page.clone()
        };
        let body = serde_json::to_vec(&response).unwrap();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(&body).unwrap();
      }
    }
  });
  (endpoint, server)
}

fn fixture(endpoint: &str) -> (tempfile::TempDir, PathBuf) {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let config = root.join("client.toml");
  std::fs::write(
    &config,
    format!("url='{endpoint}'\ntoken_env='EXPRI_TEST_CATALOG_TOKEN'\n"),
  )
  .unwrap();
  std::fs::write(root.join("expri.toml"), "this is not valid project TOML").unwrap();
  (directory, config)
}

fn cli(config: &Path, args: &[&str]) -> Value {
  let output = Command::new(env!("CARGO_BIN_EXE_expri"))
    .args(["query"])
    .args(args)
    .arg("--config")
    .arg(config)
    .env("EXPRI_TEST_CATALOG_TOKEN", "catalog-fixture-token")
    .env("NO_PROXY", "127.0.0.1,localhost")
    .current_dir(config.parent().unwrap())
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(!String::from_utf8_lossy(&output.stdout).contains("catalog-fixture-token"));
  serde_json::from_slice(&output.stdout).unwrap()
}

fn scope() -> RunScope {
  RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-one".into(),
  }
}

#[test]
fn run_list_defaults_to_all_accessible_runs_without_reading_or_writing_a_project() {
  let cursor = encode_run_cursor(&scope()).unwrap();
  let page = RunListPage {
    runs: vec![RunListEntry {
      scope: scope(),
      task: Some("train".into()),
      status: "completed".into(),
      started_at: None,
      finished_at: None,
      exit_code: Some(0),
      archived: false,
      warnings: Vec::new(),
    }],
    next_cursor: Some(cursor.clone()),
  };
  let (endpoint, server) = mock_catalog(
    vec![
      Response::RunPage { page },
      Response::RunPage {
        page: RunListPage {
          runs: Vec::new(),
          next_cursor: None,
        },
      },
    ],
    move |request, index| {
      let Request::BrowseRuns { query } = request else {
        panic!("expected run list");
      };
      assert_eq!(query.project_id, None);
      assert_eq!(query.origin, None);
      assert_eq!(query.archival, RunArchiveFilter::Active);
      assert_eq!(query.limit, 50);
      assert_eq!(query.cursor, (index == 1).then(|| cursor.clone()));
    },
  );
  let (_directory, config) = fixture(&endpoint);
  let page = cli(&config, &["runs"]);
  assert_eq!(page["runs"][0]["scope"]["origin"], "worker");
  assert_eq!(page["runs"][0]["status"], "completed");
  let next = page["next_cursor"].as_str().unwrap();
  assert_eq!(
    cli(&config, &["runs", "--cursor", next])["runs"],
    serde_json::json!([])
  );
  server.join().unwrap();
  let mut names = std::fs::read_dir(config.parent().unwrap())
    .unwrap()
    .map(|entry| entry.unwrap().file_name())
    .collect::<Vec<_>>();
  names.sort();
  assert_eq!(names, vec!["client.toml", "expri.toml"]);
}

#[test]
fn run_files_expose_pending_uploads_and_versions_with_literal_prefix_and_continuation() {
  let paths = ["outputs/a_%/pending.pt", "outputs/a_%/stale.pt"];
  let report = ReportedRunFile {
    size: 4,
    sha256: Some("a".repeat(64)),
    labels: vec!["latest".into()],
    sync_status: Some("registered".into()),
    sync_error: None,
  };
  let page = RunFilesPage {
    scope: scope(),
    files: vec![
      RunFileEntry {
        path: paths[0].into(),
        availability: RunFileAvailability::ReportedOnly,
        received: None,
        reported: Some(report.clone()),
        labels_pending: true,
      },
      RunFileEntry {
        path: paths[1].into(),
        availability: RunFileAvailability::ReceivedDifferentVersion,
        received: Some(ReceivedRunFile {
          size: 3,
          sha256: Some("b".repeat(64)),
          storage: FileStorage::Object,
        }),
        reported: Some(report),
        labels_pending: true,
      },
    ],
    next_cursor: Some(paths[1].into()),
    inventory_recorded_at: None,
    reported_inventory_truncated: false,
    warnings: Vec::new(),
  };
  let (endpoint, server) =
    mock_catalog(vec![Response::RunFilesPage { page }], move |request, _| {
      let Request::BrowseRunFiles { query } = request else {
        panic!("expected file list");
      };
      assert_eq!(query.scope, scope());
      assert_eq!(query.prefix.as_deref(), Some("outputs/a_%/"));
      assert_eq!(query.limit, 2);
      assert_eq!(query.cursor.as_deref(), Some("outputs/a_%/older.pt"));
    });
  let (_directory, config) = fixture(&endpoint);
  let page = cli(
    &config,
    &[
      "files",
      "--project-id",
      "project",
      "--origin",
      "worker",
      "--run-id",
      "run-one",
      "--prefix",
      "outputs/a_%/",
      "--limit",
      "2",
      "--cursor",
      "outputs/a_%/older.pt",
    ],
  );
  assert_eq!(page["files"][0]["availability"], "reported_only");
  assert_eq!(
    page["files"][1]["availability"],
    "received_different_version"
  );
  assert_eq!(page["files"][1]["labels_pending"], true);
  assert_eq!(page["next_cursor"], paths[1]);
  server.join().unwrap();
}

#[test]
fn global_server_setup_drives_browsing_and_never_prints_the_stored_token() {
  let (endpoint, server) = mock_catalog(
    vec![Response::RunPage {
      page: RunListPage {
        runs: Vec::new(),
        next_cursor: None,
      },
    }],
    |request, _| {
      let Request::BrowseRuns { query } = request else {
        panic!("expected run catalog request");
      };
      assert!(query.project_id.is_none());
      assert!(query.origin.is_none());
    },
  );
  let (directory, _) = fixture(&endpoint);
  let root = directory.path().canonicalize().unwrap().join("global");
  std::fs::create_dir(&root).unwrap();
  std::fs::write(
    root.join("config.toml"),
    "[system_profile]\ninterval_seconds=30\n",
  )
  .unwrap();
  let mut setup = Command::new(env!("CARGO_BIN_EXE_expri"))
    .args([
      "config",
      "server",
      "set",
      "--url",
      &endpoint,
      "--token-stdin",
    ])
    .env("EXPRI_STORAGE_DIR", &root)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  setup
    .stdin
    .take()
    .unwrap()
    .write_all(b"catalog-fixture-token\n")
    .unwrap();
  let setup = setup.wait_with_output().unwrap();
  assert!(
    setup.status.success(),
    "{}",
    String::from_utf8_lossy(&setup.stderr)
  );
  assert!(!String::from_utf8_lossy(&setup.stdout).contains("catalog-fixture-token"));
  let setup: Value = serde_json::from_slice(&setup.stdout).unwrap();
  assert_eq!(setup["server"]["token_source"], "config");
  let config: toml::Value =
    toml::from_str(&std::fs::read_to_string(root.join("config.toml")).unwrap()).unwrap();
  assert_eq!(
    config["server"]["token"].as_str(),
    Some("catalog-fixture-token")
  );
  assert_eq!(
    config["system_profile"]["interval_seconds"].as_integer(),
    Some(30)
  );
  let command = |args: &[&str]| {
    let output = Command::new(env!("CARGO_BIN_EXE_expri"))
      .args(args)
      .env("EXPRI_STORAGE_DIR", &root)
      .env_remove("EXPRI_TEST_CATALOG_TOKEN")
      .env("NO_PROXY", "127.0.0.1,localhost")
      .current_dir(directory.path())
      .output()
      .unwrap();
    assert!(
      output.status.success(),
      "{}",
      String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("catalog-fixture-token"));
    serde_json::from_slice::<Value>(&output.stdout).unwrap()
  };
  assert_eq!(
    command(&["config", "server", "show"])["server"]["url"],
    endpoint
  );
  let before = std::fs::read(root.join("config.toml")).unwrap();
  assert_eq!(command(&["query", "runs"])["runs"], serde_json::json!([]));
  assert_eq!(std::fs::read(root.join("config.toml")).unwrap(), before);
  server.join().unwrap();
  assert_eq!(command(&["config", "server", "unset"])["configured"], false);
  let config: toml::Value =
    toml::from_str(&std::fs::read_to_string(root.join("config.toml")).unwrap()).unwrap();
  assert!(config.get("server").is_none());
  assert_eq!(
    config["system_profile"]["interval_seconds"].as_integer(),
    Some(30)
  );
}

#[test]
fn missing_global_settings_give_setup_guidance_and_explicit_config_bypasses_bad_globals() {
  let (endpoint, server) = mock_catalog(
    vec![Response::RunPage {
      page: RunListPage {
        runs: Vec::new(),
        next_cursor: None,
      },
    }],
    |request, _| assert!(matches!(request, Request::BrowseRuns { .. })),
  );
  let (directory, config) = fixture(&endpoint);
  let root = directory
    .path()
    .canonicalize()
    .unwrap()
    .join("absent-global");
  let missing = Command::new(env!("CARGO_BIN_EXE_expri"))
    .args(["query", "runs"])
    .env("EXPRI_STORAGE_DIR", &root)
    .current_dir(directory.path())
    .output()
    .unwrap();
  assert!(!missing.status.success());
  assert!(String::from_utf8_lossy(&missing.stderr).contains("config server set"));
  assert!(!root.exists());
  std::fs::create_dir(&root).unwrap();
  std::fs::write(root.join("config.toml"), "[server]\ntoken='must-not-leak\n").unwrap();
  let bad = Command::new(env!("CARGO_BIN_EXE_expri"))
    .args(["config", "server", "show"])
    .env("EXPRI_STORAGE_DIR", &root)
    .output()
    .unwrap();
  assert!(!bad.status.success());
  assert!(!String::from_utf8_lossy(&bad.stderr).contains("must-not-leak"));
  let explicit = Command::new(env!("CARGO_BIN_EXE_expri"))
    .args(["query", "runs", "--config"])
    .arg(config)
    .env("EXPRI_STORAGE_DIR", &root)
    .env("EXPRI_TEST_CATALOG_TOKEN", "catalog-fixture-token")
    .env("NO_PROXY", "127.0.0.1,localhost")
    .output()
    .unwrap();
  assert!(
    explicit.status.success(),
    "{}",
    String::from_utf8_lossy(&explicit.stderr)
  );
  server.join().unwrap();
}
