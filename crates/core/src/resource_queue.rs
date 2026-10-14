//! Durable, single-machine execution queue contracts. Reservations are advisory.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::config::EnvironmentConfig;
use crate::error::{ExpriError, Result};
use crate::identifiers::validate_component;

pub const RESOURCE_QUEUE_CAPABILITY: &str = "resource-queue-v2";
pub const RESOURCE_QUEUE_GPU_AVAILABILITY_CAPABILITY: &str = "resource-queue-gpu-availability-v1";
pub const QUEUE_LEASE_MS: i64 = 60_000;
pub const GPU_AVAILABILITY_MAX_AGE_MS: i64 = 30_000;
pub const MAX_QUEUE_PAGE: usize = 100;
pub const MAX_OUTSTANDING_ATTEMPTS: usize = 128;
pub const MAX_QUEUE_SPEC_BYTES: usize = 768 * 1024;
pub const MAX_QUEUE_SOURCE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequest {
  /// Logical CPU slots reserved for this job, not a process affinity limit.
  pub cpus: u32,
  pub memory_bytes: u64,
  #[serde(default)]
  pub gpus: u32,
  #[serde(default)]
  pub gpu_ids: Vec<String>,
  #[serde(default)]
  pub gpu_model: Option<String>,
  #[serde(default)]
  pub gpu_memory_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuVendor {
  Nvidia,
  Amd,
  Apple,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerGpu {
  pub device_id: String,
  pub model: String,
  pub memory_bytes: Option<u64>,
  pub vendor: GpuVendor,
  /// One driver-visible device selector; never a comma-separated device set.
  pub selector: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCapacity {
  pub cpus: u32,
  pub memory_bytes: u64,
  #[serde(default)]
  pub gpus: Vec<WorkerGpu>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuUnavailableReason {
  Busy,
  Unverified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnavailableGpu {
  pub device_id: String,
  pub reason: GpuUnavailableReason,
}

/// GPUs whose occupancy is observed outside the server's reservation ledger.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuAvailabilityReport {
  pub unavailable_gpus: Vec<UnavailableGpu>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
  pub input_id: String,
  pub size: u64,
  pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSubmission {
  pub submission_id: String,
  pub project_id: String,
  pub task: String,
  pub command: Vec<String>,
  pub environment: EnvironmentConfig,
  #[serde(default)]
  pub extras: Vec<String>,
  #[serde(default)]
  pub sync_args: Vec<String>,
  #[serde(default)]
  pub remote_managed: Vec<String>,
  #[serde(default)]
  pub smoke: bool,
  #[serde(default = "publish_default")]
  pub publish: bool,
  pub resources: ResourceRequest,
  pub worker_id: Option<String>,
  pub source: SourceRef,
}

fn publish_default() -> bool {
  true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
  #[serde(alias = "queued")]
  Pending,
  Starting,
  Running,
  CancelRequested,
  Completed,
  Failed,
  Cancelled,
  Unknown,
  Lost,
}

impl JobStatus {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Pending => "pending",
      Self::Starting => "starting",
      Self::Running => "running",
      Self::CancelRequested => "cancel_requested",
      Self::Completed => "completed",
      Self::Failed => "failed",
      Self::Cancelled => "cancelled",
      Self::Unknown => "unknown",
      Self::Lost => "lost",
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
  Claimed,
  Running,
  Unknown,
  Completed,
  Failed,
  Cancelled,
  Lost,
}

impl AttemptStatus {
  pub fn is_terminal(self) -> bool {
    matches!(
      self,
      Self::Completed | Self::Failed | Self::Cancelled | Self::Lost
    )
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptTerminalStatus {
  Completed,
  Failed,
  Cancelled,
}

impl AttemptTerminalStatus {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Completed => "completed",
      Self::Failed => "failed",
      Self::Cancelled => "cancelled",
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptIdentity {
  pub attempt_id: String,
  pub worker_id: String,
  pub session_id: String,
  pub fencing_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceAllocation {
  pub cpus: u32,
  pub memory_bytes: u64,
  pub gpus: Vec<WorkerGpu>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptSummary {
  pub attempt_id: String,
  pub job_id: String,
  pub worker_id: String,
  pub session_id: String,
  pub claim_id: String,
  pub fencing_token: String,
  pub allocation: ResourceAllocation,
  pub run_id: Option<String>,
  /// Server receipt time for the worker's durable process-start confirmation.
  #[serde(default)]
  pub started_at_ms: Option<i64>,
  pub status: AttemptStatus,
  pub cancel_requested: bool,
  pub created_at_ms: i64,
  pub updated_at_ms: i64,
  pub exit_code: Option<i32>,
  pub message: Option<String>,
}

impl AttemptSummary {
  pub fn identity(&self) -> AttemptIdentity {
    AttemptIdentity {
      attempt_id: self.attempt_id.clone(),
      worker_id: self.worker_id.clone(),
      session_id: self.session_id.clone(),
      fencing_token: self.fencing_token.clone(),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSummary {
  pub job_id: String,
  pub project_id: String,
  pub task: String,
  pub resources: ResourceRequest,
  /// Optional placement pin from the submission.
  pub worker_id: Option<String>,
  /// The server's chosen executor, before or after the worker accepts its offer.
  #[serde(default)]
  pub assigned_worker_id: Option<String>,
  pub status: JobStatus,
  pub pending_reason: Option<String>,
  pub submitted_at_ms: i64,
  pub updated_at_ms: i64,
  pub attempt: Option<AttemptSummary>,
}

/// Public scheduling context for dashboard history. Worker fencing identities,
/// source objects, commands, and credentials never cross this boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueJobContext {
  pub job_id: String,
  pub project_id: String,
  pub task: String,
  pub status: JobStatus,
  pub resources: ResourceRequest,
  pub pending_reason: Option<String>,
  pub submitted_at_ms: i64,
  pub updated_at_ms: i64,
  pub worker_id: Option<String>,
  pub run_id: Option<String>,
  pub started_at_ms: Option<i64>,
}

impl From<&JobSummary> for QueueJobContext {
  fn from(job: &JobSummary) -> Self {
    Self {
      job_id: job.job_id.clone(),
      project_id: job.project_id.clone(),
      task: job.task.clone(),
      status: job.status,
      resources: job.resources.clone(),
      pending_reason: job.pending_reason.clone(),
      submitted_at_ms: job.submitted_at_ms,
      updated_at_ms: job.updated_at_ms,
      worker_id: job
        .attempt
        .as_ref()
        .map(|attempt| attempt.worker_id.clone())
        .or_else(|| job.assigned_worker_id.clone())
        .or_else(|| job.worker_id.clone()),
      run_id: job
        .attempt
        .as_ref()
        .and_then(|attempt| attempt.run_id.clone()),
      started_at_ms: job
        .attempt
        .as_ref()
        .and_then(|attempt| attempt.started_at_ms),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSummary {
  pub worker_id: String,
  pub session_id: String,
  /// An empty list accepts any project permitted by the worker's credentials.
  pub project_ids: Vec<String>,
  pub capacity: WorkerCapacity,
  /// Capacity remaining after both planned offers and accepted attempts.
  pub available: ResourceAllocation,
  pub reserved: ResourceAllocation,
  #[serde(default)]
  pub unavailable_gpus: Vec<UnavailableGpu>,
  /// Server receive time of the most recent GPU availability report.
  #[serde(default)]
  pub gpu_availability_observed_at_ms: Option<i64>,
  pub online: bool,
  pub last_seen_at_ms: i64,
  pub lease_expires_at_ms: i64,
  pub quarantined: bool,
  pub outstanding_attempts: u32,
  /// Scheduled jobs that the worker has not yet accepted.
  pub assigned_jobs: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobAssignment {
  pub job: JobSummary,
  pub submission: JobSubmission,
  pub attempt: AttemptSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueueRequest {
  Submit {
    submission: Box<JobSubmission>,
  },
  ListJobs {
    project_id: Option<String>,
    status: Option<JobStatus>,
    cursor: Option<String>,
    limit: usize,
  },
  GetJob {
    job_id: String,
  },
  Cancel {
    job_id: String,
  },
  ListWorkers {
    cursor: Option<String>,
    limit: usize,
  },
  RegisterWorker {
    worker_id: String,
    session_id: String,
    /// An omitted or empty list accepts any project permitted by the credentials.
    #[serde(default)]
    project_ids: Vec<String>,
    capacity: WorkerCapacity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gpu_availability: Option<GpuAvailabilityReport>,
  },
  Heartbeat {
    worker_id: String,
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gpu_availability: Option<GpuAvailabilityReport>,
  },
  // Version the claim operation as well as capability discovery: services may
  // be upgraded while an older worker retains its authenticated client.
  #[serde(rename = "claim_v2")]
  Claim {
    worker_id: String,
    session_id: String,
    claim_id: String,
  },
  Start {
    attempt: AttemptIdentity,
    run_id: String,
  },
  /// Confirm that the worker observed its durable native command-start marker.
  MarkStarted {
    attempt: AttemptIdentity,
    run_id: String,
  },
  Finish {
    attempt: AttemptIdentity,
    status: AttemptTerminalStatus,
    exit_code: Option<i32>,
    message: Option<String>,
  },
  /// Preserve reservations when a supervisor or its journal cannot be verified.
  ReportUncertain {
    attempt: AttemptIdentity,
    message: String,
  },
  /// Explicit operator acknowledgement: an uncertain supervisor has been stopped.
  Recover {
    attempt_id: String,
    confirmed_stopped: bool,
  },
  SourceUrl {
    attempt: AttemptIdentity,
  },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueueResponse {
  Job {
    job: Box<JobSummary>,
  },
  Jobs {
    jobs: Vec<JobSummary>,
    next_cursor: Option<String>,
  },
  Workers {
    workers: Vec<WorkerSummary>,
    next_cursor: Option<String>,
  },
  Worker {
    worker: WorkerSummary,
    attempts: Vec<AttemptSummary>,
  },
  Claimed {
    assignment: Option<Box<JobAssignment>>,
  },
  Attempt {
    attempt: AttemptSummary,
  },
  SourceUrl {
    url: String,
    size: u64,
    sha256: String,
  },
}

fn invalid(message: &str) -> ExpriError {
  ExpriError::Message(message.into())
}

fn text(value: &str, limit: usize, empty: bool) -> Result<()> {
  if (!empty && value.is_empty()) || value.len() > limit || value.chars().any(char::is_control) {
    return Err(invalid(
      "queue text is empty, contains control characters, or exceeds its limit",
    ));
  }
  Ok(())
}

fn slots(cpus: u32, memory_bytes: u64) -> Result<()> {
  if cpus == 0 || cpus > 1_000_000 || memory_bytes == 0 || memory_bytes > i64::MAX as u64 {
    return Err(invalid(
      "queue CPU and memory reservations must be positive bounded integers",
    ));
  }
  Ok(())
}

pub fn validate_page(limit: usize, cursor: Option<&str>) -> Result<()> {
  if !(1..=MAX_QUEUE_PAGE).contains(&limit) {
    return Err(invalid("queue page limit must be between 1 and 100"));
  }
  if let Some(cursor) = cursor {
    validate_component(cursor)?;
  }
  Ok(())
}

impl ResourceRequest {
  pub fn validate(&self) -> Result<()> {
    slots(self.cpus, self.memory_bytes)?;
    if self.gpus > 64
      || self.gpu_ids.len() > 64
      || (!self.gpu_ids.is_empty() && self.gpu_ids.len() != self.gpus as usize)
      || (self.gpus == 0 && (self.gpu_model.is_some() || self.gpu_memory_bytes.is_some()))
    {
      return Err(invalid(
        "GPU selectors require a positive GPU count and explicit IDs must match that count",
      ));
    }
    let mut ids = BTreeSet::new();
    for id in &self.gpu_ids {
      text(id, 256, false)?;
      if !ids.insert(id) {
        return Err(invalid("GPU device IDs must be unique"));
      }
    }
    if let Some(model) = &self.gpu_model {
      text(model, 256, false)?;
    }
    if self
      .gpu_memory_bytes
      .is_some_and(|bytes| bytes == 0 || bytes > i64::MAX as u64)
    {
      return Err(invalid("GPU memory must be a positive bounded byte count"));
    }
    Ok(())
  }
}

impl WorkerCapacity {
  pub fn validate(&self) -> Result<()> {
    slots(self.cpus, self.memory_bytes)?;
    if self.gpus.len() > 64 {
      return Err(invalid("a queue worker may register at most 64 whole GPUs"));
    }
    let mut ids = BTreeSet::new();
    let mut selectors = BTreeSet::new();
    for gpu in &self.gpus {
      text(&gpu.device_id, 256, false)?;
      text(&gpu.model, 256, false)?;
      if !ids.insert(&gpu.device_id) {
        return Err(invalid("worker GPU device IDs must be unique"));
      }
      if gpu
        .memory_bytes
        .is_some_and(|bytes| bytes == 0 || bytes > i64::MAX as u64)
      {
        return Err(invalid(
          "worker GPU memory must be a positive bounded byte count",
        ));
      }
      match (&gpu.vendor, &gpu.selector) {
        (GpuVendor::Nvidia | GpuVendor::Amd, None) => {
          return Err(invalid(
            "NVIDIA and AMD GPUs require a driver device selector",
          ));
        }
        (_, Some(selector)) => {
          text(selector, 256, false)?;
          let vendor = match gpu.vendor {
            GpuVendor::Nvidia => 0,
            GpuVendor::Amd => 1,
            GpuVendor::Apple => 2,
          };
          if selector.contains(',') || !selectors.insert((vendor, selector)) {
            return Err(invalid(
              "worker GPU selectors must name distinct single devices",
            ));
          }
        }
        _ => {}
      }
    }
    Ok(())
  }
}

impl GpuAvailabilityReport {
  pub fn validate(&self) -> Result<()> {
    if self.unavailable_gpus.len() > 64 {
      return Err(invalid("GPU availability may report at most 64 devices"));
    }
    let mut devices = BTreeSet::new();
    for gpu in &self.unavailable_gpus {
      text(&gpu.device_id, 256, false)?;
      if !devices.insert(&gpu.device_id) {
        return Err(invalid("GPU availability device IDs must be unique"));
      }
    }
    Ok(())
  }

  pub fn validate_capacity(&self, capacity: &WorkerCapacity) -> Result<()> {
    self.validate()?;
    capacity.validate()?;
    if self.unavailable_gpus.iter().any(|gpu| {
      !capacity
        .gpus
        .iter()
        .any(|device| device.device_id == gpu.device_id)
    }) {
      return Err(invalid("GPU availability contains an unregistered device"));
    }
    Ok(())
  }
}

impl SourceRef {
  pub fn validate(&self) -> Result<()> {
    validate_component(&self.input_id)?;
    if self.size == 0
      || self.size > MAX_QUEUE_SOURCE_BYTES
      || self.sha256.len() != 64
      || !self
        .sha256
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
      || self.input_id != format!("queue-source-{}", self.sha256)
    {
      return Err(invalid(
        "queue source must be a bounded content-addressed private input",
      ));
    }
    Ok(())
  }
}

impl JobSubmission {
  pub fn validate(&self) -> Result<()> {
    validate_component(&self.submission_id)?;
    validate_component(&self.project_id)?;
    text(&self.task, 256, false)?;
    if let Some(worker_id) = &self.worker_id {
      validate_component(worker_id)?;
    }
    if self.command.is_empty() || self.command[0].is_empty() || self.command.len() > 1024 {
      return Err(invalid("a queued job requires a bounded nonempty command"));
    }
    for values in [
      &self.command,
      &self.extras,
      &self.sync_args,
      &self.remote_managed,
    ] {
      if values.len() > 1024 {
        return Err(invalid("queue argument list exceeds its limit"));
      }
      for value in values {
        if value.len() > 16 * 1024 || value.contains('\0') {
          return Err(invalid("queue arguments contain NUL or exceed their limit"));
        }
      }
    }
    self.environment.validate()?;
    if self.environment.require_cuda && self.resources.gpus == 0 {
      return Err(invalid(
        "CUDA queue jobs require at least one GPU reservation",
      ));
    }
    for value in self.environment.env.values() {
      if value.contains('\0') {
        return Err(invalid("queue environment values contain NUL"));
      }
    }
    self.resources.validate()?;
    self.source.validate()?;
    if serde_json::to_vec(self)?.len() > MAX_QUEUE_SPEC_BYTES {
      return Err(invalid("queue submission exceeds its metadata limit"));
    }
    Ok(())
  }
}

impl AttemptIdentity {
  pub fn validate(&self) -> Result<()> {
    for id in [
      &self.attempt_id,
      &self.worker_id,
      &self.session_id,
      &self.fencing_token,
    ] {
      validate_component(id)?;
    }
    Ok(())
  }
}

impl std::fmt::Display for JobStatus {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(self.as_str())
  }
}

impl std::str::FromStr for JobStatus {
  type Err = ExpriError;

  fn from_str(value: &str) -> Result<Self> {
    match value {
      "pending" | "queued" => Ok(Self::Pending),
      "starting" => Ok(Self::Starting),
      "running" => Ok(Self::Running),
      "cancel_requested" => Ok(Self::CancelRequested),
      "completed" => Ok(Self::Completed),
      "failed" => Ok(Self::Failed),
      "cancelled" => Ok(Self::Cancelled),
      "unknown" => Ok(Self::Unknown),
      "lost" => Ok(Self::Lost),
      _ => Err(invalid("unknown queue job status")),
    }
  }
}

impl QueueRequest {
  pub fn validate(&self) -> Result<()> {
    match self {
      Self::Submit { submission } => submission.validate(),
      Self::ListJobs {
        project_id,
        cursor,
        limit,
        ..
      } => {
        if let Some(project_id) = project_id {
          validate_component(project_id)?;
        }
        validate_page(*limit, cursor.as_deref())
      }
      Self::GetJob { job_id } | Self::Cancel { job_id } => validate_component(job_id),
      Self::ListWorkers { cursor, limit } => validate_page(*limit, cursor.as_deref()),
      Self::RegisterWorker {
        worker_id,
        session_id,
        project_ids,
        capacity,
        gpu_availability,
      } => {
        validate_component(worker_id)?;
        validate_component(session_id)?;
        if project_ids.len() > 100 {
          return Err(invalid(
            "queue workers allow at most 100 eligible project IDs",
          ));
        }
        let mut unique = BTreeSet::new();
        for project in project_ids {
          validate_component(project)?;
          if !unique.insert(project) {
            return Err(invalid("worker eligible projects must be unique"));
          }
        }
        capacity.validate()?;
        if let Some(report) = gpu_availability {
          report.validate_capacity(capacity)?;
        }
        Ok(())
      }
      Self::Heartbeat {
        worker_id,
        session_id,
        gpu_availability,
      } => {
        validate_component(worker_id)?;
        validate_component(session_id)?;
        if let Some(report) = gpu_availability {
          report.validate()?;
        }
        Ok(())
      }
      Self::Claim {
        worker_id,
        session_id,
        claim_id,
      } => {
        validate_component(worker_id)?;
        validate_component(session_id)?;
        validate_component(claim_id)
      }
      Self::Start { attempt, run_id } | Self::MarkStarted { attempt, run_id } => {
        attempt.validate()?;
        validate_component(run_id)
      }
      Self::Finish {
        attempt, message, ..
      } => {
        attempt.validate()?;
        if let Some(message) = message {
          text(message, 2048, true)?;
        }
        Ok(())
      }
      Self::ReportUncertain { attempt, message } => {
        attempt.validate()?;
        text(message, 2048, false)
      }
      Self::Recover {
        attempt_id,
        confirmed_stopped,
      } => {
        validate_component(attempt_id)?;
        if !confirmed_stopped {
          return Err(invalid(
            "recovery requires explicit confirmation that the supervisor stopped",
          ));
        }
        Ok(())
      }
      Self::SourceUrl { attempt } => attempt.validate(),
    }
  }
}

impl AttemptSummary {
  pub fn validate(&self) -> Result<()> {
    self.identity().validate()?;
    validate_component(&self.job_id)?;
    validate_component(&self.claim_id)?;
    if let Some(run_id) = &self.run_id {
      validate_component(run_id)?;
    }
    WorkerCapacity {
      cpus: self.allocation.cpus,
      memory_bytes: self.allocation.memory_bytes,
      gpus: self.allocation.gpus.clone(),
    }
    .validate()?;
    if self.created_at_ms < 0 || self.updated_at_ms < self.created_at_ms {
      return Err(invalid("queue attempt timestamps are invalid"));
    }
    if self.started_at_ms.is_some_and(|started| {
      self.run_id.is_none() || started < self.created_at_ms || started > self.updated_at_ms
    }) {
      return Err(invalid("queue startup confirmation is invalid"));
    }
    if let Some(message) = &self.message {
      text(message, 2048, true)?;
    }
    Ok(())
  }
}

impl JobSummary {
  pub fn validate(&self) -> Result<()> {
    validate_component(&self.job_id)?;
    validate_component(&self.project_id)?;
    text(&self.task, 256, false)?;
    self.resources.validate()?;
    if let Some(worker_id) = &self.worker_id {
      validate_component(worker_id)?;
    }
    if let Some(worker_id) = &self.assigned_worker_id {
      validate_component(worker_id)?;
      if self.worker_id.as_ref().is_some_and(|pin| pin != worker_id) {
        return Err(invalid("queue assignment differs from the worker pin"));
      }
    }
    if self.submitted_at_ms < 0 || self.updated_at_ms < self.submitted_at_ms {
      return Err(invalid("queue job timestamps are invalid"));
    }
    if let Some(reason) = &self.pending_reason {
      text(reason, 256, false)?;
    }
    if let Some(attempt) = &self.attempt {
      attempt.validate()?;
      if attempt.job_id != self.job_id
        || self
          .worker_id
          .as_ref()
          .is_some_and(|id| id != &attempt.worker_id)
        || self
          .assigned_worker_id
          .as_ref()
          .is_some_and(|id| id != &attempt.worker_id)
      {
        return Err(invalid(
          "queue attempt belongs to a different job or placement",
        ));
      }
      if attempt.allocation.cpus != self.resources.cpus
        || attempt.allocation.memory_bytes != self.resources.memory_bytes
        || attempt.allocation.gpus.len() != self.resources.gpus as usize
      {
        return Err(invalid("queue allocation differs from the job reservation"));
      }
      if !self.resources.gpu_ids.is_empty()
        && self.resources.gpu_ids.iter().collect::<BTreeSet<_>>()
          != attempt
            .allocation
            .gpus
            .iter()
            .map(|gpu| &gpu.device_id)
            .collect::<BTreeSet<_>>()
      {
        return Err(invalid(
          "queue allocation differs from the requested GPU devices",
        ));
      }
      for gpu in &attempt.allocation.gpus {
        if self.resources.gpu_model.as_ref().is_some_and(|model| {
          !gpu
            .model
            .to_ascii_lowercase()
            .contains(&model.to_ascii_lowercase())
        }) || self
          .resources
          .gpu_memory_bytes
          .is_some_and(|bytes| gpu.memory_bytes.is_none_or(|memory| memory < bytes))
        {
          return Err(invalid(
            "queue GPU allocation does not satisfy the requested model or memory",
          ));
        }
      }
    }
    Ok(())
  }
}

impl WorkerSummary {
  pub fn validate(&self) -> Result<()> {
    QueueRequest::RegisterWorker {
      worker_id: self.worker_id.clone(),
      session_id: self.session_id.clone(),
      project_ids: self.project_ids.clone(),
      capacity: self.capacity.clone(),
      gpu_availability: Some(GpuAvailabilityReport {
        unavailable_gpus: self.unavailable_gpus.clone(),
      }),
    }
    .validate()?;
    if self.last_seen_at_ms < 0
      || self.lease_expires_at_ms < self.last_seen_at_ms
      || self.outstanding_attempts as usize > MAX_OUTSTANDING_ATTEMPTS
      || self.outstanding_attempts as usize + self.assigned_jobs as usize > MAX_OUTSTANDING_ATTEMPTS
      || self
        .gpu_availability_observed_at_ms
        .is_some_and(|observed| observed < 0 || observed > self.last_seen_at_ms)
    {
      return Err(invalid("queue worker lease or attempt count is invalid"));
    }
    if self.available.cpus.checked_add(self.reserved.cpus) != Some(self.capacity.cpus)
      || self
        .available
        .memory_bytes
        .checked_add(self.reserved.memory_bytes)
        != Some(self.capacity.memory_bytes)
    {
      return Err(invalid(
        "queue worker availability differs from its capacity",
      ));
    }
    let mut devices = BTreeSet::new();
    for gpu in self.available.gpus.iter().chain(&self.reserved.gpus) {
      if !self.capacity.gpus.contains(gpu) || !devices.insert(&gpu.device_id) {
        return Err(invalid(
          "queue worker availability contains invalid GPU devices",
        ));
      }
    }
    for gpu in &self.unavailable_gpus {
      if !devices.insert(&gpu.device_id) {
        return Err(invalid("queue worker GPU availability partitions overlap"));
      }
    }
    if devices.len() != self.capacity.gpus.len() {
      return Err(invalid("queue worker availability omits GPU devices"));
    }
    Ok(())
  }
}

impl QueueResponse {
  /// Check bounded replies and request identity before persisting a worker journal.
  pub fn validate_for(&self, request: &QueueRequest) -> Result<()> {
    let wrong = || invalid("service returned an inconsistent queue response");
    match (request, self) {
      (QueueRequest::Submit { submission }, Self::Job { job }) => {
        job.validate()?;
        if job.project_id != submission.project_id
          || job.task != submission.task
          || job.resources != submission.resources
          || job.worker_id != submission.worker_id
        {
          return Err(wrong());
        }
      }
      (QueueRequest::GetJob { job_id } | QueueRequest::Cancel { job_id }, Self::Job { job }) => {
        job.validate()?;
        if &job.job_id != job_id {
          return Err(wrong());
        }
      }
      (
        QueueRequest::ListJobs {
          project_id,
          status,
          cursor,
          limit,
        },
        Self::Jobs { jobs, next_cursor },
      ) => {
        if jobs.len() > *limit {
          return Err(wrong());
        }
        for job in jobs {
          job.validate()?;
          if project_id.as_ref().is_some_and(|id| id != &job.project_id)
            || status.is_some_and(|status| status != job.status)
            || cursor.as_ref().is_some_and(|cursor| &job.job_id <= cursor)
          {
            return Err(wrong());
          }
        }
        if jobs.windows(2).any(|pair| pair[0].job_id >= pair[1].job_id)
          || next_cursor
            .as_ref()
            .is_some_and(|next| jobs.last().is_none_or(|job| &job.job_id != next))
        {
          return Err(wrong());
        }
      }
      (
        QueueRequest::ListWorkers { cursor, limit },
        Self::Workers {
          workers,
          next_cursor,
        },
      ) => {
        if workers.len() > *limit {
          return Err(wrong());
        }
        for worker in workers {
          worker.validate()?;
          if cursor
            .as_ref()
            .is_some_and(|cursor| &worker.worker_id <= cursor)
          {
            return Err(wrong());
          }
        }
        if workers
          .windows(2)
          .any(|pair| pair[0].worker_id >= pair[1].worker_id)
          || next_cursor.as_ref().is_some_and(|next| {
            workers
              .last()
              .is_none_or(|worker| &worker.worker_id != next)
          })
        {
          return Err(wrong());
        }
      }
      (
        QueueRequest::RegisterWorker {
          worker_id,
          session_id,
          ..
        }
        | QueueRequest::Heartbeat {
          worker_id,
          session_id,
          ..
        },
        Self::Worker { worker, attempts },
      ) => {
        worker.validate()?;
        if &worker.worker_id != worker_id
          || &worker.session_id != session_id
          || attempts.len() > MAX_OUTSTANDING_ATTEMPTS
          || attempts.len() != worker.outstanding_attempts as usize
        {
          return Err(wrong());
        }
        let mut ids = BTreeSet::new();
        for attempt in attempts {
          attempt.validate()?;
          if &attempt.worker_id != worker_id
            || attempt.status.is_terminal()
            || !ids.insert(&attempt.attempt_id)
          {
            return Err(wrong());
          }
        }
      }
      (
        QueueRequest::Claim {
          worker_id,
          session_id,
          claim_id,
        },
        Self::Claimed { assignment },
      ) => {
        if let Some(assignment) = assignment {
          assignment.job.validate()?;
          assignment.submission.validate()?;
          assignment.attempt.validate()?;
          if &assignment.attempt.worker_id != worker_id
            || &assignment.attempt.session_id != session_id
            || &assignment.attempt.claim_id != claim_id
            || assignment.job.attempt.as_ref() != Some(&assignment.attempt)
            || assignment.job.project_id != assignment.submission.project_id
            || assignment.job.task != assignment.submission.task
            || assignment.job.resources != assignment.submission.resources
            || assignment.job.worker_id != assignment.submission.worker_id
          {
            return Err(wrong());
          }
        }
      }
      (
        QueueRequest::Start {
          attempt: identity,
          run_id,
        },
        Self::Attempt { attempt },
      ) => {
        attempt.validate()?;
        if attempt.attempt_id != identity.attempt_id
          || attempt.worker_id != identity.worker_id
          || attempt.session_id != identity.session_id
          || attempt.run_id.as_ref() != Some(run_id)
        {
          return Err(wrong());
        }
      }
      (
        QueueRequest::MarkStarted {
          attempt: identity,
          run_id,
        },
        Self::Attempt { attempt },
      ) => {
        attempt.validate()?;
        if attempt.identity() != *identity
          || attempt.run_id.as_ref() != Some(run_id)
          || attempt.started_at_ms.is_none()
        {
          return Err(wrong());
        }
      }
      (
        QueueRequest::Finish {
          attempt: identity,
          status,
          ..
        },
        Self::Attempt { attempt },
      ) => {
        attempt.validate()?;
        let expected = match status {
          AttemptTerminalStatus::Completed => AttemptStatus::Completed,
          AttemptTerminalStatus::Failed => AttemptStatus::Failed,
          AttemptTerminalStatus::Cancelled => AttemptStatus::Cancelled,
        };
        if attempt.identity() != *identity || attempt.status != expected {
          return Err(wrong());
        }
      }
      (
        QueueRequest::ReportUncertain {
          attempt: identity, ..
        },
        Self::Attempt { attempt },
      ) => {
        attempt.validate()?;
        if attempt.identity() != *identity || attempt.status != AttemptStatus::Unknown {
          return Err(wrong());
        }
      }
      (QueueRequest::Recover { attempt_id, .. }, Self::Attempt { attempt }) => {
        attempt.validate()?;
        if &attempt.attempt_id != attempt_id || attempt.status != AttemptStatus::Lost {
          return Err(wrong());
        }
      }
      (QueueRequest::SourceUrl { .. }, Self::SourceUrl { url, size, sha256 }) => {
        let parsed = url::Url::parse(url).map_err(|_| wrong())?;
        if !matches!(parsed.scheme(), "http" | "https")
          || parsed.host_str().is_none()
          || !parsed.username().is_empty()
          || parsed.password().is_some()
          || parsed.fragment().is_some()
          || *size == 0
          || *size > MAX_QUEUE_SOURCE_BYTES
          || sha256.len() != 64
          || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
          return Err(wrong());
        }
      }
      _ => return Err(wrong()),
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn gpu(id: &str) -> WorkerGpu {
    WorkerGpu {
      device_id: id.into(),
      model: "NVIDIA GPU".into(),
      memory_bytes: Some(8192),
      vendor: GpuVendor::Nvidia,
      selector: Some(id.into()),
    }
  }

  #[test]
  fn gpu_availability_reports_are_bounded_unique_and_registered() {
    let capacity = WorkerCapacity {
      cpus: 4,
      memory_bytes: 8192,
      gpus: vec![gpu("GPU-a")],
    };
    GpuAvailabilityReport::default()
      .validate_capacity(&capacity)
      .unwrap();
    let unavailable = UnavailableGpu {
      device_id: "GPU-a".into(),
      reason: GpuUnavailableReason::Busy,
    };
    let mut report = GpuAvailabilityReport {
      unavailable_gpus: vec![unavailable.clone()],
    };
    report.validate_capacity(&capacity).unwrap();
    report.unavailable_gpus.push(unavailable);
    assert!(report.validate().is_err());
    report.unavailable_gpus.pop();
    for id in ["", "bad\nID", &"x".repeat(257)] {
      report.unavailable_gpus[0].device_id = id.into();
      assert!(report.validate().is_err());
    }
    report.unavailable_gpus[0].device_id = "GPU-other".into();
    report.validate().unwrap();
    assert!(report.validate_capacity(&capacity).is_err());
    report.unavailable_gpus = (0..64)
      .map(|number| UnavailableGpu {
        device_id: format!("GPU-{number}"),
        reason: GpuUnavailableReason::Unverified,
      })
      .collect();
    report.validate().unwrap();
    report.unavailable_gpus.push(UnavailableGpu {
      device_id: "GPU-64".into(),
      reason: GpuUnavailableReason::Busy,
    });
    assert!(report.validate().is_err());
  }

  #[test]
  fn gpu_availability_wire_reports_preserve_legacy_and_explicit_idle_states() {
    let mut registration = serde_json::json!({
      "operation": "register_worker", "worker_id": "worker-a", "session_id": "session-a",
      "capacity": {"cpus": 4, "memory_bytes": 8192, "gpus": [gpu("GPU-a")]}
    });
    let legacy: QueueRequest = serde_json::from_value(registration.clone()).unwrap();
    legacy.validate().unwrap();
    assert!(
      serde_json::to_value(&legacy)
        .unwrap()
        .get("gpu_availability")
        .is_none()
    );
    let QueueRequest::RegisterWorker {
      gpu_availability, ..
    } = legacy
    else {
      panic!("expected registration");
    };
    assert!(gpu_availability.is_none());
    registration["gpu_availability"] = serde_json::json!({"unavailable_gpus": []});
    let explicit: QueueRequest = serde_json::from_value(registration.clone()).unwrap();
    explicit.validate().unwrap();
    let QueueRequest::RegisterWorker {
      gpu_availability, ..
    } = explicit
    else {
      panic!("expected registration");
    };
    assert_eq!(gpu_availability, Some(GpuAvailabilityReport::default()));
    registration["gpu_availability"] = serde_json::json!({"unavailable_gpus": [{
      "device_id": "GPU-other", "reason": "unverified"
    }]});
    let foreign: QueueRequest = serde_json::from_value(registration.clone()).unwrap();
    assert!(foreign.validate().is_err());
    registration["gpu_availability"]["unavailable_gpus"][0]["device_id"] =
      serde_json::json!("GPU-a");
    let verified: QueueRequest = serde_json::from_value(registration.clone()).unwrap();
    verified.validate().unwrap();
    registration["gpu_availability"]["unavailable_gpus"][0]["reason"] =
      serde_json::json!("unknown");
    assert!(serde_json::from_value::<QueueRequest>(registration).is_err());

    let mut heartbeat = serde_json::json!({
      "operation": "heartbeat", "worker_id": "worker-a", "session_id": "session-a"
    });
    let legacy: QueueRequest = serde_json::from_value(heartbeat.clone()).unwrap();
    legacy.validate().unwrap();
    assert!(
      serde_json::to_value(&legacy)
        .unwrap()
        .get("gpu_availability")
        .is_none()
    );
    heartbeat["gpu_availability"] = serde_json::json!({"unavailable_gpus": [
      {"device_id": "GPU-a", "reason": "busy"},
      {"device_id": "GPU-a", "reason": "unverified"}
    ]});
    let duplicate: QueueRequest = serde_json::from_value(heartbeat).unwrap();
    assert!(duplicate.validate().is_err());
  }

  #[test]
  fn worker_gpu_availability_partitions_cover_capacity_without_overlap() {
    let gpus = ["GPU-a", "GPU-b", "GPU-c", "GPU-d"].map(gpu);
    let mut worker = WorkerSummary {
      worker_id: "worker-a".into(),
      session_id: "session-a".into(),
      project_ids: vec![],
      capacity: WorkerCapacity {
        cpus: 4,
        memory_bytes: 8192,
        gpus: gpus.clone().into(),
      },
      available: ResourceAllocation {
        cpus: 2,
        memory_bytes: 4096,
        gpus: vec![gpus[0].clone()],
      },
      reserved: ResourceAllocation {
        cpus: 2,
        memory_bytes: 4096,
        gpus: vec![gpus[1].clone()],
      },
      unavailable_gpus: vec![
        UnavailableGpu {
          device_id: gpus[2].device_id.clone(),
          reason: GpuUnavailableReason::Busy,
        },
        UnavailableGpu {
          device_id: gpus[3].device_id.clone(),
          reason: GpuUnavailableReason::Unverified,
        },
      ],
      gpu_availability_observed_at_ms: Some(1000),
      online: true,
      last_seen_at_ms: 1000,
      lease_expires_at_ms: 61_000,
      quarantined: false,
      outstanding_attempts: 1,
      assigned_jobs: 0,
    };
    worker.validate().unwrap();
    let original = worker.clone();
    for device_id in ["GPU-a", "GPU-b", "GPU-d", "GPU-foreign"] {
      worker.unavailable_gpus[0].device_id = device_id.into();
      assert!(worker.validate().is_err());
    }
    worker = original.clone();
    worker.unavailable_gpus.pop();
    assert!(worker.validate().is_err());
    worker = original.clone();
    worker.available.gpus[0].selector = Some("GPU-foreign".into());
    assert!(worker.validate().is_err());
    worker = original.clone();
    for timestamp in [-1, 1001] {
      worker.gpu_availability_observed_at_ms = Some(timestamp);
      assert!(worker.validate().is_err());
    }
    worker.gpu_availability_observed_at_ms = None;
    worker.validate().unwrap();

    // A legacy summary can deserialize before the server capability check.
    worker.available.gpus = gpus.into();
    worker.reserved.gpus.clear();
    worker.unavailable_gpus.clear();
    let mut legacy = serde_json::to_value(&worker).unwrap();
    legacy.as_object_mut().unwrap().remove("unavailable_gpus");
    legacy
      .as_object_mut()
      .unwrap()
      .remove("gpu_availability_observed_at_ms");
    let decoded: WorkerSummary = serde_json::from_value(legacy).unwrap();
    decoded.validate().unwrap();
    assert!(decoded.unavailable_gpus.is_empty());
    assert!(decoded.gpu_availability_observed_at_ms.is_none());
  }

  #[test]
  fn worker_registration_defaults_omitted_projects_to_unrestricted() {
    let mut registration = serde_json::json!({
      "operation": "register_worker", "worker_id": "worker-a", "session_id": "session-a",
      "capacity": {"cpus": 4, "memory_bytes": 8192, "gpus": []}
    });
    for project_ids in [None, Some(serde_json::json!([]))] {
      if let Some(project_ids) = project_ids {
        registration["project_ids"] = project_ids;
      }
      let request: QueueRequest = serde_json::from_value(registration.clone()).unwrap();
      request.validate().unwrap();
      let QueueRequest::RegisterWorker { project_ids, .. } = request else {
        panic!("expected worker registration");
      };
      assert!(project_ids.is_empty());
    }
    for project_ids in [
      vec!["project-a".to_owned(), "project-a".to_owned()],
      vec!["../project-a".to_owned()],
      (0..101).map(|number| format!("project-{number}")).collect(),
    ] {
      registration["project_ids"] = serde_json::json!(project_ids);
      let request: QueueRequest = serde_json::from_value(registration.clone()).unwrap();
      assert!(request.validate().is_err());
    }
    registration["project_ids"] = serde_json::json!(["project-a"]);
    let request: QueueRequest = serde_json::from_value(registration).unwrap();
    request.validate().unwrap();
  }

  #[test]
  fn legacy_cached_worker_claims_are_rejected_before_reserving_resources() {
    let legacy = serde_json::json!({"operation":"claim", "worker_id":"worker",
      "session_id":"session", "claim_id":"claim"});
    assert!(serde_json::from_value::<QueueRequest>(legacy).is_err());
    let claim = QueueRequest::Claim {
      worker_id: "worker".into(),
      session_id: "session".into(),
      claim_id: "claim".into(),
    };
    let current = serde_json::to_value(&claim).unwrap();
    assert_eq!(current["operation"], "claim_v2");
    serde_json::from_value::<QueueRequest>(current)
      .unwrap()
      .validate()
      .unwrap();
  }

  #[test]
  fn pending_is_canonical_and_legacy_queued_records_remain_readable() {
    assert_eq!(serde_json::to_value(JobStatus::Pending).unwrap(), "pending");
    for spelling in ["pending", "queued"] {
      assert_eq!(spelling.parse::<JobStatus>().unwrap(), JobStatus::Pending);
      assert_eq!(
        serde_json::from_value::<JobStatus>(serde_json::json!(spelling)).unwrap(),
        JobStatus::Pending
      );
    }
    assert_eq!(JobStatus::Pending.as_str(), "pending");
  }

  #[test]
  fn cuda_environment_requires_a_gpu_reservation_in_direct_api_submissions() {
    let sha256 = "a".repeat(64);
    let mut submission = JobSubmission {
      submission_id: "test".into(),
      project_id: "project".into(),
      task: "train".into(),
      command: vec!["python".into()],
      environment: EnvironmentConfig {
        base_python: Some("python3".into()),
        reuse_packages: vec!["torch".into()],
        require_cuda: true,
        env: Default::default(),
      },
      extras: Vec::new(),
      sync_args: Vec::new(),
      remote_managed: Vec::new(),
      smoke: false,
      publish: false,
      resources: ResourceRequest {
        cpus: 1,
        memory_bytes: 1024,
        gpus: 0,
        gpu_ids: Vec::new(),
        gpu_model: None,
        gpu_memory_bytes: None,
      },
      worker_id: None,
      source: SourceRef {
        input_id: format!("queue-source-{sha256}"),
        size: 1,
        sha256,
      },
    };
    assert!(
      submission
        .validate()
        .unwrap_err()
        .to_string()
        .contains("GPU reservation")
    );
    submission.resources.gpus = 1;
    submission.validate().unwrap();
  }

  #[test]
  fn resource_requests_reject_partial_or_duplicate_gpu_pins() {
    let mut request = ResourceRequest {
      cpus: 1,
      memory_bytes: 1024,
      gpus: 2,
      gpu_ids: vec!["GPU-a".into()],
      gpu_model: None,
      gpu_memory_bytes: None,
    };
    assert!(request.validate().is_err());
    request.gpu_ids.push("GPU-a".into());
    assert!(request.validate().is_err());
    request.gpu_ids[1] = "GPU-b".into();
    request.validate().unwrap();
  }

  #[test]
  fn worker_gpu_aliases_cannot_reserve_the_same_driver_device_twice() {
    let gpu = WorkerGpu {
      device_id: "one".into(),
      model: "GPU".into(),
      memory_bytes: None,
      vendor: GpuVendor::Nvidia,
      selector: Some("0".into()),
    };
    let mut capacity = WorkerCapacity {
      cpus: 4,
      memory_bytes: 1024,
      gpus: vec![
        gpu.clone(),
        WorkerGpu {
          device_id: "two".into(),
          ..gpu
        },
      ],
    };
    assert!(capacity.validate().is_err());
    capacity.gpus[1].selector = Some("1".into());
    capacity.validate().unwrap();
  }

  #[test]
  fn worker_availability_accepts_zero_free_slots_and_rejects_inconsistent_totals() {
    let gpu = WorkerGpu {
      device_id: "GPU-one".into(),
      model: "NVIDIA A100".into(),
      memory_bytes: Some(80_000),
      vendor: GpuVendor::Nvidia,
      selector: Some("0".into()),
    };
    let mut worker = WorkerSummary {
      worker_id: "worker-one".into(),
      session_id: "session-one".into(),
      project_ids: vec!["project".into()],
      capacity: WorkerCapacity {
        cpus: 4,
        memory_bytes: 8_000,
        gpus: vec![gpu.clone()],
      },
      available: ResourceAllocation {
        cpus: 0,
        memory_bytes: 0,
        gpus: Vec::new(),
      },
      reserved: ResourceAllocation {
        cpus: 4,
        memory_bytes: 8_000,
        gpus: vec![gpu.clone()],
      },
      unavailable_gpus: vec![],
      gpu_availability_observed_at_ms: None,
      online: true,
      last_seen_at_ms: 1_000,
      lease_expires_at_ms: 61_000,
      quarantined: false,
      outstanding_attempts: 0,
      assigned_jobs: 1,
    };
    worker.validate().unwrap();
    worker.available.cpus = 1;
    assert!(worker.validate().is_err());
    worker.available.cpus = 0;
    worker.available.gpus.push(gpu);
    assert!(worker.validate().is_err());
    worker.available.gpus.clear();
    worker.assigned_jobs = MAX_OUTSTANDING_ATTEMPTS as u32 + 1;
    assert!(worker.validate().is_err());
  }
}
