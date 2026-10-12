use super::*;

fn inventory(time: i64) -> SystemInventory {
  SystemInventory {
    observed_at_ms: time,
    hostname: "host".into(),
    os_name: Some("Test".into()),
    os_version: None,
    kernel_version: None,
    architecture: "test".into(),
    cpu: CpuInventory {
      brand: "cpu".into(),
      vendor: "vendor".into(),
      logical_cpu_count: 2,
      physical_core_count: Some(1),
    },
    memory_total_bytes: 1024,
    measurement_scope: "host".into(),
    devices: Vec::new(),
    warnings: Vec::new(),
  }
}
fn sample(time: i64, interval: u64, gauge: f64, delta: f64) -> SystemSample {
  SystemSample {
    sequence: 0,
    session_id: "session".into(),
    observed_at_ms: time,
    interval_ms: interval,
    collection_duration_ms: 1,
    inventory_revision: 0,
    metrics: BTreeMap::from([
      (
        "cpu/usage_percent".into(),
        MetricReading {
          kind: MetricKind::Gauge,
          value: gauge,
        },
      ),
      (
        "network/test/received_bytes".into(),
        MetricReading {
          kind: MetricKind::CounterDelta,
          value: delta,
        },
      ),
    ]),
    counters: BTreeMap::new(),
    warnings: Vec::new(),
  }
}
fn query(
  machine_id: &str,
  resolution: ProfileResolution,
  start_ms: i64,
  end_ms: i64,
  limit: usize,
) -> ProfileHistoryQuery {
  ProfileHistoryQuery {
    machine_id: machine_id.into(),
    resolution,
    start_ms,
    end_ms,
    limit,
    cursor: None,
  }
}
fn local() -> (tempfile::TempDir, LocalStore) {
  let root = tempfile::tempdir().unwrap();
  let store = LocalStore::open(&root.path().join("global/profiles.sqlite3")).unwrap();
  (root, store)
}
fn receiver() -> Connection {
  let connection = Connection::open_in_memory().unwrap();
  initialize(&connection).unwrap();
  connection
}
fn code(error: ExpriError) -> u16 {
  match error {
    ExpriError::ServiceRejected { status, .. } => status,
    _ => panic!("unexpected error: {error}"),
  }
}

#[test]
fn weighted_hours_split_boundaries_and_do_not_fill_downtime() {
  let (_root, mut store) = local();
  store
    .record(
      inventory(HOUR_MS - 10_000),
      sample(HOUR_MS - 10_000, 10_000, 20.0, 100.0),
    )
    .unwrap();
  store
    .record(
      inventory(HOUR_MS + 10_000),
      sample(HOUR_MS + 10_000, 20_000, 50.0, 600.0),
    )
    .unwrap();
  store
    .record(
      inventory(4 * HOUR_MS + 10_000),
      sample(4 * HOUR_MS + 10_000, 10_000, 80.0, 200.0),
    )
    .unwrap();
  let hours = history(
    &store.connection,
    &query(
      store.machine_id(),
      ProfileResolution::Hourly,
      0,
      5 * HOUR_MS,
      100,
    ),
  )
  .unwrap()
  .hours;
  assert_eq!(
    hours
      .iter()
      .map(|hour| hour.hour_start_ms)
      .collect::<Vec<_>>(),
    [0, HOUR_MS, 4 * HOUR_MS]
  );
  let first = &hours[0].metrics["cpu/usage_percent"];
  assert_eq!(first.coverage_ms, 20_000);
  assert_eq!(first.mean(), Some(35.0));
  assert_eq!((first.min, first.max, first.last), (20.0, 50.0, 50.0));
  assert_eq!(hours[0].metrics["network/test/received_bytes"].sum, 400.0);
  assert_eq!(hours[1].metrics["network/test/received_bytes"].sum, 300.0);
  assert_eq!(hours[1].metrics["cpu/usage_percent"].coverage_ms, 10_000);
  assert_eq!(hours[2].metrics["cpu/usage_percent"].coverage_ms, 10_000);
  assert_eq!(hours[0].inventory_revisions, [1]);
}

#[test]
fn warmup_is_a_point_without_invented_coverage() {
  let (_root, mut store) = local();
  store
    .record(inventory(HOUR_MS), sample(HOUR_MS, 0, 20.0, 0.0))
    .unwrap();
  let batch = store.pending_batch("server").unwrap();
  assert_eq!(batch.hours.len(), 1);
  let cpu = &batch.hours[0].metrics["cpu/usage_percent"];
  assert_eq!(cpu.coverage_ms, 0);
  assert_eq!(cpu.mean(), None);
  assert_eq!(cpu.sample_count, 1);
  assert_eq!(cpu.sum, 0.0);
}

