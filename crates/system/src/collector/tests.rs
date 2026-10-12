use super::*;

fn counters(value: u64) -> BTreeMap<String, u64> {
  BTreeMap::from([("network/fixture/received_bytes".into(), value)])
}

#[test]
fn counters_skip_warmup_reset_and_absent_devices_then_resume_from_current_baseline() {
  let mut tracker = CounterTracker::default();
  let mut warnings = Warnings::default();
  let key = "network/fixture/received_bytes";
  assert!(
    tracker
      .observe(&counters(u64::MAX - 100), false, &mut warnings)
      .is_empty()
  );
  assert_eq!(
    tracker.observe(&counters(u64::MAX - 50), true, &mut warnings)[key].value,
    50.0
  );
  assert!(
    tracker
      .observe(&counters(5), true, &mut warnings)
      .is_empty()
  );
  assert_eq!(
    tracker.observe(&counters(25), true, &mut warnings)[key].value,
    20.0
  );
  tracker.observe(&BTreeMap::new(), true, &mut warnings);
  assert!(
    tracker
      .observe(&counters(1000), true, &mut warnings)
      .is_empty()
  );
  assert_eq!(
    tracker.observe(&counters(1000), true, &mut warnings)[key].value,
    0.0
  );
  assert!(
    warnings
      .into_vec()
      .iter()
      .any(|warning| warning.contains("Counter reset"))
  );
}

#[test]
fn gaps_replace_counter_baselines_without_smearing_downtime_into_the_next_interval() {
  let mut tracker = CounterTracker::default();
  let mut warnings = Warnings::default();
  tracker.observe(&counters(100), false, &mut warnings);
  assert!(
    tracker
      .observe(&counters(100_000), false, &mut warnings)
      .is_empty()
  );
  assert_eq!(
    tracker.observe(&counters(100_020), true, &mut warnings)["network/fixture/received_bytes"]
      .value,
    20.0
  );
}

#[test]
fn timing_uses_monotonic_intervals_and_marks_sleep_or_clock_jumps_as_gaps() {
  let start = Instant::now();
  let mut timeline = Timeline {
    interval: Duration::from_secs(10),
    previous: None,
  };
  let mut warnings = Warnings::default();
  assert_eq!(timeline.observe(start, 100_000, &mut warnings), 0);
  assert_eq!(
    timeline.observe(start + Duration::from_secs(10), 110_000, &mut warnings),
    10_000
  );
  assert_eq!(
    timeline.observe(start + Duration::from_secs(40), 140_000, &mut warnings),
    0
  );
  assert_eq!(
    timeline.observe(start + Duration::from_secs(50), 50_000, &mut warnings),
    0
  );
  assert_eq!(
    timeline.observe(start + Duration::from_secs(60), 60_000, &mut warnings),
    10_000
  );
  assert_eq!(
    timeline.observe(start + Duration::from_secs(70), 360_000, &mut warnings),
    0
  );
}

#[test]
fn device_ids_are_deterministic_domain_separated_and_unambiguous() {
  let id = stable_id("disk", &[b"/Volumes/model data", b"/dev/disk1"]);
  assert_eq!(
    id,
    stable_id("disk", &[b"/Volumes/model data", b"/dev/disk1"])
  );
  expri_core::identifiers::validate_component(&id).unwrap();
  assert_ne!(
    stable_id("disk", &[b"ab", b"c"]),
    stable_id("disk", &[b"a", b"bc"])
  );
  assert_ne!(
    stable_id("disk", &[b"eth0"]),
    stable_id("network", &[b"eth0"])
  );
}

#[test]
fn a_backward_half_second_clock_step_cannot_overlap_a_one_second_interval() {
  let start = Instant::now();
  let mut timeline = Timeline {
    interval: Duration::from_secs(1),
    previous: None,
  };
  let mut warnings = Warnings::default();
  assert_eq!(timeline.observe(start, 100_000, &mut warnings), 0);
  assert_eq!(
    timeline.observe(start + Duration::from_secs(1), 100_500, &mut warnings),
    0
  );
  assert_eq!(
    timeline.observe(start + Duration::from_secs(2), 101_500, &mut warnings),
    1000
  );
}

