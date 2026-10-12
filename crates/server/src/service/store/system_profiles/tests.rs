use super::*;
use crate::service::store::tests::MockStorage;
use expri_core::system_profile::*;

fn batch(at_ms: i64) -> ProfileBatch {
  ProfileBatch {
    schema_version: PROFILE_SCHEMA_VERSION,
    machine_id: "machine-test".into(),
    inventories: vec![InventoryRevision {
      revision: 1,
      inventory: SystemInventory {
        observed_at_ms: at_ms,
        hostname: "worker-test".into(),
        os_name: Some("Linux".into()),
        os_version: None,
        kernel_version: None,
        architecture: "x86_64".into(),
        cpu: CpuInventory {
          brand: "CPU".into(),
          vendor: "vendor".into(),
          logical_cpu_count: 8,
          physical_core_count: Some(4),
        },
        memory_total_bytes: 16 * 1024 * 1024 * 1024,
        measurement_scope: "host".into(),
        devices: Vec::new(),
        warnings: Vec::new(),
      },
    }],
    samples: vec![SystemSample {
      sequence: 1,
      session_id: "session-test".into(),
      observed_at_ms: at_ms,
      interval_ms: 10_000,
      collection_duration_ms: 1,
      inventory_revision: 1,
      metrics: BTreeMap::from([(
        "cpu/usage_percent".into(),
        MetricReading {
          kind: MetricKind::Gauge,
          value: 20.0,
        },
      )]),
      counters: BTreeMap::new(),
      warnings: Vec::new(),
    }],
    hours: vec![SystemHour {
      hour_start_ms: at_ms / HOUR_MS * HOUR_MS,
      revision: 1,
      sample_count: 1,
      first_sample_at_ms: at_ms,
      last_sample_at_ms: at_ms,
      inventory_revisions: vec![1],
      warnings: Vec::new(),
      metrics: BTreeMap::from([(
        "cpu/usage_percent".into(),
        MetricAggregate {
          kind: MetricKind::Gauge,
          sample_count: 1,
          coverage_ms: 10_000,
          sum: 200_000.0,
          min: 20.0,
          max: 20.0,
          last: 20.0,
          last_at_ms: at_ms,
        },
      )]),
    }],
  }
}

fn query(at_ms: i64, resolution: ProfileResolution) -> ProfileHistoryQuery {
  ProfileHistoryQuery {
    machine_id: "machine-test".into(),
    start_ms: at_ms / HOUR_MS * HOUR_MS,
    end_ms: at_ms + HOUR_MS,
    resolution,
    cursor: None,
    limit: 100,
  }
}

#[test]
fn retries_and_stale_hours_are_idempotent_and_conflicts_roll_back() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let at_ms = 100 * HOUR_MS + 10_000;
  let original = batch(at_ms);
  let first = profiles::ingest(&mut store.db().unwrap(), &original, at_ms).unwrap();
  let retry = profiles::ingest(&mut store.db().unwrap(), &original, at_ms).unwrap();
  assert_eq!(first, retry);
  assert_eq!(
    store
      .system_history_at(&query(at_ms, ProfileResolution::Detail), at_ms)
      .unwrap()
      .samples
      .len(),
    1
  );

  let mut newer = original.clone();
  newer.samples.clear();
  newer.inventories.clear();
  newer.hours[0].revision = 2;
  newer.hours[0].sample_count = 2;
  profiles::ingest(&mut store.db().unwrap(), &newer, at_ms).unwrap();
  profiles::ingest(&mut store.db().unwrap(), &original, at_ms).unwrap();
  let history = store
    .system_history_at(&query(at_ms, ProfileResolution::Hourly), at_ms)
    .unwrap();
  assert_eq!(history.hours[0].revision, 2);
  assert_eq!(history.hours[0].sample_count, 2);

  let mut conflicting = original.clone();
  conflicting.samples[0]
    .metrics
    .get_mut("cpu/usage_percent")
    .unwrap()
    .value = 99.0;
  conflicting.inventories[0].revision = 2;
  conflicting.inventories[0].inventory.hostname = "must-roll-back".into();
  assert!(matches!(
    profiles::ingest(&mut store.db().unwrap(), &conflicting, at_ms),
    Err(crate::error::ExpriError::ServiceRejected { status: 409, .. })
  ));
  assert_eq!(
    store
      .system_machine("machine-test", Some(2))
      .unwrap_err()
      .status,
    404
  );
  assert_eq!(
    store
      .system_machine("machine-test", None)
      .unwrap()
      .inventory
      .hostname,
    "worker-test"
  );
}