#[test]
fn sequences_and_semantic_inventory_revisions_survive_reopen() {
  let (root, mut store) = local();
  let id = store.machine_id().to_string();
  let first = store
    .record(inventory(10_000), sample(10_000, 10_000, 20.0, 1.0))
    .unwrap();
  let second = store
    .record(inventory(20_000), sample(20_000, 10_000, 30.0, 1.0))
    .unwrap();
  assert_eq!(
    (first.sequence, second.sequence, second.inventory_revision),
    (1, 2, 1)
  );
  drop(store);
  let mut store = LocalStore::open(&root.path().join("global/profiles.sqlite3")).unwrap();
  assert_eq!(store.machine_id(), id);
  let mut changed = inventory(30_000);
  changed.memory_total_bytes = 2048;
  let third = store
    .record(changed, sample(30_000, 10_000, 40.0, 1.0))
    .unwrap();
  assert_eq!((third.sequence, third.inventory_revision), (3, 2));
  assert_eq!(
    store
      .inventory(Some(1))
      .unwrap()
      .unwrap()
      .inventory
      .memory_total_bytes,
    1024
  );
}

#[test]
fn failed_record_rolls_back_sequence_inventory_and_raw_data() {
  let (_root, mut store) = local();
  store
    .record(inventory(10_000), sample(10_000, 10_000, 20.0, 1.0))
    .unwrap();
  let before = store.status().unwrap();
  let mut bad = sample(20_000, 10_000, 30.0, 1.0);
  bad.metrics.get_mut("cpu/usage_percent").unwrap().kind = MetricKind::CounterDelta;
  let mut changed = inventory(20_000);
  changed.memory_total_bytes = 2048;
  assert_eq!(code(store.record(changed, bad).unwrap_err()), 400);
  let after = store.status().unwrap();
  assert_eq!(
    (
      before.sample_sequence,
      before.inventory_revision,
      before.detail_count,
      before.hour_count
    ),
    (
      after.sample_sequence,
      after.inventory_revision,
      after.detail_count,
      after.hour_count
    )
  );
  assert_eq!(
    store
      .record(inventory(30_000), sample(30_000, 10_000, 40.0, 1.0))
      .unwrap()
      .sequence,
    2
  );
}

#[test]
fn receiver_deduplicates_retries_and_rejects_conflicts_atomically() {
  let (_root, mut store) = local();
  store
    .record(inventory(10_000), sample(10_000, 10_000, 20.0, 1.0))
    .unwrap();
  let batch = store.pending_batch("server").unwrap();
  let mut connection = receiver();
  let first_ack = ingest(&mut connection, &batch, 10_000).unwrap();
  assert_eq!(ingest(&mut connection, &batch, 20_000).unwrap(), first_ack);
  let hours = history(
    &connection,
    &query(
      store.machine_id(),
      ProfileResolution::Hourly,
      0,
      HOUR_MS,
      100,
    ),
  )
  .unwrap()
  .hours;
  assert_eq!(hours, batch.hours);
  let mut conflicting = batch.clone();
  conflicting.samples[0]
    .metrics
    .get_mut("cpu/usage_percent")
    .unwrap()
    .value = 99.0;
  assert_eq!(
    code(ingest(&mut connection, &conflicting, 30_000).unwrap_err()),
    409
  );
  let mut conflicting = batch.clone();
  conflicting.hours[0]
    .metrics
    .get_mut("cpu/usage_percent")
    .unwrap()
    .sum = 99.0;
  assert_eq!(
    code(ingest(&mut connection, &conflicting, 30_000).unwrap_err()),
    409
  );
  let mut conflicting = batch.clone();
  conflicting.inventories[0].inventory.hostname = "other".into();
  assert_eq!(
    code(ingest(&mut connection, &conflicting, 30_000).unwrap_err()),
    409
  );
  assert_eq!(
    history(
      &connection,
      &query(
        store.machine_id(),
        ProfileResolution::Detail,
        0,
        HOUR_MS,
        100
      )
    )
    .unwrap()
    .samples,
    batch.samples
  );
}

#[test]
fn newer_hour_snapshots_replace_and_old_retries_cannot_reduce_them() {
  let (_root, mut store) = local();
  store
    .record(inventory(10_000), sample(10_000, 10_000, 20.0, 100.0))
    .unwrap();
  let first = store.pending_batch("server").unwrap();
  let mut connection = receiver();
  let first_ack = ingest(&mut connection, &first, 10_000).unwrap();
  store.acknowledge("server", &first, &first_ack).unwrap();
  store
    .record(inventory(20_000), sample(20_000, 10_000, 40.0, 200.0))
    .unwrap();
  let second = store.pending_batch("server").unwrap();
  ingest(&mut connection, &second, 20_000).unwrap();
  ingest(&mut connection, &first, 20_000).unwrap();
  let hours = history(
    &connection,
    &query(
      store.machine_id(),
      ProfileResolution::Hourly,
      0,
      HOUR_MS,
      100,
    ),
  )
  .unwrap()
  .hours;
  assert_eq!(hours[0].revision, 2);
  assert_eq!(hours[0].metrics["network/test/received_bytes"].sum, 300.0);
  assert_eq!(hours[0].sample_count, 2);
}

