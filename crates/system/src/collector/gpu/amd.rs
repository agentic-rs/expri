//! Optional AMDGPU sysfs readings; no privileged driver controls are touched.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{GpuReadings, MAX_GPUS, REDISCOVER_AFTER, Warnings};
use crate::collector::{clean_text, fill_metadata, stable_id};
use crate::{DeviceInventory, DeviceKind, MetricReading};

pub(super) struct AmdCollector {
  devices: Vec<AmdDevice>,
  last_discovery: Instant,
  discovery_error: Option<String>,
}

struct AmdDevice {
  path: PathBuf,
  hwmon: Option<PathBuf>,
  inventory: DeviceInventory,
}

impl AmdCollector {
  #[cfg(target_os = "linux")]
  pub fn new() -> Self {
    let mut collector = Self {
      devices: Vec::new(),
      last_discovery: Instant::now(),
      discovery_error: None,
    };
    collector.rediscover(Path::new("/sys/class/drm"));
    collector
  }

  fn rediscover(&mut self, root: &Path) {
    match discover(root) {
      Ok(mut devices) => {
        for device in &mut devices {
          if let Some(previous) = self.devices.iter().find(|old| old.path == device.path) {
            let old_uid = previous.inventory.attributes.get("hardware_uid");
            let new_uid = device.inventory.attributes.get("hardware_uid");
            if let Some(old_uid) = old_uid.filter(|_| new_uid.is_none()) {
              device.inventory.device_id = previous.inventory.device_id.clone();
              device
                .inventory
                .attributes
                .insert("hardware_uid".into(), old_uid.clone());
            }
            if device.inventory.device_id == previous.inventory.device_id {
              let name = (!device.inventory.attributes.contains_key("model_status"))
                .then_some(device.inventory.name.clone());
              let total_bytes = device.inventory.total_bytes;
              device.inventory.name = previous.inventory.name.clone();
              device.inventory.total_bytes = previous.inventory.total_bytes;
              if let Some(status) = previous.inventory.attributes.get("model_status") {
                device
                  .inventory
                  .attributes
                  .insert("model_status".into(), status.clone());
              } else {
                device.inventory.attributes.remove("model_status");
              }
              fill_metadata(&mut device.inventory, name.as_deref(), total_bytes);
            }
          }
        }
        self.devices = devices;
        self.discovery_error = None;
      }
      Err(error) => {
        self.discovery_error = Some(clean_text(&error.to_string(), 384));
      }
    }
    self.last_discovery = Instant::now();
  }

  pub fn collect(
    &mut self,
    limit: usize,
    metrics: &mut BTreeMap<String, MetricReading>,
    warnings: &mut Warnings,
  ) -> Vec<DeviceInventory> {
    if self.last_discovery.elapsed() >= REDISCOVER_AFTER {
      self.rediscover(Path::new("/sys/class/drm"));
    }
    if let Some(error) = &self.discovery_error {
      warnings.push(format!(
        "AMD GPU discovery is unavailable; known inventory was retained: {error}"
      ));
    }
    if self.devices.len() > limit {
      warnings.push("GPU inventory is limited to 64 devices.");
    }
    self
      .devices
      .iter_mut()
      .take(limit)
      .map(|device| {
        let total_bytes = number(&device.path.join("mem_info_vram_total"));
        let model = device
          .inventory
          .attributes
          .contains_key("model_status")
          .then(|| text(&device.path.join("product_name")))
          .flatten();
        fill_metadata(&mut device.inventory, model.as_deref(), total_bytes);
        let readings = GpuReadings {
          usage_percent: number(&device.path.join("gpu_busy_percent")).map(|value| value as f64),
          memory_used_bytes: number(&device.path.join("mem_info_vram_used")),
          memory_total_bytes: total_bytes,
          temperature_celsius: device
            .hwmon
            .as_ref()
            .and_then(|path| number(&path.join("temp1_input")))
            .map(|value| value as f64 / 1000.0),
          power_watts: device
            .hwmon
            .as_ref()
            .and_then(|path| {
              number(&path.join("power1_average")).or_else(|| number(&path.join("power1_input")))
            })
            .map(|value| value as f64 / 1_000_000.0),
          ..Default::default()
        };
        readings.record(&device.inventory.device_id, metrics, warnings);
        device.inventory.clone()
      })
      .collect()
  }
}

