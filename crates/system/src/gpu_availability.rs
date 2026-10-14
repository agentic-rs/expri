//! Fresh compute-context checks for whole GPUs offered to the shared scheduler.

use std::collections::BTreeMap;

use expri_core::resource_queue::{
  GpuAvailabilityReport, GpuUnavailableReason, GpuVendor, UnavailableGpu, WorkerCapacity, WorkerGpu,
};
use nvml_wrapper::{Nvml, enums::device::UsedGpuMemory, error::NvmlError};

/// Check configured GPU execution selectors without inspecting process names or owners.
///
/// A successful NVIDIA compute-process query includes active CUDA contexts, including
/// the MPS server that owns its clients' context. Graphics-only contexts are omitted.
/// A device is busy when any process individually exceeds `threshold_bytes`; memory
/// is never summed across processes. Zero means any compute context blocks, regardless
/// of its memory. With a positive threshold, unknown memory leaves the device unverified
/// unless another process proves it busy. Process identifiers are never inspected.
/// Unsupported providers, missing drivers, partitioned devices, query errors, and
/// selectors that cannot be mapped reliably are reported as unverified, never free.
/// Numeric selectors are unverified because NVML and CUDA indices can differ.
/// Callers must pass a validated capacity and treat this as a point-in-time observation.
pub fn probe_gpu_availability(
  capacity: &WorkerCapacity,
  threshold_bytes: u64,
) -> GpuAvailabilityReport {
  let nvml = capacity
    .gpus
    .iter()
    .any(|gpu| gpu.vendor == GpuVendor::Nvidia)
    .then(Nvml::init)
    .and_then(std::result::Result::ok);
  probe_with(&capacity.gpus, threshold_bytes, |selector| {
    read_nvidia(nvml.as_ref()?, selector)
  })
}

struct NvidiaObservation {
  uuid: uuid::Uuid,
  partitioned: Option<bool>,
  compute_process_memory: Option<Vec<UsedGpuMemory>>,
}

fn read_nvidia(nvml: &Nvml, selector: &str) -> Option<NvidiaObservation> {
  let device = nvml.device_by_uuid(selector).ok()?;
  let uuid = full_nvidia_uuid(&device.uuid().ok()?)?;
  let partitioned = match device.mig_mode() {
    Ok(mode) => match mode.current {
      0 => Some(false),
      1 => Some(true),
      _ => None,
    },
    Err(NvmlError::NotSupported) => Some(false),
    Err(_) => None,
  };
  Some(NvidiaObservation {
    uuid,
    partitioned,
    // Retain only the memory values needed by the policy; names, owners and PIDs
    // are never inspected, transmitted, or retained in the availability report.
    compute_process_memory: device.running_compute_processes().ok().map(|processes| {
      processes
        .into_iter()
        .map(|process| process.used_gpu_memory)
        .collect()
    }),
  })
}

fn full_nvidia_uuid(selector: &str) -> Option<uuid::Uuid> {
  uuid::Uuid::parse_str(selector.strip_prefix("GPU-")?).ok()
}

fn probe_with(
  gpus: &[WorkerGpu],
  threshold_bytes: u64,
  mut probe: impl FnMut(&str) -> Option<NvidiaObservation>,
) -> GpuAvailabilityReport {
  let observations: Vec<_> = gpus
    .iter()
    .map(|gpu| {
      if gpu.vendor != GpuVendor::Nvidia {
        return None;
      }
      let selector = gpu.selector.as_deref()?;
      let requested_uuid = full_nvidia_uuid(selector)?;
      let observation = probe(selector)?;
      // A readable handle is insufficient if it identifies a different device.
      (observation.uuid == requested_uuid).then_some(observation)
    })
    .collect();
  let mut identities = BTreeMap::new();
  for observation in observations.iter().flatten() {
    *identities.entry(observation.uuid).or_insert(0) += 1;
  }
  GpuAvailabilityReport {
    unavailable_gpus: gpus
      .iter()
      .zip(observations)
      .filter_map(|(gpu, observation)| {
        let reason = match observation {
          Some(observation)
            if observation.partitioned == Some(false) && identities[&observation.uuid] == 1 =>
          {
            process_memory_reason(
              observation.compute_process_memory.as_deref(),
              threshold_bytes,
            )?
          }
          _ => GpuUnavailableReason::Unverified,
        };
        Some(UnavailableGpu {
          device_id: gpu.device_id.clone(),
          reason,
        })
      })
      .collect(),
  }
}

