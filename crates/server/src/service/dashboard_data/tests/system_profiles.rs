//! Exercise the browser cap after the existing byte-bounded SQLite readers.
use super::*;
use crate::dashboard::DashboardView;
use crate::service::store::tests::MockStorage;
use expri_core::system_profile::*;

fn inventory(at: i64) -> SystemInventory {
  SystemInventory {
    observed_at_ms: at,
    hostname: "monitor-test".into(),
    os_name: None,
    os_version: None,
    kernel_version: None,
    architecture: "x86_64".into(),
    cpu: CpuInventory {
      brand: "CPU".into(),
      vendor: "vendor".into(),
      logical_cpu_count: 4,
      physical_core_count: None,
    },
    memory_total_bytes: 1024,
    measurement_scope: "host".into(),
    devices: Vec::new(),
    warnings: Vec::new(),
  }
}

fn batch(machine_id: &str, at: i64) -> ProfileBatch {
  ProfileBatch {
    schema_version: PROFILE_SCHEMA_VERSION,
    machine_id: machine_id.into(),
    inventories: vec![InventoryRevision {
      revision: 1,
      inventory: inventory(at),
    }],
    samples: Vec::new(),
    hours: Vec::new(),
  }
}

fn sample(sequence: u64, at: i64, key_length: usize) -> SystemSample {
  SystemSample {
    sequence,
    session_id: "session-test".into(),
    observed_at_ms: at,
    interval_ms: 0,
    collection_duration_ms: 0,
    inventory_revision: 1,
    metrics: (0..2048)
      .map(|key| {
        (
          format!("metric/{key}/{}", "x".repeat(key_length)),
          MetricReading {
            kind: MetricKind::Gauge,
            value: 1.0,
          },
        )
      })
      .collect(),
    counters: BTreeMap::new(),
    warnings: Vec::new(),
  }
}

#[test]
fn sqlite_history_pages_remain_byte_bounded_before_browser_truncation_and_never_skip_rows() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let at = chrono::Utc::now().timestamp_millis();
  for sequence in 1..=5 {
    let mut batch = batch("machine-history", at);
    batch
      .samples
      .push(sample(sequence, at + sequence as i64, 150));
    store.execute(Request::PutSystemProfile { batch }).unwrap();
  }
  let dashboard = HostedDashboard::new(&store).unwrap();
  let selection = ProfileHistoryQuery {
    machine_id: "machine-history".into(),
    start_ms: at,
    end_ms: at + 10,
    resolution: ProfileResolution::Detail,
    cursor: None,
    limit: 1024,
  };
  let source = dashboard.system_history(&selection).unwrap().unwrap();
  assert!(
    source.samples.len() < 5,
    "SQLite loader stops at its 900 KiB budget before materializing every requested row"
  );
  assert!(serde_json::to_vec(&source).unwrap().len() < 900 * 1024);
  let mut cursor = None;
  let mut sequences = Vec::new();
  loop {
    let mut path = format!(
      "/api/system-history?machine_id=machine-history&start_ms={at}&end_ms={}&limit=1024",
      at + 10
    );
    if let Some(cursor) = cursor {
      path.push_str(&format!("&cursor={cursor}"));
    }
    let reply = crate::dashboard::server::route_content(&dashboard, &path);
    assert_eq!(reply.status, 200);
    assert!(reply.body.len() <= 512 * 1024);
    let page: ProfileHistory = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(
      page.samples.len(),
      1,
      "wide valid rows fit one per browser page"
    );
    sequences.extend(page.samples.iter().map(|sample| sample.sequence));
    cursor = page.next_cursor;
    if cursor.is_none() {
      break;
    }
  }
  assert_eq!(sequences, vec![1, 2, 3, 4, 5]);
  let mut oversized = batch("machine-oversize", at);
  oversized.samples.push(sample(1, at, 240));
  validate_profile_batch(&oversized).unwrap();
  store
    .execute(Request::PutSystemProfile { batch: oversized })
    .unwrap();
  let reply = crate::dashboard::server::route_content(
    &dashboard,
    &format!(
      "/api/system-history?machine_id=machine-oversize&start_ms={at}&end_ms={}&limit=1024",
      at + 1
    ),
  );
  assert_eq!(
    reply.status, 413,
    "one valid storage row above the browser cap is explicit"
  );
}

#[test]
fn sqlite_inventory_pages_remain_byte_bounded_and_browser_cursors_cover_every_machine() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let at = chrono::Utc::now().timestamp_millis();
  for machine_id in [
    "machine-a",
    "machine-b",
    "machine-c",
    "machine-d",
    "machine-e",
  ] {
    let mut batch = batch(machine_id, at);
    batch.inventories[0].inventory.devices = (0..32)
      .map(|device| DeviceInventory {
        device_id: format!("disk-{device}"),
        kind: DeviceKind::Disk,
        name: "disk".into(),
        total_bytes: None,
        attributes: (0..8)
          .map(|key| (format!("attribute-{key}"), "x".repeat(1024)))
          .collect(),
      })
      .collect();
    store.execute(Request::PutSystemProfile { batch }).unwrap();
  }
  let dashboard = HostedDashboard::new(&store).unwrap();
  let source = dashboard.system_machines(100, None).unwrap().unwrap();
  assert!(
    source.machines.len() < 5,
    "SQLite inventory loader already caps materialized pages at 900 KiB"
  );
  assert!(serde_json::to_vec(&source).unwrap().len() < 900 * 1024);
  let mut cursor = None;
  let mut identities = Vec::new();
  loop {
    let path = cursor.as_ref().map_or_else(
      || "/api/system-machines?limit=100".to_string(),
      |cursor| format!("/api/system-machines?limit=100&cursor={cursor}"),
    );
    let reply = crate::dashboard::server::route_content(&dashboard, &path);
    assert_eq!(reply.status, 200);
    assert!(reply.body.len() <= 512 * 1024);
    let page: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(page["machines"].as_array().unwrap().len(), 1);
    identities.extend(
      page["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|machine| machine["machine_id"].as_str().unwrap().to_string()),
    );
    cursor = page["next_cursor"].as_str().map(str::to_string);
    if cursor.is_none() {
      break;
    }
  }
  assert_eq!(
    identities,
    [
      "machine-a",
      "machine-b",
      "machine-c",
      "machine-d",
      "machine-e"
    ]
  );
}
