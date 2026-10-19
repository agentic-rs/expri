//! Immutable private registrations and one cooperative publishing slice.
use std::path::{Component, Path, PathBuf};
use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
  Intent, PublishingState, STATE, error, fs, mark_abandoned, now, owned_lease, runtime_config,
  safe_error,
};
use crate::PublishOptions;
use crate::client::http::Api;
use crate::client::{self, Publisher, ResolvedClient};
use crate::error::Result;
use crate::types::{RunScope, validate_scope};

/// Preflight before stopping a task; unsupported services leave it untouched.
pub fn check_attempt_support(run_dir: &Path) -> Result<()> {
  let paths = expri_core::storage::StoragePaths::resolve(None)?;
  let Some(job) = continuation_job(&paths, run_dir)? else {
    return Ok(());
  };
  let api = Api::new_with_resolved(&job.client, None)?;
  client::check_task_attempt_support(&api, &job.scope)?;
  let _lease = continuation_lease(run_dir)?;
  let recovered = client::reconcile_task_attempt(&continuation_options(&job, run_dir), api)?;
  drop(_lease);
  if recovered {
    crate::worker_service::rearm_publishing_job(&paths, job)?;
  }
  Ok(())
}

/// Advance the same immutable publication binding after the previous worker
/// stopped. The caller owns the run lease until the next supervisor takes over.
pub fn begin_attempt(
  run_dir: &Path,
  request_id: &str,
  expected_attempt: u64,
  next_attempt: u64,
) -> Result<()> {
  let directory = run_dir.canonicalize()?;
  let paths = expri_core::storage::StoragePaths::resolve(None)?;
  let Some(job) = continuation_job(&paths, &directory)? else {
    crate::registrations::reopen_for_attempt(&directory)?;
    return Ok(());
  };
  let _lease = continuation_lease(&directory)?;
  job.validate()?;
  client::begin_task_attempt(
    &continuation_options(&job, &directory),
    Api::new_with_resolved(&job.client, None)?,
    request_id,
    expected_attempt,
    next_attempt,
  )?;
  crate::registrations::reopen_for_attempt(&directory)?;
  let mut state = read_existing_state(&directory, &job)?;
  state.last_error = None;
  state.retry_after_seconds = None;
  state.save(&directory, "pending")?;
  drop(_lease);
  crate::worker_service::rearm_publishing_job(&paths, job)
}

fn continuation_options(job: &PreparedJob, run_dir: &Path) -> PublishOptions {
  PublishOptions {
    config: PathBuf::new(),
    run_dir: run_dir.into(),
    project_id: job.scope.project_id.clone(),
    origin: job.scope.origin.clone(),
    artifacts: job.artifacts.clone(),
    watch: job.watch,
    queue_dir: job.queue_dir.clone(),
  }
}

fn continuation_lease(run_dir: &Path) -> Result<crate::lock::FileLock> {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(35);
  loop {
    if let Some(lease) = owned_lease(run_dir)? {
      return Ok(lease);
    }
    if std::time::Instant::now() >= deadline {
      return Err(error("publisher is busy; retry this continuation request"));
    }
    std::thread::sleep(std::time::Duration::from_millis(25));
  }
}

fn continuation_job(
  paths: &expri_core::storage::StoragePaths,
  run_dir: &Path,
) -> Result<Option<PreparedJob>> {
  if let Some(job) = crate::worker_service::saved_publishing_job(paths, run_dir)? {
    return Ok(Some(job));
  }
  if fs::inspect(&run_dir.join(super::REQUEST))?.is_some() {
    let job = prepare_automatic(&super::read_intent(run_dir)?)?;
    crate::worker_service::register(paths, job.clone())?;
    return Ok(Some(job));
  }
  if fs::inspect(&run_dir.join("publishing-binding.json"))?.is_some()
    || fs::inspect(&run_dir.join(super::STATE))?.is_some()
  {
    return Err(error(
      "this run's saved publishing registration is unavailable; restore the worker service registry before continuing",
    ));
  }
  Ok(None)
}

