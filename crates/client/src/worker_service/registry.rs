use std::fs;
use std::path::{Path, PathBuf};

use expri_core::error::Result;
use expri_core::lock::{FileLock, LockAttempt, try_lock_file};
use expri_core::storage::StoragePaths;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::publishing::PreparedJob;

use super::{error, files};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobState {
  pub schema_version: u32,
  pub job_id: String,
  pub status: String,
  pub updated_at_ms: i64,
  pub retry_at_ms: i64,
  pub failures: u32,
  pub last_error: Option<String>,
  pub report: Value,
  pub progress: Value,
  #[serde(default)]
  pub publication_epoch: u64,
}

impl JobState {
  fn new(job_id: &str, job: &PreparedJob) -> Result<Self> {
    Ok(Self {
      schema_version: 1,
      job_id: job_id.into(),
      status: "queued".into(),
      updated_at_ms: now(),
      retry_at_ms: 0,
      failures: 0,
      last_error: None,
      report: job.safe_report(),
      progress: json!({}),
      publication_epoch: job_epoch(job)?,
    })
  }

  pub fn save(&mut self, directory: &Path) -> Result<()> {
    self.updated_at_ms = now();
    files::write(&directory.join("state.json"), self)
  }

  /// Serialize with continuation rearm. An older slice may finish after its
  /// publication lease was released, but cannot mark the new attempt published.
  pub fn save_if_current(&mut self, directory: &Path) -> Result<bool> {
    let _lease = state_lease(directory)?;
    if read_state(directory)?.publication_epoch != self.publication_epoch {
      return Ok(false);
    }
    self.save(directory)?;
    Ok(true)
  }

  pub fn value(&self) -> Value {
    json!({
      "job_id": self.job_id, "status": self.status, "updated_at_ms": self.updated_at_ms,
      "retry_at_ms": self.retry_at_ms, "last_error": self.last_error,
      "registration": self.report, "progress": self.progress,
      "publication_epoch": self.publication_epoch,
    })
  }
}

pub(super) fn now() -> i64 {
  chrono::Utc::now().timestamp_millis()
}

pub(super) fn valid_id(id: &str) -> Result<()> {
  if id.len() != 64
    || !id
      .bytes()
      .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
  {
    return Err(error("worker service job identity is invalid"));
  }
  Ok(())
}

pub(super) fn job_id(job: &PreparedJob) -> Result<String> {
  let id = job.identity()?;
  valid_id(&id)?;
  Ok(id)
}

fn lease(directory: &Path) -> Result<FileLock> {
  let path = directory.join("registry.lock");
  files::private_file(&path)?;
  let LockAttempt::Acquired(lease) = try_lock_file(&path, false)? else {
    return Err(error("worker service registration is busy; retry shortly"));
  };
  Ok(lease)
}

fn state_lease(directory: &Path) -> Result<FileLock> {
  let path = directory.join("state.lock");
  files::private_file(&path)?;
  for _ in 0..100 {
    if let LockAttempt::Acquired(lease) = try_lock_file(&path, false)? {
      return Ok(lease);
    }
    std::thread::sleep(std::time::Duration::from_millis(10));
  }
  Err(error("publishing job state is busy; retry shortly"))
}

pub(super) fn register(paths: &StoragePaths, job: PreparedJob) -> Result<Value> {
  job.validate()?;
  job.validate_storage(paths.root())?;
  let directory = files::ensure(paths)?;
  for ancestor in directory.ancestors() {
    if ancestor.join(".git").try_exists()? {
      return Err(error(
        "worker service credentials must be stored outside a source checkout; choose another --storage-dir",
      ));
    }
  }
  // Brief contention is normal when concurrent runs first register.
  let mut attempts = 0;
  let _lease = loop {
    match lease(&directory) {
      Ok(lease) => break lease,
      Err(failure) if attempts < 100 => {
        let _ = failure;
        attempts += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
      }
      Err(failure) => return Err(failure),
    }
  };
  let jobs = directory.join("jobs");
  files::directory(&jobs)?;
  let id = job_id(&job)?;
  reject_different_destination(&jobs, &job, &id)?;
  bind_run_destination(&job, &id)?;
  let root = jobs.join(&id);
  files::directory(&root)?;
  let request = root.join("request.json");
  let repeated = request.try_exists()?;
  if repeated {
    let existing = read_job(&root)?;
    if existing != job {
      return Err(error(
        "this run/destination already has a worker service registration with different options or credentials",
      ));
    }
  } else {
    files::write(&request, &job)?;
  }
  let _state_lease = state_lease(&root)?;
  let mut state = if root.join("state.json").try_exists()? {
    read_state(&root)?
  } else {
    JobState::new(&id, &job)?
  };
  if state.status == "error" || state.status == "paused" {
    state.status = "queued".into();
    state.retry_at_ms = 0;
    state.failures = 0;
    state.last_error = None;
  }
  if !repeated || !root.join("state.json").try_exists()? || state.status == "queued" {
    state.save(&root)?;
  }
  let mut report = state.value();
  report["accepted"] = json!(true);
  report["already_registered"] = json!(repeated);
  Ok(report)
}

