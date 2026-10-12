//! Machine-scoped system inventory and time-series contracts.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};
use crate::identifiers::validate_component;

pub const SYSTEM_PROFILE_CAPABILITY: &str = "system-profile-v1";
pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const HOUR_MS: i64 = 60 * 60 * 1000;
pub const DETAIL_RETENTION_MS: i64 = 72 * HOUR_MS;
pub const MAX_BATCH_SAMPLES: usize = 128;
pub const MAX_BATCH_HOURS: usize = 64;
pub const MAX_BATCH_INVENTORIES: usize = 64;
pub const MAX_HISTORY_ROWS: usize = 1024;
pub const MAX_METRICS: usize = 2048;
pub const MAX_PROFILE_RECORD_BYTES: usize = 800 * 1024;
pub const MAX_PROFILE_BATCH_BYTES: usize = 900 * 1024;
/// Measured windows include scheduler jitter; nominal collection is at most 60 seconds.
pub const MAX_INTERVAL_MS: u64 = 120 * 1000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuInventory {
  pub brand: String,
  pub vendor: String,
  pub logical_cpu_count: u32,
  pub physical_core_count: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
  Gpu,
  Disk,
  Network,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceInventory {
  pub device_id: String,
  pub kind: DeviceKind,
  pub name: String,
  pub total_bytes: Option<u64>,
  pub attributes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemInventory {
  pub observed_at_ms: i64,
  pub hostname: String,
  pub os_name: Option<String>,
  pub os_version: Option<String>,
  pub kernel_version: Option<String>,
  pub architecture: String,
  pub cpu: CpuInventory,
  pub memory_total_bytes: u64,
  /// Host/system readings; these are not a promise of container cgroup limits.
  pub measurement_scope: String,
  pub devices: Vec<DeviceInventory>,
  pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryRevision {
  pub revision: u64,
  pub inventory: SystemInventory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
  Gauge,
  /// Nonnegative change over this sample's measured interval, not a cumulative value.
  CounterDelta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricReading {
  pub kind: MetricKind,
  pub value: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemSample {
  pub sequence: u64,
  pub session_id: String,
  pub observed_at_ms: i64,
  pub interval_ms: u64,
  pub collection_duration_ms: u64,
  pub inventory_revision: u64,
  pub metrics: BTreeMap<String, MetricReading>,
  /// Decimal strings on the wire preserve exact counters for JavaScript readers.
  #[serde(with = "counter_values")]
  pub counters: BTreeMap<String, u64>,
  pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricAggregate {
  pub kind: MetricKind,
  pub sample_count: u64,
  pub coverage_ms: u64,
  /// Gauges store value * covered milliseconds; counter deltas store their total.
  pub sum: f64,
  pub min: f64,
  pub max: f64,
  pub last: f64,
  pub last_at_ms: i64,
}

impl MetricAggregate {
  /// Gauge mean, or counter throughput per second, across measured coverage only.
  pub fn mean(&self) -> Option<f64> {
    if self.coverage_ms == 0 {
      return None;
    }
    Some(match self.kind {
      MetricKind::Gauge => self.sum / self.coverage_ms as f64,
      MetricKind::CounterDelta => self.sum * 1000.0 / self.coverage_ms as f64,
    })
  }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemHour {
  pub hour_start_ms: i64,
  pub revision: u64,
  pub sample_count: u64,
  pub first_sample_at_ms: i64,
  pub last_sample_at_ms: i64,
  pub inventory_revisions: Vec<u64>,
  pub metrics: BTreeMap<String, MetricAggregate>,
  /// Explicitly describes bounded omissions when transient devices exceed an hour's budget.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBatch {
  pub schema_version: u32,
  pub machine_id: String,
  pub inventories: Vec<InventoryRevision>,
  pub samples: Vec<SystemSample>,
  /// Absolute authoritative local snapshots; receivers must not add these together.
  pub hours: Vec<SystemHour>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileAcknowledgement {
  pub machine_id: String,
  pub sample_sequence: u64,
  pub inventory_revision: u64,
  #[serde(with = "hour_revision_values")]
  pub hour_revisions: BTreeMap<i64, u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineProfile {
  pub machine_id: String,
  pub inventory_revision: u64,
  pub inventory: SystemInventory,
  pub last_sample_at_ms: Option<i64>,
  pub last_received_at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileResolution {
  Detail,
  Hourly,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileHistoryQuery {
  pub machine_id: String,
  pub start_ms: i64,
  pub end_ms: i64,
  pub resolution: ProfileResolution,
  pub cursor: Option<i64>,
  pub limit: usize,
}

impl ProfileHistoryQuery {
  pub fn validate(&self) -> Result<()> {
    validate_component(&self.machine_id)?;
    if self.start_ms < 0 || self.end_ms <= self.start_ms {
      return Err(invalid(
        "history requires a nonnegative start and a later end",
      ));
    }
    if !(1..=MAX_HISTORY_ROWS).contains(&self.limit) {
      return Err(invalid("system history limit must be between 1 and 1024"));
    }
    if self.cursor.is_some_and(|cursor| cursor < 0) {
      return Err(invalid("system history cursor must be nonnegative"));
    }
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileHistory {
  pub machine_id: String,
  pub resolution: ProfileResolution,
  pub samples: Vec<SystemSample>,
  pub hours: Vec<SystemHour>,
  pub next_cursor: Option<i64>,
}

fn invalid(message: &str) -> ExpriError {
  ExpriError::Message(message.into())
}

fn text(value: &str, limit: usize) -> Result<()> {
  if value.len() > limit || value.chars().any(char::is_control) {
    return Err(invalid(
      "system profile text is invalid or exceeds its limit",
    ));
  }
  Ok(())
}

fn warnings(values: &[String]) -> Result<()> {
  if values.len() > 32 {
    return Err(invalid("system profile has too many warnings"));
  }
  for warning in values {
    text(warning, 512)?;
  }
  Ok(())
}

pub fn validate_metric_name(name: &str) -> Result<()> {
  if name.is_empty()
    || name.len() > 256
    || !name
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || b"/_-.:".contains(&byte))
  {
    return Err(invalid("invalid system metric name"));
  }
  Ok(())
}

pub fn validate_inventory(inventory: &SystemInventory) -> Result<()> {
  if inventory.observed_at_ms < 0
    || inventory.cpu.logical_cpu_count == 0
    || inventory.devices.len() > 256
  {
    return Err(invalid(
      "invalid system inventory timestamp, CPU count, or device count",
    ));
  }
  for value in [
    &inventory.hostname,
    &inventory.architecture,
    &inventory.cpu.brand,
    &inventory.cpu.vendor,
    &inventory.measurement_scope,
  ] {
    text(value, 512)?;
  }
  for value in [
    &inventory.os_name,
    &inventory.os_version,
    &inventory.kernel_version,
  ]
  .into_iter()
  .flatten()
  {
    text(value, 512)?;
  }
  let mut ids = BTreeSet::new();
  for device in &inventory.devices {
    validate_component(&device.device_id)?;
    if !ids.insert(&device.device_id) || device.attributes.len() > 32 {
      return Err(invalid(
        "system device IDs must be unique and attributes bounded",
      ));
    }
    text(&device.name, 1024)?;
    for (key, value) in &device.attributes {
      text(key, 64)?;
      text(value, 1024)?;
    }
  }
  warnings(&inventory.warnings)?;
  validate_encoded_size(inventory, MAX_PROFILE_RECORD_BYTES)
}

pub fn validate_sample(sample: &SystemSample) -> Result<()> {
  validate_component(&sample.session_id)?;
  if sample.sequence == 0
    || sample.sequence > i64::MAX as u64
    || sample.inventory_revision == 0
    || sample.inventory_revision > i64::MAX as u64
    || sample.observed_at_ms < 0
    || sample.interval_ms > MAX_INTERVAL_MS
    || sample.metrics.len() > MAX_METRICS
    || sample.counters.len() > MAX_METRICS
  {
    return Err(invalid(
      "system sample has invalid identity, time, interval, or metric count",
    ));
  }
  for (name, metric) in &sample.metrics {
    validate_metric_name(name)?;
    if !metric.value.is_finite() || (metric.kind == MetricKind::CounterDelta && metric.value < 0.0)
    {
      return Err(invalid(
        "system metrics must be finite and counter deltas nonnegative",
      ));
    }
  }
  for name in sample.counters.keys() {
    validate_metric_name(name)?;
  }
  warnings(&sample.warnings)?;
  validate_encoded_size(sample, MAX_PROFILE_RECORD_BYTES)
}

pub fn validate_hour(hour: &SystemHour) -> Result<()> {
  if hour.hour_start_ms < 0
    || hour.hour_start_ms % HOUR_MS != 0
    || hour.revision == 0
    || hour.revision > i64::MAX as u64
    || hour.sample_count == 0
    || hour.first_sample_at_ms < 0
    || hour.last_sample_at_ms < hour.first_sample_at_ms
    || hour.inventory_revisions.is_empty()
    || hour.inventory_revisions.len() > 4096
    || hour.metrics.len() > MAX_METRICS
  {
    return Err(invalid("invalid hourly system summary"));
  }
  let revisions: BTreeSet<_> = hour.inventory_revisions.iter().copied().collect();
  if revisions.len() != hour.inventory_revisions.len()
    || revisions
      .iter()
      .any(|revision| *revision == 0 || *revision > i64::MAX as u64)
  {
    return Err(invalid("invalid hourly inventory revisions"));
  }
  for (name, metric) in &hour.metrics {
    validate_metric_name(name)?;
    if metric.sample_count == 0
      || metric.sample_count > hour.sample_count
      || ![metric.sum, metric.min, metric.max, metric.last]
        .iter()
        .all(|value| value.is_finite())
      || metric.min > metric.max
      || metric.last < metric.min
      || metric.last > metric.max
      || metric.last_at_ms < 0
      || (metric.kind == MetricKind::CounterDelta && metric.sum < 0.0)
    {
      return Err(invalid("invalid hourly metric aggregate"));
    }
  }
  warnings(&hour.warnings)?;
  validate_encoded_size(hour, MAX_PROFILE_RECORD_BYTES)
}

pub fn validate_profile_batch(batch: &ProfileBatch) -> Result<()> {
  validate_component(&batch.machine_id)?;
  if batch.schema_version != PROFILE_SCHEMA_VERSION
    || batch.samples.len() > MAX_BATCH_SAMPLES
    || batch.hours.len() > MAX_BATCH_HOURS
    || batch.inventories.len() > MAX_BATCH_INVENTORIES
  {
    return Err(invalid("unsupported or oversized system profile batch"));
  }
  let mut inventories = BTreeSet::new();
  for inventory in &batch.inventories {
    if inventory.revision == 0
      || inventory.revision > i64::MAX as u64
      || !inventories.insert(inventory.revision)
    {
      return Err(invalid("invalid or duplicate inventory revision"));
    }
    validate_inventory(&inventory.inventory)?;
  }
  let mut samples = BTreeSet::new();
  for sample in &batch.samples {
    validate_sample(sample)?;
    if !samples.insert(sample.sequence) {
      return Err(invalid("duplicate system sample sequence"));
    }
  }
  let mut hours = BTreeSet::new();
  for hour in &batch.hours {
    validate_hour(hour)?;
    if !hours.insert(hour.hour_start_ms) {
      return Err(invalid("duplicate system hour"));
    }
  }
  validate_encoded_size(batch, MAX_PROFILE_BATCH_BYTES)
}

fn validate_encoded_size(value: &impl Serialize, limit: usize) -> Result<()> {
  if serde_json::to_vec(value)?.len() > limit {
    return Err(ExpriError::ServiceRejected {
      status: 413,
      detail: "system profile record or batch exceeds its byte limit".into(),
    });
  }
  Ok(())
}

mod counter_values {
  use super::*;
  use serde::{Deserializer, Serializer};

  pub fn serialize<S: Serializer>(
    values: &BTreeMap<String, u64>,
    serializer: S,
  ) -> std::result::Result<S::Ok, S::Error> {
    values
      .iter()
      .map(|(key, value)| (key, value.to_string()))
      .collect::<BTreeMap<_, _>>()
      .serialize(serializer)
  }

  pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
  ) -> std::result::Result<BTreeMap<String, u64>, D::Error> {
    BTreeMap::<String, String>::deserialize(deserializer)?
      .into_iter()
      .map(|(key, value)| {
        value
          .parse()
          .map(|value| (key, value))
          .map_err(serde::de::Error::custom)
      })
      .collect()
  }
}

// Internally tagged service responses deserialize through serde's content tree,
// where JSON object keys remain strings. Parse hour keys explicitly there too.
mod hour_revision_values {
  use super::*;
  use serde::{Deserializer, Serializer};

  pub fn serialize<S: Serializer>(
    values: &BTreeMap<i64, u64>,
    serializer: S,
  ) -> std::result::Result<S::Ok, S::Error> {
    values
      .iter()
      .map(|(key, value)| (key.to_string(), value))
      .collect::<BTreeMap<_, _>>()
      .serialize(serializer)
  }

  pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
  ) -> std::result::Result<BTreeMap<i64, u64>, D::Error> {
    BTreeMap::<String, u64>::deserialize(deserializer)?
      .into_iter()
      .map(|(key, value)| {
        let hour: i64 = key.parse().map_err(serde::de::Error::custom)?;
        if hour < 0 || hour % HOUR_MS != 0 || hour.to_string() != key {
          return Err(serde::de::Error::custom("invalid acknowledged UTC hour"));
        }
        Ok((hour, value))
      })
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn nonempty_hour_acknowledgements_round_trip_inside_tagged_service_responses() {
    let acknowledgement = ProfileAcknowledgement {
      machine_id: "machine-test".into(),
      sample_sequence: 3,
      inventory_revision: 1,
      hour_revisions: BTreeMap::from([(0, 2), (HOUR_MS, 3)]),
    };
    let response = crate::service::Response::SystemProfileAcknowledged {
      acknowledgement: acknowledgement.clone(),
    };
    let bytes = serde_json::to_vec(&response).unwrap();
    let crate::service::Response::SystemProfileAcknowledged {
      acknowledgement: decoded,
    } = serde_json::from_slice(&bytes).unwrap()
    else {
      panic!("expected profile acknowledgement");
    };
    assert_eq!(decoded, acknowledgement);
  }

  #[test]
  fn counters_round_trip_without_losing_integer_precision() {
    let sample = SystemSample {
      sequence: 1,
      session_id: "session-1".into(),
      observed_at_ms: 1,
      interval_ms: 0,
      collection_duration_ms: 0,
      inventory_revision: 1,
      metrics: BTreeMap::new(),
      counters: BTreeMap::from([("network/test/received_bytes".into(), u64::MAX)]),
      warnings: Vec::new(),
    };
    let bytes = serde_json::to_vec(&sample).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
      json["counters"]["network/test/received_bytes"],
      u64::MAX.to_string()
    );
    assert_eq!(
      serde_json::from_slice::<SystemSample>(&bytes).unwrap(),
      sample
    );
  }

  #[test]
  fn means_use_coverage_and_counter_totals() {
    let mut aggregate = MetricAggregate {
      kind: MetricKind::Gauge,
      sample_count: 2,
      coverage_ms: 30_000,
      sum: 1_200_000.0,
      min: 20.0,
      max: 50.0,
      last: 50.0,
      last_at_ms: 30_000,
    };
    assert_eq!(aggregate.mean(), Some(40.0));
    aggregate.kind = MetricKind::CounterDelta;
    aggregate.sum = 9000.0;
    assert_eq!(aggregate.mean(), Some(300.0));
    aggregate.coverage_ms = 0;
    assert_eq!(aggregate.mean(), None);
  }
}