#[test]
fn offline_beyond_retention_keeps_hour_history_and_can_publish_it() {
  let (_root, mut store) = local();
  store
    .record(inventory(10_000), sample(10_000, 10_000, 20.0, 100.0))
    .unwrap();
  let now = 80 * HOUR_MS + 10_000;
  store
    .record(inventory(now), sample(now, 10_000, 40.0, 200.0))
    .unwrap();
  store.compact(now).unwrap();
  assert_eq!(store.status().unwrap().detail_count, 1);
  let batch = store.pending_batch("server").unwrap();
  assert_eq!(batch.samples.len(), 1);
  assert_eq!(batch.samples[0].sequence, 2);
  assert_eq!(batch.hours.len(), 2);
  let mut connection = receiver();
  let ack = ingest(&mut connection, &batch, now).unwrap();
  store.acknowledge("server", &batch, &ack).unwrap();
  assert!(store.pending_batch("server").unwrap().hours.is_empty());
  assert_eq!(
    history(
      &connection,
      &query(
        store.machine_id(),
        ProfileResolution::Hourly,
        0,
        81 * HOUR_MS,
        100
      )
    )
    .unwrap()
    .hours
    .len(),
    2
  );
  assert_eq!(
    history(
      &connection,
      &query(
        store.machine_id(),
        ProfileResolution::Detail,
        0,
        81 * HOUR_MS,
        100
      )
    )
    .unwrap()
    .samples
    .len(),
    1
  );
}

#[test]
fn acknowledgements_are_endpoint_scoped_and_match_exact_snapshots() {
  let (_root, mut store) = local();
  store
    .record(inventory(10_000), sample(10_000, 10_000, 20.0, 100.0))
    .unwrap();
  let batch = store.pending_batch("first").unwrap();
  let mut connection = receiver();
  let ack = ingest(&mut connection, &batch, 10_000).unwrap();
  let mut bad = ack.clone();
  bad.hour_revisions.clear();
  assert!(store.acknowledge("first", &batch, &bad).is_err());
  assert_eq!(store.pending_batch("first").unwrap(), batch);
  store.acknowledge("first", &batch, &ack).unwrap();
  assert!(store.pending_batch("first").unwrap().samples.is_empty());
  assert_eq!(store.pending_batch("second").unwrap(), batch);
  // A newer summary recorded while upload was in progress must remain pending.
  store
    .record(inventory(20_000), sample(20_000, 10_000, 40.0, 100.0))
    .unwrap();
  store.acknowledge("first", &batch, &ack).unwrap();
  assert_eq!(store.pending_batch("first").unwrap().hours[0].revision, 2);
}

#[test]
fn inventory_backlog_larger_than_one_batch_progresses_before_referencing_hours() {
  let (_root, mut store) = local();
  for revision in 1..=70 {
    let time = revision * HOUR_MS + 10_000;
    let mut inv = inventory(time);
    inv.memory_total_bytes = revision as u64;
    store.record(inv, sample(time, 10_000, 20.0, 1.0)).unwrap();
  }
  let first = store.pending_batch("server").unwrap();
  assert_eq!(first.inventories.len(), 64);
  assert_eq!(first.hours.len(), 64);
  assert!(
    first
      .samples
      .iter()
      .all(|sample| sample.inventory_revision <= 64)
  );
  let mut connection = receiver();
  let ack = ingest(&mut connection, &first, 71 * HOUR_MS).unwrap();
  store.acknowledge("server", &first, &ack).unwrap();
  let next = store.pending_batch("server").unwrap();
  assert_eq!(next.inventories.len(), 6);
  assert_eq!(next.hours.len(), 6);
  let ack = ingest(&mut connection, &next, 71 * HOUR_MS).unwrap();
  store.acknowledge("server", &next, &ack).unwrap();
  assert!(store.pending_batch("server").unwrap().hours.is_empty());
}

