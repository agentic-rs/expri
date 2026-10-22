//! One private worker process with independently controlled background components.
//!
//! Credentials stay in the private registry; control IPC contains only public status.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use expri_core::error::{ExpriError, Result};
use expri_core::storage::StoragePaths;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::publishing::PreparedJob;

mod files;
mod preferences;
mod registry;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::{
  ensure_collection_ready, ensure_started, observe, restart, restart_component, run_foreground,
  run_foreground_components, start_component, status, stop, stop_component,
};

/// Independent components within the shared service process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceComponent {
  Collection,
  Publishing,
  QueueExecution,
  RunControl,
}

impl ServiceComponent {
  pub const ALL: [Self; 4] = [
    Self::Collection,
    Self::Publishing,
    Self::QueueExecution,
    Self::RunControl,
  ];

  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Collection => "collection",
      Self::Publishing => "publishing",
      Self::QueueExecution => "queue_execution",
      Self::RunControl => "run_control",
    }
  }

  fn thread_name(self) -> &'static str {
    match self {
      Self::Collection => "expri-collection",
      Self::Publishing => "expri-publishing",
      Self::QueueExecution => "expri-queue-execution",
      Self::RunControl => "expri-run-control",
    }
  }
}

impl fmt::Display for ServiceComponent {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.as_str().replace('_', "-"))
  }
}

impl FromStr for ServiceComponent {
  type Err = String;

  fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
    match value.replace('-', "_").as_str() {
      "collection" => Ok(Self::Collection),
      "publishing" => Ok(Self::Publishing),
      "queue_execution" => Ok(Self::QueueExecution),
      "run_control" => Ok(Self::RunControl),
      _ => Err("component must be collection, publishing, queue-execution or run-control".into()),
    }
  }
}

/// A hook is called afresh for each component generation, after its predecessor joins.
pub type ComponentRunner = Arc<dyn Fn(ServiceContext) -> Result<()> + Send + Sync>;

