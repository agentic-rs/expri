//! GPU providers are optional: absent libraries and unsupported sensors never become zero.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use nvml_wrapper::{Nvml, enum_wrappers::device::TemperatureSensor, error::NvmlError};

use super::{Warnings, clean_text, fill_metadata, gauge};
use crate::{DeviceInventory, DeviceKind, MetricReading};

#[cfg(any(target_os = "linux", test))]
mod amd;
#[cfg(target_os = "macos")]
mod apple;

const MAX_GPUS: usize = 64;
const REDISCOVER_AFTER: Duration = Duration::from_secs(60);

pub(super) struct GpuCollector {
  nvidia: Option<Nvml>,
  nvidia_error: Option<String>,
  nvidia_inventory: BTreeMap<String, DeviceInventory>,
  last_attempt: Instant,
  #[cfg(target_os = "linux")]
  amd: amd::AmdCollector,
  #[cfg(target_os = "macos")]
  apple: apple::AppleCollector,
}

impl GpuCollector {
  pub fn new() -> Self {
    let (nvidia, nvidia_error) = initialize_nvidia();
    Self {
      nvidia,
      nvidia_error,
      nvidia_inventory: BTreeMap::new(),
      last_attempt: Instant::now(),
      #[cfg(target_os = "linux")]
      amd: amd::AmdCollector::new(),
      #[cfg(target_os = "macos")]
      apple: apple::AppleCollector::new(),
    }
  }

  pub fn collect(
    &mut self,
    metrics: &mut BTreeMap<String, MetricReading>,
    warnings: &mut Warnings,
  ) -> Vec<DeviceInventory> {
    if self.nvidia.is_none() && self.last_attempt.elapsed() >= REDISCOVER_AFTER {
      (self.nvidia, self.nvidia_error) = initialize_nvidia();
      self.last_attempt = Instant::now();
    }
    if let Some(error) = &self.nvidia_error {
      warnings.push(format!("NVIDIA GPU collection is unavailable: {error}"));
    }
    let mut devices = self.collect_nvidia(metrics, warnings);
    #[cfg(target_os = "linux")]
    devices.extend(
      self
        .amd
        .collect(MAX_GPUS.saturating_sub(devices.len()), metrics, warnings),
    );
    #[cfg(target_os = "macos")]
    devices.extend(
      self
        .apple
        .collect(MAX_GPUS.saturating_sub(devices.len()), metrics, warnings),
    );
    if devices.is_empty() {
      warnings.push("No supported GPU is available; GPU metrics are omitted.");
    }
    devices
  }

