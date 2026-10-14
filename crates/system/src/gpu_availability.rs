//! Fresh compute-context checks for whole GPUs offered to the shared scheduler.

use std::collections::BTreeMap;

use expri_core::resource_queue::{
  GpuAvailabilityReport, GpuUnavailableReason, GpuVendor, UnavailableGpu, WorkerCapacity, WorkerGpu,
};
use nvml_wrapper::{Nvml, error::NvmlError};

/// Check configured GPU execution selectors without inspecting process names or owners.
///
/// A successful NVIDIA compute-process query includes active CUDA contexts, including
/// the MPS server that owns its clients' context. Graphics-only contexts are omitted.
/// Unsupported providers, missing drivers, partitioned devices, query errors, and
/// selectors that cannot be mapped reliably are reported as unverified, never free.
/// Numeric selectors are unverified because NVML and CUDA indices can differ.
/// Callers must pass a validated capacity and treat this as a point-in-time observation.
pub fn probe_gpu_availability(capacity: &WorkerCapacity) -> GpuAvailabilityReport {
  let nvml = capacity
    .gpus
    .iter()
    .any(|gpu| gpu.vendor == GpuVendor::Nvidia)
    .then(Nvml::init)
    .and_then(std::result::Result::ok);
  probe_with(&capacity.gpus, |selector| {
    read_nvidia(nvml.as_ref()?, selector)
  })
}

struct NvidiaObservation {
  uuid: uuid::Uuid,
  partitioned: Option<bool>,
  compute_process_count: Option<u32>,
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
    // The count avoids copying any PIDs or GPU-memory values and needs no allocation.
    compute_process_count: device.running_compute_processes_count().ok(),
  })
}

fn full_nvidia_uuid(selector: &str) -> Option<uuid::Uuid> {
  uuid::Uuid::parse_str(selector.strip_prefix("GPU-")?).ok()
}

fn probe_with(
  gpus: &[WorkerGpu],
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
            match observation.compute_process_count {
              Some(0) => return None,
              Some(_) => GpuUnavailableReason::Busy,
              None => GpuUnavailableReason::Unverified,
            }
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

#[cfg(test)]
mod tests {
  use super::*;

  const FIRST: &str = "GPU-01234567-89ab-cdef-0123-456789abcdef";
  const SECOND: &str = "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

  fn gpu(id: &str, vendor: GpuVendor, selector: Option<&str>) -> WorkerGpu {
    WorkerGpu {
      device_id: id.into(),
      model: "Fixture GPU".into(),
      memory_bytes: None,
      vendor,
      selector: selector.map(str::to_owned),
    }
  }

  fn observation(uuid: &str, processes: u32) -> NvidiaObservation {
    NvidiaObservation {
      uuid: full_nvidia_uuid(uuid).unwrap(),
      partitioned: Some(false),
      compute_process_count: Some(processes),
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
    let report = probe_with(&capacity.gpus, |selector| match selector {
      FIRST => Some(observation(FIRST, 3)),
      SECOND => Some(observation(SECOND, 0)),
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
    let report = probe_with(&gpus, |_| panic!("unreliable selector must not be probed"));
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
        compute_process_count: None,
        ..observation(FIRST, 0)
      }),
      Some(NvidiaObservation {
        partitioned: None,
        ..observation(FIRST, 0)
      }),
      Some(NvidiaObservation {
        partitioned: Some(true),
        ..observation(FIRST, 0)
      }),
      Some(observation(SECOND, 0)),
    ] {
      let mut value = value;
      let report = probe_with(&gpus, |_| value.take());
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
    let report = probe_with(&gpus, |_| Some(observation(SECOND, 0)));
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
      probe_gpu_availability(&capacity)
        .unavailable_gpus
        .is_empty()
    );
    capacity.gpus.push(gpu("apple", GpuVendor::Apple, None));
    assert_eq!(
      probe_gpu_availability(&capacity).unavailable_gpus[0].reason,
      GpuUnavailableReason::Unverified
    );
  }
}
