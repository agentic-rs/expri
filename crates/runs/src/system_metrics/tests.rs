use super::*;
use expri_core::system_profile::*;
use serde_json::json;
use std::io::Cursor;

fn state(status: &str) -> Value {
  let mut state = json!({"schema_version":1,"run_id":"run-test","machine_id":"machine-test","system_capture_id":"capture-test","status":status,"started_at":"1970-01-01T00:00:10Z"});
  if is_terminal_status(status) {
    state["finished_at"] = json!("1970-01-01T00:00:20Z");
  }
  state
}

fn header() -> RunSystemRecord {
  RunSystemRecord::Capture {
    schema_version: 1,
    capture_id: "capture-test".into(),
    machine_id: "machine-test".into(),
    start_ms: 10_000,
  }
}

fn inventory() -> RunSystemRecord {
  RunSystemRecord::Inventory {
    inventory: InventoryRevision {
      revision: 1,
      inventory: SystemInventory {
        observed_at_ms: 10_000,
        hostname: "machine-test".into(),
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
      },
    },
  }
}

fn sample(sequence: u64, observed: i64, interval: u64, value: f64) -> RunSystemRecord {
  RunSystemRecord::Sample {
    sample: SystemSample {
      sequence,
      session_id: "session-test".into(),
      observed_at_ms: observed,
      interval_ms: interval,
      collection_duration_ms: 1,
      inventory_revision: 1,
      metrics: BTreeMap::from([
        (
          "cpu/usage_percent".into(),
          MetricReading {
            kind: MetricKind::Gauge,
            value,
          },
        ),
        (
          "network/device/received_bytes".into(),
          MetricReading {
            kind: MetricKind::CounterDelta,
            value: value * 100.0,
          },
        ),
      ]),
      counters: BTreeMap::from([("network/device/received_bytes".into(), u64::MAX)]),
      warnings: Vec::new(),
    },
  }
}

fn end(complete: bool) -> RunSystemRecord {
  RunSystemRecord::End {
    end_ms: 20_000,
    complete,
    warnings: Vec::new(),
  }
}
fn bytes(records: &[RunSystemRecord]) -> Vec<u8> {
  let mut bytes = Vec::new();
  for record in records {
    bytes.extend(serde_json::to_vec(record).unwrap());
    bytes.push(b'\n');
  }
  bytes
}
fn project(
  records: &[RunSystemRecord],
  cursor: Option<u64>,
  limit: usize,
) -> Result<RunSystemMetrics> {
  let bytes = bytes(records);
  read(
    Cursor::new(&bytes),
    || Ok(Cursor::new(&bytes)),
    "run-test",
    &RunSystemWindow::from_state("run-test", &state("completed")).unwrap(),
    cursor,
    limit,
  )
}

#[test]
fn active_windows_keep_waiting_samples_and_ignore_nonterminal_finish_timestamps() {
  let data = bytes(&[header(), inventory(), sample(1, 25_000, 1000, 2.0)]);
  for status in ["preparing", "running", "waiting"] {
    let mut state = state(status);
    state["finished_at"] = json!("1970-01-01T00:00:20Z");
    let window = RunSystemWindow::from_state("run-test", &state).unwrap();
    assert!(!window.terminal, "{status}");
    assert_eq!(window.end_ms, None, "{status}");
    let result = read(
      Cursor::new(&data),
      || Ok(Cursor::new(&data)),
      "run-test",
      &window,
      None,
      10,
    )
    .unwrap();
    assert_eq!(result.status, "recording", "{status}");
    assert_eq!(result.samples.len(), 1, "{status}");
    assert_eq!(result.samples[0].observed_at_ms, 25_000);
  }
  assert!(RunSystemWindow::from_state("run-test", &state("unknown")).is_none());
}

fn continued_state(status: &str) -> Value {
  let mut state = state(status);
  state["system_capture_id"] = json!("capture-second");
  state["system_capture_started_at"] = json!("1970-01-01T00:00:30Z");
  if is_terminal_status(status) {
    state["finished_at"] = json!("1970-01-01T00:00:40Z");
  }
  state["task_attempts"] = json!({"attempts":[
    {"attempt":1,"system_capture_id":"capture-test","system_capture_started_at":"1970-01-01T00:00:10Z","system_capture_finished_at":"1970-01-01T00:00:20Z"},
    {"attempt":2,"system_capture_id":"capture-second","system_capture_started_at":"1970-01-01T00:00:30Z"}
  ]});
  state
}

