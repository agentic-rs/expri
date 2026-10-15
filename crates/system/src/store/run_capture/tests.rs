use super::*;

fn fixture() -> (tempfile::TempDir, LocalStore, PathBuf) {
  let directory = tempfile::tempdir().unwrap();
  let store = LocalStore::open(&directory.path().join("global/profiles.sqlite3")).unwrap();
  let run = fs::canonicalize(directory.path())
    .unwrap()
    .join("run-fixture");
  fs::create_dir_all(run.join("outputs")).unwrap();
  (directory, store, run)
}

fn observation(time: i64) -> (SystemInventory, SystemSample) {
  let inventory = SystemInventory {
    observed_at_ms: time,
    hostname: "host".into(),
    os_name: None,
    os_version: None,
    kernel_version: None,
    architecture: "test".into(),
    cpu: CpuInventory {
      brand: "cpu".into(),
      vendor: "test".into(),
      logical_cpu_count: 2,
      physical_core_count: Some(1),
    },
    memory_total_bytes: 1024,
    measurement_scope: "whole machine".into(),
    devices: Vec::new(),
    warnings: Vec::new(),
  };
  let sample = SystemSample {
    sequence: 0,
    session_id: "session-test".into(),
    observed_at_ms: time,
    interval_ms: 10,
    collection_duration_ms: 1,
    inventory_revision: 0,
    metrics: BTreeMap::from([(
      "cpu/usage_percent".into(),
      MetricReading {
        kind: MetricKind::Gauge,
        value: 30.0,
      },
    )]),
    counters: BTreeMap::from([("network/test/received_bytes".into(), u64::MAX)]),
    warnings: Vec::new(),
  };
  (inventory, sample)
}

fn record(store: &mut LocalStore, time: i64) {
  let (inventory, sample) = observation(time);
  store.record(inventory, sample).unwrap();
}

fn records(run: &Path) -> Vec<RunSystemRecord> {
  fs::read_to_string(run.join(RUN_SYSTEM_PATH))
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect()
}

fn state(store: &LocalStore, id: &str) -> Value {
  serde_json::json!({ "schema_version":1,"run_id":"run-fixture","machine_id":store.machine_id(),"system_capture_id":id,"system_capture_status":"capturing","status":"completed","exit_code":0,"started_at":"1970-01-01T00:00:01Z","finished_at":"1970-01-01T00:00:02Z" })
}

#[test]
fn continuation_segments_preserve_committed_prefix_and_historical_exporters_cannot_truncate() {
  let (_directory, mut store, run) = fixture();
  let first = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 1200);
  store.close_run_capture(&first, 2000).unwrap();
  store.finish_capture(&first, true, Vec::new()).unwrap();
  let prefix = fs::read(run.join(RUN_SYSTEM_PATH)).unwrap();
  let second = store
    .begin_run_capture_segment(&run, "run-fixture", 3000)
    .unwrap();
  assert_ne!(first, second);
  assert!(
    fs::read(run.join(RUN_SYSTEM_PATH))
      .unwrap()
      .starts_with(&prefix)
  );
  let row = capture(&store.connection, &second).unwrap();
  assert_eq!(row.segment_offset, prefix.len() as u64);
  record(&mut store, 3500);
  store.export_run_capture(&second).unwrap();
  let before_old_export = fs::read(run.join(RUN_SYSTEM_PATH)).unwrap();
  assert!(!store.export_run_capture(&first).unwrap());
  store.finish_capture(&first, true, Vec::new()).unwrap();
  assert_eq!(
    fs::read(run.join(RUN_SYSTEM_PATH)).unwrap(),
    before_old_export
  );
  store.close_run_capture(&second, 4000).unwrap();
  store.finish_capture(&second, true, Vec::new()).unwrap();
  let final_bytes = fs::read(run.join(RUN_SYSTEM_PATH)).unwrap();
  assert!(final_bytes.starts_with(&prefix));
  let mut current = state(&store, &second);
  current["system_capture_status"] = serde_json::json!("complete");
  current["system_capture_started_at"] = serde_json::json!("1970-01-01T00:00:03Z");
  current["system_capture_start_offset"] = serde_json::json!(prefix.len());
  current["finished_at"] = serde_json::json!("1970-01-01T00:00:04Z");
  assert_eq!(
    committed_prefix(&run, &current).unwrap(),
    Some(final_bytes.len() as u64)
  );
  assert_eq!(
    records(&run)
      .iter()
      .filter(|record| matches!(record, RunSystemRecord::Capture { .. }))
      .count(),
    2
  );
  assert_eq!(
    records(&run)
      .iter()
      .filter(|record| matches!(record, RunSystemRecord::End { .. }))
      .count(),
    2
  );
}

