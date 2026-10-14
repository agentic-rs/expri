//! Durable reservation ledger. Expiration quarantines allocations; it never retries work.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use expri_core::resource_queue::*;

use super::*;

const MAX_WORKERS: usize = 256;
const MAX_QUEUED_JOBS: usize = 10_000;
// The shared transport limits the complete response, including both envelopes.
const PAGE_ITEM_BYTES: usize = MAX_REQUEST - 4096;

pub(super) fn now_ms() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis()
    .min(i64::MAX as u128) as i64
}

pub(super) fn initialize(db: &Connection) -> crate::error::Result<()> {
  db.execute_batch(
    "CREATE TABLE IF NOT EXISTS queue_change_revisions(project_id TEXT PRIMARY KEY,revision INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS queue_jobs (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,job_id TEXT NOT NULL UNIQUE,
    submission_id TEXT NOT NULL UNIQUE,project_id TEXT NOT NULL,spec TEXT NOT NULL,
    record TEXT NOT NULL,status TEXT NOT NULL,source_key TEXT NOT NULL,attempt_id TEXT);
    CREATE INDEX IF NOT EXISTS queue_jobs_ready ON queue_jobs(status,sequence);
    CREATE INDEX IF NOT EXISTS queue_jobs_page ON queue_jobs(project_id,status,job_id);
    CREATE TABLE IF NOT EXISTS queue_workers (
    worker_id TEXT PRIMARY KEY,session_id TEXT NOT NULL,project_ids TEXT NOT NULL,
    capacity TEXT NOT NULL,last_seen_at_ms INTEGER NOT NULL,lease_expires_at_ms INTEGER NOT NULL,
    quarantined INTEGER NOT NULL DEFAULT 0,gpu_availability TEXT,
    gpu_availability_observed_at_ms INTEGER);
    CREATE TABLE IF NOT EXISTS queue_attempts (
    attempt_id TEXT PRIMARY KEY,job_id TEXT NOT NULL UNIQUE,worker_id TEXT NOT NULL,
    session_id TEXT NOT NULL,claim_id TEXT NOT NULL,record TEXT NOT NULL,
    previous_fencing_token TEXT,released INTEGER NOT NULL DEFAULT 0);
    CREATE INDEX IF NOT EXISTS queue_attempts_worker ON queue_attempts(worker_id,released);
    CREATE TABLE IF NOT EXISTS queue_offers (
    job_id TEXT PRIMARY KEY,worker_id TEXT NOT NULL,session_id TEXT NOT NULL,
    allocation TEXT NOT NULL,created_at_ms INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS queue_offers_worker ON queue_offers(worker_id);
    CREATE TABLE IF NOT EXISTS queue_claims (
    worker_id TEXT NOT NULL,session_id TEXT NOT NULL,claim_id TEXT NOT NULL,attempt_id TEXT,
    PRIMARY KEY(worker_id,session_id,claim_id));
    CREATE TABLE IF NOT EXISTS queue_worker_sessions (
    worker_id TEXT NOT NULL,session_id TEXT NOT NULL,PRIMARY KEY(worker_id,session_id));",
  )
  .map_err(|_| crate::error::ExpriError::Message("cannot initialize execution queue".into()))?;
  db.execute_batch("INSERT OR IGNORE INTO queue_change_revisions(project_id,revision) SELECT DISTINCT project_id,1 FROM queue_jobs;")
    .map_err(|_| crate::error::ExpriError::Message("cannot initialize queue history revisions".into()))?;
  // Schema eight workers retain their reservations. Missing observations make
  // their unreserved GPUs unavailable until the updated worker reports them.
  let columns = {
    let mut statement = db
      .prepare("PRAGMA table_info(queue_workers)")
      .map_err(|_| {
        crate::error::ExpriError::Message("cannot read execution worker schema".into())
      })?;
    statement
      .query_map([], |row| row.get::<_, String>(1))
      .and_then(|rows| rows.collect::<rusqlite::Result<BTreeSet<_>>>())
      .map_err(|_| {
        crate::error::ExpriError::Message("cannot read execution worker schema".into())
      })?
  };
  for (column, kind) in [
    ("gpu_availability", "TEXT"),
    ("gpu_availability_observed_at_ms", "INTEGER"),
  ] {
    if !columns.contains(column) {
      db.execute_batch(&format!(
        "ALTER TABLE queue_workers ADD COLUMN {column} {kind}"
      ))
      .map_err(|_| {
        crate::error::ExpriError::Message("cannot upgrade execution worker schema".into())
      })?;
    }
  }
  for event in ["INSERT", "UPDATE", "DELETE"] {
    let row = if event == "DELETE" { "OLD" } else { "NEW" };
    db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS dashboard_queue_jobs_{event} AFTER {event} ON queue_jobs BEGIN
      INSERT INTO queue_change_revisions(project_id,revision) VALUES({row}.project_id,1)
      ON CONFLICT(project_id) DO UPDATE SET revision=revision+1; END;
      CREATE TRIGGER IF NOT EXISTS dashboard_queue_offers_{event} AFTER {event} ON queue_offers BEGIN
      INSERT INTO queue_change_revisions(project_id,revision)
      SELECT project_id,1 FROM queue_jobs WHERE job_id={row}.job_id
      ON CONFLICT(project_id) DO UPDATE SET revision=revision+1; END;"))
      .map_err(|_| crate::error::ExpriError::Message("cannot initialize queue history triggers".into()))?;
    db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS project_revision_queue_jobs_{event} AFTER {event} ON queue_jobs BEGIN
      INSERT INTO project_change_revisions(project_id,revision) VALUES({row}.project_id,1)
      ON CONFLICT(project_id) DO UPDATE SET revision=revision+1; END;"))
      .map_err(|_| crate::error::ExpriError::Message("cannot initialize execution queue revisions".into()))?;
    db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS project_revision_queue_offers_{event} AFTER {event} ON queue_offers BEGIN
      INSERT INTO project_change_revisions(project_id,revision)
      SELECT project_id,1 FROM queue_jobs WHERE job_id={row}.job_id
      ON CONFLICT(project_id) DO UPDATE SET revision=revision+1; END;"))
      .map_err(|_| crate::error::ExpriError::Message("cannot initialize scheduling offer revisions".into()))?;
  }
  Ok(())
}

