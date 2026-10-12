use super::*;
use expri_core::system_profile::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;

fn dashboard(root: &std::path::Path, database: &std::path::Path) -> Dashboard {
  let config = root.join("expri.toml");
  fs::write(&config, "[project]\nname = 'monitor-test'\n").unwrap();
  let mut dashboard = Dashboard::new(
    crate::context::CommandContext {
      config: crate::config::Config::load(&config).unwrap(),
      repo_root: root.into(),
      project_name: Some("monitor-test".into()),
    },
    None,
  )
  .unwrap();
  dashboard.system_profiles_path = Some(database.into());
  dashboard
}

fn record(store: &mut LocalStore, at: i64) {
  store
    .record(
      SystemInventory {
        observed_at_ms: at,
        hostname: "local-machine".into(),
        os_name: None,
        os_version: None,
        kernel_version: None,
        architecture: "x86_64".into(),
        cpu: CpuInventory {
          brand: "CPU".into(),
          vendor: "vendor".into(),
          logical_cpu_count: 4,
          physical_core_count: Some(2),
        },
        memory_total_bytes: 1024,
        measurement_scope: "host".into(),
        devices: Vec::new(),
        warnings: Vec::new(),
      },
      SystemSample {
        sequence: 0,
        session_id: "session-test".into(),
        observed_at_ms: at,
        interval_ms: 0,
        collection_duration_ms: 0,
        inventory_revision: 0,
        metrics: BTreeMap::from([(
          "memory/used_bytes".into(),
          MetricReading {
            kind: MetricKind::Gauge,
            value: 500.0,
          },
        )]),
        counters: BTreeMap::new(),
        warnings: Vec::new(),
      },
    )
    .unwrap();
}

#[test]
fn missing_local_monitor_is_read_only_and_validation_precedes_unsupported_response() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("monitor/profiles.sqlite3");
  let dashboard = dashboard(directory.path(), &database);
  for path in [
    "/api/system-machines",
    "/api/system-profile?machine_id=machine-test",
    "/api/system-history?machine_id=machine-test&start_ms=0&end_ms=10",
  ] {
    assert_eq!(
      super::super::server::route_content(&dashboard, path).status,
      404
    );
  }
  assert!(
    !database.parent().unwrap().exists(),
    "dashboard reads create no global files"
  );
  for path in [
    "/api/system-machines?limit=0",
    "/api/system-machines?limit=101",
    "/api/system-machines?cursor=..",
    "/api/system-machines?cursor=",
    "/api/system-machines?limit=1&limit=1",
    "/api/system-machines?source=local",
    "/api/system-profile",
    "/api/system-profile?machine_id=..",
    "/api/system-profile?machine_id=machine-test&inventory_revision=0",
    "/api/system-profile?machine_id=machine-test&inventory_revision=9223372036854775808",
    "/api/system-history?machine_id=machine-test&start_ms=0&end_ms=0",
    "/api/system-history?machine_id=machine-test&start_ms=-1&end_ms=10",
    "/api/system-history?machine_id=machine-test&start_ms=%2B1&end_ms=10",
    "/api/system-history?machine_id=machine-test&start_ms=0&end_ms=10&cursor=-1",
    "/api/system-history?machine_id=machine-test&start_ms=0&end_ms=10&limit=1025",
    "/api/system-history?machine_id=machine-test&start_ms=0&end_ms=10&resolution=auto",
    "/api/system-history?machine_id=machine-test&start_ms=0&end_ms=10&token=secret",
  ] {
    assert_eq!(
      super::super::server::route_content(&dashboard, path).status,
      400,
      "{path}"
    );
  }
}

#[test]
fn local_monitor_lists_existing_identity_and_history_keeps_exclusive_end_and_cursor() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("monitor/profiles.sqlite3");
  let mut store = LocalStore::open(&database).unwrap();
  let at = chrono::Utc::now().timestamp_millis();
  record(&mut store, at);
  record(&mut store, at + 1);
  record(&mut store, at + 2);
  let machine_id = store.machine_id().to_string();
  drop(store);
  let before = fs::read(&database).unwrap();
  let dashboard = dashboard(directory.path(), &database);
  let reply = super::super::server::route_content(&dashboard, "/api/system-machines?limit=1");
  assert_eq!(reply.status, 200);
  let page: Value = serde_json::from_slice(&reply.body).unwrap();
  assert_eq!(page["machines"][0]["machine_id"], machine_id);
  assert_eq!(page["next_cursor"], Value::Null);
  let path = format!("/api/system-profile?machine_id={machine_id}&inventory_revision=1");
  let reply = super::super::server::route_content(&dashboard, &path);
  assert_eq!(reply.status, 200);
  let profile: Value = serde_json::from_slice(&reply.body).unwrap();
  assert_eq!(profile["inventory"]["hostname"], "local-machine");
  let path = format!(
    "/api/system-history?machine_id={machine_id}&start_ms={at}&end_ms={}&limit=1",
    at + 2
  );
  let reply = super::super::server::route_content(&dashboard, &path);
  assert_eq!(reply.status, 200);
  let history: Value = serde_json::from_slice(&reply.body).unwrap();
  assert_eq!(history["samples"][0]["sequence"], 1);
  assert_eq!(history["next_cursor"], 1);
  let reply = super::super::server::route_content(&dashboard, &format!("{path}&cursor=1"));
  let history: Value = serde_json::from_slice(&reply.body).unwrap();
  assert_eq!(history["samples"][0]["sequence"], 2);
  assert!(history["next_cursor"].is_null());
  assert_eq!(
    fs::read(&database).unwrap(),
    before,
    "all dashboard profile operations stay read-only"
  );
  for path in [
    "/api/system-profile?machine_id=machine-other".to_string(),
    format!("/api/system-profile?machine_id={machine_id}&inventory_revision=99"),
    format!(
      "/api/system-history?machine_id=machine-other&start_ms={at}&end_ms={}",
      at + 2
    ),
  ] {
    assert_eq!(
      super::super::server::route_content(&dashboard, &path).status,
      404
    );
  }
  let reply = super::super::server::route_content(
    &dashboard,
    &format!("/api/system-machines?cursor={machine_id}"),
  );
  assert_eq!(
    serde_json::from_slice::<Value>(&reply.body).unwrap(),
    json!({"machines":[],"next_cursor":null})
  );
}

#[test]
fn system_routes_retain_the_local_host_and_origin_boundary() {
  let directory = tempfile::tempdir().unwrap();
  let dashboard = dashboard(directory.path(), &directory.path().join("missing.sqlite3"));
  for path in [
    "/api/system-machines",
    "/api/system-profile?machine_id=machine-test",
    "/api/system-history?machine_id=machine-test&start_ms=0&end_ms=10",
  ] {
    for headers in [
      vec![("Host", "attacker.invalid")],
      vec![
        ("Host", "127.0.0.1:8765"),
        ("Origin", "https://attacker.invalid"),
      ],
    ] {
      assert_eq!(
        super::super::server::route(&dashboard, "127.0.0.1:8765", "GET", path, &headers).status,
        403
      );
    }
  }
}