pub(super) fn rearm(paths: &StoragePaths, job: &PreparedJob) -> Result<()> {
  let directory = files::ensure(paths)?;
  let _lease = lease(&directory)?;
  let root = directory.join("jobs").join(job_id(job)?);
  if read_job(&root)? != *job {
    return Err(error(
      "task continuation cannot change publishing credentials or destination",
    ));
  }
  let _state_lease = state_lease(&root)?;
  let mut state = read_state(&root)?;
  let epoch = job_epoch(job)?;
  if epoch < state.publication_epoch {
    return Err(error(
      "publishing registration has a newer publication epoch",
    ));
  }
  state.publication_epoch = epoch;
  state.status = "queued".into();
  state.retry_at_ms = 0;
  state.failures = 0;
  state.last_error = None;
  state.save(&root)
}

fn job_epoch(job: &PreparedJob) -> Result<u64> {
  let state: Value = serde_json::from_slice(&crate::client::fs::read_bounded(
    &job.run_dir().join("run-state.json"),
    crate::client::RECORD_LIMIT,
  )?)?;
  match state.get("publication_epoch") {
    None => Ok(0),
    Some(value) => value
      .as_u64()
      .filter(|epoch| *epoch <= i64::MAX as u64)
      .ok_or_else(|| error("run has an invalid publication epoch")),
  }
}

const RUN_BINDING: &str = "publishing-binding.json";
const RUN_BINDING_LOCK: &str = ".publishing-binding.lock";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunBinding {
  schema_version: u32,
  run_dir: PathBuf,
  // The identity hashes normalized endpoint, complete scope and canonical path;
  // credentials and options are deliberately excluded from the public binding.
  job_id: String,
}

fn bind_run_destination(job: &PreparedJob, identity: &str) -> Result<()> {
  let run_dir = job.run_dir();
  crate::client::fs::directory(run_dir)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if fs::symlink_metadata(run_dir)?.uid() != unsafe { libc::geteuid() } {
      return Err(error(
        "publishing run storage must be owned by the current user",
      ));
    }
  }
  let lock_path = run_dir.join(RUN_BINDING_LOCK);
  if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
    files::validate(&metadata, false)?;
  }
  files::private_file(&lock_path)?;
  let mut attempts = 0;
  let _binding_lease = loop {
    match try_lock_file(&lock_path, false)? {
      LockAttempt::Acquired(lease) => break lease,
      LockAttempt::Busy if attempts < 100 => {
        attempts += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
      }
      _ => {
        return Err(error(
          "publishing destination registration is busy; retry shortly",
        ));
      }
    }
  };
  let path = run_dir.join(RUN_BINDING);
  match fs::symlink_metadata(&path) {
    Ok(metadata) => {
      files::validate(&metadata, false)?;
      let bytes = crate::client::fs::read_bounded(&path, 16 * 1024)?;
      let binding: RunBinding = serde_json::from_slice(&bytes)
        .map_err(|_| error("saved run publishing destination binding is invalid"))?;
      if binding.schema_version != 1
        || valid_id(&binding.job_id).is_err()
        || binding.run_dir != run_dir
      {
        return Err(error(
          "saved publishing destination binding belongs to another run; use a separate run with its own checkpoint handoffs",
        ));
      }
      if binding.job_id != identity {
        return Err(error(
          "this local run is already registered for another publishing destination; use a separate run with its own checkpoint handoffs",
        ));
      }
      Ok(())
    }
    Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {
      crate::client::fs::atomic_json(
        &path,
        &RunBinding {
          schema_version: 1,
          run_dir: run_dir.to_path_buf(),
          job_id: identity.to_owned(),
        },
      )
    }
    Err(failure) => Err(failure.into()),
  }
}