#[derive(Clone, Default)]
pub struct ComponentRunners(BTreeMap<ServiceComponent, BTreeMap<&'static str, ComponentRunner>>);

impl ComponentRunners {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn insert(
    &mut self,
    component: ServiceComponent,
    runner: ComponentRunner,
  ) -> Option<ComponentRunner> {
    self
      .0
      .entry(component)
      .or_default()
      .insert(component.as_str(), runner)
  }

  /// Add another worker thread controlled by the same public component.
  pub fn add(&mut self, component: ServiceComponent, name: &'static str, runner: ComponentRunner) {
    self.0.entry(component).or_default().insert(name, runner);
  }
}

struct PartState {
  phase: &'static str,
  detail: Value,
  stop: Arc<AtomicBool>,
}

struct ComponentState {
  desired_running: bool,
  generation: u64,
  launch_requested: bool,
  phase: &'static str,
  detail: Value,
  stop: Option<Arc<AtomicBool>>,
  parts: BTreeMap<&'static str, PartState>,
}

impl ComponentState {
  fn new(desired_running: bool) -> Self {
    Self {
      desired_running,
      generation: 0,
      launch_requested: desired_running,
      phase: "stopped",
      detail: json!({"status": "stopped"}),
      stop: None,
      parts: BTreeMap::new(),
    }
  }

  fn snapshot(&self) -> Value {
    let mut value = self.detail.clone();
    value["desired_running"] = json!(self.desired_running);
    value["generation"] = json!(self.generation);
    value["phase"] = json!(self.phase);
    value["status"] = json!(self.phase);
    let parts: BTreeMap<_, _> = self
      .parts
      .iter()
      .map(|(name, part)| {
        let mut detail = part.detail.clone();
        detail["phase"] = json!(part.phase);
        (*name, detail)
      })
      .collect();
    value["parts"] = json!(parts);
    value
  }

  fn refresh_phase(&mut self) {
    if self
      .parts
      .values()
      .any(|part| matches!(part.phase, "starting" | "running"))
    {
      self.phase = if self
        .stop
        .as_ref()
        .is_some_and(|stop| stop.load(Ordering::Acquire))
      {
        "stopping"
      } else if self
        .parts
        .values()
        .any(|part| matches!(part.phase, "failed" | "paused"))
      {
        "degraded"
      } else if self.parts.values().all(|part| part.phase == "starting") {
        "starting"
      } else {
        "running"
      };
    }
  }
}

struct SharedState {
  stop: AtomicBool,
  rescan: AtomicBool,
  collection: Mutex<CollectionState>,
  execution: Mutex<Value>,
  components: Mutex<BTreeMap<ServiceComponent, ComponentState>>,
  active_jobs: AtomicUsize,
  pending_jobs: AtomicUsize,
  wake: (Mutex<()>, Condvar),
}

/// Cooperative cancellation and public status for one component generation.
/// A stopped generation's flag is never reused or reset during restart.
#[derive(Clone)]
pub struct ServiceContext {
  stop: Arc<AtomicBool>,
  shared: Arc<SharedState>,
  component: Option<ServiceComponent>,
  generation: u64,
  part: Option<&'static str>,
}

#[derive(Clone)]
struct CollectionState {
  ready: bool,
  detail: Value,
}

impl ServiceContext {
  #[cfg(test)]
  fn new() -> Self {
    Self::with_preferences(&preferences::Preferences::default())
  }

  fn with_preferences(preferences: &preferences::Preferences) -> Self {
    Self {
      stop: Arc::new(AtomicBool::new(false)),
      shared: Arc::new(SharedState {
        stop: AtomicBool::new(false),
        rescan: AtomicBool::new(true),
        collection: Mutex::new(CollectionState {
          ready: false,
          detail: json!({"status": "starting"}),
        }),
        execution: Mutex::new(json!({"status": "disabled"})),
        components: Mutex::new(
          ServiceComponent::ALL
            .into_iter()
            .map(|component| {
              (
                component,
                ComponentState::new(preferences.enabled(component)),
              )
            })
            .collect(),
        ),
        active_jobs: AtomicUsize::new(0),
        pending_jobs: AtomicUsize::new(0),
        wake: (Mutex::new(()), Condvar::new()),
      }),
      component: None,
      generation: 0,
      part: None,
    }
  }

  pub fn stop_flag(&self) -> Arc<AtomicBool> {
    self.stop.clone()
  }

  pub fn is_stopping(&self) -> bool {
    self.stop.load(Ordering::Acquire) || self.shared.stop.load(Ordering::Acquire)
  }

  /// Returns true when stopping. Control requests wake sleeping components.
  pub fn wait(&self, duration: Duration) -> bool {
    let guard = self
      .shared
      .wake
      .0
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    let _ = self
      .shared
      .wake
      .1
      .wait_timeout_while(guard, duration, |_| !self.is_stopping());
    self.is_stopping()
  }

  fn current_generation(&self) -> bool {
    self.component.is_none_or(|component| {
      self
        .shared
        .components
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())[&component]
        .generation
        == self.generation
    })
  }

  /// Called after a supported local collection observation has been committed.
  pub fn collection_ready(&self, machine_id: String, sample_sequence: u64) {
    if !self.current_generation() {
      return;
    }
    let mut state = self
      .shared
      .collection
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    state.ready = !self.is_stopping();
    state.detail["status"] = json!("ready");
    state.detail["machine_id"] = json!(machine_id);
    state.detail["sample_sequence"] = json!(sample_sequence);
    drop(state);
    self.component_status(json!({"status": "running", "ready": !self.is_stopping()}));
    self.shared.wake.1.notify_all();
  }

  /// Safe operational fields only; credentials and task arguments stay private.
  pub fn collection_status(&self, detail: Value) {
    if !self.current_generation() {
      return;
    }
    merge_public(
      &mut self
        .shared
        .collection
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .detail,
      &detail,
    );
    if self.component == Some(ServiceComponent::Collection) {
      self.component_status(detail);
    }
  }

  /// Queue execution has readiness independent of local system collection.
  pub fn execution_status(&self, detail: Value) {
    if !self.current_generation() {
      return;
    }
    merge_public(
      &mut self
        .shared
        .execution
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()),
      &detail,
    );
    if self.component == Some(ServiceComponent::QueueExecution) {
      self.component_status(detail);
    }
  }

  /// Update this generation's public component status. Unknown fields are ignored.
  pub fn component_status(&self, detail: Value) {
    let Some(component) = self.component else {
      return;
    };
    let mut components = self
      .shared
      .components
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    let state = components.get_mut(&component).unwrap();
    if state.generation != self.generation {
      return;
    }
    merge_public(&mut state.detail, &detail);
    if let Some(part) = self.part.and_then(|name| state.parts.get_mut(name)) {
      merge_public(&mut part.detail, &detail);
      if part.phase == "starting" {
        part.phase = "running";
      }
    }
    state.refresh_phase();
  }

  fn request_scan(&self) {
    self.shared.rescan.store(true, Ordering::Release);
    self.shared.wake.1.notify_all();
  }

  fn request_component(&self, component: ServiceComponent, enabled: bool, force_restart: bool) {
    let mut components = self
      .shared
      .components
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    let state = components.get_mut(&component).unwrap();
    state.desired_running = enabled;
    let mut restarting = false;
    if enabled {
      if state
        .stop
        .as_ref()
        .is_none_or(|stop| stop.load(Ordering::Acquire))
      {
        state.launch_requested = true;
      } else if force_restart
        || state
          .parts
          .values()
          .any(|part| matches!(part.phase, "failed" | "paused"))
      {
        state.launch_requested = true;
        state.stop.as_ref().unwrap().store(true, Ordering::Release);
        for part in state.parts.values() {
          part.stop.store(true, Ordering::Release);
        }
        state.phase = "stopping";
        state.detail["status"] = json!("stopping");
        restarting = true;
      }
    } else {
      state.launch_requested = false;
      if let Some(stop) = &state.stop {
        stop.store(true, Ordering::Release);
        for part in state.parts.values() {
          part.stop.store(true, Ordering::Release);
        }
        state.phase = "stopping";
        state.detail["status"] = json!("stopping");
      } else {
        state.phase = "stopped";
        state.detail["status"] = json!("stopped");
      }
    }
    drop(components);
    if (!enabled || restarting) && component == ServiceComponent::Collection {
      self
        .shared
        .collection
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .ready = false;
    }
    self.shared.wake.1.notify_all();
  }

  fn request_stop(&self) {
    self.shared.stop.store(true, Ordering::Release);
    self.stop.store(true, Ordering::Release);
    let mut components = self
      .shared
      .components
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    for state in components.values_mut() {
      if let Some(stop) = &state.stop {
        stop.store(true, Ordering::Release);
        for part in state.parts.values() {
          part.stop.store(true, Ordering::Release);
        }
        state.phase = "stopping";
        state.detail["status"] = json!("stopping");
      }
    }
    self.shared.wake.1.notify_all();
  }
}

