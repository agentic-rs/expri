//! Initial execution budgets derived from the same host inventory as system profiles.

use std::collections::{BTreeMap, BTreeSet};

use expri_core::error::Result;
use expri_core::resource_queue::{GpuVendor, WorkerCapacity, WorkerGpu};

use crate::{DeviceKind, SystemInventory};

/// Detect logical CPU slots, 90% of installed RAM, and selectable whole GPUs.
///
/// This is a host budget, not current free memory or a container resource limit.
/// GPU sensor failure does not prevent a CPU-only worker from registering.
pub fn detect_worker_capacity() -> Result<WorkerCapacity> {
  let (inventory, _) = crate::collector::Collector::new()?.collect()?;
  let amd_uids = amd_compute_uids();
  capacity_from_inventory(&inventory, &amd_uids)
}

fn capacity_from_inventory(
  inventory: &SystemInventory,
  amd_uids: &BTreeSet<String>,
) -> Result<WorkerCapacity> {
  // Avoid overflowing even before validation rejects an impossible inventory.
  let memory_bytes =
    inventory.memory_total_bytes / 10 * 9 + inventory.memory_total_bytes % 10 * 9 / 10;
  let candidates: Vec<_> = inventory
    .devices
    .iter()
    .filter(|device| device.kind == DeviceKind::Gpu)
    .filter_map(|device| {
      let (vendor, selector, memory_bytes) = match device.attributes.get("provider")?.as_str() {
        "nvml" => {
          if device.attributes.contains_key("partition_mode") {
            return None;
          }
          // Inventory IDs are driver UUIDs only for NVML, never for sysfs providers.
          uuid::Uuid::parse_str(device.device_id.strip_prefix("GPU-")?).ok()?;
          (
            GpuVendor::Nvidia,
            Some(device.device_id.clone()),
            device.total_bytes,
          )
        }
        "amdgpu_sysfs" => {
          let uid = device.attributes.get("hardware_uid")?;
          if !amd_uids.contains(uid) {
            return None;
          }
          (
            GpuVendor::Amd,
            Some(format!("GPU-{uid}")),
            device.total_bytes,
          )
        }
        "apple_iokit"
          if device.attributes.get("memory_kind").map(String::as_str) == Some("shared") =>
        {
          // Apple Silicon is a cooperative shared-memory slot, with no dedicated VRAM.
          (GpuVendor::Apple, None, None)
        }
        _ => return None,
      };
      let mut model = device.name.clone();
      while model.len() > 256 {
        model.pop();
      }
      let gpu = WorkerGpu {
        device_id: device.device_id.clone(),
        model,
        memory_bytes,
        vendor,
        selector,
      };
      // Ignore unusable provider metadata instead of disabling otherwise valid CPUs/RAM.
      WorkerCapacity {
        cpus: 1,
        memory_bytes: 1,
        gpus: vec![gpu.clone()],
      }
      .validate()
      .ok()?;
      Some(gpu)
    })
    .collect();
  let mut ids = BTreeMap::new();
  let mut selectors = BTreeMap::new();
  for gpu in &candidates {
    *ids.entry(&gpu.device_id).or_insert(0) += 1;
    if let Some(selector) = &gpu.selector {
      *selectors.entry(selector).or_insert(0) += 1;
    }
  }
  let capacity = WorkerCapacity {
    cpus: inventory.cpu.logical_cpu_count,
    memory_bytes,
    gpus: candidates
      .iter()
      .filter(|gpu| {
        ids[&gpu.device_id] == 1
          && gpu
            .selector
            .as_ref()
            .is_none_or(|selector| selectors[selector] == 1)
      })
      .take(64)
      .cloned()
      .collect(),
  };
  capacity.validate()?;
  Ok(capacity)
}

#[cfg(not(target_os = "linux"))]
fn amd_compute_uids() -> BTreeSet<String> {
  BTreeSet::new()
}

#[cfg(target_os = "linux")]
fn amd_compute_uids() -> BTreeSet<String> {
  amd_compute_uids_at(
    std::path::Path::new("/sys/class/kfd/kfd/topology/nodes"),
    std::path::Path::new("/dev/kfd"),
    std::path::Path::new("/dev/dri"),
  )
}

