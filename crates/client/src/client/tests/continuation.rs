use super::*;
use std::sync::{Arc, Mutex};

fn fixture(root: &Path, url: &str, scope: &RunScope) -> PublishOptions {
  let config = config(root, url);
  let run_dir = root.join("run");
  std::fs::create_dir(&run_dir).unwrap();
  std::fs::write(
    run_dir.join("run-state.json"),
    serde_json::to_vec(&json!({"run_id":scope.run_id,"status":"preparing","publication_epoch":0}))
      .unwrap(),
  )
  .unwrap();
  let options = PublishOptions {
    config,
    run_dir,
    project_id: scope.project_id.clone(),
    origin: scope.origin.clone(),
    artifacts: Vec::new(),
    watch: true,
    queue_dir: root.join("queue"),
  };
  let mut publisher = Publisher::new(&options).unwrap();
  publisher.queue.state.protocol = Some(queue::Protocol::TrackingV1);
  publisher
    .queue
    .state
    .streams
    .insert("outputs/metrics.jsonl".into(), 17);
  publisher.queue.state.documents.insert(
    "run-state.json".into(),
    queue::SavedDocument {
      revision: 7,
      size: 2,
      sha256: "a".repeat(64),
      snapshot: None,
      offset: 2,
      complete: true,
    },
  );
  publisher.queue.state.archive = Some(ArchiveRecord {
    status: "archived".into(),
    incomplete: false,
    file: None,
    last_error: None,
  });
  publisher.queue.save().unwrap();
  options
}

fn sealed_files(scope: &RunScope) -> Vec<FileRecord> {
  [("run-state.json", 7, 2), ("outputs/metrics.jsonl", 1, 17)]
    .into_iter()
    .map(|(path, revision, size)| FileRecord {
      target: run_target(scope, path),
      size,
      sha256: None,
      provenance: None,
      storage: FileStorage::Tracking {
        revision,
        sealed: true,
      },
    })
    .collect()
}