#[test]
fn exporter_rechecks_a_cached_open_capture_after_its_successor_was_registered() {
  let (_directory, mut store, run) = fixture();
  let first = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  let cached = capture(&store.connection, &first).unwrap();
  assert!(!cached.closed);
  store.close_run_capture(&first, 2000).unwrap();
  store.finish_capture(&first, true, Vec::new()).unwrap();
  let second = store
    .begin_run_capture_segment(&run, "run-fixture", 3000)
    .unwrap();
  record(&mut store, 3500);
  store.export_run_capture(&second).unwrap();
  let before = fs::read(run.join(RUN_SYSTEM_PATH)).unwrap();
  assert!(!store.export_capture_from(cached).unwrap());
  assert_eq!(fs::read(run.join(RUN_SYSTEM_PATH)).unwrap(), before);
}

#[test]
fn rejected_capture_registration_preserves_the_previous_segment_and_checkpoint() {
  let (_directory, mut store, run) = fixture();
  let first = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  store.close_run_capture(&first, 2000).unwrap();
  store.finish_capture(&first, true, Vec::new()).unwrap();
  let before = fs::read(run.join(RUN_SYSTEM_PATH)).unwrap();
  let checkpoint = fs::read(run.join(CHECKPOINT)).unwrap();
  store.connection.execute_batch("CREATE TRIGGER reject_next_capture BEFORE INSERT ON system_run_captures BEGIN SELECT RAISE(ABORT, 'test registration failure'); END;").unwrap();
  assert!(
    store
      .begin_run_capture_segment(&run, "run-fixture", 3000)
      .is_err()
  );
  assert_eq!(fs::read(run.join(RUN_SYSTEM_PATH)).unwrap(), before);
  assert_eq!(fs::read(run.join(CHECKPOINT)).unwrap(), checkpoint);
  store
    .connection
    .execute_batch("DROP TRIGGER reject_next_capture")
    .unwrap();
  assert!(
    store
      .begin_run_capture_segment(&run, "run-fixture", 3000)
      .is_ok()
  );
}

#[test]
fn registered_pending_capture_repairs_its_header_without_changing_the_old_prefix() {
  let (_directory, mut store, run) = fixture();
  let first = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  store.close_run_capture(&first, 2000).unwrap();
  store.finish_capture(&first, true, Vec::new()).unwrap();
  let before = fs::read(run.join(RUN_SYSTEM_PATH)).unwrap();
  let old_checkpoint = fs::read(run.join(CHECKPOINT)).unwrap();
  let second = store
    .begin_run_capture_segment(&run, "run-fixture", 3000)
    .unwrap();
  // Reproduce a crash after the journal commit, including a partially written
  // header. The registered identity is sufficient for an exporter to repair it.
  let file = OpenOptions::new()
    .write(true)
    .open(run.join(RUN_SYSTEM_PATH))
    .unwrap();
  file.set_len(before.len() as u64 + 7).unwrap();
  fs::write(run.join(CHECKPOINT), old_checkpoint).unwrap();
  store
    .connection
    .execute(
      "UPDATE system_run_captures SET header_pending=1 WHERE capture_id=?1",
      [&second],
    )
    .unwrap();
  store.export_run_capture(&second).unwrap();
  assert!(
    fs::read(run.join(RUN_SYSTEM_PATH))
      .unwrap()
      .starts_with(&before)
  );
  assert!(!capture(&store.connection, &second).unwrap().header_pending);
  let saved: Checkpoint = serde_json::from_slice(&fs::read(run.join(CHECKPOINT)).unwrap()).unwrap();
  assert_eq!(saved.capture_id, second);
  assert_eq!(
    records(&run)
      .iter()
      .filter(|record| matches!(record, RunSystemRecord::Capture { .. }))
      .count(),
    2
  );
}