#[test]
fn detail_pagination_preserves_equal_timestamps_and_hour_pagination_is_stable() {
  let (_root, mut store) = local();
  for _ in 0..3 {
    store
      .record(inventory(10_000), sample(10_000, 0, 20.0, 0.0))
      .unwrap();
  }
  let mut q = query(store.machine_id(), ProfileResolution::Detail, 0, HOUR_MS, 2);
  let page = history(&store.connection, &q).unwrap();
  assert_eq!(
    page
      .samples
      .iter()
      .map(|sample| sample.sequence)
      .collect::<Vec<_>>(),
    [1, 2]
  );
  q.cursor = page.next_cursor;
  assert_eq!(
    history(&store.connection, &q).unwrap().samples[0].sequence,
    3
  );
  assert_eq!(history(&store.connection, &q).unwrap().next_cursor, None);
  store
    .record(
      inventory(HOUR_MS + 10_000),
      sample(HOUR_MS + 10_000, 10_000, 40.0, 0.0),
    )
    .unwrap();
  let mut q = query(
    store.machine_id(),
    ProfileResolution::Hourly,
    0,
    2 * HOUR_MS,
    1,
  );
  let page = history(&store.connection, &q).unwrap();
  q.cursor = page.next_cursor;
  assert_eq!(q.cursor, Some(0));
  assert_eq!(
    history(&store.connection, &q).unwrap().hours[0].hour_start_ms,
    HOUR_MS
  );
}

#[test]
fn pruning_is_bounded_and_does_not_remove_summaries_or_inventories() {
  let (_root, mut store) = local();
  for i in 1..=5 {
    store
      .record(inventory(i * 10_000), sample(i * 10_000, 10_000, 20.0, 1.0))
      .unwrap();
  }
  let mut connection = receiver();
  let batch = store.pending_batch("server").unwrap();
  ingest(&mut connection, &batch, 50_000).unwrap();
  assert_eq!(prune(&mut connection, 80 * HOUR_MS, 2).unwrap(), 2);
  assert_eq!(
    history(
      &connection,
      &query(
        store.machine_id(),
        ProfileResolution::Detail,
        0,
        HOUR_MS,
        100
      )
    )
    .unwrap()
    .samples
    .len(),
    3
  );
  assert_eq!(compact(&mut connection, 80 * HOUR_MS).unwrap(), 3);
  assert_eq!(
    history(
      &connection,
      &query(
        store.machine_id(),
        ProfileResolution::Hourly,
        0,
        HOUR_MS,
        100
      )
    )
    .unwrap()
    .hours
    .len(),
    1
  );
  assert!(
    machine_profile(&connection, store.machine_id(), Some(1))
      .unwrap()
      .is_some()
  );
}

#[test]
fn machine_list_paginates_and_unknown_inventory_rolls_back_receiver() {
  let mut connection = receiver();
  for machine in ["machine-a", "machine-b", "machine-c"] {
    let batch = ProfileBatch {
      schema_version: 1,
      machine_id: machine.into(),
      inventories: vec![InventoryRevision {
        revision: 1,
        inventory: inventory(10_000),
      }],
      samples: Vec::new(),
      hours: Vec::new(),
    };
    ingest(&mut connection, &batch, 10_000).unwrap();
  }
  let (first, cursor) = list_machines(&connection, 2, None).unwrap();
  assert_eq!(first.len(), 2);
  assert_eq!(cursor.as_deref(), Some("machine-b"));
  let (second, cursor) = list_machines(&connection, 2, cursor.as_deref()).unwrap();
  assert_eq!(second[0].machine_id, "machine-c");
  assert_eq!(cursor, None);
  let mut unknown = sample(10_000, 10_000, 20.0, 1.0);
  unknown.sequence = 1;
  unknown.inventory_revision = 9;
  let batch = ProfileBatch {
    schema_version: 1,
    machine_id: "unknown-machine".into(),
    inventories: vec![InventoryRevision {
      revision: 1,
      inventory: inventory(10_000),
    }],
    samples: vec![unknown],
    hours: Vec::new(),
  };
  assert_eq!(
    code(ingest(&mut connection, &batch, 10_000).unwrap_err()),
    400
  );
  assert!(
    machine_profile(&connection, "unknown-machine", None)
      .unwrap()
      .is_none()
  );
}

#[cfg(unix)]
#[test]
fn local_database_is_private_and_rejects_database_or_sidecar_symlinks() {
  use std::os::unix::fs::{PermissionsExt, symlink};
  let (root, store) = local();
  let path = root.path().join("global/profiles.sqlite3");
  assert_eq!(
    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
    0o600
  );
  assert_eq!(
    std::fs::metadata(path.parent().unwrap())
      .unwrap()
      .permissions()
      .mode()
      & 0o777,
    0o700
  );
  drop(store);
  let outside = root.path().join("outside");
  std::fs::write(&outside, b"outside").unwrap();
  symlink(&outside, path.with_file_name("profiles.sqlite3-journal")).unwrap();
  assert!(LocalStore::open(&path).is_err());
  std::fs::remove_file(path.with_file_name("profiles.sqlite3-journal")).unwrap();
  std::fs::remove_file(&path).unwrap();
  symlink(&outside, &path).unwrap();
  assert!(LocalStore::open(&path).is_err());
  assert_eq!(std::fs::read(outside).unwrap(), b"outside");
}