// Run-owned checkpoint acknowledgements and publishing-state.json describe one
// destination. Independent canonical runs have their own checkpoint handoffs.
fn reject_different_destination(
  jobs: &Path,
  incoming: &PreparedJob,
  incoming_id: &str,
) -> Result<()> {
  for entry in fs::read_dir(jobs)? {
    let path = entry?.path();
    let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
      continue;
    };
    if valid_id(id).is_err() || id == incoming_id {
      continue;
    }
    let run_dir = read_state(&path)
      .ok()
      .and_then(|state| validated_report_run(&state))
      .or_else(|| read_job(&path).ok().map(|job| job.run_dir().to_path_buf()));
    if run_dir.as_deref() == Some(incoming.run_dir()) {
      return Err(error(
        "this local run is already registered for another publishing destination; use a separate run with its own checkpoint handoffs",
      ));
    }
  }
  Ok(())
}

/// Validate the non-secret binding against its stable job identity before using
/// its path. This also works for completed runs whose original files were removed.
fn validated_report_run(state: &JobState) -> Option<PathBuf> {
  let run_dir = PathBuf::from(state.report["run_dir"].as_str()?);
  if !run_dir.is_absolute()
    || run_dir
      .components()
      .any(|part| matches!(part, std::path::Component::ParentDir))
  {
    return None;
  }
  let scope: crate::types::RunScope = serde_json::from_value(state.report["scope"].clone()).ok()?;
  crate::types::validate_scope(&scope).ok()?;
  let endpoint = state.report["endpoint"].as_str()?;
  let url = reqwest::Url::parse(endpoint).ok()?;
  if !matches!(url.scheme(), "http" | "https")
    || !url.username().is_empty()
    || url.password().is_some()
    || url.query().is_some()
    || url.fragment().is_some()
    || !url.path().ends_with("/v1/request")
  {
    return None;
  }
  let binding =
    serde_json::to_vec(&json!({"endpoint":endpoint,"scope":scope,"run_dir":run_dir})).ok()?;
  let identity: String = Sha256::digest(binding)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  (identity == state.job_id).then_some(run_dir)
}

pub(super) fn job_status(paths: &StoragePaths, id: &str) -> Result<Value> {
  valid_id(id)?;
  let directory =
    files::existing(paths)?.ok_or_else(|| error("worker service job is not registered"))?;
  let jobs = directory.join("jobs");
  files::validate(&fs::symlink_metadata(&jobs)?, true)?;
  Ok(read_state(&jobs.join(id))?.value())
}

pub(super) fn read_job(directory: &Path) -> Result<PreparedJob> {
  files::validate(&fs::symlink_metadata(directory)?, true)?;
  let job: PreparedJob = serde_json::from_slice(&files::read(&directory.join("request.json"))?)
    .map_err(|_| error("worker service job request is invalid"))?;
  job
    .validate()
    .map_err(|_| error("worker service job request is invalid"))?;
  if directory.file_name().and_then(|name| name.to_str()) != Some(job_id(&job)?.as_str()) {
    return Err(error(
      "worker service job request identity differs from its registry",
    ));
  }
  Ok(job)
}

pub(super) fn read_state(directory: &Path) -> Result<JobState> {
  files::validate(&fs::symlink_metadata(directory)?, true)?;
  let state: JobState = serde_json::from_slice(&files::read(&directory.join("state.json"))?)
    .map_err(|_| error("worker service job state is invalid"))?;
  if state.schema_version != 1
    || directory.file_name().and_then(|name| name.to_str()) != Some(&state.job_id)
    || !matches!(
      state.status.as_str(),
      "queued" | "publishing" | "retrying" | "published" | "error" | "paused"
    )
  {
    return Err(error("worker service job state identity is invalid"));
  }
  Ok(state)
}

pub(super) fn pending(directory: &Path) -> Result<Vec<PathBuf>> {
  let jobs = directory.join("jobs");
  if !jobs.try_exists()? {
    return Ok(Vec::new());
  }
  files::validate(&fs::symlink_metadata(&jobs)?, true)?;
  let mut result = Vec::new();
  for entry in fs::read_dir(jobs)? {
    let path = entry?.path();
    let Some(id) = path.file_name().and_then(|id| id.to_str()) else {
      continue;
    };
    if valid_id(id).is_err() {
      continue;
    }
    // Recover a crash after request commit but before the public state commit.
    let state = if path.join("state.json").try_exists()? {
      read_state(&path)
    } else {
      read_job(&path).and_then(|job| {
        let mut state = JobState::new(id, &job)?;
        state.save(&path)?;
        Ok(state)
      })
    };
    if let Ok(state) = state
      && matches!(state.status.as_str(), "queued" | "publishing" | "retrying")
    {
      result.push((state.retry_at_ms, state.updated_at_ms, path));
    }
  }
  result.sort_by_key(|entry| (entry.0, entry.1));
  Ok(result.into_iter().map(|entry| entry.2).collect())
}

