use std::path::Path;

use sha2::{Digest, Sha256};

use super::*;
use crate::service::client::download::mock::mock;

fn root() -> (tempfile::TempDir, PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let root = std::fs::canonicalize(temporary.path()).unwrap();
  (temporary, root)
}

fn config(root: &Path, url: &str) -> PathBuf {
  let path = root.join("client.toml");
  std::fs::write(&path, format!("url={url:?}\ntoken_env='PATH'\n")).unwrap();
  path
}

fn scope() -> RunScope {
  RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-sync".into(),
  }
}

fn record(path: &str, bytes: &[u8]) -> FileRecord {
  FileRecord {
    target: FileTarget::Run {
      scope: scope(),
      path: path.into(),
    },
    size: bytes.len() as u64,
    sha256: Some(fs::hex(&Sha256::digest(bytes))),
    storage: FileStorage::Object,
  }
}

fn options(root: &Path, config: PathBuf) -> FileSyncOptions {
  FileSyncOptions {
    config,
    project_id: "project".into(),
    origins: vec!["worker".into()],
    repo: root.into(),
    results_dir: "results".into(),
    artifacts: vec!["outputs/checkpoint.pt".into()],
    labels: Vec::new(),
    watch: false,
    dry_run: false,
    quiet: true,
  }
}

#[test]
fn missing_checkpoint_stays_pending_then_downloads_and_unchanged_cycle_skips_transfers() {
  let (_temporary, root) = root();
  let state = br#"{"run_id":"run-sync","status":"running"}"#.to_vec();
  let checkpoint = b"finalized checkpoint".to_vec();
  let mut cycle = 0;
  let mut object_gets = 0;
  let (url, task) = mock(14, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::ListRuns { project_id, origin } => {
          assert_eq!(project_id, "project");
          assert_eq!(origin, "worker");
          cycle += 1;
          Response::Runs {
            runs: vec![scope()],
          }
        }
        Request::ListFiles { .. } => {
          let mut files = vec![record("run-state.json", &state)];
          if cycle >= 2 {
            files.push(record("outputs/checkpoint.pt", &checkpoint));
          }
          Response::Files { files }
        }
        Request::DownloadUrl {
          target: FileTarget::Run { path, .. },
        } => {
          if path == "outputs/checkpoint.pt" {
            object_gets += 1;
            assert_eq!(object_gets, 1);
          }
          Response::Url {
            url: format!("{origin}/{path}"),
          }
        }
        other => panic!("unexpected watch request: {other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      assert!(!request.headers.contains_key("authorization"));
      let bytes = if request.path == "/run-state.json" {
        &state
      } else {
        &checkpoint
      };
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()),
        )],
        bytes.clone(),
      )
    }
  });
  let options = options(&root, config(&root, &url));
  let mut watcher = Watcher::new(&options).unwrap();
  let first = watcher.cycle().unwrap();
  assert_eq!(first["status"], "pending");
  assert_eq!(first["retrying_runs"], 0);
  assert_eq!(
    first["runs"][0]["pending_files"],
    json!(["outputs/checkpoint.pt"])
  );
  let second = watcher.cycle().unwrap();
  assert_eq!(second["status"], "synchronized");
  let destination = PathBuf::from(second["runs"][0]["destination"].as_str().unwrap());
  assert_eq!(
    std::fs::read(destination.join("outputs/checkpoint.pt")).unwrap(),
    b"finalized checkpoint"
  );
  let third = watcher.cycle().unwrap();
  assert_eq!(third["runs"][0]["downloaded_bytes"], 0);
  task.join().unwrap();
}

#[test]
fn offline_cycle_retains_cache_and_provider_error_text_never_reaches_output() {
  let (_temporary, root) = root();
  let marker = root.join("saved-checkpoint");
  std::fs::write(&marker, b"keep").unwrap();
  let (url, task) = mock(1, |_request, _| {
    (
      503,
      Vec::new(),
      br#"{"error":"provider https://s3.invalid?X-Amz-Signature=private-token"}"#.to_vec(),
    )
  });
  let options = options(&root, config(&root, &url));
  let mut watcher = Watcher::new(&options).unwrap();
  let report = watcher.cycle().unwrap();
  task.join().unwrap();
  assert_eq!(report["status"], "retrying");
  assert!(!report.to_string().contains("private-token"));
  assert!(!report.to_string().contains("s3.invalid"));
  assert_eq!(watcher.cycle().unwrap()["status"], "retrying");
  assert_eq!(std::fs::read(marker).unwrap(), b"keep");
  assert_eq!(poll_delay(0), 5);
  assert_eq!(poll_delay(20), 60);
}

