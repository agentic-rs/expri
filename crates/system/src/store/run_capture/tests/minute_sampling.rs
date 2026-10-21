use super::*;

fn samples(run: &Path) -> Vec<SystemSample> {
  records(run)
    .into_iter()
    .filter_map(|record| match record {
      RunSystemRecord::Sample { sample } => Some(sample),
      _ => None,
    })
    .collect()
}

fn disable_minute_triggers(connection: &Connection) {
  connection.execute_batch("DROP TRIGGER system_run_capture_minute_gate; DROP TRIGGER system_run_capture_minute_reserve;").unwrap();
}

#[test]
fn minute_selection_preserves_real_payloads_and_high_rate_machine_collection() {
  let (_directory, mut store, run) = fixture();
  let training = run.join("outputs/metrics.jsonl");
  fs::write(&training, b"{\"step\":7,\"metrics\":{\"loss\":0.5}}\n").unwrap();
  let id = store
    .begin_run_capture(&run, "run-fixture", 10_000)
    .unwrap();
  for time in [9999, 10_000, 10_001, 59_999, 60_000, 60_001, 120_000] {
    record(&mut store, time);
  }
  store.close_run_capture(&id, 120_001).unwrap();
  record(&mut store, 120_001);
  assert!(capture(&store.connection, &id).unwrap().acknowledged);
  assert_eq!(store.status().unwrap().detail_count, 8);
  store.finish_capture(&id, true, Vec::new()).unwrap();
  let retained = samples(&run);
  assert_eq!(
    retained
      .iter()
      .map(|sample| sample.observed_at_ms)
      .collect::<Vec<_>>(),
    [10_000, 60_000, 120_000]
  );
  assert_eq!(
    retained
      .iter()
      .map(|sample| sample.sequence)
      .collect::<Vec<_>>(),
    [2, 5, 7]
  );
  assert!(
    retained.iter().all(|sample| sample.interval_ms == 10
      && sample.counters["network/test/received_bytes"] == u64::MAX)
  );
  assert_eq!(
    fs::read(&training).unwrap(),
    b"{\"step\":7,\"metrics\":{\"loss\":0.5}}\n"
  );
}

#[test]
fn seen_minutes_survive_export_reopen_and_revisited_clock_buckets() {
  let (_directory, mut store, run) = fixture();
  let database = PathBuf::from(store.connection.path().unwrap());
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 121_000);
  store.export_run_capture(&id).unwrap();
  drop(store);
  let mut store = LocalStore::open(&database).unwrap();
  for time in [1000, 122_000, 61_000, 1500] {
    record(&mut store, time);
    store.export_run_capture(&id).unwrap();
  }
  assert_eq!(
    samples(&run)
      .iter()
      .map(|sample| sample.observed_at_ms)
      .collect::<Vec<_>>(),
    [121_000, 1000, 61_000]
  );
}

#[test]
fn backfill_selects_the_first_sequence_in_each_eligible_minute() {
  let (_directory, mut store, run) = fixture();
  for time in [999, 1000, 1999, 60_000, 61_000] {
    record(&mut store, time);
  }
  store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  assert_eq!(
    samples(&run)
      .iter()
      .map(|sample| sample.observed_at_ms)
      .collect::<Vec<_>>(),
    [1000, 60_000]
  );
}

#[test]
fn sqlite_gate_also_constrains_an_old_collectors_unfiltered_insert() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 1000);
  store.connection.execute(
    "INSERT INTO system_run_capture_samples(capture_id,sequence,observed_at_ms,inventory,sample) SELECT capture_id,sequence+100,1500,inventory,sample FROM system_run_capture_samples WHERE capture_id=?1",
    [&id],
  ).unwrap();
  assert_eq!(
    store
      .connection
      .query_row(
        "SELECT COUNT(*) FROM system_run_capture_samples WHERE capture_id=?1",
        [&id],
        |row| row.get::<_, u64>(0)
      )
      .unwrap(),
    1
  );
}

#[test]
fn sql_minute_floor_matches_rust_for_negative_and_boundary_timestamps() {
  let (_directory, store, _run) = fixture();
  for (sequence, time) in [-1_i64, -2, -60_000, -60_001, 0, 59_999, 60_000]
    .into_iter()
    .enumerate()
  {
    store
      .connection
      .execute(
        "INSERT INTO system_run_capture_samples VALUES('capture-floor',?1,?2,'{}','{}')",
        params![sequence as i64 + 1, time],
      )
      .unwrap();
  }
  let mut statement = store
    .connection
    .prepare("SELECT observed_at_ms FROM system_run_capture_samples ORDER BY observed_at_ms")
    .unwrap();
  let retained = statement
    .query_map([], |row| row.get::<_, i64>(0))
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap();
  assert_eq!(retained, [-60_001, -1, 0, 60_000]);
  for time in retained {
    assert!(store.connection.query_row("SELECT EXISTS(SELECT 1 FROM system_run_capture_minutes WHERE capture_id='capture-floor' AND minute=?1)", [time.div_euclid(60_000)], |row| row.get::<_, bool>(0)).unwrap());
  }
}