#[cfg(any(target_os = "linux", test))]
fn amd_compute_uids_at(
  nodes: &std::path::Path,
  kfd: &std::path::Path,
  dri: &std::path::Path,
) -> BTreeSet<String> {
  use std::fs::OpenOptions;

  // A graphics inventory is insufficient: compute also requires an accessible KFD device.
  if OpenOptions::new().read(true).write(true).open(kfd).is_err() {
    return BTreeSet::new();
  }
  let Ok(entries) = std::fs::read_dir(nodes) else {
    return BTreeSet::new();
  };
  let mut counts = BTreeMap::new();
  for entry in entries.take(512).filter_map(std::result::Result::ok) {
    let path = entry.path();
    let Some(gpu_id) =
      read_bounded(&path.join("gpu_id")).and_then(|v| v.trim().parse::<u64>().ok())
    else {
      continue;
    };
    if gpu_id == 0 {
      continue;
    }
    let Some(properties) = read_bounded(&path.join("properties")) else {
      continue;
    };
    let properties: BTreeMap<_, _> = properties
      .lines()
      .filter_map(|line| {
        let mut parts = line.split_whitespace();
        let key = parts.next()?;
        let value = parts.next()?.parse::<u64>().ok()?;
        parts.next().is_none().then_some((key, value))
      })
      .collect();
    let Some(uid) = properties.get("unique_id").copied().filter(|uid| *uid != 0) else {
      continue;
    };
    let Some(render) = properties
      .get("drm_render_minor")
      .filter(|minor| **minor >= 128)
    else {
      continue;
    };
    if properties.get("vendor_id") != Some(&0x1002)
      || properties.get("simd_count").is_none_or(|count| *count == 0)
      || OpenOptions::new()
        .read(true)
        .write(true)
        .open(dri.join(format!("renderD{render}")))
        .is_err()
    {
      continue;
    }
    // ROCr's GPU UUID is GPU-<KFD UniqueID as 16 hex digits>. Match that exact UID
    // to the DRM inventory; hashed inventory IDs and DRM card numbers are not selectors.
    *counts.entry(format!("{uid:016x}")).or_insert(0) += 1;
  }
  counts
    .into_iter()
    .filter_map(|(uid, count)| (count == 1).then_some(uid))
    .collect()
}