fn read_existing_state(directory: &Path, job: &PreparedJob) -> Result<PublishingState> {
  let mut state = super::read_state(directory)?;
  if state.run_id != job.scope.run_id {
    return Err(error("publisher identity changed during continuation"));
  }
  state.progress = json!({"publication_epoch":serde_json::from_slice::<Value>(&fs::read_bounded(&directory.join("run-state.json"),client::RECORD_LIMIT)?)?["publication_epoch"]});
  Ok(state)
}

/// The token is persisted only by the worker service in its private registry.
/// Neither Debug nor a public secret accessor is provided.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedJob {
  schema_version: u32,
  client: ResolvedClient,
  run_dir: PathBuf,
  scope: RunScope,
  queue_dir: PathBuf,
  artifacts: Vec<String>,
  watch: bool,
  automatic: bool,
  repo_root: Option<PathBuf>,
  dashboard_url: Option<String>,
}

fn canonical_destination(path: &Path) -> Result<PathBuf> {
  let absolute = std::path::absolute(path)?;
  if absolute
    .components()
    .any(|component| matches!(component, Component::ParentDir))
  {
    return Err(error(
      "publisher queue path cannot contain parent traversal",
    ));
  }
  let mut missing = Vec::new();
  let mut ancestor = absolute.as_path();
  while !ancestor.try_exists()? {
    missing.push(
      ancestor
        .file_name()
        .ok_or_else(|| error("publisher queue path is invalid"))?
        .to_owned(),
    );
    ancestor = ancestor
      .parent()
      .ok_or_else(|| error("publisher queue path is invalid"))?;
  }
  let mut canonical = ancestor.canonicalize()?;
  fs::directory(&canonical)?;
  for component in missing.into_iter().rev() {
    canonical.push(component);
  }
  Ok(canonical)
}

/// Read the caller's config and its named token once, without network requests.
pub fn prepare_manual(options: &PublishOptions) -> Result<PreparedJob> {
  let client = ResolvedClient::resolve(&options.config)?;
  let run_dir = options.run_dir.canonicalize()?;
  fs::directory(&run_dir)?;
  let state: Value = serde_json::from_slice(&fs::read_bounded(
    &run_dir.join("run-state.json"),
    crate::client::RECORD_LIMIT,
  )?)?;
  let scope = RunScope {
    project_id: options.project_id.clone(),
    origin: options.origin.clone(),
    run_id: state["run_id"]
      .as_str()
      .ok_or_else(|| error("run state has no run_id"))?
      .to_owned(),
  };
  let artifacts = client::artifacts(&options.artifacts)?
    .into_iter()
    .collect::<Vec<_>>();
  if artifacts.iter().any(|path| path == "result.zip") {
    return Err(error(
      "result.zip is created by the service; select it with service fetch",
    ));
  }
  let mut job = PreparedJob {
    schema_version: 1,
    client,
    run_dir,
    scope,
    queue_dir: canonical_destination(&options.queue_dir)?,
    artifacts,
    watch: options.watch,
    automatic: false,
    repo_root: None,
    dashboard_url: None,
  };
  // An explicit request to observe/resume the same automatic run retains its
  // existing source context instead of registering a conflicting second job.
  if options.watch && job.artifacts.is_empty() && job.run_dir.join(super::REQUEST).try_exists()? {
    let intent = super::read_intent(&job.run_dir)?;
    if intent.config.project_id == job.scope.project_id
      && intent.config.origin == job.scope.origin
      && job.queue_dir == intent.repo_root.join(".expri/publish")
    {
      job.automatic = true;
      job.repo_root = Some(intent.repo_root.clone());
      job.dashboard_url = super::dashboard_url(&intent.config, &job.scope.run_id);
    }
  }
  job.validate()?;
  Ok(job)
}

pub fn prepare(options: &PublishOptions) -> Result<PreparedJob> {
  prepare_manual(options)
}

