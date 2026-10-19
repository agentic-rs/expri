use super::*;
use crate::service::store::tests::MockStorage;

mod observations;

const AT: i64 = 100_000;

fn scope() -> RunScope {
  RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-1".into(),
  }
}

fn poll(result: Option<ControlResult>) -> RunControlRequest {
  RunControlRequest::Poll {
    scope: scope(),
    binding_id: "binding-1".into(),
    attempt: 1,
    generation: 1,
    status: "running".into(),
    capabilities: vec![
      ControlAction::Pause,
      ControlAction::Resume,
      ControlAction::Stop,
      ControlAction::Continue,
      ControlAction::Restart,
    ],
    command: vec!["python".into(), "train.py".into()],
    checkpoints: vec!["checkpoints/last.pt".into()],
    lifecycle: None,
    result,
  }
}

fn selection(action: ControlAction) -> ControlSubmission {
  let scope = scope();
  ControlSubmission {
    project_id: scope.project_id,
    origin: scope.origin,
    run_id: scope.run_id,
    request_id: "request-1".into(),
    binding_id: "binding-1".into(),
    expected_attempt: 1,
    expected_generation: 1,
    action,
    arguments: None,
    checkpoint: (action == ControlAction::Continue).then(|| "checkpoints/last.pt".into()),
    reason: "Requested from dashboard".into(),
    grace_seconds: 30,
  }
}

fn result(status: &str) -> ControlResult {
  ControlResult {
    request_id: "request-1".into(),
    status: status.into(),
    error: None,
    new_run_id: None,
  }
}

fn fixture() -> (tempfile::TempDir, Store<MockStorage>) {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  store.poll_run_control_at(poll(None), AT).unwrap();
  (directory, store)
}

fn view(store: &Store<MockStorage>, at: i64) -> Value {
  snapshot(&store.db().unwrap(), &encode(&scope()).unwrap(), at).unwrap()
}

#[test]
fn commands_are_durable_idempotent_and_only_worker_acknowledgements_change_status() {
  let (directory, store) = fixture();
  let command = selection(ControlAction::Pause);
  let queued = store
    .submit_run_control_at(command.clone(), AT + 1)
    .unwrap();
  assert_eq!(queued["status"], "running");
  assert_eq!(queued["request"]["status"], "requested");
  assert_eq!(
    store
      .submit_run_control_at(command.clone(), AT + 2)
      .unwrap(),
    queued
  );
  let mut conflict = command;
  conflict.reason = "different".into();
  assert_eq!(
    store
      .submit_run_control_at(conflict, AT + 2)
      .unwrap_err()
      .status,
    409
  );
  let delivered = store
    .poll_run_control_at(poll(None), AT + 3)
    .unwrap()
    .command
    .unwrap();
  assert_eq!(delivered.expires_at_ms, AT + 1 + DELIVERY_MS);
  assert_eq!(
    store
      .poll_run_control_at(poll(None), AT + 4)
      .unwrap()
      .command,
    Some(delivered)
  );
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert_eq!(view(&store, AT + 5)["request"]["status"], "delivered");
  let mut acknowledged = poll(Some(result("applied")));
  let RunControlRequest::Poll { status, .. } = &mut acknowledged;
  *status = "paused".into();
  assert!(
    store
      .poll_run_control_at(acknowledged.clone(), AT + 6)
      .unwrap()
      .command
      .is_none()
  );
  assert_eq!(view(&store, AT + 6)["status"], "paused");
  assert_eq!(view(&store, AT + 6)["request"]["status"], "applied");
  store.poll_run_control_at(acknowledged, AT + 7).unwrap();
}

#[test]
fn offline_stale_or_unadvertised_commands_are_rejected_and_delivery_expires() {
  let (_directory, store) = fixture();
  assert_eq!(
    store
      .submit_run_control_at(selection(ControlAction::Pause), AT + ONLINE_MS + 1)
      .unwrap_err()
      .status,
    409
  );
  for field in ["binding", "attempt", "generation"] {
    let mut command = selection(ControlAction::Pause);
    match field {
      "binding" => command.binding_id = "other".into(),
      "attempt" => command.expected_attempt = 2,
      _ => command.expected_generation = 2,
    }
    assert_eq!(
      store
        .submit_run_control_at(command, AT + 1)
        .unwrap_err()
        .status,
      409
    );
  }
  let mut update = poll(None);
  let RunControlRequest::Poll { capabilities, .. } = &mut update;
  capabilities.clear();
  store.poll_run_control_at(update, AT + 2).unwrap();
  assert_eq!(
    store
      .submit_run_control_at(selection(ControlAction::Pause), AT + 3)
      .unwrap_err()
      .status,
    409
  );
  store.poll_run_control_at(poll(None), AT + 4).unwrap();
  store
    .submit_run_control_at(selection(ControlAction::Pause), AT + 5)
    .unwrap();
  assert!(
    store
      .poll_run_control_at(poll(None), AT + 5 + DELIVERY_MS)
      .unwrap()
      .command
      .is_none()
  );
  assert_eq!(
    view(&store, AT + 5 + DELIVERY_MS)["request"]["status"],
    "expired"
  );
}