#[test]
#[cfg(unix)]
fn reserved_capture_child_process_registers_without_publishing_metadata() {
  let Some(database) = std::env::var_os("EXPRI_TEST_RESERVED_CAPTURE_DATABASE") else {
    return;
  };
  let run = PathBuf::from(std::env::var_os("EXPRI_TEST_RESERVED_CAPTURE_RUN").unwrap());
  let mut store = LocalStore::open_existing(Path::new(&database))
    .unwrap()
    .unwrap();
  store
    .begin_run_capture_segment_reserved(&run, "run-fixture", 3000, "capture-hard-death")
    .unwrap();
  // Kill the process after durable registration/header writes and before any
  // caller could publish its returned identity into run-state.json.
  unsafe {
    libc::kill(std::process::id() as i32, libc::SIGKILL);
  }
  unreachable!("SIGKILL terminated the owned test subprocess");
}

#[test]
#[cfg(unix)]
fn reserved_capture_journal_recovers_after_process_death_before_state_publication() {
  let (directory, mut store, run) = fixture();
  let first = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  store.close_run_capture(&first, 2000).unwrap();
  store.finish_capture(&first, true, Vec::new()).unwrap();
  let prefix = fs::read(run.join(RUN_SYSTEM_PATH)).unwrap();
  let mut previous = state(&store, &first);
  previous["system_capture_status"] = serde_json::json!("complete");
  previous["system_capture_finished_at"] = serde_json::json!("1970-01-01T00:00:02Z");
  previous["task_attempts"] = serde_json::json!({"attempts":[{"attempt":1,"system_capture_id":first,"system_capture_finished_at":"1970-01-01T00:00:02Z"}]});
  fs::write(
    run.join("run-state.json"),
    serde_json::to_vec(&previous).unwrap(),
  )
  .unwrap();
  let database = directory.path().join("global/profiles.sqlite3");
  drop(store);
  let status = std::process::Command::new(std::env::current_exe().unwrap())
    .args(["--exact", "store::run_capture::tests::reserved_capture_child_process_registers_without_publishing_metadata", "--nocapture"])
    .env("EXPRI_TEST_RESERVED_CAPTURE_DATABASE", &database)
    .env("EXPRI_TEST_RESERVED_CAPTURE_RUN", &run)
    .current_dir(directory.path())
    .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
  use std::os::unix::process::ExitStatusExt;
  assert_eq!(status.signal(), Some(libc::SIGKILL));
  let mut store = LocalStore::open_existing(&database).unwrap().unwrap();
  let mut recovered = read_state(&run.join("run-state.json")).unwrap();
  assert_eq!(recovered["system_capture_id"], first);
  let initial_attempt = recovered["task_attempts"]["attempts"][0].clone();
  assert!(
    recover_attempt_from_store(
      &store,
      &run,
      &mut recovered,
      "capture-hard-death",
      "1970-01-01T00:00:03Z"
    )
    .unwrap()
  );
  assert_eq!(recovered["system_capture_id"], "capture-hard-death");
  assert_eq!(recovered["system_capture_start_offset"], prefix.len());
  assert_eq!(recovered["task_attempts"]["attempts"][0], initial_attempt);
  store.close_run_capture("capture-hard-death", 4000).unwrap();
  store
    .finish_capture("capture-hard-death", true, Vec::new())
    .unwrap();
  assert!(
    recover_attempt_from_store(
      &store,
      &run,
      &mut recovered,
      "capture-hard-death",
      "1970-01-01T00:00:03Z"
    )
    .unwrap()
  );
  assert_eq!(recovered["system_capture_status"], "complete");
  assert_eq!(
    recovered["system_capture_finished_at"],
    "1970-01-01T00:00:04+00:00"
  );
  assert!(
    fs::read(run.join(RUN_SYSTEM_PATH))
      .unwrap()
      .starts_with(&prefix)
  );
  for field in ["run_id", "machine_id"] {
    let mut wrong = previous.clone();
    wrong[field] = serde_json::json!("another-identity");
    let unchanged = wrong.clone();
    assert!(
      recover_attempt_from_store(
        &store,
        &run,
        &mut wrong,
        "capture-hard-death",
        "1970-01-01T00:00:03Z"
      )
      .is_err()
    );
    assert_eq!(wrong, unchanged);
  }
  let mut wrong = previous.clone();
  assert!(
    recover_attempt_from_store(
      &store,
      &run,
      &mut wrong,
      "capture-hard-death",
      "1970-01-01T00:00:05Z"
    )
    .is_err()
  );
  assert_eq!(wrong, previous);
  assert!(
    !recover_attempt_from_store(
      &store,
      &run,
      &mut wrong,
      "capture-not-registered",
      "1970-01-01T00:00:03Z"
    )
    .unwrap()
  );
  assert_eq!(wrong, previous);
}