fn encode<T: serde::Serialize>(value: &T) -> ApiResult<String> {
  serde_json::to_string(value)
    .map_err(|_| ApiError::new(500, "cannot encode execution queue metadata"))
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> ApiResult<T> {
  serde_json::from_str(value)
    .map_err(|_| ApiError::new(500, "stored execution queue metadata is invalid"))
}

fn page<T: serde::Serialize>(
  ids: Vec<String>,
  limit: usize,
  mut load: impl FnMut(&str) -> ApiResult<T>,
) -> ApiResult<(Vec<T>, Option<String>)> {
  let count_has_more = ids.len() > limit;
  let mut items = Vec::new();
  let mut remaining = PAGE_ITEM_BYTES;
  let mut last_id = None;
  for id in ids.into_iter().take(limit) {
    let item = load(&id)?;
    let size = encode(&item)?.len().saturating_add(1);
    if size > PAGE_ITEM_BYTES {
      return Err(ApiError::new(
        413,
        "queue item exceeds the response page size limit",
      ));
    }
    if size > remaining {
      return Ok((items, last_id));
    }
    remaining -= size;
    items.push(item);
    last_id = Some(id);
  }
  Ok((items, if count_has_more { last_id } else { None }))
}

fn identifier(prefix: &str) -> ApiResult<String> {
  let mut bytes = [0; 16];
  rustls::crypto::ring::default_provider()
    .secure_random
    .fill(&mut bytes)
    .map_err(|_| ApiError::new(500, "execution identity could not be created"))?;
  Ok(format!(
    "{prefix}{}",
    bytes
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect::<String>()
  ))
}

fn attempt(db: &Connection, attempt_id: &str) -> ApiResult<AttemptSummary> {
  let record: Option<String> = db
    .query_row(
      "SELECT record FROM queue_attempts WHERE attempt_id=?1",
      [attempt_id],
      |row| row.get(0),
    )
    .optional()
    .map_err(database)?;
  decode(&record.ok_or_else(|| ApiError::new(404, "queue attempt is missing"))?)
}

fn outstanding(db: &Connection, worker_id: &str) -> ApiResult<Vec<AttemptSummary>> {
  let mut statement = db.prepare("SELECT record FROM queue_attempts WHERE worker_id=?1 AND released=0 ORDER BY attempt_id LIMIT ?2").map_err(database)?;
  let records = statement
    .query_map(params![worker_id, MAX_OUTSTANDING_ATTEMPTS + 1], |row| {
      row.get::<_, String>(0)
    })
    .map_err(database)?;
  let mut attempts = Vec::new();
  for record in records {
    attempts.push(decode(&record.map_err(database)?)?);
  }
  if attempts.len() > MAX_OUTSTANDING_ATTEMPTS {
    return Err(ApiError::new(
      500,
      "worker execution ledger exceeds its limit",
    ));
  }
  Ok(attempts)
}

fn reservations(db: &Connection, worker_id: &str) -> ApiResult<Vec<ResourceAllocation>> {
  let mut allocations = outstanding(db, worker_id)?
    .into_iter()
    .map(|attempt| attempt.allocation)
    .collect::<Vec<_>>();
  let mut statement = db
    .prepare("SELECT allocation FROM queue_offers WHERE worker_id=?1 ORDER BY job_id")
    .map_err(database)?;
  for record in statement
    .query_map([worker_id], |row| row.get::<_, String>(0))
    .map_err(database)?
  {
    allocations.push(decode(&record.map_err(database)?)?);
  }
  if allocations.len() > MAX_OUTSTANDING_ATTEMPTS {
    return Err(ApiError::new(
      500,
      "worker reservation ledger exceeds its limit",
    ));
  }
  Ok(allocations)
}

fn availability(
  capacity: &WorkerCapacity,
  allocations: &[ResourceAllocation],
) -> ApiResult<(ResourceAllocation, ResourceAllocation)> {
  let cpus = allocations
    .iter()
    .try_fold(0u32, |total, allocation| total.checked_add(allocation.cpus));
  let memory_bytes = allocations.iter().try_fold(0u64, |total, allocation| {
    total.checked_add(allocation.memory_bytes)
  });
  let (Some(cpus), Some(memory_bytes)) = (cpus, memory_bytes) else {
    return Err(ApiError::new(500, "worker reservation totals overflow"));
  };
  let mut gpus = allocations
    .iter()
    .flat_map(|allocation| allocation.gpus.iter().cloned())
    .collect::<Vec<_>>();
  gpus.sort_by(|left, right| left.device_id.cmp(&right.device_id));
  let ids = gpus
    .iter()
    .map(|gpu| &gpu.device_id)
    .collect::<BTreeSet<_>>();
  if cpus > capacity.cpus
    || memory_bytes > capacity.memory_bytes
    || ids.len() != gpus.len()
    || gpus.iter().any(|gpu| !capacity.gpus.contains(gpu))
  {
    return Err(ApiError::new(
      500,
      "worker reservation exceeds registered capacity",
    ));
  }
  let available = ResourceAllocation {
    cpus: capacity.cpus - cpus,
    memory_bytes: capacity.memory_bytes - memory_bytes,
    gpus: capacity
      .gpus
      .iter()
      .filter(|gpu| !ids.contains(&gpu.device_id))
      .cloned()
      .collect(),
  };
  Ok((
    available,
    ResourceAllocation {
      cpus,
      memory_bytes,
      gpus,
    },
  ))
}

fn unavailable_gpus(
  capacity: &WorkerCapacity,
  report: Option<&GpuAvailabilityReport>,
  observed_at_ms: Option<i64>,
  now: i64,
) -> Vec<UnavailableGpu> {
  if let Some(report) = report
    && observed_at_ms.is_some_and(|observed| {
      now
        .checked_sub(observed)
        .is_some_and(|age| (0..GPU_AVAILABILITY_MAX_AGE_MS).contains(&age))
    })
  {
    return report.unavailable_gpus.clone();
  }
  capacity
    .gpus
    .iter()
    .map(|gpu| UnavailableGpu {
      device_id: gpu.device_id.clone(),
      reason: GpuUnavailableReason::Unverified,
    })
    .collect()
}

struct WorkerRecord {
  session_id: String,
  project_ids: String,
  capacity: String,
  last_seen_at_ms: i64,
  lease_expires_at_ms: i64,
  quarantined: bool,
  gpu_availability: Option<String>,
  gpu_availability_observed_at_ms: Option<i64>,
}

fn worker(db: &Connection, worker_id: &str, now: i64) -> ApiResult<WorkerSummary> {
  let row: Option<WorkerRecord> = db.query_row(
    "SELECT session_id,project_ids,capacity,last_seen_at_ms,lease_expires_at_ms,quarantined,gpu_availability,gpu_availability_observed_at_ms FROM queue_workers WHERE worker_id=?1",
    [worker_id], |row| Ok(WorkerRecord {
      session_id: row.get(0)?,
      project_ids: row.get(1)?,
      capacity: row.get(2)?,
      last_seen_at_ms: row.get(3)?,
      lease_expires_at_ms: row.get(4)?,
      quarantined: row.get(5)?,
      gpu_availability: row.get(6)?,
      gpu_availability_observed_at_ms: row.get(7)?,
    }),
  ).optional().map_err(database)?;
  let WorkerRecord {
    session_id,
    project_ids,
    capacity,
    last_seen_at_ms,
    lease_expires_at_ms,
    quarantined,
    gpu_availability: report,
    gpu_availability_observed_at_ms,
  } = row.ok_or_else(|| ApiError::new(404, "queue worker is not registered"))?;
  let outstanding_attempts = db
    .query_row(
      "SELECT COUNT(*) FROM queue_attempts WHERE worker_id=?1 AND released=0",
      [worker_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  let capacity = decode(&capacity)?;
  let allocations = reservations(db, worker_id)?;
  let assigned_jobs = allocations.len() as u32 - outstanding_attempts;
  let (mut available, reserved) = availability(&capacity, &allocations)?;
  let report = report
    .map(|value| decode::<GpuAvailabilityReport>(&value))
    .transpose()?;
  let unavailable_gpus = unavailable_gpus(
    &capacity,
    report.as_ref(),
    gpu_availability_observed_at_ms,
    now,
  )
  .into_iter()
  .filter(|gpu| {
    !reserved
      .gpus
      .iter()
      .any(|reserved| reserved.device_id == gpu.device_id)
  })
  .collect::<Vec<_>>();
  available.gpus.retain(|gpu| {
    !unavailable_gpus
      .iter()
      .any(|unavailable| unavailable.device_id == gpu.device_id)
  });
  Ok(WorkerSummary {
    worker_id: worker_id.into(),
    session_id,
    project_ids: decode(&project_ids)?,
    capacity,
    available,
    reserved,
    unavailable_gpus,
    gpu_availability_observed_at_ms,
    online: lease_expires_at_ms > now,
    last_seen_at_ms,
    lease_expires_at_ms,
    quarantined,
    outstanding_attempts,
    assigned_jobs,
  })
}

fn current_worker(
  db: &Connection,
  worker_id: &str,
  session_id: &str,
  now: i64,
) -> ApiResult<WorkerSummary> {
  let worker = worker(db, worker_id, now)?;
  if worker.session_id != session_id {
    return Err(ApiError::new(
      409,
      "worker session has been fenced by a newer registration",
    ));
  }
  if worker.lease_expires_at_ms <= now {
    return Err(ApiError::new(
      409,
      "worker session expired; register a new session",
    ));
  }
  Ok(worker)
}

fn save_attempt(db: &Connection, attempt: &AttemptSummary, released: bool) -> ApiResult<()> {
  db.execute(
    "UPDATE queue_attempts SET session_id=?2,record=?3,released=?4 WHERE attempt_id=?1",
    params![
      attempt.attempt_id,
      attempt.session_id,
      encode(attempt)?,
      released
    ],
  )
  .map_err(database)?;
  Ok(())
}

// Keep the existing indexed reservation ledger stable while public JSON uses pending.
fn stored_job_status(status: JobStatus) -> &'static str {
  if status == JobStatus::Pending {
    "queued"
  } else {
    status.as_str()
  }
}

fn set_job_status(db: &Connection, job_id: &str, status: &str, now: i64) -> ApiResult<()> {
  db.execute("UPDATE queue_jobs SET status=?2,record=json_set(record,'$.status',?2,'$.updated_at_ms',?3) WHERE job_id=?1", params![job_id, status, now]).map_err(database)?;
  Ok(())
}

fn refresh_quarantine(db: &Connection, worker_id: &str) -> ApiResult<()> {
  db.execute("UPDATE queue_workers SET quarantined=EXISTS(SELECT 1 FROM queue_attempts WHERE worker_id=?1 AND released=0 AND json_extract(record,'$.status')='unknown') WHERE worker_id=?1", [worker_id]).map_err(database)?;
  Ok(())
}

fn expire(db: &Connection, now: i64) -> ApiResult<()> {
  let mut statement = db.prepare("SELECT a.record FROM queue_attempts a JOIN queue_workers w ON w.worker_id=a.worker_id WHERE a.released=0 AND w.lease_expires_at_ms<=?1 AND json_extract(a.record,'$.status')!='unknown'").map_err(database)?;
  let records = statement
    .query_map([now], |row| row.get::<_, String>(0))
    .map_err(database)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(database)?;
  drop(statement);
  for record in records {
    let mut attempt: AttemptSummary = decode(&record)?;
    attempt.status = AttemptStatus::Unknown;
    attempt.updated_at_ms = now;
    attempt.message = Some(
      "Worker lease expired; reservation retained until reconciliation or confirmed recovery."
        .into(),
    );
    save_attempt(db, &attempt, false)?;
    set_job_status(
      db,
      &attempt.job_id,
      if attempt.cancel_requested {
        "cancel_requested"
      } else {
        "unknown"
      },
      now,
    )?;
  }
  db.execute("UPDATE queue_workers SET quarantined=1 WHERE lease_expires_at_ms<=?1 AND EXISTS(SELECT 1 FROM queue_attempts WHERE worker_id=queue_workers.worker_id AND released=0)", [now]).map_err(database)?;
  prune_offers(db, now)?;
  Ok(())
}

fn allocate(
  capacity: &WorkerCapacity,
  request: &ResourceRequest,
  used: &[ResourceAllocation],
  require_cuda: bool,
  unavailable: &[UnavailableGpu],
) -> Option<ResourceAllocation> {
  let cpus = used.iter().try_fold(0u64, |total, allocation| {
    total.checked_add(allocation.cpus as u64)
  })?;
  let memory = used.iter().try_fold(0u64, |total, allocation| {
    total.checked_add(allocation.memory_bytes)
  })?;
  if cpus.checked_add(request.cpus as u64)? > capacity.cpus as u64
    || memory.checked_add(request.memory_bytes)? > capacity.memory_bytes
  {
    return None;
  }
  let reserved: BTreeSet<&str> = used
    .iter()
    .flat_map(|allocation| allocation.gpus.iter().map(|gpu| gpu.device_id.as_str()))
    .collect();
  let model = request
    .gpu_model
    .as_ref()
    .map(|value| value.to_ascii_lowercase());
  let mut gpus: Vec<WorkerGpu> = capacity
    .gpus
    .iter()
    .filter(|gpu| {
      !reserved.contains(gpu.device_id.as_str())
        && !unavailable
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
    .collect();
  gpus.sort_by(|left, right| left.device_id.cmp(&right.device_id));
  if gpus.len() < request.gpus as usize {
    return None;
  }
  gpus.truncate(request.gpus as usize);
  Some(ResourceAllocation {
    cpus: request.cpus,
    memory_bytes: request.memory_bytes,
    gpus,
  })
}

/// Offers have never been accepted and cannot authorize execution. Unlike an
/// expired accepted attempt, they may safely be released and placed elsewhere.
fn prune_offers(db: &Connection, now: i64) -> ApiResult<()> {
  db.execute("DELETE FROM queue_offers WHERE NOT EXISTS(
    SELECT 1 FROM queue_jobs j JOIN queue_workers w ON w.worker_id=queue_offers.worker_id
    WHERE j.job_id=queue_offers.job_id AND j.status='queued'
    AND w.session_id=queue_offers.session_id AND w.lease_expires_at_ms>?1 AND w.quarantined=0
    AND NOT EXISTS(SELECT 1 FROM project_deletions WHERE project_id=j.project_id)
    AND (json_array_length(w.project_ids)=0 OR EXISTS(SELECT 1 FROM json_each(w.project_ids) WHERE value=j.project_id))
    AND NOT EXISTS(SELECT 1 FROM json_each(queue_offers.allocation,'$.gpus') gpu
      WHERE w.gpu_availability IS NULL OR w.gpu_availability_observed_at_ms IS NULL
      OR w.gpu_availability_observed_at_ms<=?2 OR w.gpu_availability_observed_at_ms>?1
      OR EXISTS(SELECT 1 FROM json_each(w.gpu_availability,'$.unavailable_gpus') unavailable
        WHERE json_extract(unavailable.value,'$.device_id')=json_extract(gpu.value,'$.device_id')))
    AND (json_extract(j.record,'$.worker_id') IS NULL OR json_extract(j.record,'$.worker_id')=w.worker_id))", params![now, now.saturating_sub(GPU_AVAILABILITY_MAX_AGE_MS)]).map_err(database)?;
  Ok(())
}

/// Place jobs globally in submission order, skipping jobs that currently fit no
/// worker. Existing valid offers remain stable until acceptance or invalidation.
fn schedule(db: &Connection, now: i64) -> ApiResult<()> {
  prune_offers(db, now)?;
  let pending: bool = db
    .query_row(
      "SELECT EXISTS(SELECT 1 FROM queue_jobs j WHERE j.status='queued'
    AND NOT EXISTS(SELECT 1 FROM queue_offers WHERE job_id=j.job_id)
    AND NOT EXISTS(SELECT 1 FROM project_deletions WHERE project_id=j.project_id))",
      [],
      |row| row.get(0),
    )
    .map_err(database)?;
  if !pending {
    return Ok(());
  }
  let ids = {
    let mut statement = db.prepare("SELECT worker_id FROM queue_workers WHERE lease_expires_at_ms>?1 AND quarantined=0 ORDER BY worker_id").map_err(database)?;
    statement
      .query_map([now], |row| row.get::<_, String>(0))
      .map_err(database)?
      .collect::<rusqlite::Result<Vec<_>>>()
      .map_err(database)?
  };
  let mut workers = ids
    .into_iter()
    .map(|id| Ok((worker(db, &id, now)?, reservations(db, &id)?)))
    .collect::<ApiResult<Vec<_>>>()?;
  // Leave GPU machines available for GPU work whenever a CPU machine fits.
  // Stable worker IDs break ties independently of the order of worker polls.
  workers.sort_by(|(left, _), (right, _)| {
    left
      .capacity
      .gpus
      .len()
      .cmp(&right.capacity.gpus.len())
      .then_with(|| left.worker_id.cmp(&right.worker_id))
  });
  let mut statement = db
    .prepare(
      "SELECT j.job_id,j.record,json_extract(j.spec,'$.environment.require_cuda') FROM queue_jobs j
    WHERE j.status='queued' AND NOT EXISTS(SELECT 1 FROM queue_offers WHERE job_id=j.job_id)
    AND NOT EXISTS(SELECT 1 FROM project_deletions WHERE project_id=j.project_id)
    ORDER BY j.sequence",
    )
    .map_err(database)?;
  let candidates = statement
    .query_map([], |row| {
      Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, bool>(2)?,
      ))
    })
    .map_err(database)?;
  for candidate in candidates {
    let (job_id, record, require_cuda) = candidate.map_err(database)?;
    let summary: JobSummary = decode(&record)?;
    for (worker, used) in &mut workers {
      if used.len() >= MAX_OUTSTANDING_ATTEMPTS
        || (!worker.project_ids.is_empty() && !worker.project_ids.contains(&summary.project_id))
        || summary
          .worker_id
          .as_ref()
          .is_some_and(|pin| pin != &worker.worker_id)
      {
        continue;
      }
      if let Some(allocation) = allocate(
        &worker.capacity,
        &summary.resources,
        used,
        require_cuda,
        &worker.unavailable_gpus,
      ) {
        db.execute("INSERT INTO queue_offers(job_id,worker_id,session_id,allocation,created_at_ms) VALUES(?1,?2,?3,?4,?5)", params![job_id, worker.worker_id, worker.session_id, encode(&allocation)?, now]).map_err(database)?;
        used.push(allocation);
        break;
      }
    }
  }
  Ok(())
}

fn pending_reason(db: &Connection, job: &JobSummary, now: i64) -> ApiResult<String> {
  let require_cuda: bool = db
    .query_row(
      "SELECT json_extract(spec,'$.environment.require_cuda') FROM queue_jobs WHERE job_id=?1",
      [&job.job_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  let mut statement = db.prepare("SELECT worker_id FROM queue_workers WHERE (?1 IS NULL OR worker_id=?1) AND (json_array_length(project_ids)=0 OR EXISTS(SELECT 1 FROM json_each(project_ids) WHERE value=?2)) ORDER BY worker_id LIMIT ?3").map_err(database)?;
  let ids = statement
    .query_map(params![job.worker_id, job.project_id, MAX_WORKERS], |row| {
      row.get::<_, String>(0)
    })
    .map_err(database)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(database)?;
  if ids.is_empty() {
    return Ok("no_eligible_worker".into());
  }
  let mut online = false;
  let mut available = false;
  for id in ids {
    let worker = worker(db, &id, now)?;
    if worker.lease_expires_at_ms <= now {
      continue;
    }
    online = true;
    if worker.quarantined {
      continue;
    }
    available = true;
    let reserved = reservations(db, &id)?;
    if reserved.len() >= MAX_OUTSTANDING_ATTEMPTS {
      continue;
    }
    if allocate(
      &worker.capacity,
      &job.resources,
      &reserved,
      require_cuda,
      &worker.unavailable_gpus,
    )
    .is_some()
    {
      return Ok("waiting_for_worker_claim".into());
    }
  }
  Ok(
    if !online {
      "eligible_workers_offline"
    } else if !available {
      "workers_need_reconciliation"
    } else {
      "resources_unavailable"
    }
    .into(),
  )
}

fn job(db: &Connection, job_id: &str, now: i64) -> ApiResult<JobSummary> {
  let row: Option<(String, Option<String>)> = db
    .query_row(
      "SELECT record,attempt_id FROM queue_jobs WHERE job_id=?1",
      [job_id],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)?;
  let (record, attempt_id) = row.ok_or_else(|| ApiError::new(404, "queue job is missing"))?;
  let mut job: JobSummary = decode(&record)?;
  job.attempt = attempt_id.map(|id| attempt(db, &id)).transpose()?;
  job.assigned_worker_id = if let Some(attempt) = &job.attempt {
    Some(attempt.worker_id.clone())
  } else {
    db.query_row(
      "SELECT worker_id FROM queue_offers WHERE job_id=?1",
      [job_id],
      |row| row.get(0),
    )
    .optional()
    .map_err(database)?
  };
  job.pending_reason = if job.status == JobStatus::Pending {
    Some(if job.assigned_worker_id.is_some() {
      "waiting_for_worker_claim".into()
    } else {
      pending_reason(db, &job, now)?
    })
  } else {
    None
  };
  Ok(job)
}

fn assignment(db: &Connection, attempt_id: &str, now: i64) -> ApiResult<Option<JobAssignment>> {
  let attempt = attempt(db, attempt_id)?;
  let (spec, project_id, status): (String, String, String) = db
    .query_row(
      "SELECT spec,project_id,status FROM queue_jobs WHERE job_id=?1",
      [&attempt.job_id],
      |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .map_err(database)?;
  if spec.is_empty() || status == "cancelled" || project_deleted(db, &project_id)? {
    return Ok(None);
  }
  Ok(Some(JobAssignment {
    job: job(db, &attempt.job_id, now)?,
    submission: decode(&spec)?,
    attempt,
  }))
}

fn project_deleted(db: &Connection, project_id: &str) -> ApiResult<bool> {
  db.query_row(
    "SELECT EXISTS(SELECT 1 FROM project_deletions WHERE project_id=?1)",
    [project_id],
    |row| row.get(0),
  )
  .map_err(database)
}

fn attempt_project_deleted(db: &Connection, job_id: &str) -> ApiResult<Option<String>> {
  db.query_row(
    "SELECT j.project_id FROM queue_jobs j JOIN project_deletions d ON d.project_id=j.project_id WHERE j.job_id=?1",
    [job_id],
    |row| row.get(0),
  ).optional().map_err(database)
}

/// Active specs survive only until acknowledgement of stopping. Permanent
/// deletion then retains queue tombstones and resource ledger metadata only.
fn redact_stopped_project_payloads(db: &Connection, project_id: &str) -> ApiResult<()> {
  db.execute("UPDATE queue_jobs SET spec='',source_key='',record=json_set(record,'$.task','deleted') WHERE project_id=?1 AND NOT EXISTS(SELECT 1 FROM queue_attempts WHERE job_id=queue_jobs.job_id AND released=0)", [project_id]).map_err(database)?;
  db.execute("UPDATE queue_attempts SET record=json_set(record,'$.message',NULL) WHERE released=1 AND job_id IN (SELECT job_id FROM queue_jobs WHERE project_id=?1)", [project_id]).map_err(database)?;
  Ok(())
}

pub(super) fn cancel_project(db: &Connection, project_id: &str, now: i64) -> ApiResult<()> {
  db.execute("UPDATE queue_jobs SET status='cancelled',record=json_set(record,'$.status','cancelled','$.updated_at_ms',?2) WHERE project_id=?1 AND status='queued'", params![project_id, now]).map_err(database)?;
  db.execute(
    "DELETE FROM queue_offers WHERE job_id IN (SELECT job_id FROM queue_jobs WHERE project_id=?1)",
    [project_id],
  )
  .map_err(database)?;
  let mut statement = db.prepare("SELECT a.record FROM queue_attempts a JOIN queue_jobs j ON j.job_id=a.job_id WHERE j.project_id=?1 AND a.released=0").map_err(database)?;
  let records = statement
    .query_map([project_id], |row| row.get::<_, String>(0))
    .map_err(database)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(database)?;
  drop(statement);
  for record in records {
    let mut attempt: AttemptSummary = decode(&record)?;
    attempt.cancel_requested = true;
    attempt.updated_at_ms = now;
    save_attempt(db, &attempt, false)?;
    set_job_status(db, &attempt.job_id, "cancel_requested", now)?;
  }
  redact_stopped_project_payloads(db, project_id)?;
  Ok(())
}

impl<S: ObjectStorage> Store<S> {
  /// Bounded, read-only scheduling history without worker launch credentials.
  pub(in crate::service) fn dashboard_queue_jobs(
    &self,
    project_id: &str,
    origin: Option<&str>,
    limit: usize,
    offset: usize,
  ) -> ApiResult<DashboardPage<QueueJobContext>> {
    validate_component(project_id).map_err(bad)?;
    if let Some(origin) = origin {
      validate_component(origin).map_err(bad)?;
    }
    dashboard_page_bounds(limit, offset)?;
    let db = self.db()?;
    let selection = "FROM queue_jobs j LEFT JOIN queue_attempts a ON a.attempt_id=j.attempt_id LEFT JOIN queue_offers o ON o.job_id=j.job_id WHERE j.project_id=?1 AND (?2 IS NULL OR COALESCE(json_extract(a.record,'$.worker_id'),o.worker_id,json_extract(j.record,'$.worker_id'))=?2) AND NOT EXISTS(SELECT 1 FROM project_deletions d WHERE d.project_id=j.project_id)";
    let total_count = db
      .query_row(
        &format!("SELECT COUNT(*) {selection}"),
        params![project_id, origin],
        |row| row.get(0),
      )
      .map_err(database)?;
    let mut statement = db
      .prepare(&format!(
        "SELECT j.job_id {selection} ORDER BY j.sequence DESC LIMIT ?3 OFFSET ?4"
      ))
      .map_err(database)?;
    let ids = statement
      .query_map(params![project_id, origin, limit, offset], |row| {
        row.get::<_, String>(0)
      })
      .map_err(database)?
      .collect::<rusqlite::Result<Vec<_>>>()
      .map_err(database)?;
    let now = now_ms();
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
      let summary = job(&db, &id, now)?;
      summary.validate().map_err(bad)?;
      items.push(QueueJobContext::from(&summary));
    }
    Ok(DashboardPage {
      items,
      total_count,
      legacy_order: false,
    })
  }

  pub(in crate::service) fn dashboard_run_queue_context(
    &self,
    scope: &RunScope,
  ) -> ApiResult<Option<QueueJobContext>> {
    validate_scope(scope).map_err(bad)?;
    let db = self.db()?;
    let id: Option<String> = db.query_row("SELECT j.job_id FROM queue_jobs j JOIN queue_attempts a ON a.attempt_id=j.attempt_id WHERE j.project_id=?1 AND json_extract(a.record,'$.worker_id')=?2 AND json_extract(a.record,'$.run_id')=?3 AND NOT EXISTS(SELECT 1 FROM project_deletions d WHERE d.project_id=j.project_id) LIMIT 1",params![scope.project_id,scope.origin,scope.run_id],|row|row.get(0)).optional().map_err(database)?;
    id.map(|id| job(&db, &id, now_ms()).map(|summary| QueueJobContext::from(&summary)))
      .transpose()
  }

  pub(in crate::service) fn dashboard_queue_projects(
    &self,
    limit: usize,
  ) -> ApiResult<Vec<String>> {
    dashboard_page_bounds(limit, 0)?;
    let db = self.db()?;
    let mut statement = db.prepare("SELECT DISTINCT project_id FROM queue_jobs j WHERE NOT EXISTS(SELECT 1 FROM project_deletions d WHERE d.project_id=j.project_id) ORDER BY project_id LIMIT ?1").map_err(database)?;
    statement
      .query_map([limit], |row| row.get(0))
      .map_err(database)?
      .collect::<rusqlite::Result<Vec<_>>>()
      .map_err(database)
  }

  /// A scoped, cheap change hint also reflects eligibility after a worker lease expires.
  pub(in crate::service) fn dashboard_queue_revision(
    &self,
    project_id: Option<&str>,
    origin: Option<&str>,
  ) -> ApiResult<Option<String>> {
    use sha2::{Digest, Sha256};
    if let Some(project_id) = project_id {
      validate_component(project_id).map_err(bad)?;
    }
    if let Some(origin) = origin {
      validate_component(origin).map_err(bad)?;
    }
    let db = self.db()?;
    let present: bool = db
      .query_row(
        "SELECT EXISTS(SELECT 1 FROM queue_jobs WHERE (?1 IS NULL OR project_id=?1))",
        [project_id],
        |row| row.get(0),
      )
      .map_err(database)?;
    if !present {
      return Ok(None);
    }
    let revision: i64 = db.query_row("SELECT COALESCE(SUM(revision),0) FROM queue_change_revisions WHERE (?1 IS NULL OR project_id=?1)",[project_id],|row|row.get(0)).map_err(database)?;
    let mut statement = db.prepare("SELECT worker_id FROM queue_workers WHERE (?1 IS NULL OR json_array_length(project_ids)=0 OR EXISTS(SELECT 1 FROM json_each(project_ids) WHERE value=?1)) AND (?2 IS NULL OR worker_id=?2) ORDER BY worker_id LIMIT ?3").map_err(database)?;
    let ids = statement
      .query_map(params![project_id, origin, MAX_WORKERS], |row| {
        row.get::<_, String>(0)
      })
      .map_err(database)?
      .collect::<rusqlite::Result<Vec<_>>>()
      .map_err(database)?;
    let now = now_ms();
    let mut resources = Vec::with_capacity(ids.len());
    for id in ids {
      let worker = worker(&db, &id, now)?;
      // Shared reservations may change in another project. Hash only public
      // resource totals, never the other jobs, projects, or fencing identities.
      resources.push((
        worker.worker_id,
        worker.capacity,
        worker.available,
        worker.reserved,
        worker.online,
        worker.quarantined,
        worker.outstanding_attempts,
        worker.assigned_jobs,
      ));
    }
    let bytes = encode(&(revision, resources))?;
    Ok(Some(
      Sha256::digest(bytes.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect(),
    ))
  }

  pub(in crate::service) fn dashboard_queue_job(
    &self,
    project_id: &str,
    origin: Option<&str>,
    job_id: &str,
  ) -> ApiResult<QueueJobContext> {
    validate_component(project_id).map_err(bad)?;
    validate_component(job_id).map_err(bad)?;
    if let Some(origin) = origin {
      validate_component(origin).map_err(bad)?;
    }
    let db = self.db()?;
    let available: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM queue_jobs j WHERE j.job_id=?1 AND j.project_id=?2 AND NOT EXISTS(SELECT 1 FROM project_deletions d WHERE d.project_id=j.project_id))",params![job_id,project_id],|row|row.get(0)).map_err(database)?;
    if !available {
      return Err(ApiError::new(404, "queue job is missing"));
    }
    let summary = job(&db, job_id, now_ms())?;
    summary.validate().map_err(bad)?;
    let context = QueueJobContext::from(&summary);
    if origin.is_some_and(|origin| context.worker_id.as_deref() != Some(origin)) {
      return Err(ApiError::new(
        404,
        "queue job is outside the dashboard source",
      ));
    }
    Ok(context)
  }

  pub(super) fn resource_queue(&self, request: QueueRequest) -> ApiResult<QueueResponse> {
    self.resource_queue_with_clock(request, None)
  }

  #[cfg(test)]
  fn resource_queue_at(&self, request: QueueRequest, now: i64) -> ApiResult<QueueResponse> {
    self.resource_queue_with_clock(request, Some(now))
  }

  fn resource_queue_with_clock(
    &self,
    request: QueueRequest,
    injected_now: Option<i64>,
  ) -> ApiResult<QueueResponse> {
    request.validate().map_err(bad)?;
    // Commit expiration independently: a rejected stale worker must still quarantine its allocations.
    let mut db = self.db()?;
    // Waiting for the metadata lock must not extend an already expired lease.
    let now = injected_now.unwrap_or_else(now_ms);
    let expiration = db.transaction().map_err(database)?;
    expire(&expiration, now)?;
    expiration.commit().map_err(database)?;
    let transaction = db.transaction().map_err(database)?;
    let response = self.queue_operation(&transaction, request, now)?;
    transaction.commit().map_err(database)?;
    drop(db);
    match response {
      OperationResult::Response(response) => Ok(*response),
      OperationResult::Source {
        key,
        size,
        sha256,
        project_id,
      } => {
        let gate = self.project_gate(&project_id)?;
        let _operation = gate
          .read()
          .map_err(|_| ApiError::new(503, "project operation unavailable"))?;
        projects::ensure_active(&*self.db()?, &project_id)?;
        let url = self
          .storage
          .presign_get(&key, super::super::storage::MAX_PRESIGN_EXPIRY_SECS)
          .map_err(|_| ApiError::new(502, "queue source download URL could not be prepared"))?;
        Ok(QueueResponse::SourceUrl { url, size, sha256 })
      }
    }
  }

  fn queue_operation(
    &self,
    db: &Connection,
    request: QueueRequest,
    now: i64,
  ) -> ApiResult<OperationResult> {
    schedule(db, now)?;
    let response = match request {
      QueueRequest::Submit { submission } => submit(db, *submission, now)?,
      QueueRequest::ListJobs {
        project_id,
        status,
        cursor,
        limit,
      } => {
        let mut statement = db.prepare("SELECT job_id FROM queue_jobs WHERE (?1 IS NULL OR project_id=?1) AND (?2 IS NULL OR status=?2) AND job_id>?3 ORDER BY job_id LIMIT ?4").map_err(database)?;
        let ids = statement
          .query_map(
            params![
              project_id,
              status.map(stored_job_status),
              cursor.unwrap_or_default(),
              limit + 1
            ],
            |row| row.get::<_, String>(0),
          )
          .map_err(database)?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(database)?;
        let (jobs, next_cursor) = page(ids, limit, |id| job(db, id, now))?;
        QueueResponse::Jobs { jobs, next_cursor }
      }
      QueueRequest::GetJob { job_id } => QueueResponse::Job {
        job: Box::new(job(db, &job_id, now)?),
      },
      QueueRequest::Cancel { job_id } => cancel(db, &job_id, now)?,
      QueueRequest::ListWorkers { cursor, limit } => {
        let mut statement = db
          .prepare(
            "SELECT worker_id FROM queue_workers WHERE worker_id>?1 ORDER BY worker_id LIMIT ?2",
          )
          .map_err(database)?;
        let ids = statement
          .query_map(params![cursor.unwrap_or_default(), limit + 1], |row| {
            row.get::<_, String>(0)
          })
          .map_err(database)?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(database)?;
        let (workers, next_cursor) = page(ids, limit, |id| worker(db, id, now))?;
        QueueResponse::Workers {
          workers,
          next_cursor,
        }
      }
      QueueRequest::RegisterWorker {
        worker_id,
        session_id,
        mut project_ids,
        capacity,
        gpu_availability,
      } => {
        project_ids.sort();
        register(
          db,
          &worker_id,
          &session_id,
          project_ids,
          capacity,
          gpu_availability,
          now,
        )?
      }
      QueueRequest::Heartbeat {
        worker_id,
        session_id,
        gpu_availability,
      } => {
        let current = current_worker(db, &worker_id, &session_id, now)?;
        observe_gpus(
          db,
          &worker_id,
          &current.capacity,
          gpu_availability.as_ref(),
          now,
        )?;
        renew(db, &worker_id, now)?;
        QueueResponse::Worker {
          worker: worker(db, &worker_id, now)?,
          attempts: outstanding(db, &worker_id)?,
        }
      }
      QueueRequest::Claim {
        worker_id,
        session_id,
        claim_id,
      } => claim(db, &worker_id, &session_id, &claim_id, now)?,
      QueueRequest::Start { attempt, run_id } => start(db, &attempt, &run_id, now)?,
      QueueRequest::MarkStarted { attempt, run_id } => mark_started(db, &attempt, &run_id, now)?,
      QueueRequest::Finish {
        attempt,
        status,
        exit_code,
        message,
      } => finish(db, &attempt, status, exit_code, message, now)?,
      QueueRequest::ReportUncertain {
        attempt: identity,
        message,
      } => {
        let mut attempt = fenced_attempt(db, &identity, now)?;
        if attempt.status.is_terminal() {
          return Err(ApiError::new(
            409,
            "finished attempt cannot be marked uncertain",
          ));
        }
        attempt.status = AttemptStatus::Unknown;
        attempt.message = Some(message);
        attempt.updated_at_ms = now;
        save_attempt(db, &attempt, false)?;
        set_job_status(
          db,
          &attempt.job_id,
          if attempt.cancel_requested {
            "cancel_requested"
          } else {
            "unknown"
          },
          now,
        )?;
        refresh_quarantine(db, &attempt.worker_id)?;
        QueueResponse::Attempt { attempt }
      }
      QueueRequest::Recover { attempt_id, .. } => recover(db, &attempt_id, now)?,
      QueueRequest::SourceUrl { attempt: identity } => {
        let attempt = fenced_attempt(db, &identity, now)?;
        if !matches!(
          attempt.status,
          AttemptStatus::Claimed | AttemptStatus::Running
        ) {
          return Err(ApiError::new(
            409,
            "queue source is unavailable for an uncertain or finished attempt",
          ));
        }
        if attempt.cancel_requested {
          return Err(ApiError::new(409, "queue job cancellation was requested"));
        }
        let (key, project_id, spec): (String, String, String) = db
          .query_row(
            "SELECT source_key,project_id,spec FROM queue_jobs WHERE job_id=?1",
            [&attempt.job_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
          )
          .map_err(database)?;
        projects::ensure_active(db, &project_id)?;
        let submission: JobSubmission = decode(&spec)?;
        return Ok(OperationResult::Source {
          key,
          project_id,
          size: submission.source.size,
          sha256: submission.source.sha256,
        });
      }
    };
    schedule(db, now)?;
    let response = match response {
      QueueResponse::Job { job: summary } => QueueResponse::Job {
        job: Box::new(job(db, &summary.job_id, now)?),
      },
      QueueResponse::Worker {
        worker: summary,
        attempts,
      } => QueueResponse::Worker {
        worker: worker(db, &summary.worker_id, now)?,
        attempts,
      },
      response => response,
    };
    Ok(OperationResult::Response(Box::new(response)))
  }
}

enum OperationResult {
  Response(Box<QueueResponse>),
  Source {
    key: String,
    project_id: String,
    size: u64,
    sha256: String,
  },
}

fn submit(db: &Connection, submission: JobSubmission, now: i64) -> ApiResult<QueueResponse> {
  projects::ensure_active(db, &submission.project_id)?;
  let existing: Option<(String, String)> = db
    .query_row(
      "SELECT job_id,spec FROM queue_jobs WHERE submission_id=?1",
      [&submission.submission_id],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)?;
  if let Some((job_id, spec)) = existing {
    if spec.is_empty() {
      return Err(ApiError::new(
        409,
        "submission ID belongs to a deleted project; use a new submission ID",
      ));
    }
    let previous: JobSubmission = decode(&spec)?;
    if previous != submission {
      return Err(ApiError::new(
        409,
        "submission ID is assigned to a different queue specification",
      ));
    }
    return Ok(QueueResponse::Job {
      job: Box::new(job(db, &job_id, now)?),
    });
  }
  let count: usize = db
    .query_row(
      "SELECT COUNT(*) FROM queue_jobs WHERE status='queued'",
      [],
      |row| row.get(0),
    )
    .map_err(database)?;
  if count >= MAX_QUEUED_JOBS {
    return Err(ApiError::new(
      503,
      "execution queue is full; retry after jobs leave the queue",
    ));
  }
  let target = FileTarget::Input {
    project_id: submission.project_id.clone(),
    input_id: submission.source.input_id.clone(),
  };
  let source: Option<(String, String)> = db
    .query_row(
      "SELECT record,object_key FROM files WHERE target=?1",
      [target_json(&target)?],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)?;
  let (record, source_key) =
    source.ok_or_else(|| ApiError::new(409, "queue source upload is not complete"))?;
  let file: FileRecord = decode(&record)?;
  if file.target != target
    || !matches!(file.storage, FileStorage::Object)
    || file.size != submission.source.size
    || file.sha256.as_ref() != Some(&submission.source.sha256)
    || source_key.is_empty()
  {
    return Err(ApiError::new(
      409,
      "queue source does not match the uploaded object",
    ));
  }
  let job_id = identifier("job-")?;
  let summary = JobSummary {
    job_id: job_id.clone(),
    project_id: submission.project_id.clone(),
    task: submission.task.clone(),
    resources: submission.resources.clone(),
    worker_id: submission.worker_id.clone(),
    assigned_worker_id: None,
    status: JobStatus::Pending,
    pending_reason: None,
    submitted_at_ms: now,
    updated_at_ms: now,
    attempt: None,
  };
  db.execute("INSERT INTO queue_jobs(job_id,submission_id,project_id,spec,record,status,source_key) VALUES(?1,?2,?3,?4,?5,'queued',?6)", params![job_id, submission.submission_id, submission.project_id, encode(&submission)?, encode(&summary)?, source_key]).map_err(database)?;
  Ok(QueueResponse::Job {
    job: Box::new(job(db, &job_id, now)?),
  })
}

fn cancel(db: &Connection, job_id: &str, now: i64) -> ApiResult<QueueResponse> {
  let current = job(db, job_id, now)?;
  if current.status == JobStatus::Pending {
    set_job_status(db, job_id, "cancelled", now)?;
    db.execute("DELETE FROM queue_offers WHERE job_id=?1", [job_id])
      .map_err(database)?;
  } else if let Some(mut attempt) = current.attempt
    && !attempt.status.is_terminal()
  {
    attempt.cancel_requested = true;
    attempt.updated_at_ms = now;
    save_attempt(db, &attempt, false)?;
    set_job_status(db, job_id, "cancel_requested", now)?;
  }
  Ok(QueueResponse::Job {
    job: Box::new(job(db, job_id, now)?),
  })
}

fn renew(db: &Connection, worker_id: &str, now: i64) -> ApiResult<()> {
  db.execute(
    "UPDATE queue_workers SET last_seen_at_ms=?2,lease_expires_at_ms=?3 WHERE worker_id=?1",
    params![worker_id, now, now.saturating_add(QUEUE_LEASE_MS)],
  )
  .map_err(database)?;
  Ok(())
}

fn register(
  db: &Connection,
  worker_id: &str,
  session_id: &str,
  project_ids: Vec<String>,
  capacity: WorkerCapacity,
  gpu_availability: Option<GpuAvailabilityReport>,
  now: i64,
) -> ApiResult<QueueResponse> {
  if let Some(report) = &gpu_availability {
    report.validate_capacity(&capacity).map_err(bad)?;
  }
  let existing: bool = db
    .query_row(
      "SELECT EXISTS(SELECT 1 FROM queue_workers WHERE worker_id=?1)",
      [worker_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  if existing {
    let previous = worker(db, worker_id, now)?;
    if previous.lease_expires_at_ms > now && previous.session_id != session_id {
      return Err(ApiError::new(
        409,
        "another worker session still owns the active lease",
      ));
    }
    if previous.lease_expires_at_ms <= now && previous.session_id == session_id {
      return Err(ApiError::new(
        409,
        "worker session expired; register a new session",
      ));
    }
    if previous.session_id != session_id {
      let reused: bool = db
        .query_row(
          "SELECT EXISTS(SELECT 1 FROM queue_worker_sessions WHERE worker_id=?1 AND session_id=?2)",
          params![worker_id, session_id],
          |row| row.get(0),
        )
        .map_err(database)?;
      if reused {
        return Err(ApiError::new(
          409,
          "a fenced worker session cannot be reused",
        ));
      }
    }
    if previous.outstanding_attempts != 0
      && (previous.capacity != capacity || previous.project_ids != project_ids)
    {
      return Err(ApiError::new(
        409,
        "worker capacity and eligible projects cannot change while reservations remain",
      ));
    }
    if previous.session_id != session_id
      || previous.capacity != capacity
      || previous.project_ids != project_ids
    {
      db.execute("DELETE FROM queue_offers WHERE worker_id=?1", [worker_id])
        .map_err(database)?;
    }
    db.execute(
      "UPDATE queue_workers SET session_id=?2,capacity=?3,project_ids=?4 WHERE worker_id=?1",
      params![
        worker_id,
        session_id,
        encode(&capacity)?,
        encode(&project_ids)?
      ],
    )
    .map_err(database)?;
  } else {
    let count: usize = db
      .query_row("SELECT COUNT(*) FROM queue_workers", [], |row| row.get(0))
      .map_err(database)?;
    if count >= MAX_WORKERS {
      return Err(ApiError::new(503, "execution worker registry is full"));
    }
    db.execute("INSERT INTO queue_workers(worker_id,session_id,project_ids,capacity,last_seen_at_ms,lease_expires_at_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![worker_id, session_id, encode(&project_ids)?, encode(&capacity)?, now, now.saturating_add(QUEUE_LEASE_MS)]).map_err(database)?;
  }
  db.execute(
    "INSERT OR IGNORE INTO queue_worker_sessions(worker_id,session_id) VALUES(?1,?2)",
    params![worker_id, session_id],
  )
  .map_err(database)?;
  renew(db, worker_id, now)?;
  observe_gpus(db, worker_id, &capacity, gpu_availability.as_ref(), now)?;
  refresh_quarantine(db, worker_id)?;
  Ok(QueueResponse::Worker {
    worker: worker(db, worker_id, now)?,
    attempts: outstanding(db, worker_id)?,
  })
}

fn observe_gpus(
  db: &Connection,
  worker_id: &str,
  capacity: &WorkerCapacity,
  report: Option<&GpuAvailabilityReport>,
  now: i64,
) -> ApiResult<()> {
  if let Some(report) = report {
    report.validate_capacity(capacity).map_err(bad)?;
  }
  db.execute("UPDATE queue_workers SET gpu_availability=?2,gpu_availability_observed_at_ms=?3 WHERE worker_id=?1", params![worker_id, report.map(encode).transpose()?, report.map(|_| now)]).map_err(database)?;
  Ok(())
}

fn claim(
  db: &Connection,
  worker_id: &str,
  session_id: &str,
  claim_id: &str,
  now: i64,
) -> ApiResult<QueueResponse> {
  let worker = current_worker(db, worker_id, session_id, now)?;
  let previous: Option<Option<String>> = db
    .query_row(
      "SELECT attempt_id FROM queue_claims WHERE worker_id=?1 AND session_id=?2 AND claim_id=?3",
      params![worker_id, session_id, claim_id],
      |row| row.get(0),
    )
    .optional()
    .map_err(database)?;
  if let Some(previous) = previous {
    return Ok(QueueResponse::Claimed {
      assignment: match previous {
        Some(id) => assignment(db, &id, now)?.map(Box::new),
        None => None,
      },
    });
  }
  if worker.quarantined {
    return Err(ApiError::new(
      409,
      "worker reservations require reconciliation before new claims",
    ));
  }
  let used = outstanding(db, worker_id)?;
  if used.len() >= MAX_OUTSTANDING_ATTEMPTS {
    return Err(ApiError::new(
      409,
      "worker has reached its outstanding attempt limit",
    ));
  }
  // The coordinator already selected the executor and reserved its resources.
  // Acceptance is the first moment execution can become possible; converting
  // the offer and recording the idempotent claim happen in one transaction.
  let selected: Option<(String, String)> = db
    .query_row(
      "SELECT o.job_id,o.allocation FROM queue_offers o JOIN queue_jobs j ON j.job_id=o.job_id
    WHERE o.worker_id=?1 AND o.session_id=?2 AND j.status='queued' ORDER BY j.sequence LIMIT 1",
      params![worker_id, session_id],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)?;
  let attempt_id = if let Some((job_id, allocation)) = selected {
    let attempt_id = identifier("attempt-")?;
    let attempt = AttemptSummary {
      attempt_id: attempt_id.clone(),
      job_id: job_id.clone(),
      worker_id: worker_id.into(),
      session_id: session_id.into(),
      claim_id: claim_id.into(),
      fencing_token: identifier("fence-")?,
      allocation: decode(&allocation)?,
      run_id: None,
      started_at_ms: None,
      status: AttemptStatus::Claimed,
      cancel_requested: false,
      created_at_ms: now,
      updated_at_ms: now,
      exit_code: None,
      message: None,
    };
    db.execute("INSERT INTO queue_attempts(attempt_id,job_id,worker_id,session_id,claim_id,record) VALUES(?1,?2,?3,?4,?5,?6)", params![attempt_id, job_id, worker_id, session_id, claim_id, encode(&attempt)?]).map_err(database)?;
    db.execute("DELETE FROM queue_offers WHERE job_id=?1", [&job_id])
      .map_err(database)?;
    db.execute(
      "UPDATE queue_jobs SET attempt_id=?2 WHERE job_id=?1 AND status='queued'",
      params![job_id, attempt_id],
    )
    .map_err(database)?;
    set_job_status(db, &job_id, "starting", now)?;
    Some(attempt_id)
  } else {
    None
  };
  // Empty polls allocate nothing. Record successful claim IDs durably so a lost
  // assignment reply cannot launch a second attempt; avoid an unbounded empty-poll log.
  if attempt_id.is_some() {
    db.execute(
      "INSERT INTO queue_claims(worker_id,session_id,claim_id,attempt_id) VALUES(?1,?2,?3,?4)",
      params![worker_id, session_id, claim_id, attempt_id],
    )
    .map_err(database)?;
  }
  Ok(QueueResponse::Claimed {
    assignment: match attempt_id {
      Some(id) => assignment(db, &id, now)?.map(Box::new),
      None => None,
    },
  })
}

fn fenced_attempt(
  db: &Connection,
  identity: &AttemptIdentity,
  now: i64,
) -> ApiResult<AttemptSummary> {
  current_worker(db, &identity.worker_id, &identity.session_id, now)?;
  let attempt = attempt(db, &identity.attempt_id)?;
  if attempt.worker_id != identity.worker_id
    || attempt.session_id != identity.session_id
    || attempt.fencing_token != identity.fencing_token
  {
    return Err(ApiError::new(409, "queue attempt identity has been fenced"));
  }
  Ok(attempt)
}

fn start(
  db: &Connection,
  identity: &AttemptIdentity,
  run_id: &str,
  now: i64,
) -> ApiResult<QueueResponse> {
  current_worker(db, &identity.worker_id, &identity.session_id, now)?;
  let mut attempt = attempt(db, &identity.attempt_id)?;
  if attempt.worker_id != identity.worker_id {
    return Err(ApiError::new(
      409,
      "queue attempt belongs to a different worker",
    ));
  }
  let previous_fence: Option<String> = db
    .query_row(
      "SELECT previous_fencing_token FROM queue_attempts WHERE attempt_id=?1",
      [&identity.attempt_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  // An adoption reply may be lost. Only its identical Start may replay the previous fence.
  let adoption_retry = attempt.session_id == identity.session_id
    && attempt.run_id.as_deref() == Some(run_id)
    && previous_fence.as_ref() == Some(&identity.fencing_token);
  if attempt.fencing_token != identity.fencing_token && !adoption_retry {
    return Err(ApiError::new(409, "queue attempt identity has been fenced"));
  }
  if attempt.status.is_terminal() {
    return Err(ApiError::new(409, "queue attempt is already finished"));
  }
  if attempt
    .run_id
    .as_ref()
    .is_some_and(|previous| previous != run_id)
  {
    return Err(ApiError::new(
      409,
      "queue attempt is linked to a different run",
    ));
  }
  if attempt.status == AttemptStatus::Unknown {
    if attempt.run_id.as_deref() != Some(run_id) {
      return Err(ApiError::new(
        409,
        "uncertain attempt requires its already linked verified supervisor",
      ));
    }
    let previous = attempt.fencing_token.clone();
    attempt.session_id = identity.session_id.clone();
    attempt.fencing_token = identifier("fence-")?;
    db.execute(
      "UPDATE queue_attempts SET previous_fencing_token=?2 WHERE attempt_id=?1",
      params![attempt.attempt_id, previous],
    )
    .map_err(database)?;
  } else if attempt.session_id != identity.session_id {
    return Err(ApiError::new(
      409,
      "queue attempt belongs to a fenced session",
    ));
  }
  if attempt.cancel_requested && attempt.run_id.is_none() {
    return Err(ApiError::new(
      409,
      "queue job cancellation was requested before launch",
    ));
  }
  let project_id: String = db
    .query_row(
      "SELECT project_id FROM queue_jobs WHERE job_id=?1",
      [&attempt.job_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  // Reconciliation of a cancelled/deleted project's already running supervisor is still allowed.
  if attempt.run_id.is_none() {
    projects::ensure_active(db, &project_id)?;
  }
  attempt.run_id = Some(run_id.into());
  attempt.status = AttemptStatus::Running;
  attempt.updated_at_ms = now;
  attempt.message = None;
  save_attempt(db, &attempt, false)?;
  set_job_status(
    db,
    &attempt.job_id,
    if attempt.cancel_requested {
      "cancel_requested"
    } else {
      "running"
    },
    now,
  )?;
  refresh_quarantine(db, &attempt.worker_id)?;
  Ok(QueueResponse::Attempt { attempt })
}

fn mark_started(
  db: &Connection,
  identity: &AttemptIdentity,
  run_id: &str,
  now: i64,
) -> ApiResult<QueueResponse> {
  let mut attempt = fenced_attempt(db, identity, now)?;
  if attempt.run_id.as_deref() != Some(run_id) {
    return Err(ApiError::new(
      409,
      "startup confirmation requires the linked run identity",
    ));
  }
  // A fast task may finish before its startup confirmation is retried. Its
  // terminal phase and allocation release remain authoritative.
  if attempt.started_at_ms.is_none() {
    let confirmed = now.max(attempt.created_at_ms).max(attempt.updated_at_ms);
    attempt.started_at_ms = Some(confirmed);
    attempt.updated_at_ms = confirmed;
    let released: bool = db
      .query_row(
        "SELECT released FROM queue_attempts WHERE attempt_id=?1",
        [&attempt.attempt_id],
        |row| row.get(0),
      )
      .map_err(database)?;
    save_attempt(db, &attempt, released)?;
    db.execute(
      "UPDATE queue_jobs SET record=json_set(record,'$.updated_at_ms',?2) WHERE job_id=?1",
      params![attempt.job_id, confirmed],
    )
    .map_err(database)?;
  }
  Ok(QueueResponse::Attempt { attempt })
}

fn finish(
  db: &Connection,
  identity: &AttemptIdentity,
  status: AttemptTerminalStatus,
  exit_code: Option<i32>,
  message: Option<String>,
  now: i64,
) -> ApiResult<QueueResponse> {
  let mut attempt = fenced_attempt(db, identity, now)?;
  let deleted_project = attempt_project_deleted(db, &attempt.job_id)?;
  // A terminal retry cannot restore a deleted project's private diagnostics.
  let message = if deleted_project.is_some() {
    None
  } else {
    message
  };
  let target = match status {
    AttemptTerminalStatus::Completed => AttemptStatus::Completed,
    AttemptTerminalStatus::Failed => AttemptStatus::Failed,
    AttemptTerminalStatus::Cancelled => AttemptStatus::Cancelled,
  };
  if attempt.status.is_terminal() {
    if attempt.status != target || attempt.exit_code != exit_code || attempt.message != message {
      return Err(ApiError::new(
        409,
        "terminal queue report conflicts with the saved result",
      ));
    }
    return Ok(QueueResponse::Attempt { attempt });
  }
  if attempt.status == AttemptStatus::Unknown {
    return Err(ApiError::new(
      409,
      "uncertain attempt requires reconciliation or explicit confirmed recovery",
    ));
  }
  attempt.status = target;
  attempt.exit_code = exit_code;
  attempt.message = message;
  attempt.updated_at_ms = now;
  save_attempt(db, &attempt, true)?;
  set_job_status(db, &attempt.job_id, status.as_str(), now)?;
  refresh_quarantine(db, &attempt.worker_id)?;
  if let Some(project_id) = deleted_project {
    redact_stopped_project_payloads(db, &project_id)?;
  }
  Ok(QueueResponse::Attempt { attempt })
}

fn recover(db: &Connection, attempt_id: &str, now: i64) -> ApiResult<QueueResponse> {
  let mut attempt = attempt(db, attempt_id)?;
  if attempt.status == AttemptStatus::Lost {
    return Ok(QueueResponse::Attempt { attempt });
  }
  if attempt.status != AttemptStatus::Unknown {
    return Err(ApiError::new(
      409,
      "only an uncertain attempt can be recovered",
    ));
  }
  attempt.status = AttemptStatus::Lost;
  attempt.updated_at_ms = now;
  attempt.message = Some(
    "Operator confirmed the previous supervisor stopped; reservation released without retry."
      .into(),
  );
  let deleted_project = attempt_project_deleted(db, &attempt.job_id)?;
  if deleted_project.is_some() {
    attempt.message = None;
  }
  save_attempt(db, &attempt, true)?;
  set_job_status(db, &attempt.job_id, "lost", now)?;
  refresh_quarantine(db, &attempt.worker_id)?;
  if let Some(project_id) = deleted_project {
    redact_stopped_project_payloads(db, &project_id)?;
  }
  Ok(QueueResponse::Attempt { attempt })
}

#[cfg(test)]
mod tests;