#[test]
fn sealed_attempt_retries_the_exact_cas_without_rewriting_old_metadata() {
  let (_temporary, root) = root();
  let scope = RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "continued".into(),
  };
  let captured = Arc::new(Mutex::new(Vec::new()));
  let requests = captured.clone();
  let expected = scope.clone();
  let (url, task) = mock(10, move |request, _| {
    let request: Request = serde_json::from_slice(&request.body).unwrap();
    let response = match request {
      Request::Capabilities => Response::Capabilities {
        features: vec!["tracking-v1".into(), "run-task-continuation-v1".into()],
      },
      Request::CheckRunContinuation { scope } => {
        assert_eq!(scope, expected);
        Response::RunAttempt { epoch: 0 }
      }
      Request::ListFiles { scope } => {
        assert_eq!(scope, expected);
        Response::Files {
          files: vec![
            FileRecord {
              target: run_target(&scope, "run-state.json"),
              size: 2,
              sha256: None,
              provenance: None,
              storage: FileStorage::Tracking {
                revision: 7,
                sealed: true,
              },
            },
            FileRecord {
              target: run_target(&scope, "outputs/metrics.jsonl"),
              size: 17,
              sha256: None,
              provenance: None,
              storage: FileStorage::Tracking {
                revision: 1,
                sealed: true,
              },
            },
          ],
        }
      }
      Request::BeginRunAttempt {
        ref scope,
        ref request_id,
        expected_epoch,
        attempt,
        ref documents,
        ref streams,
      } => {
        assert_eq!(scope, &expected);
        assert_eq!(request_id, "continue-3");
        assert_eq!((expected_epoch, attempt), (0, 3));
        assert_eq!(documents["run-state.json"], 7);
        assert_eq!(streams["outputs/metrics.jsonl"], 17);
        let mut saved = requests.lock().unwrap();
        saved.push(serde_json::to_value(&request).unwrap());
        if saved.len() == 1 {
          return (
            503,
            Vec::new(),
            br#"{"error":"response unavailable after commit"}"#.to_vec(),
          );
        }
        Response::RunAttempt { epoch: 1 }
      }
      other => {
        panic!("sealed continuation must not publish preparing metadata before its CAS: {other:?}")
      }
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let options = fixture(&root, &url, &scope);
  let config = options.config.clone();
  let run_dir = options.run_dir.clone();
  assert!(begin_task_attempt(&options, Api::new(&config).unwrap(), "continue-3", 1, 3).is_err());
  let state: Value =
    serde_json::from_slice(&std::fs::read(run_dir.join("run-state.json")).unwrap()).unwrap();
  assert_eq!(state["publication_epoch"], 0);
  begin_task_attempt(&options, Api::new(&config).unwrap(), "continue-3", 1, 3).unwrap();
  begin_task_attempt(&options, Api::new(&config).unwrap(), "continue-3", 1, 3).unwrap();
  let publisher = Publisher::new(&options).unwrap();
  assert_eq!(publisher.api.publication_epoch, 1);
  assert_eq!(
    publisher.queue.state.documents["run-state.json"].revision,
    7
  );
  assert_eq!(publisher.queue.state.streams["outputs/metrics.jsonl"], 17);
  assert!(publisher.queue.state.archive.is_none());
  assert_eq!(publisher.queue.state.archive_history.len(), 1);
  task.join().unwrap();
  let requests = captured.lock().unwrap();
  assert_eq!(requests.len(), 3);
  assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn new_attempt_recovers_uncertain_publication_even_if_reconciliation_ack_is_lost() {
  for committed in [false, true] {
    let (_temporary, root) = root();
    let scope = RunScope {
      project_id: "project".into(),
      origin: "worker".into(),
      run_id: "continued".into(),
    };
    let expected = scope.clone();
    let mut first_begin = true;
    let mut first_reconcile = true;
    let old_epoch = u64::from(committed);
    let (url, task) = mock(10, move |request, _| {
      let operation: Request = serde_json::from_slice(&request.body).unwrap();
      let response = match operation {
        Request::Capabilities => Response::Capabilities {
          features: vec!["tracking-v1".into(), "run-task-continuation-v1".into()],
        },
        Request::CheckRunContinuation { scope } => {
          assert_eq!(scope, expected);
          Response::RunAttempt {
            epoch: if first_begin { 0 } else { old_epoch },
          }
        }
        Request::ListFiles { scope } => {
          assert_eq!(scope, expected);
          Response::Files {
            files: sealed_files(&scope),
          }
        }
        Request::BeginRunAttempt {
          scope,
          request_id,
          expected_epoch,
          attempt,
          ..
        } => {
          assert_eq!(scope, expected);
          if first_begin {
            first_begin = false;
            assert_eq!(
              (request_id.as_str(), expected_epoch, attempt),
              ("uncertain", 0, 3)
            );
            return (
              503,
              Vec::new(),
              br#"{"error":"uncertain CAS outcome"}"#.to_vec(),
            );
          }
          assert_eq!(
            (request_id.as_str(), expected_epoch, attempt),
            ("fresh", old_epoch, 4)
          );
          Response::RunAttempt {
            epoch: old_epoch + 1,
          }
        }
        Request::ReconcileRunAttempt {
          scope,
          request_id,
          expected_epoch,
        } => {
          assert_eq!(scope, expected);
          assert_eq!((request_id.as_str(), expected_epoch), ("uncertain", 0));
          if first_reconcile {
            first_reconcile = false;
            return (
              503,
              Vec::new(),
              br#"{"error":"lost reconciliation acknowledgement"}"#.to_vec(),
            );
          }
          Response::ReconciledRunAttempt {
            committed_epoch: committed.then_some(1),
          }
        }
        other => panic!("unexpected recovery request {other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    });
    let options = fixture(&root, &url, &scope);
    assert!(
      begin_task_attempt(
        &options,
        Api::new(&options.config).unwrap(),
        "uncertain",
        1,
        3
      )
      .is_err()
    );
    assert!(reconcile_task_attempt(&options, Api::new(&options.config).unwrap()).is_err());
    let state: Value =
      serde_json::from_slice(&std::fs::read(options.run_dir.join("run-state.json")).unwrap())
        .unwrap();
    assert_eq!(state["publication_epoch"], 0);
    reconcile_task_attempt(&options, Api::new(&options.config).unwrap()).unwrap();
    // The failed task attempt is never relaunched; a new explicit request can
    // now allocate the next generation from a fresh acknowledged snapshot.
    begin_task_attempt(&options, Api::new(&options.config).unwrap(), "fresh", 1, 4).unwrap();
    let publisher = Publisher::new(&options).unwrap();
    assert_eq!(publisher.api.publication_epoch, old_epoch + 1);
    assert_eq!(publisher.queue.state.streams["outputs/metrics.jsonl"], 17);
    assert_eq!(publisher.queue.state.archive_history.len(), 1);
    task.join().unwrap();
  }
}