fn second_header() -> RunSystemRecord {
  RunSystemRecord::Capture {
    schema_version: 1,
    capture_id: "capture-second".into(),
    machine_id: "machine-test".into(),
    start_ms: 30_000,
  }
}

#[test]
fn continued_captures_preserve_prior_readings_and_leave_inactive_period_empty() {
  let data = bytes(&[
    header(),
    inventory(),
    sample(1, 11_000, 1000, 2.0),
    end(true),
    second_header(),
    inventory(),
    sample(9, 25_000, 1000, 99.0),
    sample(10, 31_000, 1000, 4.0),
    RunSystemRecord::End {
      end_ms: 40_000,
      complete: true,
      warnings: vec![],
    },
  ]);
  let state = continued_state("completed");
  let window = RunSystemWindow::from_state("run-test", &state).unwrap();
  let result = read(
    Cursor::new(&data),
    || Ok(Cursor::new(&data)),
    "run-test",
    &window,
    None,
    10,
  )
  .unwrap();
  assert_eq!(result.start_ms, Some(10_000));
  assert_eq!(result.end_ms, Some(40_000));
  assert_eq!(result.status, "complete");
  assert_eq!(result.sample_count, 3);
  assert_eq!(
    result
      .samples
      .iter()
      .map(|sample| sample.observed_at_ms)
      .collect::<Vec<_>>(),
    [11_000, 31_000]
  );
  assert_eq!(result.metric_summaries["cpu/usage_percent"].count, 2);
  assert_eq!(result.metric_summaries["cpu/usage_percent"].mean, Some(3.0));
  assert!(
    result
      .warnings
      .iter()
      .any(|warning| warning.contains("between attempts"))
  );
  let page = read(
    Cursor::new(&data),
    || Ok(Cursor::new(&data)),
    "run-test",
    &window,
    Some(1),
    1,
  )
  .unwrap();
  assert_eq!(page.samples[0].sequence, 10);
}

#[test]
fn continued_capture_identity_and_closed_segment_boundaries_are_validated() {
  let window = RunSystemWindow::from_state("run-test", &continued_state("running")).unwrap();
  for records in [
    vec![header(), inventory(), second_header()],
    vec![header(), inventory(), end(true), header()],
    vec![
      header(),
      inventory(),
      RunSystemRecord::End {
        end_ms: 21_000,
        complete: true,
        warnings: vec![],
      },
      second_header(),
    ],
  ] {
    let data = bytes(&records);
    assert!(
      read(
        Cursor::new(&data),
        || Ok(Cursor::new(&data)),
        "run-test",
        &window,
        None,
        10
      )
      .is_err()
    );
  }
  let mut state = continued_state("running");
  state["task_attempts"]["attempts"][0]["system_capture_finished_at"] =
    json!("1970-01-01T00:00:35Z");
  assert!(RunSystemWindow::from_state("run-test", &state).is_none());
}

#[test]
fn previous_capture_remains_available_if_new_attempt_cannot_capture() {
  let mut state = continued_state("completed");
  state.as_object_mut().unwrap().remove("system_capture_id");
  state["task_attempts"]["attempts"]
    .as_array_mut()
    .unwrap()
    .pop();
  let window = RunSystemWindow::from_state("run-test", &state).unwrap();
  let data = bytes(&[
    header(),
    inventory(),
    sample(1, 11_000, 1000, 2.0),
    end(true),
  ]);
  let result = read(
    Cursor::new(&data),
    || Ok(Cursor::new(&data)),
    "run-test",
    &window,
    None,
    10,
  )
  .unwrap();
  assert_eq!(result.samples.len(), 1);
  assert_eq!(result.status, "incomplete");
}

#[test]
fn aligned_raw_samples_keep_original_identity_and_complete_gauge_and_rate_summaries() {
  let records = vec![
    header(),
    inventory(),
    sample(4, 9_999, 1_000, 1.0),
    sample(9, 10_000, 0, 2.0),
    sample(12, 12_000, 2_000, 4.0),
    sample(15, 20_000, 1_000, 10.0),
    end(true),
  ];
  let result = project(&records, None, 1).unwrap();
  assert_eq!(result.status, "complete");
  assert_eq!(result.sample_count, 4);
  assert_eq!(result.next_cursor, Some(2));
  assert_eq!(result.samples[0].sequence, 9);
  assert_eq!(
    result.samples[0].counters["network/device/received_bytes"],
    u64::MAX
  );
  assert_eq!(result.metric_summaries["cpu/usage_percent"].count, 2);
  assert_eq!(result.metric_summaries["cpu/usage_percent"].mean, Some(4.0));
  assert_eq!(
    result.metric_summaries["network/device/received_bytes"].last,
    200.0
  );
  assert_eq!(
    result.metric_summaries["network/device/received_bytes"]
      .unit
      .as_deref(),
    Some("bytes/s")
  );
  let next = project(&records, result.next_cursor, 1).unwrap();
  assert_eq!(next.samples[0].sequence, 12);
  assert_eq!(next.next_cursor, None);
  assert_eq!(next.metric_summaries["cpu/usage_percent"].count, 2);
}