/// Public operational summaries only; never deserialize credential requests.
pub(super) fn summaries(directory: &Path) -> Result<Value> {
  let jobs = directory.join("jobs");
  if !jobs.try_exists()? {
    return Ok(
      json!({"jobs": [], "jobs_total": 0, "jobs_truncated": 0, "invalid_jobs": 0, "job_counts": {}}),
    );
  }
  files::validate(&fs::symlink_metadata(&jobs)?, true)?;
  let mut total = 0usize;
  let mut invalid = 0usize;
  let mut counts = std::collections::BTreeMap::<String, usize>::new();
  let mut summaries = Vec::new();
  for entry in fs::read_dir(jobs)? {
    let path = entry?.path();
    let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
      continue;
    };
    if valid_id(id).is_err() {
      continue;
    }
    total += 1;
    match read_state(&path) {
      Ok(state) => {
        *counts.entry(state.status.clone()).or_default() += 1;
        let summary = json!({"job_id":state.job_id,"status":state.status,
          "updated_at_ms":state.updated_at_ms,"retry_at_ms":state.retry_at_ms,
          "last_error":state.last_error,"scope":state.report["scope"],
          "endpoint":bounded_string(&state.report["endpoint"],512),
          "run_dir":bounded_string(&state.report["run_dir"],1024)});
        summaries.push((state.updated_at_ms, summary));
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.0));
        summaries.truncate(32);
      }
      Err(_) => invalid += 1,
    }
  }
  let mut bytes = 0;
  let result: Vec<_> = summaries
    .into_iter()
    .map(|(_, summary)| summary)
    .take_while(|summary| {
      bytes += serde_json::to_vec(summary)
        .map(|value| value.len())
        .unwrap_or(files::LIMIT as usize);
      bytes <= 96 * 1024
    })
    .collect();
  Ok(
    json!({"jobs_truncated":total.saturating_sub(result.len() + invalid),
    "jobs_total":total,"invalid_jobs":invalid,"job_counts":counts,"jobs":result}),
  )
}

fn bounded_string(value: &Value, limit: usize) -> Value {
  value
    .as_str()
    .map(|value| json!(value.chars().take(limit).collect::<String>()))
    .unwrap_or(Value::Null)
}

pub(super) fn saved_run(paths: &StoragePaths, run_dir: &Path) -> Result<Option<PreparedJob>> {
  let run_dir = run_dir.canonicalize()?;
  let Some(directory) = files::existing(paths)? else {
    return Ok(None);
  };
  let jobs = directory.join("jobs");
  if !jobs.try_exists()? {
    return Ok(None);
  }
  files::validate(&fs::symlink_metadata(&jobs)?, true)?;
  let mut found = None;
  let mut inspected = 0;
  for entry in fs::read_dir(jobs)? {
    let path = entry?.path();
    let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
      continue;
    };
    if valid_id(id).is_err() {
      continue;
    }
    inspected += 1;
    if inspected > 4096 {
      return Err(error(
        "worker service has too many saved jobs for an unqualified run resume; inspect service status and publish with an explicit destination",
      ));
    }
    let matches = if path.join("state.json").try_exists()? {
      match read_state(&path) {
        Ok(state) => state.report["run_dir"]
          .as_str()
          .is_some_and(|value| Path::new(value) == run_dir),
        Err(_) => {
          // A corrupt unrelated job must not block recovery. A valid private
          // binding can still identify corruption in the selected run itself.
          if let Ok(job) = read_job(&path)
            && job.run_dir() == run_dir
          {
            return Err(error(
              "saved publishing state for the selected run is invalid; restore its private metadata before resuming",
            ));
          }
          continue;
        }
      }
    } else {
      // A crash between the private request and public state commits is a
      // recoverable registration, rather than corrupt selected-job state.
      match read_job(&path) {
        Ok(job) => job.run_dir() == run_dir,
        Err(_) => continue,
      }
    };
    if !matches {
      continue;
    }
    let job = read_job(&path)?;
    if job.run_dir() != run_dir {
      continue;
    }
    if found.is_some() {
      return Err(error(
        "this run has multiple saved publishing destinations; publish with an explicit destination instead of unqualified resume",
      ));
    }
    found = Some(job);
  }
  Ok(found)
}
