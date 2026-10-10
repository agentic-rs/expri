//! Local durable handoff of finalized outputs; registration never reads file contents.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::client::fs::{self, message};
use crate::error::Result;
use crate::lock::{FileLock, LockAttempt};

const RECORD: &str = "checkpoint-registrations.json";
const LIMIT: u64 = 1024 * 1024;
const FILE_LIMIT: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
struct Identity {
  size: u64,
  modified: SystemTime,
  #[cfg(unix)]
  device: u64,
  #[cfg(unix)]
  inode: u64,
  #[cfg(unix)]
  changed_seconds: i64,
  #[cfg(unix)]
  changed_nanoseconds: i64,
}

impl Identity {
  fn of(metadata: &Metadata) -> Result<Self> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Self {
      size: metadata.len(),
      modified: metadata.modified()?,
      #[cfg(unix)]
      device: metadata.dev(),
      #[cfg(unix)]
      inode: metadata.ino(),
      #[cfg(unix)]
      changed_seconds: metadata.ctime(),
      #[cfg(unix)]
      changed_nanoseconds: metadata.ctime_nsec(),
    })
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Registration {
  pub(super) path: String,
  pub(super) size: u64,
  pub(super) registered_at: String,
  pub(super) sync_status: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub(super) sync_error: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub(super) sha256: Option<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub(super) labels: Vec<String>,
  identity: Identity,
}

#[derive(Deserialize, Serialize)]
struct Registry {
  schema_version: u32,
  run_id: String,
  #[serde(default)]
  closed: bool,
  #[serde(default)]
  retired_paths: std::collections::BTreeSet<String>,
  files: BTreeMap<String, Registration>,
}

fn run_id(run_dir: &Path) -> Result<String> {
  fs::directory(run_dir)?;
  let state: Value = serde_json::from_slice(&fs::read_bounded(
    &run_dir.join("run-state.json"),
    16 * 1024 * 1024,
  )?)?;
  let run_id = state["run_id"]
    .as_str()
    .ok_or_else(|| message("checkpoint registration requires a run identity"))?;
  super::types::validate_component(run_id)?;
  Ok(run_id.into())
}

fn previously_synced(run_dir: &Path) -> Result<bool> {
  let path = run_dir.join("publishing-state.json");
  if fs::inspect(&path)?.is_none() {
    return Ok(false);
  }
  let state: Value = serde_json::from_slice(&fs::read_bounded(&path, 32 * 1024)?)?;
  Ok(state["status"] == "synced")
}

fn load(run_dir: &Path) -> Result<Registry> {
  let run_id = run_id(run_dir)?;
  let path = run_dir.join(RECORD);
  if fs::inspect(&path)?.is_none() {
    return Ok(Registry {
      schema_version: 1,
      run_id,
      closed: previously_synced(run_dir)?,
      retired_paths: std::collections::BTreeSet::new(),
      files: BTreeMap::new(),
    });
  }
  let saved: Registry = serde_json::from_slice(&fs::read_bounded(&path, LIMIT)?)?;
  if saved.schema_version != 1
    || saved.run_id != run_id
    || saved.files.len() + saved.retired_paths.len() > FILE_LIMIT
  {
    return Err(message(
      "checkpoint registry belongs to another run or exceeds its limit",
    ));
  }
  for path in &saved.retired_paths {
    validate_path(path)?;
    if saved.files.contains_key(path) {
      return Err(message(
        "checkpoint registry contains a retired active path",
      ));
    }
  }
  let mut labels = std::collections::BTreeSet::new();
  for (path, registration) in &saved.files {
    validate_path(path)?;
    if registration
      .labels
      .iter()
      .any(|label| !labels.insert(label))
      || registration.labels.len() > 2
      || registration
        .labels
        .iter()
        .any(|label| !matches!(label.as_str(), "best" | "latest"))
      || registration.path != *path
      || registration.sha256.as_ref().is_some_and(|digest| {
        digest.len() != 64
          || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
      })
      || registration.size != registration.identity.size
      || !matches!(
        registration.sync_status.as_str(),
        "registered" | "uploading" | "cloud" | "needs_attention"
      )
    {
      return Err(message(
        "checkpoint registry contains an invalid readiness record",
      ));
    }
  }
  Ok(saved)
}