fn discover(root: &Path) -> io::Result<Vec<AmdDevice>> {
  let entries = match std::fs::read_dir(root) {
    Ok(entries) => entries,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(error),
  };
  let mut paths = Vec::new();
  for entry in entries {
    let entry = entry?;
    let is_card = {
      let name = entry.file_name();
      name
        .to_str()
        .and_then(|name| name.strip_prefix("card"))
        .is_some_and(|number| {
          !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
    };
    if is_card {
      paths.push(entry.path().join("device"));
    }
  }
  paths.sort();
  let mut devices = Vec::new();
  for path in paths {
    let vendor = match read_text(&path.join("vendor")) {
      Ok(vendor) => vendor,
      Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
      Err(error) => return Err(error),
    };
    if vendor != "0x1002" {
      continue;
    }
    let path = std::fs::canonicalize(path)?;
    let uid = text(&path.join("unique_id")).and_then(|value| hardware_uid(&value));
    let id = match &uid {
      Some(uid) => stable_id("amd", &[uid.as_bytes()]),
      None => stable_id("amd", &[path.as_os_str().as_encoded_bytes()]),
    };
    let model = text(&path.join("product_name")).filter(|value| !value.is_empty());
    let name = model.clone().unwrap_or_else(|| {
      format!(
        "AMD GPU {}",
        text(&path.join("device")).unwrap_or_else(|| "unknown model".into())
      )
    });
    let inventory = DeviceInventory {
      device_id: id,
      kind: DeviceKind::Gpu,
      name: clean_text(&name, 1024),
      total_bytes: number(&path.join("mem_info_vram_total")),
      attributes: {
        let mut attributes = BTreeMap::from([
          ("provider".into(), "amdgpu_sysfs".into()),
          ("vendor".into(), "AMD".into()),
          ("memory_kind".into(), "dedicated".into()),
          (
            "pci_address".into(),
            clean_text(
              &path.file_name().unwrap_or_default().to_string_lossy(),
              1024,
            ),
          ),
        ]);
        if model.is_none() {
          attributes.insert("model_status".into(), "unavailable".into());
        }
        if let Some(uid) = uid {
          attributes.insert("hardware_uid".into(), uid);
        }
        attributes
      },
    };
    let hwmon = std::fs::read_dir(path.join("hwmon"))
      .ok()
      .and_then(|entries| {
        let mut paths: Vec<_> = entries
          .filter_map(std::result::Result::ok)
          .map(|entry| entry.path())
          .collect();
        paths.sort();
        paths
          .into_iter()
          .find(|path| text(&path.join("name")).as_deref() == Some("amdgpu"))
      });
    devices.push(AmdDevice {
      path,
      hwmon,
      inventory,
    });
    if devices.len() == MAX_GPUS {
      break;
    }
  }
  Ok(devices)
}

fn hardware_uid(value: &str) -> Option<String> {
  let value = value.trim();
  let value = value
    .strip_prefix("0x")
    .or_else(|| value.strip_prefix("0X"))
    .unwrap_or(value);
  u64::from_str_radix(value, 16)
    .ok()
    .filter(|value| *value != 0)
    .map(|value| format!("{value:016x}"))
}

fn read_text(path: &Path) -> io::Result<String> {
  let mut value = String::new();
  std::fs::File::open(path)?
    .take(257)
    .read_to_string(&mut value)?;
  if value.len() > 256 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "AMD sysfs value exceeds its limit",
    ));
  }
  Ok(value.trim().to_owned())
}

fn text(path: &Path) -> Option<String> {
  read_text(path).ok()
}