#[test]
fn missing_spoofed_or_conflicting_capture_metadata_never_produces_other_machine_metrics() {
  let mut absent = state("completed");
  absent.as_object_mut().unwrap().remove("system_capture_id");
  assert!(RunSystemWindow::from_state("run-test", &absent).is_none());
  assert!(RunSystemWindow::from_state("other-run", &state("completed")).is_none());
  let mut wrong = header();
  if let RunSystemRecord::Capture { machine_id, .. } = &mut wrong {
    *machine_id = "machine-other".into();
  }
  assert!(matches!(
    project(
      &[wrong, inventory(), sample(1, 11_000, 1000, 2.0)],
      None,
      10
    ),
    Err(ExpriError::ServiceRejected { status: 409, .. })
  ));
  assert!(matches!(
    project(&[header(), sample(1, 11_000, 1000, 2.0)], None, 10),
    Err(ExpriError::ServiceRejected { status: 422, .. })
  ));
  let mut mismatch = end(true);
  if let RunSystemRecord::End { end_ms, .. } = &mut mismatch {
    *end_ms = 20_001;
  }
  assert!(matches!(
    project(&[header(), inventory(), mismatch], None, 10),
    Err(ExpriError::ServiceRejected { status: 409, .. })
  ));
}

#[test]
fn old_retained_readings_and_clock_regressions_are_preserved_without_hourly_reconstruction() {
  let result = project(
    &[
      header(),
      inventory(),
      sample(100, 15_000, 1000, 4.0),
      sample(2, 14_000, 0, 2.0),
      end(false),
    ],
    None,
    10,
  )
  .unwrap();
  assert_eq!(
    result
      .samples
      .iter()
      .map(|sample| sample.sequence)
      .collect::<Vec<_>>(),
    [100, 2]
  );
  assert_eq!(result.status, "incomplete");
  assert!(
    result
      .warnings
      .iter()
      .any(|warning| warning.contains("backward"))
  );
  assert_eq!(
    result.metric_summaries["network/device/received_bytes"].count,
    1
  );
}

#[test]
fn partial_tail_and_pre_state_completion_are_explicit_and_not_false_complete() {
  let mut data = bytes(&[header(), inventory(), sample(1, 11_000, 1000, 2.0)]);
  data.extend(b"{\"kind\":\"sample\"");
  let result = read(
    Cursor::new(&data),
    || Ok(Cursor::new(&data)),
    "run-test",
    &RunSystemWindow::from_state("run-test", &state("completed")).unwrap(),
    None,
    10,
  )
  .unwrap();
  assert_eq!(result.status, "incomplete");
  assert_eq!(result.sample_count, 1);
  let data = bytes(&[
    header(),
    inventory(),
    sample(1, 11_000, 1000, 2.0),
    end(true),
  ]);
  let result = read(
    Cursor::new(&data),
    || Ok(Cursor::new(&data)),
    "run-test",
    &RunSystemWindow::from_state("run-test", &state("running")).unwrap(),
    None,
    10,
  )
  .unwrap();
  assert_eq!(result.status, "recording");
  assert_eq!(result.end_ms, None);
}

#[test]
fn byte_bounded_pages_resume_by_stream_ordinal_and_summaries_cover_every_row() {
  let mut records = vec![header(), inventory()];
  for sequence in 1..=5 {
    let mut row = sample(sequence, 11_000 + sequence as i64, 1000, 2.0);
    if let RunSystemRecord::Sample { sample } = &mut row {
      sample.metrics = (0..1500)
        .map(|key| {
          (
            format!("metric/{key}/{}", "x".repeat(150)),
            MetricReading {
              kind: MetricKind::Gauge,
              value: sequence as f64,
            },
          )
        })
        .collect();
    }
    records.push(row);
  }
  records.push(end(true));
  let mut cursor = None;
  let mut sequences = Vec::new();
  loop {
    let result = project(&records, cursor, 1024).unwrap();
    assert!(serde_json::to_vec(&result).unwrap().len() <= PAGE_BYTES);
    assert_eq!(result.metric_summaries.len(), 256);
    assert_eq!(result.metric_summaries.values().next().unwrap().count, 5);
    sequences.extend(result.samples.iter().map(|sample| sample.sequence));
    cursor = result.next_cursor;
    if cursor.is_none() {
      break;
    }
  }
  assert_eq!(sequences, [1, 2, 3, 4, 5]);
}