#[test]
fn authentication_rejection_stops_watch_without_replaying_or_printing_credentials() {
  let (_temporary, root) = root();
  let (url, task) = mock(1, |_request, _| {
    (401, Vec::new(), br#"{"error":"private-token"}"#.to_vec())
  });
  let options = options(&root, config(&root, &url));
  let error = sync(options).unwrap_err().to_string();
  task.join().unwrap();
  assert!(error.contains("authorization"));
  assert!(!error.contains("private-token"));
}

#[test]
fn one_shot_sync_returns_a_safe_error_when_the_service_is_unavailable() {
  let (_temporary, root) = root();
  let (url, task) = mock(1, |_, _| {
    (503, Vec::new(), br#"{"error":"private-token"}"#.to_vec())
  });
  let error = sync(options(&root, config(&root, &url))).unwrap_err();
  task.join().unwrap();
  assert_ne!(error.exit_code(), 0);
  assert!(error.to_string().contains("--watch"));
  assert!(!error.to_string().contains("private-token"));
}

#[test]
fn selected_label_identity_is_pinned_across_sequential_and_background_catalog_refresh() {
  for watch in [false, true] {
    let (_temporary, root) = root();
    let state = br#"{"run_id":"run-sync","status":"running"}"#.to_vec();
    let old = b"registered checkpoint";
    let new = b"replacement checkpoint";
    let inventory = serde_json::to_vec(&json!({"files":[{
      "path":"outputs/checkpoint.pt","size":old.len(),"sync_status":"cloud",
      "sha256":fs::hex(&Sha256::digest(old)),"labels":["best"]}],"truncated":false}))
    .unwrap();
    let mut catalogs = 0;
    let (url, task) = mock(if watch { 10 } else { 8 }, move |request, origin| {
      if request.path == "/v1/request" {
        let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
          Request::ListRuns { .. } => Response::Runs {
            runs: vec![scope()],
          },
          Request::ListFiles { .. } => {
            catalogs += 1;
            Response::Files {
              files: vec![
                record("run-state.json", &state),
                record(crate::run_artifacts::INVENTORY_PATH, &inventory),
                record(
                  "outputs/checkpoint.pt",
                  if catalogs <= 2 { old } else { new },
                ),
              ],
            }
          }
          Request::DownloadUrl {
            target: FileTarget::Run { path, .. },
          } => {
            assert_ne!(
              path, "outputs/checkpoint.pt",
              "a replaced checkpoint must not be downloaded"
            );
            Response::Url {
              url: format!("{origin}/{path}"),
            }
          }
          other => panic!("unexpected pinned selection request: {other:?}"),
        };
        (200, Vec::new(), serde_json::to_vec(&response).unwrap())
      } else {
        let bytes = if request.path == "/run-state.json" {
          &state
        } else {
          assert_eq!(
            request.path,
            format!("/{}", crate::run_artifacts::INVENTORY_PATH)
          );
          &inventory
        };
        (
          206,
          vec![(
            "Content-Range".into(),
            format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()),
          )],
          bytes.clone(),
        )
      }
    });
    let mut options = options(&root, config(&root, &url));
    options.artifacts.clear();
    options.labels = vec!["best".into()];
    options.watch = watch;
    let mut watcher = Watcher::new(&options).unwrap();
    let mut report = watcher.cycle().unwrap();
    if watch {
      let deadline = std::time::Instant::now() + Duration::from_secs(5);
      while !watcher.task.as_ref().unwrap().worker.is_finished() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
      }
      report = watcher.cycle().unwrap();
    }
    assert_eq!(report["status"], "pending");
    assert_eq!(report["retrying_runs"], 0);
    let cached =
      crate::controller::run_pull::cached_runs_dir(&root, "results", "service-project-worker")
        .unwrap()
        .join("run-sync");
    assert!(!cached.join("outputs/checkpoint.pt").exists());
    task.join().unwrap();
  }
}