#[test]
fn floating_point_wire_retries_preserve_bits_without_duplicating_hourly_totals() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let at_ms = now_ms();
  let value = 4.264386931886279e-8;
  let mut transmitted = batch(at_ms);
  transmitted.samples[0]
    .metrics
    .get_mut("cpu/usage_percent")
    .unwrap()
    .value = value;
  let aggregate = transmitted.hours[0]
    .metrics
    .get_mut("cpu/usage_percent")
    .unwrap();
  aggregate.sum = value * aggregate.coverage_ms as f64;
  aggregate.min = value;
  aggregate.max = value;
  aggregate.last = value;
  let expected_sum_bits = aggregate.sum.to_bits();

  for _ in 0..4 {
    let bytes = serde_json::to_vec(&Request::PutSystemProfile { batch: transmitted }).unwrap();
    let operation: Request = serde_json::from_slice(&bytes).unwrap();
    let Request::PutSystemProfile { batch } = &operation else {
      unreachable!()
    };
    assert_eq!(
      batch.samples[0].metrics["cpu/usage_percent"]
        .value
        .to_bits(),
      value.to_bits()
    );
    assert_eq!(
      batch.hours[0].metrics["cpu/usage_percent"].sum.to_bits(),
      expected_sum_bits
    );
    transmitted = batch.clone();
    let Response::SystemProfileAcknowledged { acknowledgement } = store.execute(operation).unwrap()
    else {
      panic!("expected profile acknowledgement")
    };
    assert_eq!(acknowledgement.sample_sequence, 1);
  }

  let detail = store
    .system_history_at(&query(at_ms, ProfileResolution::Detail), at_ms)
    .unwrap();
  assert_eq!(detail.samples.len(), 1);
  assert_eq!(
    detail.samples[0].metrics["cpu/usage_percent"]
      .value
      .to_bits(),
    value.to_bits()
  );
  let hours = store
    .system_history_at(&query(at_ms, ProfileResolution::Hourly), at_ms)
    .unwrap()
    .hours;
  assert_eq!(hours.len(), 1);
  assert_eq!(hours[0].sample_count, 1);
  assert_eq!(hours[0].metrics["cpu/usage_percent"].sample_count, 1);
  assert_eq!(
    hours[0].metrics["cpu/usage_percent"].sum.to_bits(),
    expected_sum_bits
  );
}

#[test]
fn expired_details_are_hidden_and_deleted_while_late_history_is_preserved() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let at_ms = 100 * HOUR_MS + 10_000;
  let original = batch(at_ms);
  profiles::ingest(&mut store.db().unwrap(), &original, at_ms).unwrap();
  let expired_at = at_ms + DETAIL_RETENTION_MS + 1;
  assert!(
    store
      .system_history_at(&query(at_ms, ProfileResolution::Detail), expired_at)
      .unwrap()
      .samples
      .is_empty()
  );
  assert_eq!(
    store.system_profile_retention_cycle_at(expired_at).unwrap(),
    1
  );
  assert_eq!(
    store.system_profile_retention_cycle_at(expired_at).unwrap(),
    0
  );
  let ack = profiles::ingest(&mut store.db().unwrap(), &original, expired_at).unwrap();
  assert_eq!(ack.sample_sequence, 1);
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row("SELECT COUNT(*) FROM system_samples", [], |row| row
        .get::<_, u64>(0))
      .unwrap(),
    0
  );
  assert_eq!(
    store
      .system_history_at(&query(at_ms, ProfileResolution::Hourly), expired_at)
      .unwrap()
      .hours
      .len(),
    1
  );
  assert_eq!(
    store
      .system_machine("machine-test", Some(1))
      .unwrap()
      .inventory
      .hostname,
    "worker-test"
  );
}

#[test]
fn machine_and_history_queries_are_bounded_and_missing_profiles_return_404() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  for limit in [0, MAX_SYSTEM_MACHINES + 1] {
    assert_eq!(
      store.list_system_machines(limit, None).unwrap_err().status,
      400
    );
  }
  assert_eq!(
    store
      .list_system_machines(1, Some("../escape"))
      .unwrap_err()
      .status,
    400
  );
  assert_eq!(
    store.system_machine("missing", None).unwrap_err().status,
    404
  );
  let at_ms = 100 * HOUR_MS + 10_000;
  let mut invalid = query(at_ms, ProfileResolution::Detail);
  invalid.limit = MAX_HISTORY_ROWS + 1;
  assert_eq!(
    store.system_history_at(&invalid, at_ms).unwrap_err().status,
    400
  );
  invalid.limit = 1;
  assert_eq!(
    store.system_history_at(&invalid, at_ms).unwrap_err().status,
    404
  );

  let mut original = batch(at_ms);
  original.samples.push(original.samples[0].clone());
  original.samples[1].sequence = 2;
  profiles::ingest(&mut store.db().unwrap(), &original, at_ms).unwrap();
  let mut selection = query(at_ms, ProfileResolution::Detail);
  selection.limit = 1;
  let first = store.system_history_at(&selection, at_ms).unwrap();
  assert_eq!(first.samples[0].sequence, 1);
  assert_eq!(first.next_cursor, Some(1));
  selection.cursor = first.next_cursor;
  let second = store.system_history_at(&selection, at_ms).unwrap();
  assert_eq!(second.samples[0].sequence, 2);
  assert_eq!(second.next_cursor, None);
}