#[test]
fn waiting_keeps_capture_open_and_records_observations_after_a_stale_finish() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  let mut waiting = state(&store, &id);
  waiting["status"] = "waiting".into();
  let before = waiting.clone();
  close_state(&run, &mut waiting);
  finalize_state(&run, &mut waiting);
  assert_eq!(waiting, before);
  assert!(!terminal(&waiting));
  assert!(!terminal_capture(&waiting));
  let row = capture(&store.connection, &id).unwrap();
  assert!(!row.closed);
  assert_eq!(row.end_ms, None);
  record(&mut store, 2500);
  store.export_run_capture(&id).unwrap();
  assert!(records(&run).iter().any(|record| matches!(
    record,
    RunSystemRecord::Sample { sample } if sample.observed_at_ms == 2500
  )));
  assert!(
    !records(&run)
      .iter()
      .any(|record| matches!(record, RunSystemRecord::End { .. }))
  );
}

#[test]
fn collector_metadata_leaves_waiting_intact_and_preserves_fields_when_terminal() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  drop(lock::run_lock(&run).unwrap());
  let mut waiting = state(&store, &id);
  waiting["status"] = "waiting".into();
  waiting["nickname"] = "experiment one".into();
  waiting["tags"] = serde_json::json!(["batch"]);
  waiting["task_metadata"] = serde_json::json!({"attempt": 2});
  let path = run.join("run-state.json");
  fs::write(&path, serde_json::to_vec(&waiting).unwrap()).unwrap();
  store.close_run_capture(&id, 2000).unwrap();
  store.finish_capture(&id, true, Vec::new()).unwrap();
  let row = capture(&store.connection, &id).unwrap();
  publish_capture_metadata(&store.connection, &row).unwrap();
  assert_eq!(read_state(&path).unwrap(), waiting);
  assert!(
    !sql::<bool>(store.connection.query_row(
      "SELECT metadata_saved FROM system_run_captures WHERE capture_id=?1",
      [&id],
      |row| row.get(0),
    ))
    .unwrap()
  );
  waiting["status"] = "completed".into();
  fs::write(&path, serde_json::to_vec(&waiting).unwrap()).unwrap();
  publish_capture_metadata(&store.connection, &row).unwrap();
  let published = read_state(&path).unwrap();
  assert_eq!(published["status"], "completed");
  assert_eq!(published["exit_code"], waiting["exit_code"]);
  assert_eq!(published["system_capture_status"], "complete");
  assert_eq!(published["nickname"], waiting["nickname"]);
  assert_eq!(published["tags"], waiting["tags"]);
  assert_eq!(published["task_metadata"], waiting["task_metadata"]);
}