fn validate_path(path: &str) -> Result<()> {
  crate::run_artifacts::validate_path(path)?;
  if path == "outputs/metrics.jsonl" || path == "outputs/params.json" {
    return Err(message(
      "tracking files cannot be registered as checkpoints",
    ));
  }
  Ok(())
}

fn lock(run_dir: &Path) -> Result<FileLock> {
  fs::directory(run_dir)?;
  let deadline = Instant::now() + Duration::from_secs(1);
  loop {
    match crate::lock::try_lock_file(&run_dir.join(".checkpoint-registrations.lock"), true)? {
      LockAttempt::Acquired(lease) => return Ok(lease),
      LockAttempt::Busy if Instant::now() < deadline => {
        std::thread::sleep(Duration::from_millis(10));
      }
      _ => return Err(message("checkpoint registry is busy; retry registration")),
    }
  }
}

fn save(run_dir: &Path, registry: &Registry) -> Result<()> {
  if serde_json::to_vec_pretty(registry)?.len() as u64 + 1 > LIMIT {
    return Err(message("checkpoint registry exceeds its metadata limit"));
  }
  fs::atomic_json(&run_dir.join(RECORD), registry)
}

/// Save the completed-file handoff before returning; hashes and transfers happen later.
pub(crate) fn register(run_dir: &Path, path: &str) -> Result<Value> {
  register_with_labels(run_dir, path, &[])
}

pub(crate) fn register_with_labels(run_dir: &Path, path: &str, labels: &[String]) -> Result<Value> {
  validate_path(path)?;
  if labels.len() > 2
    || labels
      .iter()
      .any(|label| !matches!(label.as_str(), "best" | "latest"))
  {
    return Err(message("checkpoint labels must be best or latest"));
  }
  let _lease = lock(run_dir)?;
  let mut registry = load(run_dir)?;
  let source = fs::open(&run_dir.join(path))?;
  let identity = Identity::of(&source.metadata()?)?;
  if let Some(previous) = registry.files.get(path) {
    if previous.identity != identity {
      return Err(message(
        "registered checkpoint changed; unregister its readiness record and finalize it under a new path",
      ));
    }
    let previous_labels = previous.labels.clone();
    if registry.closed && labels.iter().any(|label| !previous_labels.contains(label)) {
      return Err(message(
        "run file inventory is sealed; use service file-upload for post-run outputs",
      ));
    }
    assign_labels(&mut registry, path, labels);
    if registry.files[path].labels != previous_labels || !labels.is_empty() {
      save(run_dir, &registry)?;
    }
    return Ok(public_record(&registry.files[path]));
  }
  if registry.closed {
    return Err(message(
      "run file inventory is sealed; use service file-upload for post-run outputs",
    ));
  }
  if registry.retired_paths.contains(path) {
    return Err(message(
      "checkpoint path was retired; finalize the corrected checkpoint under a new path",
    ));
  }
  if registry.files.len() + registry.retired_paths.len() == FILE_LIMIT {
    return Err(message("register at most 200 checkpoints for a run"));
  }
  let registration = Registration {
    path: path.into(),
    size: identity.size,
    registered_at: chrono::Utc::now().to_rfc3339(),
    sync_status: "registered".into(),
    sync_error: None,
    sha256: None,
    labels: Vec::new(),
    identity,
  };
  registry.files.insert(path.into(), registration);
  assign_labels(&mut registry, path, labels);
  let report = public_record(&registry.files[path]);
  save(run_dir, &registry)?;
  Ok(report)
}

/// Discard only a failed readiness handoff; file bytes and upload receipts are retained.
pub(crate) fn unregister(run_dir: &Path, path: &str) -> Result<Value> {
  validate_path(path)?;
  let _lease = lock(run_dir)?;
  let mut registry = load(run_dir)?;
  if registry.closed {
    return Err(message("run file inventory is sealed"));
  }
  let registration = registry
    .files
    .get(path)
    .ok_or_else(|| message("checkpoint is not registered"))?;
  if registration.sync_status != "needs_attention" {
    return Err(message(
      "only checkpoints that need attention can be unregistered",
    ));
  }
  registry.files.remove(path);
  registry.retired_paths.insert(path.into());
  save(run_dir, &registry)?;
  Ok(json!({"path":path,"unregistered":true}))
}