#[test]
fn profile_size_rejections_preserve_413_without_creating_machine_records() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let mut oversized = batch(now_ms());
  oversized.samples.clear();
  oversized.hours.clear();
  oversized.inventories[0].inventory.devices = (0..25)
    .map(|index| DeviceInventory {
      device_id: format!("disk-{index}"),
      kind: DeviceKind::Disk,
      name: "disk".into(),
      total_bytes: Some(1024),
      attributes: (0..32)
        .map(|attribute| (format!("attribute-{attribute}"), "x".repeat(1024)))
        .collect(),
    })
    .collect();
  assert_eq!(
    store.put_system_profile(&oversized).unwrap_err().status,
    413
  );
  assert!(store.list_system_machines(1, None).unwrap().0.is_empty());
  oversized.schema_version = PROFILE_SCHEMA_VERSION + 1;
  assert_eq!(
    store.put_system_profile(&oversized).unwrap_err().status,
    400
  );
}

#[test]
fn old_service_schemas_upgrade_without_losing_existing_records_and_reopen() {
  for version in [5, 6] {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metadata.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TABLE preservation_marker(value TEXT); INSERT INTO preservation_marker VALUES('retained');").unwrap();
    connection
      .pragma_update(None, "user_version", version)
      .unwrap();
    drop(connection);
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    assert_eq!(
      store
        .db()
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get::<_, u64>(0))
        .unwrap(),
      METADATA_SCHEMA_VERSION as u64
    );
    assert_eq!(
      store
        .db()
        .unwrap()
        .query_row("SELECT value FROM preservation_marker", [], |row| row
          .get::<_, String>(0))
        .unwrap(),
      "retained"
    );
    store.put_system_profile(&batch(now_ms())).unwrap();
    drop(store);
    let reopened = Store::open(directory.path(), MockStorage::default()).unwrap();
    assert_eq!(
      reopened
        .system_machine("machine-test", None)
        .unwrap()
        .inventory
        .hostname,
      "worker-test"
    );
  }
  let directory = tempfile::tempdir().unwrap();
  let connection = Connection::open(directory.path().join("metadata.sqlite3")).unwrap();
  connection
    .pragma_update(None, "user_version", METADATA_SCHEMA_VERSION + 1)
    .unwrap();
  drop(connection);
  assert!(
    Store::open(directory.path(), MockStorage::default())
      .err()
      .unwrap()
      .to_string()
      .contains("newer")
  );
}

#[test]
fn retention_and_ingestion_can_run_concurrently_without_altering_hour_snapshots() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let at_ms = 100 * HOUR_MS + 10_000;
  let original = batch(at_ms);
  profiles::ingest(&mut store.db().unwrap(), &original, at_ms).unwrap();
  let expired_at = at_ms + DETAIL_RETENTION_MS + 1;
  std::thread::scope(|scope| {
    scope.spawn(|| {
      for revision in 2..=32 {
        let mut next = original.clone();
        next.samples[0].sequence = revision;
        next.hours[0].revision = revision;
        next.hours[0].sample_count = revision;
        profiles::ingest(&mut store.db().unwrap(), &next, at_ms).unwrap();
      }
    });
    scope.spawn(|| {
      for _ in 0..32 {
        store.system_profile_retention_cycle_at(expired_at).unwrap();
      }
    });
  });
  store.system_profile_retention_cycle_at(expired_at).unwrap();
  assert!(
    store
      .system_history_at(&query(at_ms, ProfileResolution::Detail), expired_at)
      .unwrap()
      .samples
      .is_empty()
  );
  let history = store
    .system_history_at(&query(at_ms, ProfileResolution::Hourly), expired_at)
    .unwrap();
  assert_eq!(history.hours[0].revision, 32);
  assert_eq!(history.hours[0].sample_count, 32);
}