#[test]
fn outbox_retains_exact_period_samples_after_raw_expiry() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 999);
  record(&mut store, 1000);
  store.close_run_capture(&id, 2000).unwrap();
  record(&mut store, 1999); // collection began before close and commits afterward
  record(&mut store, 2000); // collector acknowledges the exclusive end boundary
  prune(&mut store.connection, DETAIL_RETENTION_MS + 10_000, 10_000).unwrap();
  assert_eq!(store.status().unwrap().detail_count, 0);
  assert!(capture(&store.connection, &id).unwrap().acknowledged);
  store.finish_capture(&id, true, Vec::new()).unwrap();
  let records = records(&run);
  let samples = records
    .iter()
    .filter_map(|record| {
      if let RunSystemRecord::Sample { sample } = record {
        Some(sample)
      } else {
        None
      }
    })
    .collect::<Vec<_>>();
  assert_eq!(
    samples
      .iter()
      .map(|sample| sample.observed_at_ms)
      .collect::<Vec<_>>(),
    [1000, 1999]
  );
  assert_eq!(samples[0].counters["network/test/received_bytes"], u64::MAX);
  assert!(matches!(
    records.last(),
    Some(RunSystemRecord::End {
      end_ms: 2000,
      complete: true,
      ..
    })
  ));
  let raw = fs::read_to_string(run.join(RUN_SYSTEM_PATH)).unwrap();
  assert!(raw.contains("\"18446744073709551615\""));
  assert!(!raw.contains("\"step\""));
}

#[test]
fn published_prefix_excludes_uncheckpointed_tail_and_recovery_does_not_duplicate_samples() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 1000);
  store.export_run_capture(&id).unwrap();
  let mut state = state(&store, &id);
  let committed = committed_prefix(&run, &state).unwrap().unwrap();
  let mut stream = OpenOptions::new()
    .append(true)
    .open(run.join(RUN_SYSTEM_PATH))
    .unwrap();
  stream
    .write_all(b"uncheckpointed producer crash tail\n")
    .unwrap();
  stream.sync_all().unwrap();
  assert_eq!(committed_prefix(&run, &state).unwrap(), Some(committed));
  store.export_run_capture(&id).unwrap();
  assert_eq!(
    fs::metadata(run.join(RUN_SYSTEM_PATH)).unwrap().len(),
    committed
  );
  store.close_run_capture(&id, 2000).unwrap();
  record(&mut store, 2000);
  store.finish_capture(&id, true, Vec::new()).unwrap();
  assert_eq!(
    records(&run)
      .iter()
      .filter(|record| matches!(record, RunSystemRecord::Sample { .. }))
      .count(),
    1
  );
  state["system_capture_status"] = "complete".into();
  fs::remove_file(run.join(CHECKPOINT)).unwrap();
  assert_eq!(
    committed_prefix(&run, &state).unwrap(),
    Some(fs::metadata(run.join(RUN_SYSTEM_PATH)).unwrap().len())
  );
  state["system_capture_status"] = "capturing".into();
  assert!(committed_prefix(&run, &state).is_err());
}

#[test]
fn capture_never_overwrites_an_existing_user_output() {
  let (_directory, mut store, run) = fixture();
  let path = run.join(RUN_SYSTEM_PATH);
  fs::write(&path, b"user output").unwrap();
  assert!(store.begin_run_capture(&run, "run-fixture", 1000).is_err());
  assert_eq!(fs::read(path).unwrap(), b"user output");
  assert!(!run.join(CHECKPOINT).exists());
  let count: u64 = store
    .connection
    .query_row("SELECT COUNT(*) FROM system_run_captures", [], |row| {
      row.get(0)
    })
    .unwrap();
  assert_eq!(count, 0);
}