#[test]
fn readonly_inspection_does_not_create_or_initialize_missing_data() {
  let root = tempfile::tempdir().unwrap();
  let path = root.path().join("absent/profiles.sqlite3");
  assert!(LocalStore::open_readonly(&path).unwrap().is_none());
  assert!(!path.parent().unwrap().exists());
  let mut writer = LocalStore::open(&path).unwrap();
  writer
    .record(inventory(10_000), sample(10_000, 10_000, 20.0, 1.0))
    .unwrap();
  let id = writer.machine_id().to_string();
  drop(writer);
  let reader = LocalStore::open_readonly(&path).unwrap().unwrap();
  assert_eq!(reader.machine_id(), id);
  assert_eq!(reader.status().unwrap().detail_count, 1);
  assert!(
    reader
      .history_at(
        &query(&id, ProfileResolution::Detail, 0, HOUR_MS, 100),
        80 * HOUR_MS
      )
      .unwrap()
      .samples
      .is_empty()
  );
  assert_eq!(reader.status().unwrap().detail_count, 1);
}

#[test]
fn future_observations_are_rejected_without_creating_a_machine() {
  let mut connection = receiver();
  let batch = ProfileBatch {
    schema_version: 1,
    machine_id: "future-machine".into(),
    inventories: vec![InventoryRevision {
      revision: 1,
      inventory: inventory(10 * HOUR_MS),
    }],
    samples: Vec::new(),
    hours: Vec::new(),
  };
  assert_eq!(
    code(ingest(&mut connection, &batch, 10_000).unwrap_err()),
    400
  );
  assert!(
    machine_profile(&connection, "future-machine", None)
      .unwrap()
      .is_none()
  );
}

#[test]
fn large_history_pages_stop_at_byte_budget_and_resume_without_lost_rows() {
  let (_root, mut store) = local();
  for i in 1..=5 {
    let mut reading = sample(i * 10_000, 0, 20.0, 0.0);
    // Valid long keys make each record large while staying within metric limits.
    for metric in 0..1800 {
      reading.metrics.insert(
        format!("metric/{metric}/{}", "x".repeat(200)),
        MetricReading {
          kind: MetricKind::Gauge,
          value: 1.0,
        },
      );
    }
    store.record(inventory(i * 10_000), reading).unwrap();
  }
  let mut q = query(
    store.machine_id(),
    ProfileResolution::Detail,
    0,
    HOUR_MS,
    1024,
  );
  let first = history(&store.connection, &q).unwrap();
  assert!(first.samples.len() < 5);
  assert!(first.next_cursor.is_some());
  let mut count = first.samples.len();
  q.cursor = first.next_cursor;
  loop {
    let page = history(&store.connection, &q).unwrap();
    count += page.samples.len();
    q.cursor = page.next_cursor;
    if q.cursor.is_none() {
      break;
    }
  }
  assert_eq!(count, 5);
}

#[test]
fn clock_correction_quarantines_future_points_without_pruning_good_past() {
  let root = tempfile::tempdir().unwrap();
  let path = root.path().join("profiles.sqlite3");
  let now = 100 * HOUR_MS + 10_000;
  let mut store = LocalStore::open_at(&path, now).unwrap();
  store
    .record(
      inventory(now - HOUR_MS),
      sample(now - HOUR_MS, 10_000, 20.0, 100.0),
    )
    .unwrap();
  let future = now + 100 * HOUR_MS;
  store
    .record(inventory(future), sample(future, 0, 90.0, 0.0))
    .unwrap();
  store.compact(future).unwrap();
  assert_eq!(
    store.status().unwrap().detail_count,
    2,
    "forward wall jump must not prune good past"
  );
  store
    .record(inventory(now), sample(now, 0, 40.0, 0.0))
    .unwrap();
  let status = store.status().unwrap();
  assert_eq!(status.quarantined_sample_count, 1);
  assert_eq!(status.quarantined_hour_count, 1);
  let batch = store.pending_batch("endpoint").unwrap();
  assert!(
    batch
      .samples
      .iter()
      .all(|sample| sample.observed_at_ms <= now)
  );
  assert!(
    batch
      .hours
      .iter()
      .all(|hour| hour.hour_start_ms <= now / HOUR_MS * HOUR_MS)
  );
  assert_eq!(batch.hours.len(), 2, "true older hour must remain intact");
  let mut server = receiver();
  let ack = ingest(&mut server, &batch, now).unwrap();
  store.acknowledge("endpoint", &batch, &ack).unwrap();
  let raw_quarantine: u64 = sql(store.connection.query_row(
    "SELECT COUNT(*) FROM system_local_quarantine WHERE kind='sample' AND payload IS NOT NULL",
    [],
    |row| row.get(0),
  ))
  .unwrap();
  assert_eq!(raw_quarantine, 0);
  drop(store);
  let mut store = LocalStore::open_at(&path, now).unwrap();
  assert_eq!(store.status().unwrap().quarantined_sample_count, 1);
  assert!(store.pending_batch("endpoint").unwrap().hours.is_empty());
  store.compact(future).unwrap();
  assert!(
    store.pending_batch("endpoint").unwrap().hours.is_empty(),
    "bad future snapshots must not reappear when their old date arrives"
  );
}