#[test]
fn cached_metadata_fills_unknown_values_once_and_preserves_known_values_on_failure() {
  let mut inventory = DeviceInventory {
    device_id: "GPU-fixture".into(),
    kind: DeviceKind::Gpu,
    name: "NVIDIA GPU".into(),
    total_bytes: None,
    attributes: BTreeMap::from([("model_status".into(), "unavailable".into())]),
  };
  fill_metadata(&mut inventory, Some("Recovered model"), Some(4096));
  assert_eq!(inventory.name, "Recovered model");
  assert_eq!(inventory.total_bytes, Some(4096));
  assert!(!inventory.attributes.contains_key("model_status"));
  fill_metadata(&mut inventory, None, None);
  fill_metadata(
    &mut inventory,
    Some("Transient different model"),
    Some(8192),
  );
  assert_eq!(inventory.name, "Recovered model");
  assert_eq!(inventory.total_bytes, Some(4096));
  inventory.kind = DeviceKind::Disk;
  inventory.name.clear();
  inventory.total_bytes = None;
  fill_metadata(&mut inventory, Some("/dev/recovered"), Some(16384));
  assert_eq!(inventory.name, "/dev/recovered");
  assert_eq!(inventory.total_bytes, Some(16384));
}

#[test]
fn normal_scheduler_jitter_preserves_measured_intervals_up_to_the_60_second_configuration() {
  let start = Instant::now();
  for seconds in [1, 10, 60] {
    let interval = Duration::from_secs(seconds);
    let elapsed = interval + Duration::from_millis(1);
    let expected_ms = seconds * 1000 + 1;
    let mut timeline = Timeline {
      interval,
      previous: None,
    };
    let mut warnings = Warnings::default();
    assert_eq!(timeline.observe(start, 100_000, &mut warnings), 0);
    assert_eq!(
      timeline.observe(start + elapsed, 100_000 + expected_ms as i64, &mut warnings),
      expected_ms
    );
    assert_eq!(
      timeline.observe(
        start + elapsed + interval * 2 + Duration::from_millis(1),
        100_000 + expected_ms as i64 + (seconds * 2000 + 1) as i64,
        &mut warnings
      ),
      0
    );
  }
  assert!(Collector::with_interval(Duration::from_secs(61)).is_err());
}

#[test]
fn sensor_warnings_are_deduplicated_bounded_and_safe_to_serialize() {
  let mut warnings = Warnings::default();
  warnings.push("duplicate");
  warnings.push("duplicate");
  for index in 0..100 {
    warnings.push(format!("warning {index}\n{}", "é".repeat(512)));
  }
  let warnings = warnings.into_vec();
  assert_eq!(warnings.len(), MAX_WARNINGS);
  assert_eq!(
    warnings.last().unwrap(),
    "Additional sensor warnings were omitted."
  );
  assert!(
    warnings
      .iter()
      .all(|warning| warning.len() <= 512 && !warning.chars().any(char::is_control))
  );
}

#[test]
fn collection_has_unique_sessions_warmup_rates_and_no_process_scan() {
  if !sysinfo::IS_SUPPORTED_SYSTEM {
    return;
  }
  let mut collector = Collector::new().unwrap();
  let other = Collector::new().unwrap();
  assert_ne!(collector.session_id, other.session_id);
  let (inventory, mut sample) = collector.collect().unwrap();
  assert!(collector.system.processes().is_empty());
  assert_eq!(sample.interval_ms, 0);
  assert!(!sample.metrics.contains_key("cpu/usage_percent"));
  assert!(
    sample
      .metrics
      .values()
      .all(|value| value.kind == MetricKind::Gauge)
  );
  assert!(sample.metrics.values().all(|value| value.value.is_finite()));
  expri_core::system_profile::validate_inventory(&inventory).unwrap();
  sample.sequence = 1;
  sample.inventory_revision = 1;
  expri_core::system_profile::validate_sample(&sample).unwrap();
  let json = serde_json::to_value(&sample).unwrap();
  assert!(
    json["counters"]
      .as_object()
      .unwrap()
      .values()
      .all(serde_json::Value::is_string)
  );
}
