use super::*;
use expri_core::resource_requirements::DiskRequirement;

fn policy() -> ResourceRequirements {
  ResourceRequirements {
    disk: vec![DiskRequirement {
      path: "outputs".into(),
      min_free_bytes: 100,
      resume_free_bytes: 120,
    }],
    recovery_seconds: 30,
  }
}

fn evaluate(
  guard: &mut Guard,
  policy: &ResourceRequirements,
  start: Instant,
  seconds: u64,
  bytes: io::Result<u64>,
  mount: u64,
) -> ResourceStatus {
  let timestamp = chrono::DateTime::parse_from_rfc3339("2026-10-20T00:00:00Z").unwrap()
    + chrono::Duration::seconds(seconds as i64);
  let mut bytes = Some(bytes);
  let status = guard.evaluate(
    policy,
    Path::new("/run/outputs"),
    start + Duration::from_secs(seconds),
    timestamp.to_rfc3339(),
    |_, _| {
      bytes.take().unwrap().map(|available| Reading {
        available,
        filesystem: Filesystem { device: 1, mount },
      })
    },
  );
  status.validate(policy).unwrap();
  status
}

#[test]
fn disk_guard_uses_hysteresis_and_continuous_recovery_time() {
  let start = Instant::now();
  let mut guard = Guard::default();
  let policy = policy();
  assert!(!evaluate(&mut guard, &policy, start, 0, Ok(110), 1).blocked);
  assert_eq!(
    evaluate(&mut guard, &policy, start, 5, Ok(99), 1).phase,
    ResourcePhase::Blocked
  );
  assert_eq!(
    evaluate(&mut guard, &policy, start, 10, Ok(110), 1).phase,
    ResourcePhase::Blocked
  );
  assert_eq!(
    evaluate(&mut guard, &policy, start, 15, Ok(120), 1).phase,
    ResourcePhase::Recovering
  );
  assert_eq!(
    evaluate(&mut guard, &policy, start, 44, Ok(119), 1).phase,
    ResourcePhase::Blocked
  );
  assert_eq!(
    evaluate(&mut guard, &policy, start, 45, Ok(130), 1).phase,
    ResourcePhase::Recovering
  );
  assert!(evaluate(&mut guard, &policy, start, 74, Ok(130), 1).blocked);
  assert!(!evaluate(&mut guard, &policy, start, 75, Ok(130), 1).blocked);
}

#[test]
fn missing_or_changed_mount_blocks_and_restarts_recovery() {
  let start = Instant::now();
  let mut guard = Guard::default();
  let policy = policy();
  evaluate(&mut guard, &policy, start, 0, Ok(99), 1);
  evaluate(&mut guard, &policy, start, 5, Ok(130), 1);
  let missing = evaluate(
    &mut guard,
    &policy,
    start,
    30,
    Err(io::Error::from(io::ErrorKind::NotFound)),
    1,
  );
  assert_eq!(missing.phase, ResourcePhase::Unavailable);
  assert_eq!(missing.disk[0].available_bytes, None);
  assert_eq!(missing.recovery_started_at, None);
  evaluate(&mut guard, &policy, start, 35, Ok(130), 1);
  let changed = evaluate(&mut guard, &policy, start, 60, Ok(130), 2);
  assert_eq!(changed.phase, ResourcePhase::Unavailable);
  assert!(
    changed.disk[0]
      .error
      .as_ref()
      .unwrap()
      .contains("mount changed")
  );
  assert_eq!(
    evaluate(&mut guard, &policy, start, 65, Ok(130), 2).phase,
    ResourcePhase::Recovering
  );
  assert!(evaluate(&mut guard, &policy, start, 90, Ok(130), 2).blocked);
  assert!(!evaluate(&mut guard, &policy, start, 95, Ok(130), 2).blocked);
}