  fn collect_nvidia(
    &mut self,
    metrics: &mut BTreeMap<String, MetricReading>,
    warnings: &mut Warnings,
  ) -> Vec<DeviceInventory> {
    let Some(nvml) = &self.nvidia else {
      return Vec::new();
    };
    let count = match nvml.device_count() {
      Ok(count) => count,
      Err(_) => {
        warnings.push("NVIDIA GPU enumeration is unavailable; GPU readings were omitted.");
        return bounded_cached_inventory(&mut self.nvidia_inventory);
      }
    };
    if count as usize > MAX_GPUS {
      warnings.push("GPU inventory is limited to 64 devices.");
    }
    let mut devices = Vec::new();
    let mut incomplete = false;
    for index in 0..count.min(MAX_GPUS as u32) {
      let device = match nvml.device_by_index(index) {
        Ok(device) => device,
        Err(_) => {
          incomplete = true;
          warnings.push(format!(
            "NVIDIA GPU {index} is inaccessible; its readings were omitted."
          ));
          continue;
        }
      };
      let id = match device.uuid() {
        Ok(id) if expri_core::identifiers::validate_component(&id).is_ok() => id,
        _ => {
          incomplete = true;
          warnings.push(format!(
            "NVIDIA GPU {index} has no usable UUID; its readings were omitted."
          ));
          continue;
        }
      };
      let memory = device.memory_info().ok();
      let usage = device.utilization_rates().ok();
      let unknown_name = self.nvidia_inventory.get(&id).is_none_or(|inventory| {
        inventory.name.is_empty()
          || inventory
            .attributes
            .get("model_status")
            .is_some_and(|value| value == "unavailable")
      });
      let model = if unknown_name {
        device
          .name()
          .ok()
          .map(|value| clean_text(&value, 1024))
          .filter(|value| !value.is_empty())
      } else {
        None
      };
      let inventory = self
        .nvidia_inventory
        .entry(id.clone())
        .or_insert_with(|| DeviceInventory {
          device_id: id.clone(),
          kind: DeviceKind::Gpu,
          name: model.clone().unwrap_or_else(|| "NVIDIA GPU".into()),
          total_bytes: memory.as_ref().map(|value| value.total),
          attributes: {
            let mut attributes = BTreeMap::from([
              ("provider".into(), "nvml".into()),
              ("vendor".into(), "NVIDIA".into()),
              ("memory_kind".into(), "dedicated".into()),
            ]);
            if model.is_none() {
              attributes.insert("model_status".into(), "unavailable".into());
            }
            attributes
          },
        });
      fill_metadata(
        inventory,
        model.as_deref(),
        memory.as_ref().map(|value| value.total),
      );
      record_partition_mode(inventory, device.mig_mode().map(|mode| mode.current));
      devices.push(inventory.clone());
      let readings = GpuReadings {
        usage_percent: usage.as_ref().map(|value| f64::from(value.gpu)),
        memory_used_bytes: memory.as_ref().map(|value| value.used),
        memory_total_bytes: memory.as_ref().map(|value| value.total),
        memory_allocated_bytes: None,
        temperature_celsius: device
          .temperature(TemperatureSensor::Gpu)
          .ok()
          .map(f64::from),
        power_watts: device
          .power_usage()
          .ok()
          .map(|value| f64::from(value) / 1000.0),
        shared_memory: false,
      };
      readings.record(&id, metrics, warnings);
    }
    let active: std::collections::BTreeSet<_> = devices
      .iter()
      .map(|value| value.device_id.clone())
      .collect();
    if incomplete {
      // Sensor/handle errors do not imply that previously inventoried hardware vanished.
      let remaining = MAX_GPUS.saturating_sub(devices.len());
      devices.extend(
        self
          .nvidia_inventory
          .values()
          .filter(|device| !active.contains(&device.device_id))
          .take(remaining)
          .cloned(),
      );
    }
    retain_inventory(&mut self.nvidia_inventory, &devices);
    devices
  }
}

fn record_partition_mode(inventory: &mut DeviceInventory, mode: Result<u32, NvmlError>) {
  match mode {
    Ok(0) | Err(NvmlError::NotSupported) => {
      inventory.attributes.remove("partition_mode");
    }
    Ok(1) => {
      // A partitioned physical UUID is not a selectable whole CUDA device.
      inventory
        .attributes
        .insert("partition_mode".into(), "mig".into());
    }
    _ => {
      // A failed query cannot certify a whole GPU or erase known partitioning.
      if inventory
        .attributes
        .get("partition_mode")
        .map(String::as_str)
        != Some("mig")
      {
        inventory
          .attributes
          .insert("partition_mode".into(), "unknown".into());
      }
    }
  }
}

fn bounded_cached_inventory(cache: &mut BTreeMap<String, DeviceInventory>) -> Vec<DeviceInventory> {
  let devices: Vec<_> = cache.values().take(MAX_GPUS).cloned().collect();
  retain_inventory(cache, &devices);
  devices
}

fn retain_inventory(cache: &mut BTreeMap<String, DeviceInventory>, devices: &[DeviceInventory]) {
  let active: std::collections::BTreeSet<_> = devices
    .iter()
    .take(MAX_GPUS)
    .map(|device| device.device_id.as_str())
    .collect();
  cache.retain(|id, _| active.contains(id.as_str()));
}

fn initialize_nvidia() -> (Option<Nvml>, Option<String>) {
  if cfg!(target_os = "macos") {
    return (None, None);
  }
  // NVML dynamically loads the installed driver library; no driver is needed at build time.
  match Nvml::init() {
    Ok(nvml) => (Some(nvml), None),
    Err(error) => (None, Some(clean_text(&error.to_string(), 384))),
  }
}

#[derive(Default)]
struct GpuReadings {
  usage_percent: Option<f64>,
  memory_used_bytes: Option<u64>,
  memory_total_bytes: Option<u64>,
  memory_allocated_bytes: Option<u64>,
  temperature_celsius: Option<f64>,
  power_watts: Option<f64>,
  shared_memory: bool,
}

