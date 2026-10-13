use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use expri_client::SystemProfileClient;
use expri_client::client::ResolvedClient;
use expri_client::worker_service::ServiceContext;
use expri_core::lock::{self, LockAttempt};
use expri_core::storage::{StoragePaths, ensure_private_file};
use expri_core::system_profile::ProfileAcknowledgement;
use expri_system::collector::Collector;
use expri_system::store::LocalStore;
use serde_json::{Value, json};

use crate::error::{ExpriError, Result};

use super::config::{CollectionConfig, PublishingMode};
use super::now_ms;

const STOP_POLL: Duration = Duration::from_millis(100);
// HTTP control requests have a thirty-second timeout. Release the collector
// lease first, so an unavailable publisher cannot delay a terminal run fence.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(35);

enum BackgroundKind {
  Exporter,
  Publisher,
}

impl BackgroundKind {
  fn name(&self) -> &'static str {
    match self {
      Self::Exporter => "expri-run-capture-exporter",
      Self::Publisher => "expri-system-publisher",
    }
  }
}

struct BackgroundTask {
  kind: BackgroundKind,
  handle: JoinHandle<()>,
}

trait CollectionControl: Clone + Send + 'static {
  fn stop_flag(&self) -> Arc<AtomicBool>;
  fn is_stopping(&self) -> bool;
  fn wait(&self, duration: Duration) -> bool;
  fn collection_ready(&self, machine_id: String, sample_sequence: u64);
  fn collection_status(&self, detail: Value);
}

impl CollectionControl for ServiceContext {
  fn stop_flag(&self) -> Arc<AtomicBool> {
    ServiceContext::stop_flag(self)
  }
  fn is_stopping(&self) -> bool {
    ServiceContext::is_stopping(self)
  }
  fn wait(&self, duration: Duration) -> bool {
    ServiceContext::wait(self, duration)
  }
  fn collection_ready(&self, machine_id: String, sample_sequence: u64) {
    ServiceContext::collection_ready(self, machine_id, sample_sequence);
  }
  fn collection_status(&self, detail: Value) {
    ServiceContext::collection_status(self, detail);
  }
}

/// Collection runs in the shared worker process, independently of uploads.
pub(super) fn run(
  paths: StoragePaths,
  config: CollectionConfig,
  context: ServiceContext,
) -> Result<()> {
  collect(paths, config, context)
}

fn collect<C: CollectionControl>(
  paths: StoragePaths,
  config: CollectionConfig,
  context: C,
) -> Result<()> {
  let lease = collector_lease(&paths)?;
  let mut store = LocalStore::open(&paths.profile_database())?;
  let mut collector = Collector::with_interval(config.interval)?;
  let machine_id = store.machine_id().to_owned();
  let publisher = Publisher::snapshot(config.publishing);
  let mut children = Vec::new();
  spawn_background(&mut children, BackgroundKind::Exporter, {
    let paths = paths.clone();
    let context = context.clone();
    move || materialize_loop(paths, context)
  });
  match publisher {
    Publisher::Connected(client) => {
      save_status(&paths, &context, "starting", None);
      let publishing_paths = paths.clone();
      let publishing_context = context.clone();
      if !spawn_background(&mut children, BackgroundKind::Publisher, move || {
        publish_loop(publishing_paths, client, publishing_context);
      }) {
        save_status(
          &paths,
          &context,
          "paused",
          Some("system publishing thread could not start; local collection continues".into()),
        );
      }
    }
    Publisher::Inactive { status, error } => save_status(&paths, &context, status, error),
  }
  eprintln!(
    "System profile: {} (every {}s, {})",
    machine_id,
    config.interval.as_secs(),
    paths.root().display()
  );
  let mut ready = false;
  let mut deadline = Instant::now();
  let outcome = (|| -> Result<()> {
    while !context.is_stopping() {
      // Finish observations already in progress even if stop arrives during
      // collection. record commits its capture outbox and support marker together.
      let (inventory, sample) = collector.collect()?;
      let sample = store.record(inventory, sample)?;
      collection_status(
        &paths,
        &context,
        json!({
          "status": "collecting", "updated_at_ms": now_ms(), "last_error": null,
          "machine_id": machine_id, "sample_sequence": sample.sequence,
          "inventory_revision": sample.inventory_revision,
          "interval_seconds": config.interval.as_secs(),
        }),
      );
      if !ready {
        context.collection_ready(machine_id.clone(), sample.sequence);
        ready = true;
      }
      store.compact(now_ms())?;
      deadline += config.interval;
      if deadline <= Instant::now() {
        deadline = Instant::now() + config.interval;
      }
      if wait_until(&context, deadline) {
        break;
      }
    }
    Ok(())
  })();
  context.stop_flag().store(true, Ordering::Release);
  // No collector-owned observation remains in flight. Releasing the lease lets
  // a terminal run finalize safely while network shutdown drains.
  drop(collector);
  drop(store);
  drop(lease);
  collection_status(
    &paths,
    &context,
    json!({
      "status": if outcome.is_ok() { "stopped" } else { "failed" },
      "updated_at_ms": now_ms(), "machine_id": machine_id,
      "last_error": outcome.as_ref().err().map(safe_publishing_error),
    }),
  );
  join_background(children);
  outcome
}