pub(super) fn prepare_automatic(intent: &Intent) -> Result<PreparedJob> {
  let config = runtime_config(intent)?;
  let mut job = prepare_manual(&PublishOptions {
    config,
    run_dir: intent.run_dir.clone(),
    project_id: intent.config.project_id.clone(),
    origin: intent.config.origin.clone(),
    artifacts: Vec::new(),
    watch: true,
    queue_dir: intent.repo_root.join(".expri/publish"),
  })?;
  job.automatic = true;
  job.repo_root = Some(intent.repo_root.clone());
  job.dashboard_url = super::dashboard_url(&intent.config, &job.scope.run_id);
  job.validate()?;
  Ok(job)
}

impl PreparedJob {
  pub fn validate(&self) -> Result<()> {
    self.client.validate()?;
    validate_scope(&self.scope)?;
    if self.schema_version != 1
      || !self.run_dir.is_absolute()
      || !self.queue_dir.is_absolute()
      || self.run_dir.canonicalize()? != self.run_dir
      || canonical_destination(&self.queue_dir)? != self.queue_dir
      || serde_json::to_vec(self)?.len() > 256 * 1024
    {
      return Err(error("invalid prepared publishing job"));
    }
    fs::directory(&self.run_dir)?;
    let state: Value = serde_json::from_slice(&fs::read_bounded(
      &self.run_dir.join("run-state.json"),
      crate::client::RECORD_LIMIT,
    )?)?;
    if state["run_id"].as_str() != Some(self.scope.run_id.as_str()) {
      return Err(error("prepared publishing job belongs to another run"));
    }
    if client::artifacts(&self.artifacts)?
      .into_iter()
      .collect::<Vec<_>>()
      != self.artifacts
      || self.artifacts.iter().any(|path| path == "result.zip")
    {
      return Err(error("invalid prepared artifact selection"));
    }
    if self.automatic {
      let root = self
        .repo_root
        .as_ref()
        .ok_or_else(|| error("automatic publishing job has no source root"))?;
      if root.canonicalize()? != *root
        || self.run_dir != root.join(".expri/runs").join(&self.scope.run_id)
        || self.queue_dir != root.join(".expri/publish")
        || !self.watch
      {
        return Err(error("automatic publishing job changed its source context"));
      }
    } else if self.repo_root.is_some() || self.dashboard_url.is_some() {
      return Err(error(
        "manual publishing job has unexpected automatic context",
      ));
    }
    if let Some(url) = &self.dashboard_url {
      let url = reqwest::Url::parse(url).map_err(|_| error("invalid job dashboard URL"))?;
      if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.path() != "/"
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
      {
        return Err(error("invalid job dashboard URL"));
      }
    }
    Ok(())
  }

  /// Stable target identity; conflicting credentials/options cannot create a
  /// second job for the same target. Registry equality checks the full binding.
  pub fn identity(&self) -> Result<String> {
    let identity = serde_json::to_vec(
      &json!({"endpoint": self.client.endpoint(), "scope": self.scope, "run_dir": self.run_dir}),
    )?;
    Ok(
      Sha256::digest(identity)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect(),
    )
  }

  pub fn validate_storage(&self, root: &Path) -> Result<()> {
    let root = canonical_destination(root)?;
    if root.starts_with(&self.run_dir)
      || self
        .repo_root
        .as_ref()
        .is_some_and(|repo| root.starts_with(repo))
    {
      return Err(error(
        "worker credentials must stay outside the source checkout and run directory",
      ));
    }
    Ok(())
  }

  pub fn source_root(&self) -> Option<&Path> {
    self.repo_root.as_deref()
  }
  pub fn scope(&self) -> &RunScope {
    &self.scope
  }