fn process_memory_reason(
  processes: Option<&[UsedGpuMemory]>,
  threshold_bytes: u64,
) -> Option<GpuUnavailableReason> {
  let Some(processes) = processes else {
    return Some(GpuUnavailableReason::Unverified);
  };
  if threshold_bytes == 0 && !processes.is_empty()
    || processes
      .iter()
      .any(|memory| matches!(memory, UsedGpuMemory::Used(bytes) if *bytes > threshold_bytes))
  {
    return Some(GpuUnavailableReason::Busy);
  }
  processes
    .iter()
    .any(|memory| matches!(memory, UsedGpuMemory::Unavailable))
    .then_some(GpuUnavailableReason::Unverified)
}

#[cfg(test)]
mod tests {
  use super::*;

  const FIRST: &str = "GPU-01234567-89ab-cdef-0123-456789abcdef";
  const SECOND: &str = "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
  const THRESHOLD: u64 = 64 * 1024 * 1024;

  fn gpu(id: &str, vendor: GpuVendor, selector: Option<&str>) -> WorkerGpu {
    WorkerGpu {
      device_id: id.into(),
      model: "Fixture GPU".into(),
      memory_bytes: None,
      vendor,
      selector: selector.map(str::to_owned),
    }
  }

  fn observation(uuid: &str, processes: Vec<UsedGpuMemory>) -> NvidiaObservation {
    NvidiaObservation {
      uuid: full_nvidia_uuid(uuid).unwrap(),
      partitioned: Some(false),
      compute_process_memory: Some(processes),
    }
  }

  #[test]
  fn independent_gpu_status_uses_execution_selector_and_reports_configured_id() {
    let capacity = WorkerCapacity {
      cpus: 2,
      memory_bytes: 4096,
      gpus: vec![
        gpu("training-gpu", GpuVendor::Nvidia, Some(FIRST)),
        gpu("spare-gpu", GpuVendor::Nvidia, Some(SECOND)),
      ],
    };
    let report = probe_with(&capacity.gpus, THRESHOLD, |selector| match selector {
      FIRST => Some(observation(FIRST, vec![UsedGpuMemory::Used(THRESHOLD + 1)])),
      SECOND => Some(observation(SECOND, vec![UsedGpuMemory::Used(THRESHOLD)])),
      _ => panic!("unexpected selector"),
    });
    assert_eq!(
      report.unavailable_gpus,
      vec![UnavailableGpu {
        device_id: "training-gpu".into(),
        reason: GpuUnavailableReason::Busy,
      }]
    );
    report.validate_capacity(&capacity).unwrap();
  }

  #[test]
  fn unsupported_providers_or_unmapped_selectors_are_never_reported_free() {
    let gpus = vec![
      gpu("amd", GpuVendor::Amd, Some(FIRST)),
      gpu("apple", GpuVendor::Apple, None),
      gpu("ordinal", GpuVendor::Nvidia, Some("0")),
      gpu("partial-uuid", GpuVendor::Nvidia, Some("GPU-01234567")),
      gpu(
        "mig",
        GpuVendor::Nvidia,
        Some("MIG-01234567-89ab-cdef-0123-456789abcdef"),
      ),
      gpu("missing-selector", GpuVendor::Nvidia, None),
    ];
    let report = probe_with(&gpus, THRESHOLD, |_| {
      panic!("unreliable selector must not be probed")
    });
    assert_eq!(report.unavailable_gpus.len(), gpus.len());
    assert!(
      report
        .unavailable_gpus
        .iter()
        .all(|gpu| gpu.reason == GpuUnavailableReason::Unverified)
    );
  }

  #[test]
  fn failed_queries_unknown_or_enabled_partitions_and_wrong_handles_fail_closed() {
    let gpus = vec![gpu("configured", GpuVendor::Nvidia, Some(FIRST))];
    for value in [
      None,
      Some(NvidiaObservation {
        compute_process_memory: None,
        ..observation(FIRST, vec![])
      }),
      Some(NvidiaObservation {
        partitioned: None,
        ..observation(FIRST, vec![])
      }),
      Some(NvidiaObservation {
        partitioned: Some(true),
        ..observation(FIRST, vec![])
      }),
      Some(observation(SECOND, vec![])),
    ] {
      let mut value = value;
      let report = probe_with(&gpus, THRESHOLD, |_| value.take());
      assert_eq!(
        report.unavailable_gpus[0].reason,
        GpuUnavailableReason::Unverified
      );
    }
  }