#[test]
fn sample_and_capture_outbox_commit_atomically() {
  let (_directory, mut store, run) = fixture();
  store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  store.connection.execute_batch("CREATE TRIGGER capture_failure BEFORE INSERT ON system_run_capture_samples BEGIN SELECT RAISE(ABORT,'capture failure'); END;").unwrap();
  let (inventory, sample) = observation(1000);
  assert!(store.record(inventory, sample).is_err());
  assert_eq!(store.status().unwrap().sample_sequence, 0);
  assert_eq!(store.status().unwrap().detail_count, 0);
  assert!(store.profile().unwrap().is_none());
}

#[test]
fn collector_completes_metadata_only_after_the_supervisor_releases_its_lease() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  let lease = lock::run_lock(&run).unwrap();
  let state = state(&store, &id);
  fs::write(
    run.join("run-state.json"),
    serde_json::to_vec(&state).unwrap(),
  )
  .unwrap();
  store.close_run_capture(&id, 2000).unwrap();
  record(&mut store, 1999);
  record(&mut store, 2000);
  store.export_run_captures().unwrap();
  assert_eq!(
    read_state(&run.join("run-state.json")).unwrap()["system_capture_status"],
    "capturing"
  );
  drop(lease);
  store.export_run_captures().unwrap();
  let finished = read_state(&run.join("run-state.json")).unwrap();
  assert_eq!(finished["status"], "completed");
  assert_eq!(finished["exit_code"], 0);
  assert_eq!(finished["system_capture_status"], "complete");
}

#[test]
fn unsupported_collector_cannot_start_or_falsely_complete_a_capture() {
  let (_directory, mut store, run) = fixture();
  let database = Path::new(store.connection.path().unwrap());
  let lease_path = database.parent().unwrap().join("collector.lock");
  let lease = match lock::try_lock_file(&lease_path, true).unwrap() {
    LockAttempt::Acquired(lease) => lease,
    _ => panic!("fixture collector lease is available"),
  };
  assert!(store.begin_run_capture(&run, "run-fixture", 1000).is_err());
  assert!(!run.join(RUN_SYSTEM_PATH).exists());
  record(&mut store, 999);
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 1000);
  store
    .connection
    .execute("UPDATE system_local SET sequence=sequence+1", [])
    .unwrap();
  drop(lease);
  store.close_run_capture(&id, 2000).unwrap();
  store.finish_capture(&id, true, Vec::new()).unwrap();
  assert!(
    matches!(records(&run).last(), Some(RunSystemRecord::End { complete:false, warnings, .. }) if !warnings.is_empty())
  );
}

#[test]
fn upgraded_collector_keeps_a_durable_gap_after_a_downgrade() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 1000);
  store
    .connection
    .execute("UPDATE system_local SET sequence=sequence+1", [])
    .unwrap();
  record(&mut store, 1500);
  store.close_run_capture(&id, 2000).unwrap();
  record(&mut store, 2000);
  assert!(capture(&store.connection, &id).unwrap().acknowledged);
  store.finish_capture(&id, true, Vec::new()).unwrap();
  assert!(!capture(&store.connection, &id).unwrap().complete);
  assert_eq!(
    records(&run)
      .iter()
      .filter(|record| matches!(record, RunSystemRecord::Sample { .. }))
      .count(),
    2
  );
}

#[test]
fn native_clock_repair_preserves_exact_run_owned_readings_without_duplicate_backfill() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 1_000_000);
  store.export_run_capture(&id).unwrap();
  record(&mut store, 1500);
  store.close_run_capture(&id, 2_000_000).unwrap();
  record(&mut store, 2_000_000);
  store.finish_capture(&id, true, Vec::new()).unwrap();
  assert!(capture(&store.connection, &id).unwrap().complete);
  let retained = records(&run)
    .into_iter()
    .filter_map(|record| {
      if let RunSystemRecord::Sample { sample } = record {
        Some(sample)
      } else {
        None
      }
    })
    .collect::<Vec<_>>();
  assert_eq!(
    retained
      .iter()
      .map(|sample| sample.observed_at_ms)
      .collect::<Vec<_>>(),
    [1_000_000, 1500]
  );
  assert_eq!(
    retained
      .iter()
      .map(|sample| sample.sequence)
      .collect::<Vec<_>>(),
    [1, 2]
  );
}