#[test]
fn best_and_latest_labels_download_one_completed_object() {
  let (_temporary, root) = root();
  let state = br#"{"run_id":"run-sync","status":"running"}"#.to_vec();
  let checkpoint = b"complete".to_vec();
  let inventory = serde_json::to_vec(&json!({"files":[{"path":"outputs/checkpoint.pt","size":8,
    "sync_status":"cloud", "sha256":fs::hex(&Sha256::digest(&checkpoint)), "labels":["best","latest"]}],"truncated":false})).unwrap();
  let files = vec![
    record("run-state.json", &state),
    record("outputs/checkpoint.pt", &checkpoint),
    record(crate::run_artifacts::INVENTORY_PATH, &inventory),
  ];
  let mut checkpoint_requests = 0;
  let (url, task) = mock(16, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::ListRuns { .. } => Response::Runs {
          runs: vec![scope()],
        },
        Request::ListFiles { .. } => Response::Files {
          files: files.clone(),
        },
        Request::DownloadUrl {
          target: FileTarget::Run { path, .. },
        } => {
          if path == "outputs/checkpoint.pt" {
            checkpoint_requests += 1;
            assert_eq!(checkpoint_requests, 1);
          }
          Response::Url {
            url: format!("{origin}/{path}"),
          }
        }
        other => panic!("unexpected label request: {other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      let bytes = if request.path == "/run-state.json" {
        &state
      } else if request.path == format!("/{}", crate::run_artifacts::INVENTORY_PATH) {
        &inventory
      } else {
        &checkpoint
      };
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()),
        )],
        bytes.clone(),
      )
    }
  });
  let mut options = options(&root, config(&root, &url));
  options.artifacts.clear();
  options.labels = vec!["best".into(), "latest".into()];
  let mut watcher = Watcher::new(&options).unwrap();
  let first = watcher.cycle().unwrap();
  assert_eq!(first["status"], "synchronized");
  assert_eq!(first["runs"][0]["pending_files"], json!([]));
  let second = watcher.cycle().unwrap();
  assert_eq!(second["runs"][0]["downloaded_bytes"], 0);
  task.join().unwrap();
}

#[test]
fn edited_destination_and_bad_remote_bytes_do_not_publish_partial_replacements() {
  let (_temporary, root) = root();
  let state = br#"{"run_id":"run-sync","status":"running"}"#.to_vec();
  let checkpoint = b"original".to_vec();
  let files = vec![
    record("run-state.json", &state),
    record("outputs/checkpoint.pt", &checkpoint),
  ];
  let mut cycle = 0;
  let (url, task) = mock(12, move |request, origin| {
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::ListRuns { .. } => {
          cycle += 1;
          Response::Runs {
            runs: vec![scope()],
          }
        }
        Request::ListFiles { .. } => Response::Files {
          files: files.clone(),
        },
        Request::DownloadUrl {
          target: FileTarget::Run { path, .. },
        } => Response::Url {
          url: format!("{origin}/{path}"),
        },
        other => panic!("unexpected watch request: {other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      let bytes = if request.path == "/run-state.json" {
        &state
      } else if cycle == 1 {
        &checkpoint
      } else {
        &b"badbytes".to_vec()
      };
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()),
        )],
        bytes.clone(),
      )
    }
  });
  let options = options(&root, config(&root, &url));
  let mut watcher = Watcher::new(&options).unwrap();
  let first = watcher.cycle().unwrap();
  let destination = PathBuf::from(first["runs"][0]["destination"].as_str().unwrap());
  std::fs::write(destination.join("outputs/checkpoint.pt"), b"local edit").unwrap();
  let second = watcher.cycle().unwrap();
  assert_eq!(second["status"], "retrying");
  assert_eq!(
    std::fs::read(destination.join("outputs/checkpoint.pt")).unwrap(),
    b"local edit"
  );
  task.join().unwrap();
}

#[test]
fn label_waits_for_registered_identity_instead_of_adopting_an_old_same_path_object() {
  let (_temporary, root) = root();
  let mut options = options(&root, config(&root, "http://127.0.0.1:9"));
  options.artifacts.clear();
  options.labels = vec!["best".into()];
  let watcher = Watcher::new(&options).unwrap();
  fs::directories(&root.join("outputs")).unwrap();
  let inventory = root.join(crate::run_artifacts::INVENTORY_PATH);
  let old = record("outputs/checkpoint.pt", b"old bytes");
  let records = BTreeMap::from([("outputs/checkpoint.pt".into(), old.clone())]);
  for (status, digest) in [
    ("registered", old.sha256.clone().unwrap()),
    ("cloud", "a".repeat(64)),
  ] {
    fs::atomic_json(
      &inventory,
      &json!({"files":[{"path":"outputs/checkpoint.pt","size":old.size,
      "sha256":digest,"sync_status":status,"labels":["best"]}],"truncated":false}),
    )
    .unwrap();
    let resolved = watcher.resolve_labels(&root).unwrap();
    let (selected, pending) = watcher.selection(&records, &resolved);
    assert!(selected.is_empty());
    assert_eq!(pending, vec!["label:best"]);
  }
}