fn assign_labels(registry: &mut Registry, path: &str, labels: &[String]) {
  for registration in registry.files.values_mut() {
    registration.labels.retain(|label| !labels.contains(label));
  }
  let current = &mut registry.files.get_mut(path).unwrap().labels;
  current.extend(labels.iter().cloned());
  current.sort();
  current.dedup();
}

fn public_record(registration: &Registration) -> Value {
  json!({
    "path": registration.path,
    "size": registration.size,
    "registered_at": registration.registered_at,
    "sync_status": registration.sync_status,
    "sync_error": registration.sync_error,
    "labels": registration.labels,
    "sha256": registration.sha256,
  })
}

pub(crate) fn list(run_dir: &Path) -> Result<Value> {
  let registry = load(run_dir)?;
  Ok(
    json!({"run_id":registry.run_id,"files":registry.files.values().map(public_record).collect::<Vec<_>>()}),
  )
}

pub(super) fn records(run_dir: &Path) -> Result<Vec<Registration>> {
  if fs::inspect(&run_dir.join(RECORD))?.is_none() {
    return Ok(Vec::new());
  }
  Ok(load(run_dir)?.files.into_values().collect())
}

pub(super) fn verify(run_dir: &Path, registration: &Registration) -> Result<()> {
  let current = fs::open(&run_dir.join(&registration.path))?;
  verify_metadata(registration, &current.metadata()?)
}

pub(super) fn verify_metadata(registration: &Registration, metadata: &Metadata) -> Result<()> {
  if Identity::of(metadata)? != registration.identity {
    return Err(message(
      "registered checkpoint changed; unregister its readiness record and finalize it under a new path",
    ));
  }
  Ok(())
}

pub(super) fn is_closed(run_dir: &Path) -> Result<bool> {
  if fs::inspect(&run_dir.join(RECORD))?.is_none() {
    return previously_synced(run_dir);
  }
  Ok(load(run_dir)?.closed)
}

/// Freeze the handoff before attempting the immutable server seal.
/// Saving first also protects against a seal response lost on a poor connection.
pub(super) fn close_if_ready(
  run_dir: &Path,
  prepare_inventory: impl FnOnce() -> Result<()>,
) -> Result<bool> {
  let _lease = lock(run_dir)?;
  let mut registry = load(run_dir)?;
  if registry
    .files
    .values()
    .any(|file| file.sync_status != "cloud")
  {
    return Ok(false);
  }
  if !registry.closed {
    if let Err(error) = prepare_inventory()
      && !registry.files.is_empty()
    {
      return Err(error);
    }
    registry.closed = true;
    save(run_dir, &registry)?;
  }
  Ok(true)
}

pub(super) fn complete(run_dir: &Path, registration: &Registration, digest: &str) -> Result<()> {
  if digest.len() != 64
    || !digest
      .bytes()
      .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
  {
    return Err(message(
      "checkpoint upload receipt has an invalid SHA256 digest",
    ));
  }
  let _lease = lock(run_dir)?;
  let mut registry = load(run_dir)?;
  let current = registry
    .files
    .get_mut(&registration.path)
    .ok_or_else(|| message("registered checkpoint readiness record disappeared"))?;
  if current.identity != registration.identity {
    return Err(message("registered checkpoint identity changed"));
  }
  current.sync_status = "cloud".into();
  current.sync_error = None;
  current.sha256 = Some(digest.into());
  save(run_dir, &registry)
}

pub(super) fn update(
  run_dir: &Path,
  registration: &Registration,
  status: &str,
  detail: Option<&str>,
) -> Result<()> {
  let _lease = lock(run_dir)?;
  let mut registry = load(run_dir)?;
  let current = registry
    .files
    .get_mut(&registration.path)
    .ok_or_else(|| message("registered checkpoint readiness record disappeared"))?;
  if current.identity != registration.identity {
    return Err(message("registered checkpoint identity changed"));
  }
  current.sync_status = status.into();
  current.sync_error = detail.map(str::to_string);
  save(run_dir, &registry)
}

#[cfg(test)]
mod tests;