#[test]
fn exporter_bounds_memory_and_transaction_payload_for_large_observations() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  for time in [1000, 1500, 1999] {
    let (mut inventory, mut sample) = observation(time);
    // Valid source payload remains small; this simulates future payload growth
    // by adding JSON whitespace to its durable copies without changing values.
    inventory.hostname = "host".into();
    sample.metrics.clear();
    store.record(inventory, sample).unwrap();
  }
  let padding = " ".repeat(500 * 1024);
  store
    .connection
    .execute(
      "UPDATE system_run_capture_samples SET sample=sample||?1",
      [&padding],
    )
    .unwrap();
  assert!(store.export_run_capture(&id).unwrap());
  let pending: u64 = store
    .connection
    .query_row(
      "SELECT COUNT(*) FROM system_run_capture_samples WHERE capture_id=?1",
      [&id],
      |row| row.get(0),
    )
    .unwrap();
  assert_eq!(pending, 2);
  store.close_run_capture(&id, 2000).unwrap();
  record(&mut store, 2000);
  store.finish_capture(&id, true, Vec::new()).unwrap();
  assert_eq!(
    records(&run)
      .iter()
      .filter(|record| matches!(record, RunSystemRecord::Sample { .. }))
      .count(),
    3
  );
}

#[cfg(unix)]
#[test]
fn committed_prefix_only_requires_read_access_to_finalized_stream() {
  use std::os::unix::fs::PermissionsExt;
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  store.close_run_capture(&id, 2000).unwrap();
  store.finish_capture(&id, true, Vec::new()).unwrap();
  let mut state = state(&store, &id);
  state["system_capture_status"] = "complete".into();
  let path = run.join(RUN_SYSTEM_PATH);
  fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
  assert_eq!(
    committed_prefix(&run, &state).unwrap(),
    Some(fs::metadata(path).unwrap().len())
  );
}

#[test]
fn close_acknowledgement_freezes_eligibility_before_later_clock_regressions() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 1000);
  store.close_run_capture(&id, 2000).unwrap();
  record(&mut store, 1999);
  record(&mut store, 2000);
  store
    .connection
    .execute("UPDATE system_local SET sequence=sequence+1", [])
    .unwrap();
  record(&mut store, 1500);
  store.finish_capture(&id, true, Vec::new()).unwrap();
  let records = records(&run);
  let observations = records
    .iter()
    .filter_map(|record| {
      if let RunSystemRecord::Sample { sample } = record {
        Some(sample.observed_at_ms)
      } else {
        None
      }
    })
    .collect::<Vec<_>>();
  assert_eq!(observations, [1000, 1999]);
  assert!(capture(&store.connection, &id).unwrap().complete);
}

#[test]
fn independent_compaction_does_not_certify_an_old_collector() {
  let (_directory, mut store, run) = fixture();
  record(&mut store, 999);
  store
    .connection
    .execute("UPDATE system_local SET sequence=sequence+1", [])
    .unwrap();
  store.compact(1000).unwrap();
  assert!(store.begin_run_capture(&run, "run-fixture", 1000).is_err());
  assert!(!run.join(RUN_SYSTEM_PATH).exists());
}