fn spawn_background(
  children: &mut Vec<BackgroundTask>,
  kind: BackgroundKind,
  work: impl FnOnce() + Send + 'static,
) -> bool {
  let name = kind.name();
  match thread::Builder::new().name(name.into()).spawn(work) {
    Ok(handle) => {
      children.push(BackgroundTask { kind, handle });
      true
    }
    Err(_) => {
      eprintln!(
        "warning: {name} could not start; durable local observations remain available for recovery"
      );
      false
    }
  }
}

fn join_background(children: Vec<BackgroundTask>) {
  join_background_until(children, Instant::now() + SHUTDOWN_GRACE);
}

fn join_background_until(children: Vec<BackgroundTask>, deadline: Instant) {
  for child in children {
    // A publisher must finish its bounded control request before the hook can
    // return and the shared runtime can release its service lease. Only durable
    // local exporters may outlive the grace; they cannot start network transfers.
    let network_producer = matches!(child.kind, BackgroundKind::Publisher);
    while !child.handle.is_finished() && (network_producer || Instant::now() < deadline) {
      thread::sleep(STOP_POLL);
    }
    if child.handle.is_finished() {
      if child.handle.join().is_err() {
        eprintln!(
          "warning: a system background task stopped unexpectedly; its durable queue remains available"
        );
      }
    } else {
      eprintln!(
        "warning: a system background task exceeded its shutdown grace; committed observations remain available"
      );
    }
  }
}

fn wait_until(context: &impl CollectionControl, deadline: Instant) -> bool {
  while !context.is_stopping() {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      return false;
    }
    if context.wait(remaining.min(STOP_POLL)) {
      return true;
    }
  }
  true
}

fn wait(context: &impl CollectionControl, duration: Duration) -> bool {
  wait_until(context, Instant::now() + duration)
}

fn materialize_loop(paths: StoragePaths, context: impl CollectionControl) {
  while !context.is_stopping() {
    if let Ok(Some(mut store)) = LocalStore::open_existing(&paths.profile_database()) {
      while !context.is_stopping() {
        // Slow output files never share the collector's connection or schedule.
        if store.export_run_captures().is_err() || wait(&context, Duration::from_millis(250)) {
          break;
        }
      }
    }
    if wait(&context, Duration::from_secs(1)) {
      break;
    }
  }
  // The stopped collector has committed any in-flight sample. Try its durable
  // outbox once; inaccessible destinations stay queued for the next service.
  if let Ok(Some(mut store)) = LocalStore::open_existing(&paths.profile_database()) {
    let _ = store.export_run_captures();
  }
}

enum Publisher {
  Connected(ResolvedClient),
  Inactive {
    status: &'static str,
    error: Option<String>,
  },
}

impl Publisher {
  fn snapshot(mode: PublishingMode) -> Self {
    match mode {
      PublishingMode::Disabled => Self::Inactive {
        status: "disabled",
        error: None,
      },
      PublishingMode::Unconfigured => Self::Inactive {
        status: "unconfigured",
        error: None,
      },
      PublishingMode::Configured(path) => match ResolvedClient::resolve(&path) {
        Ok(client) => Self::Connected(client),
        Err(error) => Self::Inactive {
          status: "paused",
          error: Some(safe_publishing_error(&error)),
        },
      },
    }
  }
}

pub(super) fn collector_lease(paths: &StoragePaths) -> Result<expri_core::lock::FileLock> {
  let lease_path = paths.root().join("collector.lock");
  ensure_private_file(&lease_path)?;
  match lock::try_lock_file(&lease_path, false)? {
    LockAttempt::Acquired(lease) => Ok(lease),
    LockAttempt::Busy => Err(ExpriError::Message(
      "a system-profile collector is already running in this storage directory".into(),
    )),
    LockAttempt::Missing => Err(ExpriError::Message(
      "system-profile collector lease disappeared".into(),
    )),
  }
}