#[test]
fn policy_changes_and_supervisor_restarts_reset_recovery_dwell() {
  let start = Instant::now();
  let mut guard = Guard::default();
  let mut policy = policy();
  evaluate(&mut guard, &policy, start, 0, Ok(90), 1);
  evaluate(&mut guard, &policy, start, 5, Ok(130), 1);
  policy.disk[0].resume_free_bytes = 125;
  assert_eq!(
    evaluate(&mut guard, &policy, start, 30, Ok(130), 1).phase,
    ResourcePhase::Recovering
  );
  assert!(evaluate(&mut guard, &policy, start, 59, Ok(130), 1).blocked);
  let mut restarted = Guard {
    held: true,
    ..Default::default()
  };
  assert_eq!(
    evaluate(&mut restarted, &policy, start, 60, Ok(130), 1).phase,
    ResourcePhase::Recovering
  );
  assert!(evaluate(&mut restarted, &policy, start, 89, Ok(130), 1).blocked);
  assert!(!evaluate(&mut restarted, &policy, start, 90, Ok(130), 1).blocked);
}

#[test]
fn zero_dwell_recovers_immediately_and_empty_policy_releases_only_resource_hold() {
  let start = Instant::now();
  let mut guard = Guard::default();
  let mut policy = policy();
  policy.recovery_seconds = 0;
  evaluate(&mut guard, &policy, start, 0, Ok(90), 1);
  assert!(!evaluate(&mut guard, &policy, start, 1, Ok(130), 1).blocked);
  evaluate(&mut guard, &policy, start, 2, Ok(90), 1);
  policy.disk.clear();
  let status = guard.evaluate(
    &policy,
    Path::new("/unused"),
    start,
    "2026-10-20T00:00:00Z".into(),
    |_, _| panic!("empty requirements must not sample"),
  );
  assert!(!status.blocked);
}

#[test]
fn measurements_require_an_existing_readable_directory_and_bound_errors() {
  let temporary = tempfile::tempdir().unwrap();
  let reading = disk_reading(temporary.path()).unwrap();
  assert!(reading.available > 0);
  assert!(disk_reading(&temporary.path().join("missing")).is_err());
  fs::write(temporary.path().join("file"), []).unwrap();
  assert!(disk_reading(&temporary.path().join("file")).is_err());
  let mut guard = Guard::default();
  let status = evaluate(
    &mut guard,
    &policy(),
    Instant::now(),
    0,
    Err(io::Error::other("é\n".repeat(600))),
    1,
  );
  let error = status.disk[0].error.as_ref().unwrap();
  assert!(error.len() <= 512);
  assert!(!error.chars().any(char::is_control));
}

#[test]
fn outputs_alias_and_explicit_same_directory_are_independently_observed() {
  let directory = tempfile::tempdir().unwrap();
  let run = directory.path();
  fs::create_dir(run.join("outputs")).unwrap();
  let policy = ResourceRequirements {
    disk: vec![
      DiskRequirement {
        path: "outputs".into(),
        min_free_bytes: 1,
        resume_free_bytes: 2,
      },
      DiskRequirement {
        path: run.join("outputs").to_str().unwrap().into(),
        min_free_bytes: 2,
        resume_free_bytes: 3,
      },
    ],
    recovery_seconds: 0,
  };
  expri_core::run_state::write_state(
    &run.join("run-state.json"),
    &json!({
      "schema_version":1, "run_id":"test", "status":"preparing", "requirements":policy,
    }),
  )
  .unwrap();
  assert!(!Guard::default().tick(run, None).unwrap());
  let state = read_json(&run.join("run-state.json"), MAX_STATE_BYTES).unwrap();
  let status: ResourceStatus = serde_json::from_value(state["resource_status"].clone()).unwrap();
  status.validate(&policy).unwrap();
  assert!(
    status
      .disk
      .iter()
      .all(|disk| disk.available_bytes.is_some())
  );
}

