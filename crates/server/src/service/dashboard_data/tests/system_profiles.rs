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

fn publish_run_state(store: &Store<MockStorage>, scope: &RunScope, state: &Value) {
  let bytes = serde_json::to_vec(state).unwrap();
  store
    .execute(Request::PutDocument {
      scope: scope.clone(),
      path: "run-state.json".into(),
      revision: 1,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

fn append_capture(
  store: &Store<MockStorage>,
  scope: &RunScope,
  offset: u64,
  records: &[expri_core::run_system::RunSystemRecord],
) -> u64 {
  let mut bytes = Vec::new();
  for record in records {
    bytes.extend(serde_json::to_vec(record).unwrap());
    bytes.push(b'\n');
  }
  for (index, batch) in bytes.chunks(STREAM_BATCH).enumerate() {
    store
      .execute(Request::AppendTracking {
        scope: scope.clone(),
        path: expri_core::run_system::RUN_SYSTEM_PATH.into(),
        offset: offset + (index * STREAM_BATCH) as u64,
        data_base64: base64::engine::general_purpose::STANDARD.encode(batch),
      })
      .unwrap();
  }
  offset + bytes.len() as u64
}

#[test]
fn retained_run_capture_is_independent_of_global_retention_and_has_its_own_refresh_revision() {
  use expri_core::run_system::RunSystemRecord as Record;
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let scope = RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-captured".into(),
  };
  let state = json!({"schema_version":1,"run_id":scope.run_id,"machine_id":"machine-test","system_capture_id":"capture-test","system_capture_status":"complete","task":"train","status":"completed","started_at":"1970-01-01T00:00:10Z","finished_at":"1970-01-01T00:00:20Z","exit_code":0});
  publish_run_state(&store, &scope, &state);
  let dashboard = HostedDashboard::new(&store).unwrap();
  let before = dashboard
    .updates("hosted:project:worker", std::slice::from_ref(&scope.run_id))
    .unwrap();
  let mut raw = sample(999, 11_000, 0);
  raw.metrics = BTreeMap::from([(
    "cpu/usage_percent".into(),
    MetricReading {
      kind: MetricKind::Gauge,
      value: 37.0,
    },
  )]);
  let records = vec![
    Record::Capture {
      schema_version: 1,
      capture_id: "capture-test".into(),
      machine_id: "machine-test".into(),
      start_ms: 10_000,
    },
    Record::Inventory {
      inventory: InventoryRevision {
        revision: 1,
        inventory: inventory(10_000),
      },
    },
    Record::Sample {
      sample: raw.clone(),
    },
    Record::End {
      end_ms: 20_000,
      complete: true,
      warnings: Vec::new(),
    },
  ];
  append_capture(&store, &scope, 0, &records);
  let after = dashboard
    .updates("hosted:project:worker", std::slice::from_ref(&scope.run_id))
    .unwrap();
  assert!(before["runs"][0]["system_revision"].is_null());
  assert!(after["runs"][0]["system_revision"].is_string());
  assert_eq!(
    before["runs"][0]["metrics_revision"],
    after["runs"][0]["metrics_revision"]
  );
  let response = crate::dashboard::server::route_content(
    &dashboard,
    "/api/run-system?source=hosted-project:project&run_id=worker:run-captured&limit=1",
  );
  assert_eq!(response.status, 200);
  let result: expri_runs::system_metrics::RunSystemMetrics =
    serde_json::from_slice(&response.body).unwrap();
  assert_eq!(result.run_id, "worker:run-captured");
  assert_eq!(result.status, "complete");
  assert_eq!(result.samples[0], raw);
  assert_eq!(result.metric_summaries["cpu/usage_percent"].last, 37.0);
  assert!(
    store.system_machine("machine-test", None).is_err(),
    "no global machine history exists or is needed for this old retained capture"
  );
}

#[test]
fn missing_and_spoofed_run_capture_metadata_never_falls_back_to_global_machine_history() {
  use expri_core::run_system::RunSystemRecord as Record;
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let at = chrono::Utc::now().timestamp_millis();
  let mut profile = batch("machine-test", at);
  profile.samples.push(sample(1, at, 0));
  store
    .execute(Request::PutSystemProfile { batch: profile })
    .unwrap();
  let scope = RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-legacy".into(),
  };
  publish_run_state(
    &store,
    &scope,
    &json!({"schema_version":1,"run_id":scope.run_id,"machine_id":"machine-test","task":"train","status":"running","started_at":"1970-01-01T00:00:10Z"}),
  );
  let dashboard = HostedDashboard::new(&store).unwrap();
  let legacy = dashboard
    .run_system("hosted:project:worker", &scope.run_id, None, 100)
    .unwrap();
  assert_eq!(legacy.status, "unavailable");
  assert!(legacy.samples.is_empty());
  assert!(legacy.metric_summaries.is_empty());
  let scope = RunScope {
    run_id: "run-spoof".into(),
    ..scope
  };
  publish_run_state(
    &store,
    &scope,
    &json!({"schema_version":1,"run_id":scope.run_id,"machine_id":"machine-test","system_capture_id":"capture-test","task":"train","status":"running","started_at":"1970-01-01T00:00:10Z"}),
  );
  append_capture(
    &store,
    &scope,
    0,
    &[Record::Capture {
      schema_version: 1,
      capture_id: "capture-test".into(),
      machine_id: "machine-other".into(),
      start_ms: 10_000,
    }],
  );
  let response = crate::dashboard::server::route_content(
    &dashboard,
    "/api/run-system?source=hosted:project:worker&run_id=run-spoof",
  );
  assert_eq!(response.status, 409);
  for query in [
    "cursor=-1",
    "cursor=9223372036854775808",
    "limit=0",
    "limit=1025",
    "token=secret",
  ] {
    assert_eq!(
      crate::dashboard::server::route_content(
        &dashboard,
        &format!("/api/run-system?source=hosted:project:worker&run_id=run-spoof&{query}")
      )
      .status,
      400
    );
  }
}

#[test]
fn overview_schema_two_is_reparsed_for_capture_metadata_without_worker_republication() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let scope = RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-capture-meta".into(),
  };
  let state = json!({"schema_version":1,"run_id":scope.run_id,"machine_id":"machine-test","system_capture_id":"capture-test","system_capture_status":"incomplete","system_capture_error":"collector stopped","task":"train","status":"running","started_at":"1970-01-01T00:00:10Z"});
  publish_run_state(&store, &scope, &state);
  store.dashboard_cache_overview(&scope,"tracking:1",&json!({"overview_schema":2,"run":{"run_id":scope.run_id,"machine_id":"machine-test","task":"train","status":"running","started_at":state["started_at"]},"task_filter":"train","metadata_truncated":false,"warnings":[]})).unwrap();
  let dashboard = HostedDashboard::new(&store).unwrap();
  let result = dashboard
    .list("hosted:project:worker", None, None, None, 100, 0)
    .unwrap();
  assert_eq!(result["runs"][0]["system_capture_id"], "capture-test");
  assert_eq!(result["runs"][0]["system_capture_status"], "incomplete");
  assert_eq!(
    result["runs"][0]["system_capture_error"],
    "collector stopped"
  );
  assert_eq!(
    store
      .dashboard_cached_overview(&scope, "tracking:1")
      .unwrap()
      .unwrap()["overview_schema"],
    OVERVIEW_SCHEMA_VERSION
  );
}
