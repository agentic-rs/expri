//! One private, durable worker service for the current user and storage root.
//!
//! Credentials stay in the private registry; control IPC contains only public status.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use expri_core::error::{ExpriError, Result};
use expri_core::storage::StoragePaths;
use serde_json::{Value, json};

use crate::publishing::PreparedJob;

mod files;
mod registry;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::{ensure_started, observe, run_foreground, status, stop};

/// Cooperative cancellation and collection readiness, shared with collection workers.
#[derive(Clone)]
pub struct ServiceContext {
  stop: Arc<AtomicBool>,
  rescan: Arc<AtomicBool>,
  state: Arc<Mutex<CollectionState>>,
  wake: Arc<(Mutex<()>, Condvar)>,
}

#[derive(Clone)]
struct CollectionState {
  ready: bool,
  detail: Value,
}

impl ServiceContext {
  fn new() -> Self {
    Self {
      stop: Arc::new(AtomicBool::new(false)),
      rescan: Arc::new(AtomicBool::new(true)),
      state: Arc::new(Mutex::new(CollectionState {
        ready: false,
        detail: json!({"status": "starting"}),
      })),
      wake: Arc::new((Mutex::new(()), Condvar::new())),
    }
  }

  pub fn stop_flag(&self) -> Arc<AtomicBool> {
    self.stop.clone()
  }

  pub fn is_stopping(&self) -> bool {
    self.stop.load(Ordering::Acquire)
  }

  /// Returns true when stopping, waking immediately for service stop requests.
  pub fn wait(&self, duration: Duration) -> bool {
    let guard = self
      .wake
      .0
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    let _ = self
      .wake
      .1
      .wait_timeout_while(guard, duration, |_| !self.is_stopping());
    self.is_stopping()
  }

  /// Called only after a supported local collection observation has been committed.
  pub fn collection_ready(&self, machine_id: String, sample_sequence: u64) {
    let mut state = self
      .state
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    state.ready = true;
    state.detail["status"] = json!("ready");
    state.detail["machine_id"] = json!(machine_id);
    state.detail["sample_sequence"] = json!(sample_sequence);
    self.wake.1.notify_all();
  }

  /// Safe operational fields only: never pass credentials or raw process environments.
  pub fn collection_status(&self, detail: Value) {
    if detail.is_object() {
      let mut state = self
        .state
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
      for (key, value) in detail.as_object().unwrap() {
        if matches!(
          key.as_str(),
          "status"
            | "machine_id"
            | "sample_sequence"
            | "inventory_revision"
            | "interval_seconds"
            | "updated_at_ms"
            | "publishing"
            | "last_error"
            | "warnings"
        ) {
          state.detail[key] = value.clone();
        }
      }
    }
  }

  fn request_scan(&self) {
    self.rescan.store(true, Ordering::Release);
    self.wake.1.notify_all();
  }

  fn request_stop(&self) {
    self.stop.store(true, Ordering::Release);
    self.wake.1.notify_all();
  }
}

/// Accept a job durably before attempting startup. Readiness failures leave it queued.
pub fn register(paths: &StoragePaths, job: PreparedJob) -> Result<Value> {
  let mut report = registry::register(paths, job)?;
  match ensure_started(paths) {
    Ok(service) => report["service"] = service,
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

/// Retry a saved run registration using its pinned credential binding, even if
/// the caller's config or environment no longer exists.
pub fn resume_registered_run(
  paths: &StoragePaths,
  run_dir: &std::path::Path,
) -> Result<Option<Value>> {
  registry::saved_run(paths, run_dir)?
    .map(|job| register(paths, job))
    .transpose()
}

/// Locate a saved job using its stable destination/run identity, without reading credentials.
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

#[cfg(all(test, unix))]
mod tests;