#[test]
fn collector_repairs_a_failed_close_checkpoint_before_publishing_complete_metadata() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  drop(lock::run_lock(&run).unwrap());
  fs::write(
    run.join("run-state.json"),
    serde_json::to_vec(&state(&store, &id)).unwrap(),
  )
  .unwrap();
  store.close_run_capture(&id, 2000).unwrap();
  let checkpoint = fs::read(run.join(CHECKPOINT)).unwrap();
  store.finish_capture(&id, true, Vec::new()).unwrap();
  // Reproduce a filesystem failure after the End and DB close were committed,
  // before their corresponding private publication checkpoint was saved.
  fs::remove_file(run.join(CHECKPOINT)).unwrap();
  fs::create_dir(run.join(CHECKPOINT)).unwrap();
  assert!(capture(&store.connection, &id).unwrap().closed);
  store.export_run_captures().unwrap();
  assert_eq!(
    read_state(&run.join("run-state.json")).unwrap()["system_capture_status"],
    "capturing"
  );
  fs::remove_dir(run.join(CHECKPOINT)).unwrap();
  fs::write(run.join(CHECKPOINT), checkpoint).unwrap();
  store.export_run_captures().unwrap();
  let state = read_state(&run.join("run-state.json")).unwrap();
  assert_eq!(state["system_capture_status"], "complete");
  assert_eq!(
    committed_prefix(&run, &state).unwrap(),
    Some(fs::metadata(run.join(RUN_SYSTEM_PATH)).unwrap().len())
  );
}

#[test]
fn terminal_metadata_requires_a_closed_prefix_with_its_matching_end() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  let header_checkpoint = fs::read(run.join(CHECKPOINT)).unwrap();
  store.close_run_capture(&id, 2000).unwrap();
  store.finish_capture(&id, true, Vec::new()).unwrap();
  let mut state = state(&store, &id);
  state["system_capture_status"] = "complete".into();
  fs::write(run.join(CHECKPOINT), &header_checkpoint).unwrap();
  assert!(committed_prefix(&run, &state).is_err());
  let mut checkpoint: Checkpoint = serde_json::from_slice(&header_checkpoint).unwrap();
  checkpoint.closed = true;
  fs::write(
    run.join(CHECKPOINT),
    serde_json::to_vec(&checkpoint).unwrap(),
  )
  .unwrap();
  assert!(committed_prefix(&run, &state).is_err());
  save_checkpoint(&capture(&store.connection, &id).unwrap()).unwrap();
  assert!(committed_prefix(&run, &state).is_ok());
  state["finished_at"] = "1970-01-01T00:00:03Z".into();
  assert!(committed_prefix(&run, &state).is_err());
}

#[test]
fn abandoned_metadata_preserves_the_known_finish_recorded_before_terminal_publication() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 1500);
  store.close_run_capture(&id, 2000).unwrap();
  let mut state = state(&store, &id);
  state["status"] = "running".into();
  state.as_object_mut().unwrap().remove("finished_at");
  let database = PathBuf::from(store.connection.path().unwrap());
  let before = fs::read(&database).unwrap();
  let readonly = LocalStore::open_readonly(&database).unwrap().unwrap();
  assert_eq!(
    known_end_from_store(&readonly, &run, &state).unwrap(),
    Some(2000)
  );
  assert_eq!(fs::read(&database).unwrap(), before);
  state["status"] = "lost".into();
  state["finished_at"] = chrono::DateTime::from_timestamp_millis(
    known_end_from_store(&readonly, &run, &state)
      .unwrap()
      .unwrap(),
  )
  .unwrap()
  .to_rfc3339()
  .into();
  store.finish_capture(&id, true, Vec::new()).unwrap();
  state["system_capture_status"] = "complete".into();
  assert!(matches!(
    records(&run).last(),
    Some(RunSystemRecord::End {
      end_ms: 2000,
      complete: true,
      ..
    })
  ));
  assert!(committed_prefix(&run, &state).is_ok());
  state["run_id"] = "run-other".into();
  assert_eq!(known_end_from_store(&readonly, &run, &state).unwrap(), None);
  state["run_id"] = "run-fixture".into();
  state["machine_id"] = "machine-other".into();
  assert_eq!(known_end_from_store(&readonly, &run, &state).unwrap(), None);
  state["machine_id"] = store.machine_id().into();
  state["started_at"] = "1970-01-01T00:00:00Z".into();
  assert_eq!(known_end_from_store(&readonly, &run, &state).unwrap(), None);
}