fn merge_public(destination: &mut Value, detail: &Value) {
  let Some(fields) = detail.as_object() else {
    return;
  };
  for (key, value) in fields {
    if matches!(
      key.as_str(),
      "status"
        | "ready"
        | "machine_id"
        | "sample_sequence"
        | "inventory_revision"
        | "interval_seconds"
        | "updated_at_ms"
        | "publishing"
        | "last_error"
        | "warnings"
        | "worker_id"
        | "online"
        | "quarantined"
        | "capacity"
        | "gpu_busy_memory_threshold_bytes"
        | "available"
        | "reserved"
        | "unavailable_gpus"
        | "gpu_availability_observed_at_ms"
        | "assigned_jobs"
        | "outstanding_attempts"
        | "last_seen_at_ms"
        | "active_jobs"
        | "pending_jobs"
        | "job_concurrency"
    ) {
      destination[key] = value.clone();
    }
  }
}

/// Require a live publisher that understands launch-time experiment intent.
pub fn ensure_experiment_assignment_ready(paths: &StoragePaths) -> Result<()> {
  let service = ensure_started(paths)?;
  if service["ready"] != true
    || !service["capabilities"].as_array().is_some_and(|values| {
      values
        .iter()
        .any(|value| value == expri_core::experiment_assignment::EXPERIMENT_ASSIGNMENT_CAPABILITY)
    })
  {
    return Err(error(
      "the running worker service does not support experiment assignment; upgrade expri and run `expri service restart` before launching this run",
    ));
  }
  Ok(())
}