#[cfg(any(target_os = "linux", test))]
fn read_bounded(path: &std::path::Path) -> Option<String> {
  use std::io::Read;

  let mut value = String::new();
  std::fs::File::open(path)
    .ok()?
    .take(8193)
    .read_to_string(&mut value)
    .ok()?;
  (value.len() <= 8192).then_some(value)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{CpuInventory, DeviceInventory};

  fn inventory(devices: Vec<DeviceInventory>) -> SystemInventory {
    SystemInventory {
      observed_at_ms: 1,
      hostname: "fixture".into(),
      os_name: None,
      os_version: None,
      kernel_version: None,
      architecture: "fixture".into(),
      cpu: CpuInventory {
        brand: "fixture".into(),
        vendor: "fixture".into(),
        logical_cpu_count: 16,
        physical_core_count: Some(8),
      },
      memory_total_bytes: 32 * 1024 * 1024 * 1024,
      measurement_scope: "host".into(),
      devices,
      warnings: vec![],
    }
  }

  fn gpu(id: &str, provider: &str) -> DeviceInventory {
    DeviceInventory {
      device_id: id.into(),
      kind: DeviceKind::Gpu,
      name: "Fixture GPU".into(),
      total_bytes: Some(8 * 1024 * 1024 * 1024),
      attributes: BTreeMap::from([("provider".into(), provider.into())]),
    }
  }

  #[test]
  fn cpu_only_capacity_uses_logical_cores_and_reserves_ten_percent_ram() {
    let inventory = inventory(vec![]);
    let capacity = capacity_from_inventory(&inventory, &BTreeSet::new()).unwrap();
    assert_eq!(capacity.cpus, 16);
    assert_eq!(capacity.memory_bytes, 30_923_764_531);
    assert!(capacity.gpus.is_empty());
  }

  #[test]
  fn capacity_rejects_missing_cpu_or_memory_and_handles_extreme_memory_without_overflow() {
    let mut inventory = inventory(vec![]);
    inventory.cpu.logical_cpu_count = 0;
    assert!(capacity_from_inventory(&inventory, &BTreeSet::new()).is_err());
    inventory.cpu.logical_cpu_count = 16;
    inventory.memory_total_bytes = 0;
    assert!(capacity_from_inventory(&inventory, &BTreeSet::new()).is_err());
    inventory.memory_total_bytes = u64::MAX;
    assert!(capacity_from_inventory(&inventory, &BTreeSet::new()).is_err());
  }

  #[test]
  fn nvidia_uses_actual_uuid_and_apple_has_no_dedicated_memory() {
    let id = "GPU-01234567-89ab-cdef-0123-456789abcdef";
    let mut apple = gpu("apple-fixture", "apple_iokit");
    apple
      .attributes
      .insert("memory_kind".into(), "shared".into());
    let capacity = capacity_from_inventory(
      &inventory(vec![gpu(id, "nvml"), apple, gpu("unknown", "other")]),
      &BTreeSet::new(),
    )
    .unwrap();
    assert_eq!(capacity.gpus.len(), 2);
    assert_eq!(capacity.gpus[0].selector.as_deref(), Some(id));
    assert_eq!(capacity.gpus[0].vendor, GpuVendor::Nvidia);
    assert_eq!(capacity.gpus[1].vendor, GpuVendor::Apple);
    assert_eq!(capacity.gpus[1].memory_bytes, None);
    assert_eq!(capacity.gpus[1].selector, None);
  }

  #[test]
  fn amd_requires_matching_compute_uid_and_never_uses_hashed_id_or_card_ordinal() {
    let mut amd = gpu("amd-hashed-inventory-id", "amdgpu_sysfs");
    amd
      .attributes
      .insert("hardware_uid".into(), "0123456789abcdef".into());
    let inventory = inventory(vec![amd]);
    assert!(
      capacity_from_inventory(&inventory, &BTreeSet::new())
        .unwrap()
        .gpus
        .is_empty()
    );
    let capacity =
      capacity_from_inventory(&inventory, &BTreeSet::from(["0123456789abcdef".into()])).unwrap();
    assert_eq!(
      capacity.gpus[0].selector.as_deref(),
      Some("GPU-0123456789abcdef")
    );
  }

  #[test]
  fn invalid_or_ambiguous_gpu_metadata_is_omitted_without_disabling_cpu_worker() {
    let id = "GPU-01234567-89ab-cdef-0123-456789abcdef";
    let mut invalid = gpu(id, "nvml");
    invalid.total_bytes = Some(0);
    let mut partitioned = gpu("GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "nvml");
    partitioned
      .attributes
      .insert("partition_mode".into(), "mig".into());
    let mut unknown_partition = gpu("GPU-bbbbbbbb-cccc-dddd-eeee-ffffffffffff", "nvml");
    unknown_partition
      .attributes
      .insert("partition_mode".into(), "unknown".into());
    let inventory = inventory(vec![
      gpu(id, "nvml"),
      gpu(id, "nvml"),
      gpu("GPU-arbitrary-id", "nvml"),
      gpu("intel-mac", "apple_iokit"),
      invalid,
      partitioned,
      unknown_partition,
    ]);
    let capacity = capacity_from_inventory(&inventory, &BTreeSet::new()).unwrap();
    assert!(capacity.gpus.is_empty());
    assert_eq!(capacity.cpus, 16);
  }

  #[test]
  #[cfg(any(target_os = "linux", target_os = "macos"))]
  fn detected_host_capacity_is_valid_without_gpu_driver_requirements() {
    let capacity = detect_worker_capacity().unwrap();
    capacity.validate().unwrap();
  }

  #[test]
  fn amd_compute_fixture_requires_driver_access_and_unique_gpu_uid() {
    let root = tempfile::tempdir().unwrap();
    let nodes = root.path().join("nodes");
    let dri = root.path().join("dri");
    let kfd = root.path().join("kfd");
    std::fs::create_dir_all(nodes.join("1")).unwrap();
    std::fs::create_dir_all(&dri).unwrap();
    std::fs::write(nodes.join("1/gpu_id"), "42").unwrap();
    let properties = "vendor_id 4098\nsimd_count 64\nunique_id 4660\ndrm_render_minor 128\n";
    std::fs::write(nodes.join("1/properties"), properties).unwrap();
    assert!(amd_compute_uids_at(&nodes, &kfd, &dri).is_empty());
    std::fs::write(&kfd, "").unwrap();
    assert!(amd_compute_uids_at(&nodes, &kfd, &dri).is_empty());
    std::fs::write(dri.join("renderD128"), "").unwrap();
    assert_eq!(
      amd_compute_uids_at(&nodes, &kfd, &dri),
      BTreeSet::from(["0000000000001234".into()])
    );
    std::fs::create_dir(nodes.join("2")).unwrap();
    std::fs::write(nodes.join("2/gpu_id"), "43").unwrap();
    std::fs::write(nodes.join("2/properties"), properties).unwrap();
    assert!(amd_compute_uids_at(&nodes, &kfd, &dri).is_empty());
    std::fs::write(nodes.join("2/gpu_id"), "0").unwrap();
    assert_eq!(amd_compute_uids_at(&nodes, &kfd, &dri).len(), 1);
  }
}
