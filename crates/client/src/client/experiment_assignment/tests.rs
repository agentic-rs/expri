use super::*;
use crate::client::tests::mock;
use expri_core::experiment_assignment::ExperimentSelection;
use serde_json::json;

fn selection() -> ExperimentSelection {
  ExperimentSelection::JoinRun {
    run_id: "run-parent".into(),
    origin: Some("other-worker".into()),
  }
}

fn config(root: &Path, url: &str) -> std::path::PathBuf {
  let path = root.join("client.toml");
  std::fs::write(&path, format!("url={url:?}\ntoken='fixture-token'\n")).unwrap();
  path
}

fn scope() -> RunScope {
  RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-child".into(),
  }
}

fn intent() -> RunExperimentIntent {
  RunExperimentIntent {
    request_id: "create-child".into(),
    selection: selection(),
  }
}

#[test]
fn preflight_defers_unavailable_service_but_rejects_authentication_and_invalid_selection() {
  let root = tempfile::tempdir().unwrap();
  for status in [401, 403, 404, 409, 410, 500, 503] {
    let (url, server) = mock(
      2,
      move |request, _| match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::Capabilities => (
          200,
          vec![],
          serde_json::to_vec(&Response::Capabilities {
            features: vec![EXPERIMENT_ASSIGNMENT_CAPABILITY.into()],
          })
          .unwrap(),
        ),
        Request::ValidateExperimentSelection {
          project_id,
          intent: selected,
        } => {
          assert_eq!(project_id, "project");
          assert_eq!(selected, intent());
          (
            status,
            vec![],
            br#"{"error":"fixture selection denied"}"#.to_vec(),
          )
        }
        _ => panic!("unexpected preflight mutation"),
      },
    );
    assert_eq!(
      preflight(&config(root.path(), &url), "project", &intent()).is_ok(),
      status >= 500
    );
    server.join().unwrap();
  }
  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  drop(listener);
  preflight(&config(root.path(), &url), "project", &intent()).unwrap();
}

#[test]
fn preflight_rejects_an_older_server_without_launching_assignment() {
  let root = tempfile::tempdir().unwrap();
  let (url, server) = mock(1, |request, _| {
    assert!(matches!(
      serde_json::from_slice::<Request>(&request.body).unwrap(),
      Request::Capabilities
    ));
    (
      200,
      vec![],
      serde_json::to_vec(&Response::Capabilities { features: vec![] }).unwrap(),
    )
  });
  assert!(
    preflight(&config(root.path(), &url), "project", &intent())
      .unwrap_err()
      .to_string()
      .contains("upgrade the server")
  );
  server.join().unwrap();
}

#[test]
fn assignment_acknowledgement_survives_reopening_and_prevents_readding_after_ui_edits() {
  let root = tempfile::tempdir().unwrap();
  let (url, server) = mock(2, |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec![EXPERIMENT_ASSIGNMENT_CAPABILITY.into()],
      },
      Request::AssignRunExperiment {
        scope: selected,
        intent: selected_intent,
      } => {
        assert_eq!(selected, scope());
        assert_eq!(selected_intent, intent());
        Response::RunExperimentAssigned {
          assignment: RunExperimentAssignment {
            scope: selected,
            experiment_id: "exp-one".into(),
            name: "Experiment".into(),
          },
        }
      }
      _ => panic!("unexpected request"),
    };
    (200, vec![], serde_json::to_vec(&response).unwrap())
  });
  let api = Api::new(&config(root.path(), &url)).unwrap();
  let directory = root.path().canonicalize().unwrap().join("queue");
  let mut queue = Queue::new(directory.clone(), json!(scope())).unwrap();
  let state = json!({"experiment_intent": intent()});
  publish(&api, &mut queue, &scope(), &state).unwrap();
  drop(queue);
  server.join().unwrap();
  // The server is gone. A durable acknowledgement must avoid further API
  // requests, including after an owner edits the experiment from the website.
  let mut reopened = Queue::new(directory, json!(scope())).unwrap();
  publish(&api, &mut reopened, &scope(), &state).unwrap();
  let changed = json!({"experiment_intent": RunExperimentIntent { request_id: "other".into(), selection: selection() }});
  assert!(publish(&api, &mut reopened, &scope(), &changed).is_err());
}