/// Accept a job durably before startup. Paused components remain paused.
pub fn register(paths: &StoragePaths, job: PreparedJob) -> Result<Value> {
  let mut report = registry::register(paths, job)?;
  match ensure_started(paths) {
    Ok(service) => {
      if service["components"][ServiceComponent::Publishing.as_str()]["desired_running"] == false {
        report["warning"] = json!(
          "Publishing is queued; publishing is paused. Run service start --component publishing to resume."
        );
      }
      report["service"] = service;
    }
    Err(_) => {
      report["service"] =
        status(paths).unwrap_or_else(|_| json!({"running": false, "ready": false}));
      report["warning"] =
        json!("Publishing is queued; the worker service is not ready. Run service start to retry.");
    }
  }
  #[cfg(unix)]
  let _ = unix::wake(paths);
  Ok(report)
}

pub fn resume_registered_run(
  paths: &StoragePaths,
  run_dir: &std::path::Path,
) -> Result<Option<Value>> {
  registry::saved_run(paths, run_dir)?
    .map(|job| register(paths, job))
    .transpose()
}

pub(crate) fn saved_publishing_job(
  paths: &StoragePaths,
  run_dir: &std::path::Path,
) -> Result<Option<PreparedJob>> {
  registry::saved_run(paths, run_dir)
}

/// Private, validated registrations, including finished runs that can continue.
/// Credentials remain inside PreparedJob and must never enter status responses.
pub fn registered_runs(paths: &StoragePaths) -> Result<Vec<PreparedJob>> {
  registry::registered_runs(paths)
}

pub(crate) fn rearm_publishing_job(paths: &StoragePaths, job: PreparedJob) -> Result<()> {
  registry::rearm(paths, &job)?;
  register(paths, job).map(|_| ())
}

pub fn job_status(paths: &StoragePaths, job_id: &str) -> Result<Value> {
  registry::job_status(paths, job_id)
}

fn error(message: impl Into<String>) -> ExpriError {
  ExpriError::Message(message.into())
}

#[cfg(not(unix))]
pub fn ensure_started(_: &StoragePaths) -> Result<Value> {
  Err(error("shared worker services require Unix"))
}
#[cfg(not(unix))]
pub fn ensure_collection_ready(_: &StoragePaths) -> Result<Value> {
  Err(error("shared worker services require Unix"))
}
#[cfg(not(unix))]
pub fn start_component(_: &StoragePaths, _: ServiceComponent) -> Result<Value> {
  Err(error("shared worker services require Unix"))
}
#[cfg(not(unix))]
pub fn restart_component(_: &StoragePaths, _: ServiceComponent) -> Result<Value> {
  Err(error("shared worker services require Unix"))
}
#[cfg(not(unix))]
pub fn restart(_: &StoragePaths) -> Result<Value> {
  Err(error("shared worker services require Unix"))
}
#[cfg(not(unix))]
pub fn stop_component(_: &StoragePaths, _: ServiceComponent) -> Result<Value> {
  Err(error("shared worker services require Unix"))
}
#[cfg(not(unix))]
pub fn status(_: &StoragePaths) -> Result<Value> {
  Ok(json!({"running": false, "ready": false, "supported": false}))
}
#[cfg(not(unix))]
pub fn stop(_: &StoragePaths) -> Result<Value> {
  Err(error("shared worker services require Unix"))
}
#[cfg(not(unix))]
pub fn observe(_: &StoragePaths, _: &str, _: bool) -> Result<Value> {
  Err(error("shared worker services require Unix"))
}
#[cfg(not(unix))]
pub fn run_foreground(
  _: &StoragePaths,
  _: impl FnOnce(ServiceContext) -> Result<()> + Send + 'static,
) -> Result<()> {
  Err(error("shared worker services require Unix"))
}
#[cfg(not(unix))]
pub fn run_foreground_components(_: &StoragePaths, _: ComponentRunners) -> Result<()> {
  Err(error("shared worker services require Unix"))
}

#[cfg(all(test, unix))]
mod tests;