  #[test]
  fn uuid_aliases_cannot_advertise_the_same_physical_gpu_as_two_free_devices() {
    let gpus = vec![
      gpu("first", GpuVendor::Nvidia, Some(SECOND)),
      gpu(
        "alias",
        GpuVendor::Nvidia,
        Some("GPU-AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"),
      ),
    ];
    let report = probe_with(&gpus, THRESHOLD, |_| Some(observation(SECOND, vec![])));
    assert_eq!(report.unavailable_gpus.len(), 2);
    assert!(
      report
        .unavailable_gpus
        .iter()
        .all(|gpu| gpu.reason == GpuUnavailableReason::Unverified)
    );
  }

  #[test]
  fn cpu_only_probe_needs_no_gpu_library_and_unsupported_gpu_fails_closed() {
    let mut capacity = WorkerCapacity {
      cpus: 2,
      memory_bytes: 4096,
      gpus: vec![],
    };
    assert!(
      probe_gpu_availability(&capacity, THRESHOLD)
        .unavailable_gpus
        .is_empty()
    );
    capacity.gpus.push(gpu("apple", GpuVendor::Apple, None));
    assert_eq!(
      probe_gpu_availability(&capacity, THRESHOLD).unavailable_gpus[0].reason,
      GpuUnavailableReason::Unverified
    );
  }

  #[test]
  fn positive_threshold_is_per_process_and_tolerates_equal_or_lower_memory() {
    for bytes in [0, 18 * 1024 * 1024, THRESHOLD - 1, THRESHOLD] {
      assert_eq!(
        process_memory_reason(Some(&[UsedGpuMemory::Used(bytes)]), THRESHOLD),
        None
      );
    }
    // The combined footprint exceeds 64 MiB, but no individual process does.
    let small = vec![UsedGpuMemory::Used(THRESHOLD); 100];
    assert_eq!(process_memory_reason(Some(&small), THRESHOLD), None);
    assert_eq!(
      process_memory_reason(Some(&[UsedGpuMemory::Used(THRESHOLD + 1)]), THRESHOLD),
      Some(GpuUnavailableReason::Busy)
    );
  }

  #[test]
  fn unknown_memory_is_unverified_unless_any_process_proves_busy() {
    for processes in [
      vec![UsedGpuMemory::Unavailable],
      vec![UsedGpuMemory::Used(THRESHOLD), UsedGpuMemory::Unavailable],
    ] {
      assert_eq!(
        process_memory_reason(Some(&processes), THRESHOLD),
        Some(GpuUnavailableReason::Unverified)
      );
    }
    for processes in [
      vec![
        UsedGpuMemory::Used(THRESHOLD + 1),
        UsedGpuMemory::Unavailable,
      ],
      vec![
        UsedGpuMemory::Unavailable,
        UsedGpuMemory::Used(THRESHOLD + 1),
      ],
    ] {
      assert_eq!(
        process_memory_reason(Some(&processes), THRESHOLD),
        Some(GpuUnavailableReason::Busy)
      );
    }
    assert_eq!(
      process_memory_reason(None, THRESHOLD),
      Some(GpuUnavailableReason::Unverified)
    );
  }

  #[test]
  fn zero_threshold_blocks_any_context_without_requiring_memory_information() {
    assert_eq!(process_memory_reason(Some(&[]), 0), None);
    for memory in [
      UsedGpuMemory::Used(0),
      UsedGpuMemory::Used(1),
      UsedGpuMemory::Unavailable,
    ] {
      assert_eq!(
        process_memory_reason(Some(&[memory]), 0),
        Some(GpuUnavailableReason::Busy)
      );
    }
    assert_eq!(
      process_memory_reason(None, 0),
      Some(GpuUnavailableReason::Unverified)
    );
  }

  #[test]
  fn memory_comparison_handles_large_unsigned_byte_values_without_sum_or_overflow() {
    assert_eq!(
      process_memory_reason(Some(&[UsedGpuMemory::Used(u64::MAX - 1)]), u64::MAX - 2),
      Some(GpuUnavailableReason::Busy)
    );
    assert_eq!(
      process_memory_reason(
        Some(&[
          UsedGpuMemory::Used(u64::MAX - 1),
          UsedGpuMemory::Used(u64::MAX - 1),
        ]),
        u64::MAX,
      ),
      None
    );
    // The driver sentinel is unknown memory, never an enormous known allocation.
    assert_eq!(
      process_memory_reason(Some(&[UsedGpuMemory::from(u64::MAX)]), THRESHOLD),
      Some(GpuUnavailableReason::Unverified)
    );
  }
}
