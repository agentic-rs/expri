use super::*;
use expri_core::system_profile::*;
use serde_json::Value;
use std::collections::BTreeMap;

fn machine(id: &str) -> MachineProfile {
  MachineProfile {
    machine_id: id.into(),
    inventory_revision: 1,
    inventory: SystemInventory {
      observed_at_ms: 0,
      hostname: id.into(),
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
      devices: (0..32)
        .map(|index| DeviceInventory {
          device_id: format!("device-{index}"),
          kind: DeviceKind::Disk,
          name: "disk".into(),
          total_bytes: None,
          attributes: (0..8)
            .map(|key| (format!("key-{key}"), "x".repeat(1024)))
            .collect(),
        })
        .collect(),
      warnings: Vec::new(),
    },
    last_sample_at_ms: None,
    last_received_at_ms: None,
  }
}

#[test]
fn machine_byte_bound_resumes_after_last_returned_inventory() {
  let rows: Vec<_> = ["machine-a", "machine-b", "machine-c"]
    .into_iter()
    .map(machine)
    .collect();
  for row in &rows {
    validate_inventory(&row.inventory).unwrap();
  }
  let reply = machine_reply(
    crate::dashboard::SystemMachines {
      machines: rows.clone(),
      next_cursor: Some("machine-c".into()),
    },
    100,
  )
  .unwrap_or_else(|_| panic!("bounded page"));
  assert!(reply.body.len() <= JSON_LIMIT);
  let body: Value = serde_json::from_slice(&reply.body).unwrap();
  assert_eq!(body["machines"].as_array().unwrap().len(), 1);
  assert_eq!(body["next_cursor"], "machine-a");
  let next = machine_reply(
    crate::dashboard::SystemMachines {
      machines: rows[1..].to_vec(),
      next_cursor: None,
    },
    100,
  )
  .unwrap_or_else(|_| panic!("next page"));
  let body: Value = serde_json::from_slice(&next.body).unwrap();
  assert_eq!(body["machines"][0]["machine_id"], "machine-b");
  assert_eq!(body["next_cursor"], "machine-b");
}

fn sample(sequence: u64) -> SystemSample {
  SystemSample {
    sequence,
    session_id: "session-test".into(),
    observed_at_ms: sequence as i64,
    interval_ms: 1,
    collection_duration_ms: 0,
    inventory_revision: 1,
    metrics: (0..2048)
      .map(|index| {
        (
          format!("cpu/{}{index}", "x".repeat(80)),
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
fn history_byte_bound_and_row_bound_keep_durable_cursors() {
  let query = ProfileHistoryQuery {
    machine_id: "machine-test".into(),
    start_ms: 0,
    end_ms: 10,
    resolution: ProfileResolution::Detail,
    cursor: None,
    limit: 100,
  };
  let rows = vec![sample(1), sample(2), sample(3)];
  for row in &rows {
    validate_sample(row).unwrap();
  }
  let reply = history_reply(
    ProfileHistory {
      machine_id: query.machine_id.clone(),
      resolution: query.resolution,
      samples: rows,
      hours: Vec::new(),
      next_cursor: Some(3),
    },
    &query,
  )
  .unwrap_or_else(|_| panic!("bounded history"));
  assert!(reply.body.len() <= JSON_LIMIT);
  let body: Value = serde_json::from_slice(&reply.body).unwrap();
  let count = body["samples"].as_array().unwrap().len();
  assert!(count < 3 && count > 0);
  assert_eq!(body["next_cursor"], count as i64);
  let query = ProfileHistoryQuery {
    resolution: ProfileResolution::Hourly,
    limit: 1,
    ..query
  };
  let hours = [0, HOUR_MS].map(|at| SystemHour {
    hour_start_ms: at,
    revision: 1,
    sample_count: 1,
    first_sample_at_ms: at,
    last_sample_at_ms: at,
    inventory_revisions: vec![1],
    metrics: BTreeMap::new(),
    warnings: Vec::new(),
  });
  let reply = history_reply(
    ProfileHistory {
      machine_id: query.machine_id.clone(),
      resolution: query.resolution,
      samples: Vec::new(),
      hours: hours.into(),
      next_cursor: None,
    },
    &query,
  )
  .unwrap_or_else(|_| panic!("hourly page"));
  let body: Value = serde_json::from_slice(&reply.body).unwrap();
  assert_eq!(body["hours"].as_array().unwrap().len(), 1);
  assert_eq!(body["next_cursor"], 0);
}

#[test]
fn an_oversized_single_record_is_an_error_instead_of_a_skipped_row() {
  let mut row = machine("machine-a");
  row.inventory.devices.extend(row.inventory.devices.clone());
  for (index, device) in row.inventory.devices.iter_mut().enumerate() {
    device.device_id = format!("device-{index}");
  }
  validate_inventory(&row.inventory).unwrap();
  let reply = machine_reply(
    crate::dashboard::SystemMachines {
      machines: vec![row],
      next_cursor: None,
    },
    100,
  )
  .err()
  .expect("single record exceeds cap");
  assert_eq!(reply.status, 413);
}