#[test]
fn blocked_checkpoint_get_does_not_block_new_metadata_or_overwrite_its_receipt() {
  use crate::service::client::download::mock::concurrent_mock;
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::sync::{Arc, Mutex, mpsc};
  use std::time::Instant;
  let (_temporary, root) = root();
  let updated = Arc::new(AtomicBool::new(false));
  let current = updated.clone();
  let (started_tx, started_rx) = mpsc::channel();
  let (release_tx, release_rx) = mpsc::channel();
  let release = Mutex::new(release_rx);
  let old = br#"{"run_id":"run-sync","status":"running","revision":1}"#.to_vec();
  let new = br#"{"run_id":"run-sync","status":"running","revision":2}"#.to_vec();
  let expected = new.clone();
  let checkpoint = b"completed checkpoint".to_vec();
  let (url, server) = concurrent_mock(16, move |request, origin| {
    let state = if current.load(Ordering::SeqCst) {
      &new
    } else {
      &old
    };
    if request.path == "/v1/request" {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::ListRuns { .. } => Response::Runs {
          runs: vec![scope()],
        },
        Request::ListFiles { .. } => Response::Files {
          files: vec![
            record("run-state.json", state),
            record("outputs/checkpoint.pt", &checkpoint),
          ],
        },
        Request::DownloadUrl {
          target: FileTarget::Run { path, .. },
        } => Response::Url {
          url: format!("{origin}/{path}"),
        },
        Request::GetFile {
          target: FileTarget::Run { path, .. },
        } => {
          assert_eq!(path, "outputs/checkpoint.pt");
          Response::File {
            file: record(&path, &checkpoint),
          }
        }
        other => panic!("unexpected concurrent watch request:{other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    } else {
      let bytes = if request.path == "/outputs/checkpoint.pt" {
        started_tx.send(()).unwrap();
        release
          .lock()
          .unwrap()
          .recv_timeout(Duration::from_secs(10))
          .unwrap();
        &checkpoint
      } else {
        state
      };
      (
        206,
        vec![(
          "Content-Range".into(),
          format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()),
        )],
        bytes.clone(),
      )
    }
  });
  let mut options = options(&root, config(&root, &url));
  options.watch = true;
  let mut watcher = Watcher::new(&options).unwrap();
  let first = watcher.cycle().unwrap();
  assert_eq!(first["active_transfers"], 1);
  started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
  updated.store(true, Ordering::SeqCst);
  let second = watcher.cycle().unwrap();
  let destination = PathBuf::from(second["runs"][0]["destination"].as_str().unwrap());
  assert_eq!(
    std::fs::read(destination.join("run-state.json")).unwrap(),
    expected
  );
  assert!(!destination.join("outputs/checkpoint.pt").exists());
  let metadata_receipt: Value =
    serde_json::from_slice(&std::fs::read(destination.join("pull-state.json")).unwrap()).unwrap();
  release_tx.send(()).unwrap();
  let deadline = Instant::now() + Duration::from_secs(5);
  while !watcher.task.as_ref().unwrap().worker.is_finished() {
    assert!(Instant::now() < deadline, "checkpoint lane did not finish");
    thread::sleep(Duration::from_millis(10));
  }
  let final_report = watcher.cycle().unwrap();
  assert_eq!(final_report["status"], "synchronized");
  let final_receipt: Value =
    serde_json::from_slice(&std::fs::read(destination.join("pull-state.json")).unwrap()).unwrap();
  assert_eq!(final_receipt["pulled_at"], metadata_receipt["pulled_at"]);
  assert_eq!(
    final_receipt["downloaded_files"][0]["path"],
    "outputs/checkpoint.pt"
  );
  assert_eq!(
    std::fs::read(destination.join("run-state.json")).unwrap(),
    expected
  );
  server.join().unwrap();
}

#[test]
fn dry_run_explains_unresolved_labels_without_creating_a_local_cache() {
  let (_temporary, root) = root();
  let state = br#"{"run_id":"run-sync","status":"running"}"#.to_vec();
  let (url, server) = mock(2, move |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::ListRuns { .. } => Response::Runs {
        runs: vec![scope()],
      },
      Request::ListFiles { .. } => Response::Files {
        files: vec![record("run-state.json", &state)],
      },
      other => panic!("dry run requested object bytes: {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let mut options = options(&root, config(&root, &url));
  options.dry_run = true;
  options.watch = true;
  options.artifacts.clear();
  options.labels = vec!["best".into()];
  let report = sync(options).unwrap();
  server.join().unwrap();
  assert_eq!(report["runs"][0]["must_read_inventory"], true);
  assert_eq!(report["runs"][0]["requested_labels"], json!(["best"]));
  assert_eq!(report["runs"][0]["pending_files"], json!([]));
  assert!(!root.join("results").exists());
}
