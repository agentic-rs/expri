//! Lightweight host readings; collection never enumerates processes or invokes a shell.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use expri_core::error::{ExpriError, Result};
use sha2::{Digest, Sha256};
use sysinfo::{CpuRefreshKind, DiskRefreshKind, Disks, Networks, System};

use crate::{
  CpuInventory, DeviceInventory, DeviceKind, MAX_INTERVAL_MS, MetricKind, MetricReading,
  SystemInventory, SystemSample,
};

mod disk_io;
mod gpu;
mod network_io;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
const MAX_CONFIGURED_INTERVAL: Duration = Duration::from_secs(60);
const MAX_DISKS: usize = 96;
const MAX_NETWORKS: usize = 96;
const MAX_WARNINGS: usize = 32;

pub struct Collector {
  system: System,
  disks: Disks,
  networks: Networks,
  gpu: gpu::GpuCollector,
  inventory: SystemInventory,
  device_inventory: BTreeMap<String, DeviceInventory>,
  counters: CounterTracker,
  timeline: Timeline,
  session_id: String,
}

impl Collector {
  pub fn new() -> Result<Self> {
    Self::with_interval(DEFAULT_INTERVAL)
  }

  pub fn with_interval(interval: Duration) -> Result<Self> {
    if !sysinfo::IS_SUPPORTED_SYSTEM {
      return Err(message("system collection is unsupported on this platform"));
    }
    if interval < Duration::from_secs(1) || interval > MAX_CONFIGURED_INTERVAL {
      return Err(message(
        "system collection interval must be between 1 and 60 seconds",
      ));
    }
    let mut system = System::new();
    system.refresh_cpu_specifics(CpuRefreshKind::everything());
    system.refresh_memory();
    let first_cpu = system
      .cpus()
      .first()
      .ok_or_else(|| message("CPU inventory is unavailable"))?;
    let mut warnings = Warnings::default();
    if !cfg!(target_os = "linux") {
      warnings
        .push("Disk I/O counters are unavailable on this platform; disk capacity is collected.");
    }
    let inventory = SystemInventory {
      observed_at_ms: chrono::Utc::now().timestamp_millis(),
      hostname: clean_text(
        &System::host_name().unwrap_or_else(|| "unknown".into()),
        512,
      ),
      os_name: System::name().map(|value| clean_text(&value, 512)),
      os_version: System::os_version().map(|value| clean_text(&value, 512)),
      kernel_version: System::kernel_version().map(|value| clean_text(&value, 512)),
      architecture: std::env::consts::ARCH.into(),
      cpu: CpuInventory {
        brand: clean_text(first_cpu.brand(), 512),
        vendor: clean_text(first_cpu.vendor_id(), 512),
        logical_cpu_count: system
          .cpus()
          .len()
          .try_into()
          .map_err(|_| message("CPU count exceeds its limit"))?,
        physical_core_count: System::physical_core_count().and_then(|value| value.try_into().ok()),
      },
      memory_total_bytes: system.total_memory(),
      measurement_scope:
        "system readings visible to the collector; not per-process or cgroup limits".into(),
      devices: Vec::new(),
      warnings: warnings.into_vec(),
    };
    Ok(Self {
      system,
      disks: Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing().with_storage()),
      networks: Networks::new_with_refreshed_list(),
      gpu: gpu::GpuCollector::new(),
      inventory,
      device_inventory: BTreeMap::new(),
      counters: CounterTracker::default(),
      timeline: Timeline {
        interval,
        previous: None,
      },
      session_id: crate::new_session_id(),
    })
  }

  /// Sequence and inventory revision are placeholders assigned transactionally by LocalStore.
  pub fn collect(&mut self) -> Result<(SystemInventory, SystemSample)> {
    let started = Instant::now();
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    let mut warnings = Warnings::default();
    let interval_ms = self
      .timeline
      .observe(started, observed_at_ms, &mut warnings);
    self
      .system
      .refresh_cpu_specifics(CpuRefreshKind::everything());
    self.system.refresh_memory();
    self.networks.refresh(true);
    self
      .disks
      .refresh_specifics(true, DiskRefreshKind::nothing().with_storage());

    let mut metrics = BTreeMap::new();
    let mut counters = BTreeMap::new();
    if interval_ms > 0 {
      gauge(
        &mut metrics,
        "cpu/usage_percent",
        f64::from(self.system.global_cpu_usage()),
      );
    }
    let frequencies: Vec<_> = self
      .system
      .cpus()
      .iter()
      .map(|cpu| cpu.frequency())
      .filter(|value| *value > 0)
      .collect();
    if frequencies.len() == self.system.cpus().len() && !frequencies.is_empty() {
      let mean =
        frequencies.iter().map(|value| *value as f64).sum::<f64>() / frequencies.len() as f64;
      gauge(&mut metrics, "cpu/frequency_mhz", mean);
    } else {
      warnings.push("CPU frequency is unavailable for one or more logical CPUs.");
    }
    if cfg!(unix) {
      let load = System::load_average();
      for (name, value) in [
        ("cpu/load_1m", load.one),
        ("cpu/load_5m", load.five),
        ("cpu/load_15m", load.fifteen),
      ] {
        gauge(&mut metrics, name, value);
      }
    } else {
      warnings.push("CPU load averages are unavailable on this platform.");
    }
    if self.system.total_memory() > 0 {
      for (name, value) in [
        ("memory/total_bytes", self.system.total_memory()),
        ("memory/available_bytes", self.system.available_memory()),
        ("memory/used_bytes", self.system.used_memory()),
        ("memory/swap_total_bytes", self.system.total_swap()),
        ("memory/swap_used_bytes", self.system.used_swap()),
      ] {
        gauge(&mut metrics, name, value as f64);
      }
    } else {
      warnings.push("System memory readings are unavailable.");
    }

    let mut devices = self.gpu.collect(&mut metrics, &mut warnings);
    let disk_io = disk_io::read(&mut warnings);
    if self.disks.len() > MAX_DISKS {
      warnings.push("Disk inventory is limited to 96 mounted filesystems.");
    }
    let mut disks: Vec<_> = self.disks.iter().collect();
    disks.sort_by_key(|disk| (disk.mount_point().to_path_buf(), disk.name().to_os_string()));
    for disk in disks.into_iter().take(MAX_DISKS) {
      let mount = disk.mount_point().as_os_str().as_encoded_bytes();
      let name = disk.name().as_encoded_bytes();
      let id = stable_id("disk", &[mount, name]);
      let inventory = self
        .device_inventory
        .entry(id.clone())
        .or_insert_with(|| DeviceInventory {
          device_id: id.clone(),
          kind: DeviceKind::Disk,
          name: clean_text(&disk.name().to_string_lossy(), 1024),
          // Capacity can vary on shared filesystems; keep the initial inventory value.
          total_bytes: (disk.total_space() > 0).then_some(disk.total_space()),
          attributes: BTreeMap::from([
            (
              "io_scope".into(),
              "block device; multiple mounts can share the same counters".into(),
            ),
            (
              "mount_point".into(),
              clean_text(&disk.mount_point().to_string_lossy(), 1024),
            ),
            (
              "file_system".into(),
              clean_text(&disk.file_system().to_string_lossy(), 1024),
            ),
            (
              "io_provider".into(),
              if cfg!(target_os = "linux") {
                "proc_diskstats"
              } else {
                "unavailable"
              }
              .into(),
            ),
          ]),
        });
      fill_metadata(
        inventory,
        Some(&disk.name().to_string_lossy()),
        (disk.total_space() > 0).then_some(disk.total_space()),
      );
      devices.push(inventory.clone());
      if disk.total_space() > 0 {
        gauge(
          &mut metrics,
          &format!("disk/{id}/total_bytes"),
          disk.total_space() as f64,
        );
        gauge(
          &mut metrics,
          &format!("disk/{id}/available_bytes"),
          disk.available_space() as f64,
        );
      } else {
        warnings.push(format!("Disk capacity is unavailable for {id}."));
      }
      if let Some(usage) = disk_io
        .as_ref()
        .and_then(|values| disk_io::for_disk(values, disk.name()))
      {
        counters.insert(format!("disk/{id}/read_bytes"), usage.read_bytes);
        counters.insert(format!("disk/{id}/written_bytes"), usage.written_bytes);
      } else if cfg!(target_os = "linux") {
        warnings.push(format!(
          "Disk I/O is unavailable for {id}; this mount may not map to a visible block device."
        ));
      }
    }

    if self.networks.len() > MAX_NETWORKS {
      warnings.push("Network inventory is limited to 96 interfaces.");
    }
    let mut networks: Vec<_> = self.networks.iter().collect();
    networks.sort_by_key(|(name, _)| *name);
    for (name, data) in networks.into_iter().take(MAX_NETWORKS) {
      let mac = data.mac_address().to_string();
      let id = stable_id("network", &[name.as_bytes(), mac.as_bytes()]);
      let inventory = self
        .device_inventory
        .entry(id.clone())
        .or_insert_with(|| DeviceInventory {
          device_id: id.clone(),
          kind: DeviceKind::Network,
          name: clean_text(name, 1024),
          total_bytes: None,
          attributes: BTreeMap::from([("mac_address".into(), mac)]),
        });
      devices.push(inventory.clone());
      let readings = if cfg!(target_os = "linux") {
        network_io::read(name, &mut warnings)
      } else {
        BTreeMap::from([
          ("received_bytes", data.total_received()),
          ("transmitted_bytes", data.total_transmitted()),
          ("received_packets", data.total_packets_received()),
          ("transmitted_packets", data.total_packets_transmitted()),
          ("receive_errors", data.total_errors_on_received()),
          ("transmit_errors", data.total_errors_on_transmitted()),
        ])
      };
      for (metric, value) in readings {
        counters.insert(format!("network/{id}/{metric}"), value);
      }
    }
    metrics.extend(
      self
        .counters
        .observe(&counters, interval_ms > 0, &mut warnings),
    );
    devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));
    devices.dedup_by(|left, right| left.device_id == right.device_id);
    let active: BTreeSet<_> = devices
      .iter()
      .map(|device| device.device_id.clone())
      .collect();
    self.device_inventory.retain(|id, _| active.contains(id));
    let mut inventory = self.inventory.clone();
    inventory.observed_at_ms = observed_at_ms;
    inventory.devices = devices;
    let sample = SystemSample {
      sequence: 0,
      session_id: self.session_id.clone(),
      observed_at_ms,
      interval_ms,
      collection_duration_ms: milliseconds(started.elapsed()),
      inventory_revision: 0,
      metrics,
      counters,
      warnings: warnings.into_vec(),
    };
    Ok((inventory, sample))
  }
}