pub(super) fn publish_batch(
  store: &mut LocalStore,
  client: &SystemProfileClient,
) -> Result<Option<ProfileAcknowledgement>> {
  store.compact(now_ms())?;
  let batch = store.pending_batch(client.endpoint_key())?;
  if batch.samples.is_empty() && batch.hours.is_empty() && batch.inventories.is_empty() {
    return Ok(None);
  }
  let acknowledgement = client.publish(&batch)?;
  store.acknowledge(client.endpoint_key(), &batch, &acknowledgement)?;
  Ok(Some(acknowledgement))
}

fn publish_loop(paths: StoragePaths, resolved: ResolvedClient, context: impl CollectionControl) {
  let mut retry_seconds = 1;
  while !context.is_stopping() {
    let connected: Result<()> = (|| {
      let client = SystemProfileClient::from_resolved(&resolved, context.stop_flag())?;
      if context.is_stopping() {
        return Ok(());
      }
      let mut store = LocalStore::open_existing(&paths.profile_database())?
        .ok_or_else(|| ExpriError::Message("local system profile is unavailable".into()))?;
      save_status(&paths, &context, "connected", None);
      while !context.is_stopping() {
        match publish_batch(&mut store, &client)? {
          Some(acknowledgement) => {
            publishing_status(
              &paths,
              &context,
              json!({
                "status": "published", "updated_at_ms": now_ms(), "last_error": null,
                "machine_id": acknowledgement.machine_id, "sample_sequence": acknowledgement.sample_sequence,
                "inventory_revision": acknowledgement.inventory_revision,
              }),
            )?;
            retry_seconds = 1;
            if wait(&context, Duration::from_millis(100)) {
              break;
            }
          }
          None => {
            if wait(&context, Duration::from_secs(5)) {
              break;
            }
          }
        }
      }
      Ok(())
    })();
    if context.is_stopping() {
      break;
    }
    if let Err(error) = connected {
      let detail = resolved.redact(&safe_publishing_error(&error));
      save_status(&paths, &context, "retrying", Some(detail.clone()));
      eprintln!("System profile publishing will retry: {detail}");
    }
    if wait(&context, Duration::from_secs(retry_seconds)) {
      break;
    }
    retry_seconds = (retry_seconds * 2).min(30);
  }
  save_status(&paths, &context, "stopped", None);
}

fn safe_publishing_error(error: &ExpriError) -> String {
  let detail = match error {
    ExpriError::Toml(_) => "system publishing client configuration is invalid".into(),
    ExpriError::Json(_) => "system publishing metadata is invalid".into(),
    ExpriError::Io(_) | ExpriError::IoContext { .. } => {
      "system publisher could not read or save local files".into()
    }
    _ => error.to_string(),
  };
  if detail.contains("http://") || detail.contains("https://") {
    return "system publishing failed; saved samples will be retried".into();
  }
  detail
    .chars()
    .filter(|character| !character.is_control())
    .take(512)
    .collect()
}

fn collection_status(paths: &StoragePaths, context: &impl CollectionControl, status: Value) {
  context.collection_status(status.clone());
  if write_status(paths.root(), "collector.json", &status).is_err() {
    eprintln!("System collection status could not be saved");
  }
}

fn save_status(
  paths: &StoragePaths,
  context: &impl CollectionControl,
  status: &str,
  error: Option<String>,
) {
  if publishing_status(paths, context, json!({
    "status": status, "updated_at_ms": now_ms(),
    "last_error": error.map(|error| error.chars().filter(|character| !character.is_control()).take(1024).collect::<String>()),
  })).is_err() {
    eprintln!("System profile publishing status could not be saved");
  }
}

fn publishing_status(
  paths: &StoragePaths,
  context: &impl CollectionControl,
  status: Value,
) -> Result<()> {
  context.collection_status(json!({ "publishing": status.clone() }));
  write_status(paths.root(), "publishing.json", &status)
}

fn write_status(root: &Path, filename: &str, status: &Value) -> Result<()> {
  let destination = root.join(filename);
  ensure_private_file(&destination)?;
  let mut temporary = tempfile::NamedTempFile::new_in(root)?;
  temporary.write_all(&serde_json::to_vec(status)?)?;
  temporary.as_file().sync_all()?;
  temporary
    .persist(destination)
    .map_err(|error| error.error)?;
  Ok(())
}

pub(super) fn read_status(path: &Path) -> Result<Option<Value>> {
  let metadata = match std::fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(error.into()),
  };
  if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16 * 1024 {
    return Err(ExpriError::Message(
      "system status must be a bounded regular file".into(),
    ));
  }
  let mut bytes = Vec::new();
  std::fs::File::open(path)?
    .take(16 * 1024 + 1)
    .read_to_end(&mut bytes)?;
  if bytes.len() > 16 * 1024 {
    return Err(ExpriError::Message("system status exceeds 16 KiB".into()));
  }
  Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests;