#[test]
fn first_future_inventory_is_revised_on_correction_and_publishes_after_reopen() {
  let root = tempfile::tempdir().unwrap();
  let path = root.path().join("profiles.sqlite3");
  let now = 10 * HOUR_MS + 10_000;
  let future = now + 2 * HOUR_MS;
  let mut store = LocalStore::open_at(&path, now).unwrap();
  store
    .record(inventory(future), sample(future, 0, 20.0, 0.0))
    .unwrap();
  store.compact(now).unwrap();
  let status = store.status().unwrap();
  assert_eq!(status.quarantined_inventory_count, 1);
  assert_eq!(status.inventory_revision, 2);
  assert_eq!(
    store.profile().unwrap().unwrap().inventory.observed_at_ms,
    now
  );
  assert!(store.inventory(Some(1)).unwrap().is_none());
  store
    .record(inventory(now), sample(now, 0, 40.0, 0.0))
    .unwrap();
  drop(store);
  let mut store = LocalStore::open_at(&path, now).unwrap();
  let batch = store.pending_batch("endpoint").unwrap();
  assert_eq!(
    batch
      .inventories
      .iter()
      .map(|value| value.revision)
      .collect::<Vec<_>>(),
    [2]
  );
  assert!(
    batch
      .samples
      .iter()
      .all(|sample| sample.inventory_revision == 2)
  );
  let mut server = receiver();
  let ack = ingest(&mut server, &batch, now).unwrap();
  store.acknowledge("endpoint", &batch, &ack).unwrap();
  assert!(
    store
      .pending_batch("endpoint")
      .unwrap()
      .inventories
      .is_empty()
  );
  assert!(
    store
      .pending_batch("other-endpoint")
      .unwrap()
      .inventories
      .iter()
      .all(|value| value.revision != 1)
  );
}

#[test]
fn polluted_current_hour_is_rebuilt_with_a_fresh_revision_and_good_statistics() {
  let root = tempfile::tempdir().unwrap();
  let path = root.path().join("profiles.sqlite3");
  let now = 10 * HOUR_MS + 10_000;
  let mut store = LocalStore::open_at(&path, now).unwrap();
  store
    .record(inventory(now), sample(now, 10_000, 20.0, 100.0))
    .unwrap();
  let original = store.pending_batch("endpoint").unwrap();
  let mut server = receiver();
  let ack = ingest(&mut server, &original, now).unwrap();
  store.acknowledge("endpoint", &original, &ack).unwrap();
  let future = now + 20 * 60 * 1000;
  store
    .record(inventory(future), sample(future, 10_000, 90.0, 900.0))
    .unwrap();
  let polluted_revision = store.status().unwrap().sample_sequence;
  store.compact(now).unwrap();
  let batch = store.pending_batch("endpoint").unwrap();
  assert_eq!(batch.hours.len(), 1);
  assert!(batch.hours[0].revision > polluted_revision);
  assert_eq!(batch.hours[0].sample_count, 1);
  assert_eq!(
    batch.hours[0].metrics["cpu/usage_percent"].mean(),
    Some(20.0)
  );
  assert_eq!(
    batch.hours[0].metrics["network/test/received_bytes"].sum,
    100.0
  );
  let ack = ingest(&mut server, &batch, now).unwrap();
  store.acknowledge("endpoint", &batch, &ack).unwrap();
  assert!(store.pending_batch("endpoint").unwrap().hours.is_empty());
  let retained = history(
    &server,
    &query(
      store.machine_id(),
      ProfileResolution::Hourly,
      0,
      11 * HOUR_MS,
      100,
    ),
  )
  .unwrap();
  assert_eq!(
    retained.hours[0].metrics["cpu/usage_percent"].mean(),
    Some(20.0)
  );
}

#[test]
fn clock_repair_and_new_invalid_record_roll_back_together() {
  let (_root, mut store) = local();
  let now = 10 * HOUR_MS;
  store
    .record(
      inventory(now + 2 * HOUR_MS),
      sample(now + 2 * HOUR_MS, 0, 20.0, 0.0),
    )
    .unwrap();
  let mut invalid_sample = sample(now, 0, 40.0, 0.0);
  invalid_sample
    .metrics
    .get_mut("cpu/usage_percent")
    .unwrap()
    .value = f64::NAN;
  assert!(store.record(inventory(now), invalid_sample).is_err());
  assert_eq!(store.status().unwrap().quarantined_sample_count, 0);
  assert_eq!(store.status().unwrap().sample_sequence, 1);
  store.compact(now).unwrap();
  assert_eq!(store.status().unwrap().quarantined_sample_count, 1);
}