#[derive(Default)]
struct CounterTracker {
  previous: BTreeMap<String, u64>,
}

impl CounterTracker {
  fn observe(
    &mut self,
    current: &BTreeMap<String, u64>,
    valid_interval: bool,
    warnings: &mut Warnings,
  ) -> BTreeMap<String, MetricReading> {
    let mut readings = BTreeMap::new();
    if valid_interval {
      for (name, value) in current {
        if let Some(previous) = self.previous.get(name) {
          if let Some(delta) = value.checked_sub(*previous) {
            readings.insert(
              name.clone(),
              MetricReading {
                kind: MetricKind::CounterDelta,
                value: delta as f64,
              },
            );
          } else {
            warnings.push(format!(
              "Counter reset for {name}; this interval's delta is unavailable."
            ));
          }
        } else {
          warnings.push(format!(
            "Counter {name} is warming up after device discovery or an unavailable reading."
          ));
        }
      }
    }
    self.previous.clone_from(current);
    readings
  }
}

struct Timeline {
  interval: Duration,
  previous: Option<(Instant, i64)>,
}

impl Timeline {
  fn observe(&mut self, now: Instant, wall_ms: i64, warnings: &mut Warnings) -> u64 {
    let previous = self.previous.replace((now, wall_ms));
    let Some((previous, previous_wall)) = previous else {
      warnings.push("CPU utilization and counter deltas are warming up; the first measured interval is unavailable.");
      return 0;
    };
    let elapsed = now.saturating_duration_since(previous);
    let interval_ms = milliseconds(elapsed);
    let wall_elapsed = wall_ms.checked_sub(previous_wall);
    if wall_elapsed.is_none_or(|value| value <= 0 || value.abs_diff(interval_ms as i64) > 100) {
      warnings.push("The system clock moved or the machine slept; interval coverage and counter deltas are unavailable.");
      return 0;
    }
    if elapsed > self.interval.saturating_mul(2) || interval_ms > MAX_INTERVAL_MS {
      warnings.push("Collection was delayed beyond the sampling window; interval coverage and counter deltas are unavailable.");
      return 0;
    }
    if elapsed < sysinfo::MINIMUM_CPU_UPDATE_INTERVAL {
      warnings.push("Collection interval was too short for CPU utilization and counter deltas.");
      return 0;
    }
    interval_ms
  }
}

