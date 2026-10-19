use super::*;

#[test]
fn queue_ledger_rejects_worker_advertised_direct_controls_even_for_released_allocations() {
  let (_directory, store) = fixture();
  {
    let db = store.db().unwrap();
    db.execute("INSERT INTO queue_jobs(job_id,submission_id,project_id,spec,record,status,source_key,attempt_id) VALUES('queued-job','queued-submission','project','{}','{}','completed','source','queued-attempt')", []).unwrap();
    // The authoritative worker column wins over a corrupt worker_id in the
    // stored report. A completed and released allocation still belongs to the scheduler.
    db.execute("INSERT INTO queue_attempts(attempt_id,job_id,worker_id,session_id,claim_id,record,released) VALUES('queued-attempt','queued-job','worker','session','claim',?1,1)", [json!({"worker_id":"corrupt-other-worker","run_id":"run-1"}).to_string()]).unwrap();
  }
  assert_eq!(
    store
      .poll_run_control_at(poll(None), AT + 1)
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .submit_run_control_at(selection(ControlAction::Restart), AT + 1)
      .unwrap_err()
      .status,
    409
  );
  assert!(view(&store, AT + 1)["request"].is_null());
  let unavailable = store.run_control(&scope()).unwrap();
  assert_eq!(unavailable["supported"], false);
  assert_eq!(unavailable["capabilities"], json!([]));
  let mut healthy = poll(None);
  let RunControlRequest::Poll { scope, .. } = &mut healthy;
  scope.run_id = "run-direct".into();
  let Response::RunControlBatch { responses } = store
    .execute(Request::RunControlBatch {
      requests: vec![poll(None), healthy],
    })
    .unwrap()
  else {
    panic!("expected batch response")
  };
  assert!(responses[0].response.is_none());
  assert!(responses[1].response.is_some());
}

fn change_revisions(store: &Store<MockStorage>) -> (i64, i64, i64) {
  let db = store.db().unwrap();
  (
    db.query_row(
      "SELECT revision FROM run_control_bindings WHERE scope=?1",
      [encode(&scope()).unwrap()],
      |row| row.get(0),
    )
    .unwrap(),
    db.query_row(
      "SELECT revision FROM project_change_revisions WHERE project_id='project'",
      [],
      |row| row.get(0),
    )
    .unwrap(),
    db.query_row(
      "SELECT MAX(sequence) FROM dashboard_run_activity WHERE project_id='project'",
      [],
      |row| row.get(0),
    )
    .unwrap(),
  )
}

#[test]
fn semantic_observations_and_command_transitions_advance_revisions_but_heartbeats_do_not() {
  let (_directory, store) = fixture();
  let initial = change_revisions(&store);
  store.poll_run_control_at(poll(None), AT + 1).unwrap();
  assert_eq!(change_revisions(&store), initial);
  store
    .submit_run_control_at(selection(ControlAction::Pause), AT + 2)
    .unwrap();
  let requested = change_revisions(&store);
  assert!(requested.0 > initial.0 && requested.1 > initial.1 && requested.2 > initial.2);
  store.poll_run_control_at(poll(None), AT + 3).unwrap();
  let delivered = change_revisions(&store);
  assert!(delivered.0 > requested.0);
  store.poll_run_control_at(poll(None), AT + 4).unwrap();
  assert_eq!(change_revisions(&store), delivered);
  store
    .poll_run_control_at(poll(Some(result("executing"))), AT + 5)
    .unwrap();
  let executing = change_revisions(&store);
  assert!(executing.0 > delivered.0);
  store
    .poll_run_control_at(poll(Some(result("executing"))), AT + 6)
    .unwrap();
  assert_eq!(change_revisions(&store), executing);
  let mut applied = poll(Some(result("applied")));
  let RunControlRequest::Poll { status, .. } = &mut applied;
  *status = "paused".into();
  store.poll_run_control_at(applied.clone(), AT + 7).unwrap();
  let final_revision = change_revisions(&store);
  assert!(final_revision.0 > executing.0);
  store.poll_run_control_at(applied, AT + 8).unwrap();
  assert_eq!(change_revisions(&store), final_revision);
}

#[test]
fn lifecycle_timestamps_are_validated_before_storing_an_observation() {
  let (_directory, store) = fixture();
  for (start, finish) in [
    ("invalid", None),
    ("2026-10-19T00:00:00Z", Some("invalid")),
    ("2026-10-19T00:00:00Z", Some("2026-10-18T00:00:00Z")),
  ] {
    let mut request = poll(None);
    let RunControlRequest::Poll { lifecycle, .. } = &mut request;
    *lifecycle = Some(ControlLifecycle {
      started_at: start.into(),
      finished_at: finish.map(str::to_owned),
      exit_code: None,
    });
    assert_eq!(
      store
        .poll_run_control_at(request, AT + 1)
        .unwrap_err()
        .status,
      400
    );
  }
}

#[test]
fn offline_observations_remain_last_known_state_but_other_run_lifetimes_and_newer_attempts_are_not_overlaid()
 {
  let (_directory, store) = fixture();
  let mut request = poll(None);
  let RunControlRequest::Poll {
    lifecycle, status, ..
  } = &mut request;
  *status = "paused".into();
  *lifecycle = Some(ControlLifecycle {
    started_at: "2026-10-19T00:00:00Z".into(),
    finished_at: None,
    exit_code: None,
  });
  store.poll_run_control_at(request, AT + 1).unwrap();
  let original =
    json!({"status":"running","started_at":"2026-10-19T00:00:00Z","current_attempt":1});
  let mut offline = original.clone();
  store
    .decorate_run_control(&mut offline, &scope(), 1)
    .unwrap();
  assert_eq!(offline["status"], "paused");
  assert_eq!(offline["control"]["online"], false);
  let mut other_lifetime = original.clone();
  other_lifetime["started_at"] = json!("2026-10-18T00:00:00Z");
  store
    .decorate_run_control(&mut other_lifetime, &scope(), 1)
    .unwrap();
  assert_eq!(other_lifetime["status"], "running");
  let mut newer_attempt = original;
  newer_attempt["current_attempt"] = json!(2);
  store
    .decorate_run_control(&mut newer_attempt, &scope(), 1)
    .unwrap();
  assert_eq!(newer_attempt["status"], "running");
}