#[test]
fn inventory_bytes_shrink_the_sample_page_without_skipping_its_next_ordinal() {
  let mut first = inventory();
  if let RunSystemRecord::Inventory { inventory } = &mut first {
    inventory.inventory.devices = (0..32)
      .map(|index| DeviceInventory {
        device_id: format!("device-{index}"),
        kind: DeviceKind::Disk,
        name: "disk".into(),
        total_bytes: None,
        attributes: (0..8)
          .map(|key| (format!("attribute-{key}"), "x".repeat(1024)))
          .collect(),
      })
      .collect();
  }
  let mut second = first.clone();
  if let RunSystemRecord::Inventory { inventory } = &mut second {
    inventory.revision = 2;
  }
  let mut second_sample = sample(88, 12_000, 1000, 2.0);
  if let RunSystemRecord::Sample { sample } = &mut second_sample {
    sample.inventory_revision = 2;
  }
  let records = vec![
    header(),
    first,
    second,
    sample(77, 11_000, 1000, 1.0),
    second_sample,
    end(true),
  ];
  let page = project(&records, None, 1024).unwrap();
  assert_eq!(page.samples.len(), 1);
  assert_eq!(page.samples[0].sequence, 77);
  assert_eq!(page.inventories.len(), 1);
  assert_eq!(page.inventories[0].revision, 1);
  assert_eq!(page.next_cursor, Some(1));
  let next = project(&records, page.next_cursor, 1024).unwrap();
  assert_eq!(next.samples[0].sequence, 88);
  assert_eq!(next.inventories[0].revision, 2);
  assert_eq!(next.next_cursor, None);
  assert!(serde_json::to_vec(&page).unwrap().len() <= PAGE_BYTES);
  assert_eq!(page.metric_summaries["cpu/usage_percent"].count, 2);
}

#[test]
fn boundary_crossing_intervals_keep_original_rates_and_duration_weighting() {
  let result = project(
    &[
      header(),
      inventory(),
      sample(1, 11_000, 10_000, 2.0),
      sample(2, 13_000, 2_000, 4.0),
      end(true),
    ],
    None,
    10,
  )
  .unwrap();
  assert_eq!(
    result.metric_summaries["network/device/received_bytes"].min,
    20.0
  );
  assert!(
    (result.metric_summaries["network/device/received_bytes"]
      .mean
      .unwrap()
      - 50.0)
      .abs()
      < 1e-10
  );
  assert!(
    result
      .warnings
      .iter()
      .any(|warning| warning.contains("beginning before"))
  );
  assert_eq!(result.samples[0].interval_ms, 10_000);
}

#[test]
fn unavailable_capture_reports_only_a_bounded_valid_error_and_timeouts_never_return_partial_summaries()
 {
  let result = RunSystemMetrics::unavailable_for_run(
    "run-test",
    &json!({"system_capture_error":"reserved capture file already exists"}),
    "Capture unavailable.",
  );
  assert_eq!(result.status, "unavailable");
  assert!(
    result
      .warnings
      .iter()
      .any(|warning| warning.contains("reserved capture file"))
  );
  for value in [
    json!("invalid\nprivate-detail"),
    json!(true),
    json!("x".repeat(1025)),
  ] {
    let result = RunSystemMetrics::unavailable_for_run(
      "run-test",
      &json!({"system_capture_error":value}),
      "Capture unavailable.",
    );
    assert_eq!(result.warnings, ["Capture unavailable."]);
  }
  struct Timeout;
  impl Read for Timeout {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
      Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "slow capture",
      ))
    }
  }
  let result = read(
    Timeout,
    || Ok(Timeout),
    "run-test",
    &RunSystemWindow::from_state("run-test", &state("completed")).unwrap(),
    None,
    10,
  );
  assert!(matches!(
    result,
    Err(ExpriError::ServiceRejected { status: 504, .. })
  ));
}
