//! Shared placement policy for offers and their pending explanations.

use std::collections::BTreeSet;

use expri_core::resource_queue::{
  GpuVendor, JobSummary, MAX_OUTSTANDING_ATTEMPTS, ResourceAllocation, WorkerSummary,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlacementBlocker {
  WorkerMismatch,
  ExperimentUnsupported,
  Offline,
  Quarantined,
  ReservationLimit,
  Cpu,
  Memory,
  Gpu,
}

/// The furthest eligibility stage reached by any worker determines the reason.
/// Ordering preserves the public diagnostic precedence for mixed worker pools.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum PendingReason {
  NoEligibleWorker,
  ExperimentWorkerUpgradeRequired,
  EligibleWorkersOffline,
  WorkersNeedReconciliation,
  ResourcesUnavailable,
}

impl PendingReason {
  pub(super) fn as_str(self) -> &'static str {
    match self {
      Self::NoEligibleWorker => "no_eligible_worker",
      Self::ExperimentWorkerUpgradeRequired => "experiment_worker_upgrade_required",
      Self::EligibleWorkersOffline => "eligible_workers_offline",
      Self::WorkersNeedReconciliation => "workers_need_reconciliation",
      Self::ResourcesUnavailable => "resources_unavailable",
    }
  }
}

impl PlacementBlocker {
  pub(super) fn pending_reason(self) -> PendingReason {
    match self {
      Self::WorkerMismatch => PendingReason::NoEligibleWorker,
      Self::ExperimentUnsupported => PendingReason::ExperimentWorkerUpgradeRequired,
      Self::Offline => PendingReason::EligibleWorkersOffline,
      Self::Quarantined => PendingReason::WorkersNeedReconciliation,
      Self::ReservationLimit | Self::Cpu | Self::Memory | Self::Gpu => {
        PendingReason::ResourcesUnavailable
      }
    }
  }
}

/// Worker snapshots already turn missing or stale GPU observations into
/// unavailable devices. Accepted reservations and unaccepted offers both count.
pub(super) fn evaluate(
  worker: &WorkerSummary,
  job: &JobSummary,
  used: &[ResourceAllocation],
  require_cuda: bool,
  experiment: bool,
  now: i64,
) -> Result<ResourceAllocation, PlacementBlocker> {
  if (!worker.project_ids.is_empty() && !worker.project_ids.contains(&job.project_id))
    || job
      .worker_id
      .as_ref()
      .is_some_and(|pin| pin != &worker.worker_id)
  {
    return Err(PlacementBlocker::WorkerMismatch);
  }
  if experiment && !worker.experiment_assignment_supported {
    return Err(PlacementBlocker::ExperimentUnsupported);
  }
  if worker.lease_expires_at_ms <= now {
    return Err(PlacementBlocker::Offline);
  }
  if worker.quarantined {
    return Err(PlacementBlocker::Quarantined);
  }
  if used.len() >= MAX_OUTSTANDING_ATTEMPTS {
    return Err(PlacementBlocker::ReservationLimit);
  }
  let request = &job.resources;
  let cpus = used
    .iter()
    .try_fold(u64::from(request.cpus), |total, allocation| {
      total.checked_add(u64::from(allocation.cpus))
    });
  if cpus.is_none_or(|cpus| cpus > u64::from(worker.capacity.cpus)) {
    return Err(PlacementBlocker::Cpu);
  }
  let memory = used
    .iter()
    .try_fold(request.memory_bytes, |total, allocation| {
      total.checked_add(allocation.memory_bytes)
    });
  if memory.is_none_or(|memory| memory > worker.capacity.memory_bytes) {
    return Err(PlacementBlocker::Memory);
  }
  let reserved: BTreeSet<&str> = used
    .iter()
    .flat_map(|allocation| allocation.gpus.iter().map(|gpu| gpu.device_id.as_str()))
    .collect();
  let model = request
    .gpu_model
    .as_ref()
    .map(|value| value.to_ascii_lowercase());
  let mut gpus = worker
    .capacity
    .gpus
    .iter()
    .filter(|gpu| {
      !reserved.contains(gpu.device_id.as_str())
        && !worker
          .unavailable_gpus
          .iter()
          .any(|unavailable| unavailable.device_id == gpu.device_id)
        && (!require_cuda || gpu.vendor == GpuVendor::Nvidia)
        && (request.gpu_ids.is_empty() || request.gpu_ids.contains(&gpu.device_id))
        && model
          .as_ref()
          .is_none_or(|model| gpu.model.to_ascii_lowercase().contains(model))
        && request
          .gpu_memory_bytes
          .is_none_or(|bytes| gpu.memory_bytes.is_some_and(|capacity| capacity >= bytes))
    })
    .cloned()
    .collect::<Vec<_>>();
  gpus.sort_by(|left, right| left.device_id.cmp(&right.device_id));
  if gpus.len() < request.gpus as usize {
    return Err(PlacementBlocker::Gpu);
  }
  gpus.truncate(request.gpus as usize);
  Ok(ResourceAllocation {
    cpus: request.cpus,
    memory_bytes: request.memory_bytes,
    gpus,
  })
}