  /// Private pool key includes credentials, unlike the public run binding.
  /// Never include this key or its input in public operational summaries.
  pub fn run_control_destination(&self) -> Result<String> {
    Ok(
      Sha256::digest(serde_json::to_vec(&self.client)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect(),
    )
  }
  /// The service can control only a run already bound to this saved destination.
  pub fn run_control_client(
    &self,
    stop: Arc<AtomicBool>,
  ) -> Result<Option<crate::RunControlClient>> {
    self.validate()?;
    crate::RunControlClient::new(&self.client, self.scope.clone(), self.identity()?, stop)
  }
  pub fn run_dir(&self) -> &Path {
    &self.run_dir
  }
  pub fn redact(&self, detail: &str) -> String {
    self.client.redact(detail)
  }
  pub fn safe_report(&self) -> Value {
    json!({"scope":self.scope, "run_dir":self.run_dir, "queue_dir":self.queue_dir,
      "endpoint":self.client.endpoint(), "artifacts":self.artifacts, "watch":self.watch,
      "automatic":self.automatic, "dashboard_url":self.dashboard_url})
  }
}

/// Advance one job and release all leases before returning. The service owns
/// scheduling/backoff so long-lived runs cannot monopolize its bounded pool.
pub fn run_job(
  job: &PreparedJob,
  stop: Arc<AtomicBool>,
  progress: &mut dyn FnMut(Value) -> Result<()>,
) -> Result<bool> {
  if stop.load(Ordering::Acquire) {
    return Ok(false);
  }
  job.validate()?;
  let Some(_lease) = owned_lease(&job.run_dir)? else {
    return Ok(false);
  };
  let mut state = if job.run_dir.join(STATE).try_exists()? {
    let state: PublishingState = serde_json::from_slice(&fs::read_bounded(
      &job.run_dir.join(STATE),
      super::RECORD_LIMIT,
    )?)?;
    if state.schema_version != 1 || state.run_id != job.scope.run_id {
      return Err(error("publisher status does not belong to this run"));
    }
    state
  } else {
    PublishingState {
      schema_version: 1,
      run_id: job.scope.run_id.clone(),
      status: "pending".into(),
      updated_at: now(),
      last_success_at: None,
      last_error: None,
      retry_after_seconds: None,
      dashboard_url: job.dashboard_url.clone(),
      progress: json!({}),
    }
  };
  if job.automatic {
    mark_abandoned(&job.run_dir)?;
  }
  let api = Api::new_with_resolved(&job.client, Some(Arc::clone(&stop)))?;
  let mut publisher = Publisher::new_with_api(
    &PublishOptions {
      config: PathBuf::new(),
      run_dir: job.run_dir.clone(),
      project_id: job.scope.project_id.clone(),
      origin: job.scope.origin.clone(),
      artifacts: job.artifacts.clone(),
      watch: job.watch,
      queue_dir: job.queue_dir.clone(),
    },
    api,
  )?;
  state.retry_after_seconds = None;
  state.save(&job.run_dir, "publishing")?;
  let result = publisher.cycle(&mut |value| {
    state.progress = value.clone();
    state.save(&job.run_dir, "publishing")?;
    progress(value)
  });
  match result {
    Ok(done) => {
      state.last_success_at = Some(now());
      state.last_error = None;
      state.progress = publisher.progress(done);
      state.save(&job.run_dir, if done { "published" } else { "publishing" })?;
      progress(publisher.report(done))?;
      Ok(done)
    }
    Err(_) if stop.load(Ordering::Acquire) => Ok(false),
    Err(failure) => {
      let detail = Publisher::permanent_rejection(&failure)
        .map(str::to_owned)
        .unwrap_or_else(|| safe_error(&error(job.redact(&failure.to_string()))));
      state.last_error = Some(detail.clone());
      state.retry_after_seconds = Some(5);
      if Publisher::permanent_rejection(&failure).is_some() {
        state.fail(&job.run_dir, detail)?;
      } else {
        state.save(&job.run_dir, "retrying")?;
      }
      Err(failure)
    }
  }
}

#[cfg(test)]
mod tests;