impl GpuReadings {
  fn record(
    &self,
    id: &str,
    metrics: &mut BTreeMap<String, MetricReading>,
    warnings: &mut Warnings,
  ) {
    let prefix = format!("gpu/{id}");
    let values = [
      (
        "usage_percent",
        self
          .usage_percent
          .filter(|value| (0.0..=100.0).contains(value)),
      ),
      (
        if self.shared_memory {
          "shared_memory_used_bytes"
        } else {
          "memory_used_bytes"
        },
        self.memory_used_bytes.map(|value| value as f64),
      ),
      ("temperature_celsius", self.temperature_celsius),
      ("power_watts", self.power_watts),
    ];
    for (name, value) in values {
      if let Some(value) = value.filter(|value| value.is_finite()) {
        gauge(metrics, &format!("{prefix}/{name}"), value);
      } else {
        warnings.push(format!(
          "GPU sensor {prefix}/{name} is unavailable or unsupported."
        ));
      }
    }
    if !self.shared_memory {
      if let Some(total) = self.memory_total_bytes {
        gauge(
          metrics,
          &format!("{prefix}/memory_total_bytes"),
          total as f64,
        );
      } else {
        warnings.push(format!(
          "GPU sensor {prefix}/memory_total_bytes is unavailable or unsupported."
        ));
      }
    }
    if let Some(allocated) = self.memory_allocated_bytes {
      gauge(
        metrics,
        &format!("{prefix}/shared_memory_allocated_bytes"),
        allocated as f64,
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn partition_query_errors_never_certify_whole_gpus_or_erase_known_mig() {
    let mut inventory = DeviceInventory {
      device_id: "GPU-fixture".into(),
      kind: DeviceKind::Gpu,
      name: "fixture".into(),
      total_bytes: None,
      attributes: BTreeMap::new(),
    };
    record_partition_mode(&mut inventory, Err(NvmlError::NoPermission));
    assert_eq!(inventory.attributes["partition_mode"], "unknown");
    record_partition_mode(&mut inventory, Ok(1));
    assert_eq!(inventory.attributes["partition_mode"], "mig");
    record_partition_mode(&mut inventory, Err(NvmlError::GpuLost));
    assert_eq!(inventory.attributes["partition_mode"], "mig");
    record_partition_mode(&mut inventory, Ok(0));
    assert!(!inventory.attributes.contains_key("partition_mode"));
    record_partition_mode(&mut inventory, Ok(99));
    assert_eq!(inventory.attributes["partition_mode"], "unknown");
    record_partition_mode(&mut inventory, Err(NvmlError::NotSupported));
    assert!(!inventory.attributes.contains_key("partition_mode"));
  }

  #[test]
  fn absent_gpu_sensors_do_not_become_zero_readings() {
    let mut metrics = BTreeMap::new();
    let mut warnings = Warnings::default();
    GpuReadings::default().record("GPU-absent", &mut metrics, &mut warnings);
    assert!(metrics.is_empty());
    assert!(!warnings.into_vec().is_empty());
  }

  #[test]
  fn cached_gpu_error_fallback_is_bounded_after_partial_enumeration_changes() {
    let mut cache: BTreeMap<_, _> = (0..96)
      .map(|index| {
        let id = format!("GPU-{index:03}");
        (
          id.clone(),
          DeviceInventory {
            device_id: id,
            kind: DeviceKind::Gpu,
            name: "fixture".into(),
            total_bytes: None,
            attributes: BTreeMap::new(),
          },
        )
      })
      .collect();
    let fallback = bounded_cached_inventory(&mut cache);
    assert_eq!(fallback.len(), MAX_GPUS);
    assert_eq!(cache.len(), MAX_GPUS);
    let current = vec![fallback[10].clone()];
    retain_inventory(&mut cache, &current);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&current[0].device_id));
  }

  #[test]
  fn idle_gpu_zero_and_shared_memory_are_reported_without_invented_vram() {
    let mut metrics = BTreeMap::new();
    let mut warnings = Warnings::default();
    GpuReadings {
      usage_percent: Some(0.0),
      memory_used_bytes: Some(42),
      shared_memory: true,
      ..Default::default()
    }
    .record("apple-test", &mut metrics, &mut warnings);
    assert_eq!(metrics["gpu/apple-test/usage_percent"].value, 0.0);
    assert_eq!(
      metrics["gpu/apple-test/shared_memory_used_bytes"].value,
      42.0
    );
    assert!(!metrics.contains_key("gpu/apple-test/memory_total_bytes"));
  }
}