#[test]
fn assignment_rejection_does_not_permanently_disable_progress_publishing() {
  let root = tempfile::tempdir().unwrap();
  let (url, server) = mock(2, |request, _| {
    match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => (
        200,
        vec![],
        serde_json::to_vec(&Response::Capabilities {
          features: vec![EXPERIMENT_ASSIGNMENT_CAPABILITY.into()],
        })
        .unwrap(),
      ),
      Request::AssignRunExperiment { .. } => (
        403,
        vec![],
        br#"{"error":"owner credential required"}"#.to_vec(),
      ),
      _ => panic!("unexpected request"),
    }
  });
  let api = Api::new(&config(root.path(), &url)).unwrap();
  let mut queue = Queue::new(
    root.path().canonicalize().unwrap().join("queue"),
    json!(scope()),
  )
  .unwrap();
  let failure = publish(
    &api,
    &mut queue,
    &scope(),
    &json!({"experiment_intent": intent()}),
  )
  .unwrap_err();
  assert!(failure.to_string().contains("owner credential required"));
  assert!(crate::client::Publisher::permanent_rejection(&failure).is_none());
  assert!(queue.state.experiment_assignment.is_none());
  server.join().unwrap();
}

#[test]
fn terminal_capture_seals_while_grouping_is_offline_and_retries_after_reopening() {
  use crate::types::ArchiveRecord;
  use std::collections::BTreeSet;

  let root = tempfile::tempdir().unwrap();
  let root = root.path().canonicalize().unwrap();
  let run = root.join("run-child");
  std::fs::create_dir(&run).unwrap();
  let state = json!({"run_id":"run-child", "status":"completed", "experiment_intent":intent()});
  std::fs::write(
    run.join("run-state.json"),
    serde_json::to_vec(&state).unwrap(),
  )
  .unwrap();
  std::fs::write(run.join("snapshot.json"), br#"{"run_id":"run-child"}"#).unwrap();
  let mut attempts = 0;
  let mut sealed = false;
  let (url, server) = mock(10, move |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec![
          "tracking-v1".into(),
          EXPERIMENT_ASSIGNMENT_CAPABILITY.into(),
        ],
      },
      Request::PutDocument {
        revision,
        total_size,
        ..
      } => Response::DocumentAcknowledged {
        offset: total_size,
        revision,
        complete: true,
      },
      Request::AssignRunExperiment { scope, .. } => {
        attempts += 1;
        if attempts == 1 {
          return (
            503,
            vec![],
            br#"{"error":"assignment temporarily unavailable"}"#.to_vec(),
          );
        }
        assert!(sealed, "terminal run must seal before its assignment retry");
        Response::RunExperimentAssigned {
          assignment: RunExperimentAssignment {
            scope,
            experiment_id: "exp-one".into(),
            name: "Experiment".into(),
          },
        }
      }
      Request::SealRun { .. } => {
        sealed = true;
        Response::Archive {
          archive: ArchiveRecord {
            status: "pending".into(),
            incomplete: false,
            file: None,
            last_error: None,
          },
        }
      }
      _ => panic!("unexpected request"),
    };
    (200, vec![], serde_json::to_vec(&response).unwrap())
  });
  let api = Api::new(&config(&root, &url)).unwrap();
  let directory = root.join("queue");
  let mut queue = Queue::new(directory.clone(), json!(scope())).unwrap();
  let failure = crate::client::publish_cycle(
    &api,
    &mut queue,
    &scope(),
    &run,
    &BTreeSet::new(),
    crate::client::PublishPurpose::Run { watch: true },
    &mut |_| Ok(()),
  )
  .unwrap_err();
  assert!(failure.to_string().contains("temporarily unavailable"));
  assert!(
    queue.state.archive.is_some(),
    "assignment outage must not prevent result.zip sealing"
  );
  drop(queue);
  let mut reopened = Queue::new(directory, json!(scope())).unwrap();
  assert!(
    crate::client::publish_cycle(
      &api,
      &mut reopened,
      &scope(),
      &run,
      &BTreeSet::new(),
      crate::client::PublishPurpose::Run { watch: true },
      &mut |_| Ok(())
    )
    .unwrap()
  );
  assert!(reopened.state.experiment_assignment.is_some());
  server.join().unwrap();
}