#[test]
fn legal_large_hour_uses_larger_budget_and_oversized_derivation_rolls_back() {
  let (_root, mut store) = local();
  let mut reading = sample(10_000, 0, 20.0, 0.0);
  reading.metrics.clear();
  for metric in 0..2048 {
    reading.metrics.insert(
      format!("metric/{metric:04}/{}", "x".repeat(235)),
      MetricReading {
        kind: MetricKind::Gauge,
        value: 1.0,
      },
    );
  }
  let full_metrics = reading.metrics.clone();
  store.record(inventory(10_000), reading).unwrap();
  // A retained snapshot produced before conservative growth admission remains
  // valid and must be publishable under the expanded packet cap.
  let payload: String = sql(store.connection.query_row(
    "SELECT payload FROM system_hours WHERE hour_start_ms=0",
    [],
    |row| row.get(0),
  ))
  .unwrap();
  let mut legacy: SystemHour = decode(&payload).unwrap();
  legacy.metrics = full_metrics
    .into_iter()
    .map(|(name, reading)| {
      (
        name,
        MetricAggregate {
          kind: reading.kind,
          sample_count: 1,
          coverage_ms: 0,
          sum: 0.0,
          min: reading.value,
          max: reading.value,
          last: reading.value,
          last_at_ms: 10_000,
        },
      )
    })
    .collect();
  legacy.warnings.clear();
  validate_hour(&legacy).unwrap();
  sql(store.connection.execute(
    "UPDATE system_hours SET payload=?1 WHERE hour_start_ms=0",
    [json(&legacy).unwrap()],
  ))
  .unwrap();
  let batch = store.pending_batch("endpoint").unwrap();
  assert!(serde_json::to_vec(&batch).unwrap().len() <= MAX_PROFILE_BATCH_BYTES);
  assert!(!batch.samples.is_empty());
  let mut server = receiver();
  let ack = ingest(&mut server, &batch, 10_000).unwrap();
  store.acknowledge("endpoint", &batch, &ack).unwrap();
  let remainder = store.pending_batch("endpoint").unwrap();
  assert_eq!(
    remainder.hours.len(),
    1,
    "fitting inventory/sample prefix must advance to the larger summary next"
  );
  assert!(serde_json::to_vec(&remainder).unwrap().len() > 700 * 1024);
  ingest(&mut server, &remainder, 10_000).unwrap();
  let before = store.status().unwrap();
  let mut too_large = sample(20_000, 0, 20.0, 0.0);
  too_large.metrics.clear();
  for metric in 0..2048 {
    too_large.metrics.insert(
      format!("different/{metric:04}/{}", "y".repeat(235)),
      MetricReading {
        kind: MetricKind::Gauge,
        value: 1.0,
      },
    );
    too_large
      .counters
      .insert(format!("counter/{metric:04}/{}", "z".repeat(230)), u64::MAX);
  }
  let result = store.record(inventory(20_000), too_large);
  assert!(
    result.is_err(),
    "oversized raw record unexpectedly committed"
  );
  let Err(error) = result else { unreachable!() };
  assert_eq!(code(error), 413);
  assert_eq!(
    store.status().unwrap().sample_sequence,
    before.sample_sequence
  );
}

#[test]
fn five_minute_skew_can_cross_utc_hour_without_rejecting_its_summary() {
  let (_root, mut store) = local();
  let now = HOUR_MS - 30_000;
  let point = HOUR_MS + 30_000;
  store
    .record(inventory(point), sample(point, 0, 20.0, 0.0))
    .unwrap();
  let mut server = receiver();
  ingest(&mut server, &store.pending_batch("endpoint").unwrap(), now).unwrap();
}

#[test]
fn hourly_device_churn_is_bounded_and_existing_metrics_keep_updating() {
  let (_root, mut store) = local();
  for tick in 1..=3 {
    let mut point = sample(tick * 10_000, 10_000, 20.0, 0.0);
    point.metrics.clear();
    for device in 0..1000 {
      point.metrics.insert(
        format!("network/generation-{tick}/device-{device}/bytes"),
        MetricReading {
          kind: MetricKind::CounterDelta,
          value: 10.0,
        },
      );
    }
    point.metrics.insert(
      "persistent/usage".into(),
      MetricReading {
        kind: MetricKind::Gauge,
        value: tick as f64,
      },
    );
    let recorded = store.record(inventory(tick * 10_000), point).unwrap();
    if tick == 3 {
      assert!(
        recorded
          .warnings
          .iter()
          .any(|warning| warning.contains("aggregation limit"))
      );
    }
  }
  let hours = history(
    &store.connection,
    &query(
      store.machine_id(),
      ProfileResolution::Hourly,
      0,
      HOUR_MS,
      100,
    ),
  )
  .unwrap()
  .hours;
  assert!(hours[0].metrics.len() <= MAX_METRICS);
  assert_eq!(hours[0].metrics["persistent/usage"].sample_count, 3);
  assert_eq!(hours[0].metrics["persistent/usage"].last, 3.0);
  assert!(
    hours[0]
      .warnings
      .iter()
      .any(|warning| warning.contains("aggregation limit"))
  );
  assert_eq!(store.status().unwrap().detail_count, 3);
}

