use super::*;

fn interrupted(stage: &str) -> (tempfile::TempDir, PathBuf, ContinueRunRequest, String) {
  let root = tempfile::tempdir().unwrap();
  let run = root
    .path()
    .canonicalize()
    .unwrap()
    .join(".expri/runs/recover-test");
  for path in ["code", "outputs", "logs"] {
    fs::create_dir_all(run.join(path)).unwrap();
  }
  let request: ContinueRunRequest = serde_json::from_value(json!({
    "request_id":"interrupted", "reason":"extend", "expected_attempt":1,
    "checkpoint":"outputs/checkpoint.bin", "run":{"name":"train", "command":["python3", "train.py"], "environment":{"base_python":null}, "detach":true}
  })).unwrap();
  let fingerprint = hex_digest(&serde_json::to_vec(&request).unwrap());
  let mut state = json!({"schema_version":1,"run_id":"recover-test","status":"running","task":"train","command":["python3","train.py"],"detached":true,"started_at":"2026-10-15T00:00:00Z","finished_at":null,"code_dir":run.join("code"),"output_dir":run.join("outputs"),"logs":{"stdout":"logs/stdout.log","stderr":"logs/stderr.log"}});
  task_attempts::ensure_initial(&mut state).unwrap();
  task_attempts::finish_attempt(
    &mut state,
    1,
    "continued",
    "2026-10-15T00:01:00Z",
    Some(0),
    None,
  )
  .unwrap();
  task_attempts::begin_attempt(&mut state, json!({"attempt":2,"request_id":request.request_id,"reason":request.reason,"requested_at":"2026-10-15T00:01:01Z","request_fingerprint":fingerprint,"metric_step_offset":"101"})).unwrap();
  state["status"] = json!("preparing");
  fs::write(
    run.join("run-state.json"),
    serde_json::to_vec(&state).unwrap(),
  )
  .unwrap();
  let prepared = PreparedContinuation {
    request: request.clone(),
    attempt: 2,
    previous_attempt: 1,
    fingerprint: fingerprint.clone(),
    binary: "/bin/true".into(),
    code_dir: run.join("attempts/2/code"),
    checkpoint_path: run.join("attempts/2/checkpoint/checkpoint.bin"),
    checkpoint_digest: "digest".into(),
    checkpoint_size: 1,
    prepared: serde_json::from_value(
      json!({"environment_path":"unused","python":"unused","manifest_path":"unused"}),
    )
    .unwrap(),
    stage: stage.into(),
    capture_id: None,
    capture_started_at: None,
    grace_seconds: 2,
  };
  write_prepared(&run, &prepared).unwrap();
  (root, run, request, fingerprint)
}

#[test]
fn interrupted_stopped_stage_fails_durably_without_relaunch_and_replays_the_result() {
  let (_root, run, request, fingerprint) = interrupted("stopped");
  let result = wait_for_result(&run, &request, &fingerprint).unwrap();
  assert_eq!(result["continuation"]["status"], "failed");
  assert_eq!(result["continuation"]["worker_restarted"], false);
  let state = read_json(&run.join("run-state.json"), MAX_STATE_BYTES).unwrap();
  assert_eq!(state["status"], "failed");
  assert_eq!(state["task_attempts"]["current_attempt"], 1);
  assert_eq!(state["task_attempts"]["attempts"][0]["status"], "continued");
  assert_eq!(
    state["task_attempts"]["attempts"][0]["finished_at"],
    "2026-10-15T00:01:00Z"
  );
  let replay = request_continue(&run, &request).unwrap();
  assert_eq!(replay["continuation"], result["continuation"]);
}

#[test]
fn interrupted_launch_intent_never_relaunches_an_uncertain_task() {
  let (_root, run, request, fingerprint) = interrupted("launch_intent");
  let before = fs::read(run.join("run-state.json")).unwrap();
  let error = wait_for_result(&run, &request, &fingerprint).unwrap_err();
  assert!(error.to_string().contains("launch outcome is uncertain"));
  assert_eq!(fs::read(run.join("run-state.json")).unwrap(), before);
}

#[test]
fn malformed_original_metadata_is_refused_without_allocating_or_rewriting_attempts() {
  for field in [
    "started_at",
    "schema_version",
    "run_id",
    "code_dir",
    "output_dir",
  ] {
    let (_root, run, request, _) = interrupted("stopped");
    let mut state = read_json(&run.join("run-state.json"), MAX_STATE_BYTES).unwrap();
    state[field] = json!("invalid");
    fs::write(
      run.join("run-state.json"),
      serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let before = fs::read(run.join("run-state.json")).unwrap();
    assert!(
      request_continue(&run, &request).is_err(),
      "accepted {field}"
    );
    assert_eq!(fs::read(run.join("run-state.json")).unwrap(), before);
  }
}

#[test]
fn finalized_replacement_cancellation_is_not_relabelled_as_a_launch_failure() {
  let (_root, run, _, _) = interrupted("running");
  update_state(&run.join("run-state.json"), |state| {
    task_attempts::activate_attempt(state, 2, "2026-10-15T00:02:00Z", 123)?;
    task_attempts::finish_attempt(
      state,
      2,
      "cancelled",
      "2026-10-15T00:03:00Z",
      Some(130),
      Some("run cancellation requested"),
    )?;
    state["status"] = json!("cancelled");
    state["finished_at"] = json!("2026-10-15T00:03:00Z");
    state["system_capture_finished_at"] = state["finished_at"].clone();
    Ok(())
  })
  .unwrap();
  let before = fs::read(run.join("run-state.json")).unwrap();
  let prepared = read_prepared(&run).unwrap().unwrap();
  fail_after_stop(&run, &prepared, &message("cancelled run exited 130")).unwrap();
  assert_eq!(fs::read(run.join("run-state.json")).unwrap(), before);
}