#[test]
fn closing_releases_only_unexported_out_of_window_minute_reservations() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 2500);
  store.close_run_capture(&id, 2000).unwrap();
  record(&mut store, 1500);
  record(&mut store, 2000);
  store.finish_capture(&id, true, Vec::new()).unwrap();
  assert_eq!(
    samples(&run)
      .iter()
      .map(|sample| sample.observed_at_ms)
      .collect::<Vec<_>>(),
    [1500]
  );
}

#[test]
fn legacy_prefix_is_unchanged_while_unpublished_pending_rows_are_thinned() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  disable_minute_triggers(&store.connection);
  record(&mut store, 1000);
  record(&mut store, 1500);
  store.export_run_capture(&id).unwrap();
  let prefix = fs::read(run.join(RUN_SYSTEM_PATH)).unwrap();
  record(&mut store, 61_000);
  store
    .connection
    .execute_batch("DELETE FROM system_run_capture_policy; DELETE FROM system_run_capture_minutes;")
    .unwrap();
  initialize(&store.connection).unwrap();
  record(&mut store, 62_000); // old collector may finish a batch during seeding
  store.export_run_capture(&id).unwrap();
  for time in [63_000, 1800, 121_000] {
    record(&mut store, time);
  }
  store.export_run_capture(&id).unwrap();
  assert!(
    fs::read(run.join(RUN_SYSTEM_PATH))
      .unwrap()
      .starts_with(&prefix)
  );
  assert_eq!(
    samples(&run)
      .iter()
      .map(|sample| sample.observed_at_ms)
      .collect::<Vec<_>>(),
    [1000, 1500, 61_000, 121_000]
  );
  assert!(
    !store
      .connection
      .query_row(
        "SELECT EXISTS(SELECT 1 FROM system_run_capture_minute_seed WHERE capture_id=?1)",
        [&id],
        |row| row.get::<_, bool>(0)
      )
      .unwrap()
  );
}

#[test]
fn a_skipped_inventory_change_is_emitted_when_its_first_selected_sample_arrives() {
  let (_directory, mut store, run) = fixture();
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  record(&mut store, 1000);
  store.export_run_capture(&id).unwrap();
  for time in [1500, 61_000] {
    let (mut inventory, sample) = observation(time);
    inventory.hostname = "renamed-host".into();
    store.record(inventory, sample).unwrap();
  }
  store.export_run_capture(&id).unwrap();
  let records = records(&run);
  let inventories = records
    .iter()
    .filter_map(|record| match record {
      RunSystemRecord::Inventory { inventory } => Some(inventory.revision),
      _ => None,
    })
    .collect::<Vec<_>>();
  assert_eq!(inventories, [1, 2]);
  assert_eq!(
    samples(&run)
      .iter()
      .map(|sample| sample.inventory_revision)
      .collect::<Vec<_>>(),
    [1, 2]
  );
}

#[test]
fn legacy_seed_is_bounded_resumable_and_never_reopens_an_initialized_prefix() {
  let (_directory, mut store, run) = fixture();
  let database = PathBuf::from(store.connection.path().unwrap());
  let id = store.begin_run_capture(&run, "run-fixture", 1000).unwrap();
  disable_minute_triggers(&store.connection);
  let mut file = OpenOptions::new()
    .append(true)
    .open(run.join(RUN_SYSTEM_PATH))
    .unwrap();
  for sequence in 1..=5000 {
    let (_, mut sample) = observation(1000 + sequence as i64);
    sample.sequence = sequence;
    sample.inventory_revision = 1;
    line(&mut file, &RunSystemRecord::Sample { sample }).unwrap();
  }
  file.sync_all().unwrap();
  let target = file.metadata().unwrap().len();
  store
    .connection
    .execute(
      "UPDATE system_run_captures SET file_offset=?2,exported_sequence=5000 WHERE capture_id=?1",
      params![id, target],
    )
    .unwrap();
  store
    .connection
    .execute_batch("DELETE FROM system_run_capture_policy;")
    .unwrap();
  initialize(&store.connection).unwrap();
  let row = capture(&store.connection, &id).unwrap();
  assert!(!sampling::seed(&store.connection, &row).unwrap());
  let offset: u64 = store
    .connection
    .query_row(
      "SELECT scan_offset FROM system_run_capture_minute_seed WHERE capture_id=?1",
      [&id],
      |row| row.get(0),
    )
    .unwrap();
  assert!(offset > 0 && offset < target);
  drop(store);
  let store = LocalStore::open(&database).unwrap();
  assert_eq!(
    store
      .connection
      .query_row(
        "SELECT scan_offset FROM system_run_capture_minute_seed WHERE capture_id=?1",
        [&id],
        |row| row.get::<_, u64>(0)
      )
      .unwrap(),
    offset
  );
  assert!(sampling::seed(&store.connection, &capture(&store.connection, &id).unwrap()).unwrap());
  fs::remove_file(run.join(RUN_SYSTEM_PATH)).unwrap();
  assert!(sampling::seed(&store.connection, &capture(&store.connection, &id).unwrap()).unwrap());
}