#[test]
fn admitted_hourly_keys_have_reserved_numeric_width_and_keep_updating() {
  let (_root, mut store) = local();
  let mut first = sample(10_000, 10_000, 20.0, 0.0);
  first.metrics.clear();
  for key in 0..2048 {
    first.metrics.insert(
      format!("metric/{key}/{}", "x".repeat(220)),
      MetricReading {
        kind: MetricKind::Gauge,
        value: 1.0,
      },
    );
  }
  let first = store.record(inventory(10_000), first).unwrap();
  assert!(!first.warnings.is_empty());
  let initial = history(
    &store.connection,
    &query(
      store.machine_id(),
      ProfileResolution::Hourly,
      0,
      HOUR_MS,
      100,
    ),
  )
  .unwrap()
  .hours
  .remove(0);
  let mut next = sample(20_000, 10_000, 20.0, 0.0);
  next.metrics.clear();
  for name in initial.metrics.keys() {
    next.metrics.insert(
      name.clone(),
      MetricReading {
        kind: MetricKind::Gauge,
        value: 1.2345678901234567e200,
      },
    );
  }
  let next = store.record(inventory(20_000), next).unwrap();
  assert!(next.warnings.is_empty());
  let updated = history(
    &store.connection,
    &query(
      store.machine_id(),
      ProfileResolution::Hourly,
      0,
      HOUR_MS,
      100,
    ),
  )
  .unwrap()
  .hours
  .remove(0);
  assert_eq!(updated.metrics.len(), initial.metrics.len());
  assert!(
    updated
      .metrics
      .values()
      .all(|metric| metric.sample_count == 2)
  );
  assert!(serde_json::to_vec(&updated).unwrap().len() <= MAX_PROFILE_RECORD_BYTES);
}

#[test]
fn corrected_inventory_resequences_raw_when_server_commit_ack_was_lost() {
  let (_root, mut store) = local();
  let now = 10 * HOUR_MS + 10_000;
  store
    .record(inventory(now), sample(now, 10_000, 20.0, 100.0))
    .unwrap();
  let original = store.pending_batch("endpoint").unwrap();
  let mut server = receiver();
  ingest(&mut server, &original, now).unwrap();
  // Simulate a retained legacy inventory with a clock-poisoned timestamp. The
  // original raw identity has reached the server, but its ACK was lost.
  let mut legacy = original.inventories[0].clone();
  legacy.inventory.observed_at_ms = now + 2 * HOUR_MS;
  sql(store.connection.execute(
    "UPDATE system_inventories SET payload=?1 WHERE revision=1",
    [json(&legacy).unwrap()],
  ))
  .unwrap();
  store.compact(now).unwrap();
  let repaired = store.pending_batch("endpoint").unwrap();
  assert_eq!(
    repaired
      .inventories
      .iter()
      .map(|inventory| inventory.revision)
      .collect::<Vec<_>>(),
    [2]
  );
  assert_eq!(repaired.samples.len(), 1);
  assert!(repaired.samples[0].sequence > original.samples[0].sequence);
  assert_eq!(repaired.samples[0].inventory_revision, 2);
  assert_eq!(repaired.hours[0].sample_count, 1);
  assert_eq!(
    repaired.hours[0].metrics["network/test/received_bytes"].sum,
    100.0
  );
  let ack = ingest(&mut server, &repaired, now).unwrap();
  store.acknowledge("endpoint", &repaired, &ack).unwrap();
  assert!(store.pending_batch("endpoint").unwrap().samples.is_empty());
  let raw = history(
    &store.connection,
    &query(
      store.machine_id(),
      ProfileResolution::Detail,
      0,
      11 * HOUR_MS,
      100,
    ),
  )
  .unwrap();
  assert_eq!(raw.samples.len(), 1);
  assert_eq!(store.status().unwrap().quarantined_sample_count, 1);
  let summaries = history(
    &server,
    &query(
      store.machine_id(),
      ProfileResolution::Hourly,
      0,
      11 * HOUR_MS,
      100,
    ),
  )
  .unwrap();
  assert_eq!(
    summaries.hours[0].sample_count, 1,
    "replacement raw identity must not double authoritative summaries"
  );
  assert_eq!(
    summaries.hours[0].metrics["network/test/received_bytes"].sum,
    100.0
  );
}