#[test]
fn persisted_mount_identity_survives_supervisor_replacement() {
  let directory = tempfile::tempdir().unwrap();
  let run = directory.path();
  fs::create_dir(run.join("outputs")).unwrap();
  let policy = ResourceRequirements {
    recovery_seconds: 0,
    ..policy()
  };
  let mount_key = hex_digest(b"outputs");
  expri_core::run_state::write_state(
    &run.join("run-state.json"),
    &json!({
      "schema_version":1, "run_id":"test", "status":"preparing", "requirements":policy,
      "resource_mounts":{mount_key:{"device":0,"mount":0}},
    }),
  )
  .unwrap();
  let mut guard = Guard::default();
  assert!(
    guard.tick(run, None).unwrap(),
    "a changed mount must block the replacement supervisor"
  );
  let state = read_json(&run.join("run-state.json"), MAX_STATE_BYTES).unwrap();
  assert_eq!(state["resource_status"]["phase"], "unavailable");
  guard.force_check();
  assert!(!guard.tick(run, None).unwrap());
}

#[test]
fn backward_wall_clock_corrections_restart_the_monotonic_dwell() {
  let start = Instant::now();
  let mut guard = Guard::default();
  let policy = policy();
  evaluate(&mut guard, &policy, start, 0, Ok(90), 1);
  evaluate(&mut guard, &policy, start, 60, Ok(130), 1);
  let status = guard.evaluate(
    &policy,
    Path::new("/outputs"),
    start + Duration::from_secs(65),
    "2026-10-20T00:00:30Z".into(),
    |_, _| {
      Ok(Reading {
        available: 130,
        filesystem: Filesystem {
          device: 1,
          mount: 1,
        },
      })
    },
  );
  status.validate(&policy).unwrap();
  assert_eq!(
    status.recovery_started_at.as_deref(),
    Some("2026-10-20T00:00:30Z")
  );
  assert!(status.blocked);
}

#[test]
fn empty_requirements_do_not_rewrite_or_poll_run_state() {
  let directory = tempfile::tempdir().unwrap();
  let run = directory.path();
  let path = run.join("run-state.json");
  expri_core::run_state::write_state(
    &path,
    &json!({"schema_version":1,"run_id":"test","status":"running"}),
  )
  .unwrap();
  let before = fs::read(&path).unwrap();
  let identity = fs::metadata(&path).unwrap().ino();
  let mut guard = Guard::default();
  assert!(!guard.tick(run, None).unwrap());
  for _ in 0..3 {
    guard.next_check = Some(Instant::now() - CHECK_INTERVAL);
    assert!(!guard.tick(run, None).unwrap());
    assert_eq!(fs::metadata(&path).unwrap().ino(), identity);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn removing_requirements_clears_only_the_resource_hold_once_and_keeps_manual_pause() {
  for manual in [false, true] {
    let directory = tempfile::tempdir().unwrap();
    let run = directory.path();
    let path = run.join("run-state.json");
    expri_core::run_state::write_state(&path, &json!({
      "schema_version":1,"run_id":"test","status":"paused","paused_previous_status":"waiting",
      "manual_paused":manual,"resource_paused":true,"resource_status":{"phase":"blocked"},"resource_mounts":{},
    })).unwrap();
    let mut guard = Guard::default();
    assert_eq!(guard.tick(run, None).unwrap(), manual);
    let state = read_json(&path, MAX_STATE_BYTES).unwrap();
    assert_eq!(state["status"], if manual { "paused" } else { "waiting" });
    assert_eq!(state["manual_paused"], manual);
    assert!(state.get("resource_status").is_none());
    assert!(state.get("resource_paused").is_none());
    assert!(state.get("resource_mounts").is_none());
    let identity = fs::metadata(&path).unwrap().ino();
    guard.next_check = Some(Instant::now() - CHECK_INTERVAL);
    assert_eq!(guard.tick(run, None).unwrap(), manual);
    assert_eq!(fs::metadata(&path).unwrap().ino(), identity);
  }
}