fn number(path: &Path) -> Option<u64> {
  text(path)?.parse().ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sysfs_fixture_preserves_available_values_and_omits_missing_sensors() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("card0/device");
    std::fs::create_dir_all(&path).unwrap();
    for (name, value) in [
      ("vendor", "0x1002"),
      ("device", "0x744c"),
      ("unique_id", "abcd1234"),
      ("product_name", "Fixture GPU"),
      ("mem_info_vram_total", "17179869184"),
      ("mem_info_vram_used", "4096"),
      ("gpu_busy_percent", "0"),
    ] {
      std::fs::write(path.join(name), value).unwrap();
    }
    let devices = discover(root.path()).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].inventory.name, "Fixture GPU");
    let mut collector = AmdCollector {
      devices,
      last_discovery: Instant::now(),
      discovery_error: None,
    };
    let mut metrics = BTreeMap::new();
    let mut warnings = Warnings::default();
    let inventory = collector.collect(64, &mut metrics, &mut warnings);
    let prefix = format!("gpu/{}", inventory[0].device_id);
    assert_eq!(metrics[&format!("{prefix}/usage_percent")].value, 0.0);
    assert_eq!(
      metrics[&format!("{prefix}/memory_used_bytes")].value,
      4096.0
    );
    assert!(!metrics.contains_key(&format!("{prefix}/power_watts")));
    assert!(!metrics.contains_key(&format!("{prefix}/temperature_celsius")));
  }

  #[test]
  fn missing_drm_directory_is_an_empty_optional_provider() {
    let root = tempfile::tempdir().unwrap();
    assert!(discover(&root.path().join("absent")).unwrap().is_empty());
  }

  fn fixture(root: &Path, uid: Option<&str>) -> PathBuf {
    let path = root.join("card0/device");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("vendor"), "0x1002").unwrap();
    if let Some(uid) = uid {
      std::fs::write(path.join("unique_id"), uid).unwrap();
    }
    path
  }

  #[test]
  fn hardware_identity_survives_pci_location_changes_and_zero_uid_uses_path() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_path = fixture(first.path(), Some("abcd1234"));
    let second_path = fixture(second.path(), Some("0X00000000ABCD1234"));
    let first_device = discover(first.path()).unwrap().remove(0);
    let second_device = discover(second.path()).unwrap().remove(0);
    assert_ne!(first_path, second_path);
    assert_eq!(
      first_device.inventory.device_id,
      second_device.inventory.device_id
    );
    std::fs::write(first_path.join("unique_id"), "0000000000000000").unwrap();
    std::fs::remove_file(second_path.join("unique_id")).unwrap();
    assert_ne!(
      discover(first.path()).unwrap()[0].inventory.device_id,
      discover(second.path()).unwrap()[0].inventory.device_id
    );
    assert!(hardware_uid("not-hex").is_none());
  }

  #[test]
  fn failed_discovery_retains_inventory_but_successful_empty_discovery_removes_it() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), Some("1234"));
    let devices = discover(root.path()).unwrap();
    let id = devices[0].inventory.device_id.clone();
    let mut collector = AmdCollector {
      devices,
      last_discovery: Instant::now(),
      discovery_error: None,
    };
    let invalid_root = root.path().join("not-a-directory");
    std::fs::write(&invalid_root, "invalid root").unwrap();
    collector.rediscover(&invalid_root);
    let mut warnings = Warnings::default();
    let inventory = collector.collect(MAX_GPUS, &mut BTreeMap::new(), &mut warnings);
    assert_eq!(inventory[0].device_id, id);
    assert!(
      warnings
        .into_vec()
        .iter()
        .any(|warning| warning.contains("known inventory was retained"))
    );
    let empty_root = root.path().join("empty");
    std::fs::create_dir(&empty_root).unwrap();
    collector.rediscover(&empty_root);
    assert!(collector.devices.is_empty());
    assert!(collector.discovery_error.is_none());
  }

  #[test]
  fn unavailable_metadata_recovers_and_known_metadata_survives_failed_reads() {
    let root = tempfile::tempdir().unwrap();
    let path = fixture(root.path(), Some("1234"));
    let mut collector = AmdCollector {
      devices: discover(root.path()).unwrap(),
      last_discovery: Instant::now(),
      discovery_error: None,
    };
    let id = collector.devices[0].inventory.device_id.clone();
    assert!(collector.devices[0].inventory.total_bytes.is_none());
    assert!(
      collector.devices[0]
        .inventory
        .attributes
        .contains_key("model_status")
    );
    std::fs::write(path.join("product_name"), "Recovered GPU").unwrap();
    std::fs::write(path.join("mem_info_vram_total"), "8192").unwrap();
    let inventory = collector.collect(MAX_GPUS, &mut BTreeMap::new(), &mut Warnings::default());
    assert_eq!(inventory[0].name, "Recovered GPU");
    assert_eq!(inventory[0].total_bytes, Some(8192));
    assert!(!inventory[0].attributes.contains_key("model_status"));
    for name in ["unique_id", "product_name", "mem_info_vram_total"] {
      std::fs::remove_file(path.join(name)).unwrap();
    }
    collector.rediscover(root.path());
    assert_eq!(collector.devices[0].inventory.device_id, id);
    assert_eq!(collector.devices[0].inventory.name, "Recovered GPU");
    assert_eq!(collector.devices[0].inventory.total_bytes, Some(8192));
    assert_eq!(
      collector.devices[0].inventory.attributes["hardware_uid"],
      "0000000000001234"
    );
  }
}