#[test]
fn executing_commands_survive_expiry_and_advanced_attempts_until_final_acknowledgement() {
  let (_directory, store) = fixture();
  store
    .submit_run_control_at(selection(ControlAction::Continue), AT + 1)
    .unwrap();
  store.poll_run_control_at(poll(None), AT + 2).unwrap();
  let mut executing = poll(Some(result("executing")));
  let RunControlRequest::Poll {
    attempt,
    generation,
    ..
  } = &mut executing;
  *attempt = 2;
  *generation = 2;
  assert!(
    store
      .poll_run_control_at(executing.clone(), AT + 3)
      .unwrap()
      .command
      .is_none()
  );
  let mut recovering = executing.clone();
  let RunControlRequest::Poll {
    result: acknowledgement,
    ..
  } = &mut recovering;
  *acknowledgement = None;
  let replay = store
    .poll_run_control_at(recovering, AT + 4)
    .unwrap()
    .command
    .unwrap();
  assert_eq!(replay.expected_attempt, 1);
  assert_eq!(
    view(&store, AT + DELIVERY_MS * 2)["request"]["status"],
    "executing"
  );
  let RunControlRequest::Poll {
    result: acknowledgement,
    ..
  } = &mut executing;
  *acknowledgement = Some(result("applied"));
  assert!(
    store
      .poll_run_control_at(executing, AT + DELIVERY_MS * 2)
      .unwrap()
      .command
      .is_none()
  );
  assert_eq!(
    view(&store, AT + DELIVERY_MS * 2)["request"]["status"],
    "applied"
  );
  assert_eq!(
    store
      .poll_run_control_at(poll(None), AT + DELIVERY_MS * 2 + 1)
      .unwrap_err()
      .status,
    409
  );
}

#[test]
fn delivered_commands_survive_lost_acknowledgements_until_the_worker_resolves_them() {
  let (_directory, store) = fixture();
  store
    .submit_run_control_at(selection(ControlAction::Pause), AT + 1)
    .unwrap();
  store.poll_run_control_at(poll(None), AT + 2).unwrap();
  assert_eq!(
    view(&store, AT + DELIVERY_MS * 2)["request"]["status"],
    "delivered"
  );
  assert!(
    store
      .poll_run_control_at(poll(None), AT + DELIVERY_MS * 2)
      .unwrap()
      .command
      .is_some()
  );
  store
    .poll_run_control_at(poll(Some(result("applied"))), AT + DELIVERY_MS * 2 + 1)
    .unwrap();
  assert_eq!(
    view(&store, AT + DELIVERY_MS * 2 + 1)["request"]["status"],
    "applied"
  );
}

#[test]
fn binding_changes_cannot_acknowledge_or_apply_an_old_command() {
  let (_directory, store) = fixture();
  store
    .submit_run_control_at(selection(ControlAction::Pause), AT + 1)
    .unwrap();
  store.poll_run_control_at(poll(None), AT + 2).unwrap();
  let mut other = poll(Some(result("applied")));
  let RunControlRequest::Poll { binding_id, .. } = &mut other;
  *binding_id = "binding-2".into();
  assert_eq!(
    store
      .poll_run_control_at(other.clone(), AT + 3)
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .poll_run_control_at(other.clone(), AT + ONLINE_MS + 3)
      .unwrap_err()
      .status,
    409
  );
  let RunControlRequest::Poll {
    result: acknowledgement,
    ..
  } = &mut other;
  *acknowledgement = None;
  store
    .poll_run_control_at(other, AT + ONLINE_MS + 3)
    .unwrap();
  assert_eq!(
    view(&store, AT + ONLINE_MS + 3)["request"]["status"],
    "failed"
  );
  assert_eq!(
    store
      .poll_run_control_at(poll(Some(result("applied"))), AT + ONLINE_MS + 4)
      .unwrap_err()
      .status,
    409
  );
}