#[derive(Default)]
struct Warnings {
  values: Vec<String>,
}

impl Warnings {
  fn push(&mut self, value: impl AsRef<str>) {
    let value = clean_text(value.as_ref(), 512);
    if self.values.contains(&value) {
      return;
    }
    if self.values.len() < MAX_WARNINGS - 1 {
      self.values.push(value);
    } else if self.values.len() == MAX_WARNINGS - 1 {
      self
        .values
        .push("Additional sensor warnings were omitted.".into());
    }
  }

  fn into_vec(self) -> Vec<String> {
    self.values
  }
}

fn gauge(metrics: &mut BTreeMap<String, MetricReading>, name: &str, value: f64) {
  if value.is_finite() {
    metrics.insert(
      name.into(),
      MetricReading {
        kind: MetricKind::Gauge,
        value,
      },
    );
  }
}

fn fill_metadata(inventory: &mut DeviceInventory, name: Option<&str>, total_bytes: Option<u64>) {
  let unknown_name = inventory.name.is_empty()
    || inventory
      .attributes
      .get("model_status")
      .is_some_and(|value| value == "unavailable");
  if unknown_name
    && let Some(name) = name
      .map(|name| clean_text(name, 1024))
      .filter(|name| !name.is_empty())
  {
    inventory.name = name;
    inventory.attributes.remove("model_status");
  }
  if inventory.total_bytes.is_none() {
    inventory.total_bytes = total_bytes;
  }
}

fn clean_text(value: &str, limit: usize) -> String {
  let mut cleaned = String::new();
  for character in value.chars().filter(|character| !character.is_control()) {
    if cleaned.len() + character.len_utf8() > limit {
      break;
    }
    cleaned.push(character);
  }
  cleaned
}

fn stable_id(prefix: &str, parts: &[&[u8]]) -> String {
  let mut hash = Sha256::new();
  hash.update(prefix.as_bytes());
  for part in parts {
    hash.update((part.len() as u64).to_le_bytes());
    hash.update(part);
  }
  let digest = hash.finalize();
  let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
  format!("{prefix}-{hex}")
}

fn milliseconds(duration: Duration) -> u64 {
  duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests;