#[test]
fn restart_requires_a_fresh_run_id_and_failed_results_are_bounded() {
  let (_directory, store) = fixture();
  store
    .submit_run_control_at(selection(ControlAction::Restart), AT + 1)
    .unwrap();
  store.poll_run_control_at(poll(None), AT + 2).unwrap();
  assert_eq!(
    store
      .poll_run_control_at(poll(Some(result("applied"))), AT + 3)
      .unwrap_err()
      .status,
    400
  );
  let mut applied = result("applied");
  applied.new_run_id = Some(scope().run_id);
  assert_eq!(
    store
      .poll_run_control_at(poll(Some(applied.clone())), AT + 3)
      .unwrap_err()
      .status,
    400
  );
  applied.new_run_id = Some("run-2".into());
  store
    .poll_run_control_at(poll(Some(applied)), AT + 3)
    .unwrap();
  assert_eq!(view(&store, AT + 3)["request"]["new_run_id"], "run-2");
  let mut failed = result("failed");
  failed.error = Some("x".repeat(4097));
  assert_eq!(validate_result(&failed).unwrap_err().status, 400);
}

#[test]
fn checkpoint_arguments_and_reason_are_bounded_without_allowing_path_traversal() {
  let (_directory, store) = fixture();
  for path in [
    "../other.pt",
    "/tmp/checkpoint.pt",
    "checkpoints/../other.pt",
    "a\\b.pt",
    "a\nb.pt",
    "unknown.pt",
  ] {
    let mut command = selection(ControlAction::Continue);
    command.checkpoint = Some(path.into());
    assert_eq!(
      store
        .submit_run_control_at(command, AT + 1)
        .unwrap_err()
        .status,
      400,
      "{path:?}"
    );
  }
  let mut command = selection(ControlAction::Restart);
  command.arguments = Some(vec!["x".repeat(8193)]);
  assert_eq!(
    store
      .submit_run_control_at(command, AT + 1)
      .unwrap_err()
      .status,
    400
  );
  let mut command = selection(ControlAction::Stop);
  command.reason = "x".repeat(1025);
  assert_eq!(
    store
      .submit_run_control_at(command, AT + 1)
      .unwrap_err()
      .status,
    400
  );
  let mut command = selection(ControlAction::Stop);
  command.grace_seconds = 0;
  assert_eq!(
    store
      .submit_run_control_at(command, AT + 1)
      .unwrap_err()
      .status,
    400
  );
}

#[test]
fn one_command_runs_at_a_time_and_duplicate_final_ack_does_not_block_the_next_command() {
  let (_directory, store) = fixture();
  store
    .submit_run_control_at(selection(ControlAction::Pause), AT + 1)
    .unwrap();
  let mut next = selection(ControlAction::Resume);
  next.request_id = "request-2".into();
  assert_eq!(
    store
      .submit_run_control_at(next.clone(), AT + 2)
      .unwrap_err()
      .status,
    409
  );
  store.poll_run_control_at(poll(None), AT + 3).unwrap();
  store
    .poll_run_control_at(poll(Some(result("applied"))), AT + 4)
    .unwrap();
  store.submit_run_control_at(next, AT + 5).unwrap();
  assert_eq!(
    store
      .poll_run_control_at(poll(Some(result("applied"))), AT + 6)
      .unwrap()
      .command
      .unwrap()
      .request_id,
    "request-2"
  );
}

#[test]
fn batch_polls_keep_healthy_runs_available_when_another_run_was_deleted() {
  let (_directory, store) = fixture();
  store.db().unwrap().execute("INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after,status) VALUES('project','worker','run-deleted',0,0,'deleted')", []).unwrap();
  let mut deleted = poll(None);
  let RunControlRequest::Poll {
    scope: deleted_scope,
    ..
  } = &mut deleted;
  deleted_scope.run_id = "run-deleted".into();
  let mut healthy = poll(None);
  let RunControlRequest::Poll { status, .. } = &mut healthy;
  *status = "waiting".into();
  let Response::RunControlBatch { responses } = store
    .execute(Request::RunControlBatch {
      requests: vec![deleted, healthy],
    })
    .unwrap()
  else {
    panic!("expected batch responses")
  };
  assert_eq!(responses.len(), 2);
  assert!(responses[0].response.is_none());
  assert_eq!(
    responses[0].error.as_deref(),
    Some("run control poll unavailable")
  );
  assert!(responses[1].response.is_some());
  assert!(responses[1].error.is_none());
  assert_eq!(store.run_control(&scope()).unwrap()["status"], "waiting");
}

#[test]
fn batch_count_and_wire_size_are_bounded_before_any_child_mutates() {
  let (_directory, store) = fixture();
  for count in [0, 65] {
    assert_eq!(
      store
        .execute(Request::RunControlBatch {
          requests: vec![poll(None); count]
        })
        .unwrap_err()
        .status,
      400
    );
  }
  let mut oversized = poll(None);
  let RunControlRequest::Poll { command, .. } = &mut oversized;
  *command = vec!["x".repeat(8192); 6];
  assert_eq!(
    store
      .execute(Request::RunControlBatch {
        requests: vec![oversized; 64]
      })
      .unwrap_err()
      .status,
    413
  );
  assert_eq!(view(&store, AT)["status"], "running");
}

#[test]
fn full_commands_accept_the_maximum_argument_vector_plus_its_executable() {
  let arguments = vec!["".into(); 256];
  validate_argv(&arguments, true).unwrap();
  let mut full = vec!["x".repeat(8192)];
  full.extend(arguments);
  validate_argv(&full, false).unwrap();
  full.push("".into());
  assert_eq!(validate_argv(&full, false).unwrap_err().status, 400);
  let full = vec!["x".repeat(8192); 5];
  validate_argv(&full, false).unwrap();
  assert_eq!(validate_argv(&full, true).unwrap_err().status, 400);
}

#[test]
fn failed_restart_preserves_its_fresh_successor_identity_for_recovery() {
  let (_directory, store) = fixture();
  store
    .submit_run_control_at(selection(ControlAction::Restart), AT + 1)
    .unwrap();
  store.poll_run_control_at(poll(None), AT + 2).unwrap();
  let mut failed = result("failed");
  failed.error = Some("successor launch outcome is uncertain".into());
  failed.new_run_id = Some("run-successor".into());
  store
    .poll_run_control_at(poll(Some(failed)), AT + 3)
    .unwrap();
  assert_eq!(view(&store, AT + 3)["request"]["status"], "failed");
  assert_eq!(
    view(&store, AT + 3)["request"]["new_run_id"],
    "run-successor"
  );
}

#[test]
fn large_command_replies_are_bounded_and_executing_acknowledgements_make_room_for_deferred_commands()
 {
  let (_directory, store) = fixture();
  let at = now_ms();
  let mut polls = Vec::new();
  for index in 0..64 {
    let mut item = poll(None);
    let run_id = format!("run-batch-{index}");
    let RunControlRequest::Poll { scope, .. } = &mut item;
    scope.run_id = run_id.clone();
    store.poll_run_control_at(item.clone(), at).unwrap();
    let mut command = selection(ControlAction::Restart);
    command.run_id = run_id;
    command.request_id = format!("request-batch-{index}");
    command.arguments = Some(vec!["x".repeat(8192); 4]);
    store.submit_run_control_at(command, at).unwrap();
    polls.push(item);
  }
  let response = store
    .execute(Request::RunControlBatch {
      requests: polls.clone(),
    })
    .unwrap();
  assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_REQUEST);
  let Response::RunControlBatch { responses } = response else {
    panic!("expected batch response")
  };
  let mut deferred = 0;
  for (request, response) in polls.iter_mut().zip(responses) {
    let RunControlRequest::Poll { result, .. } = request;
    if let Some(command) = response.response.unwrap().command {
      *result = Some(ControlResult {
        request_id: command.request_id,
        status: "executing".into(),
        error: None,
        new_run_id: None,
      });
    } else {
      deferred += 1;
    }
  }
  assert!(deferred > 0 && deferred < 64);
  let response = store
    .execute(Request::RunControlBatch {
      requests: polls.clone(),
    })
    .unwrap();
  assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_REQUEST);
  let Response::RunControlBatch { responses } = response else {
    panic!("expected batch response")
  };
  let delivered = responses
    .into_iter()
    .filter(|item| item.response.as_ref().unwrap().command.is_some())
    .count();
  assert!(delivered > 0);
  // A service restart without an in-flight acknowledgement still recovers the
  // executing command through the local durable execution journal.
  let RunControlRequest::Poll { result, .. } = &mut polls[0];
  *result = None;
  assert!(
    store
      .poll_run_control(polls.remove(0))
      .unwrap()
      .command
      .is_some()
  );
}
