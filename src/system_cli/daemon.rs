use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use expri_client::SystemProfileClient;
use expri_core::lock::{self, LockAttempt};
use expri_core::storage::{StoragePaths, ensure_private_file};
use expri_core::system_profile::ProfileAcknowledgement;
use expri_system::collector::Collector;
use expri_system::store::LocalStore;
use serde_json::{Value, json};

use crate::error::{ExpriError, Result};

use super::now_ms;

pub(super) fn run(
  paths: StoragePaths,
  interval: Duration,
  client_config: Option<PathBuf>,
) -> Result<()> {
  let _lease = collector_lease(&paths)?;
  let mut store = LocalStore::open(&paths.profile_database())?;
  let mut collector = Collector::with_interval(interval)?;
  let capture_paths = paths.clone();
  if thread::Builder::new()
    .name("expri-run-capture-exporter".into())
    .spawn(move || materialize_loop(capture_paths))
    .is_err()
  {
    eprintln!(
      "warning: run capture exporter could not start; observations remain in the durable outbox for recovery"
    );
  }
  eprintln!(
    "System profile: {} (every {}s, {})",
    store.machine_id(),
    interval.as_secs(),
    paths.root().display()
  );
  if let Some(client_config) = client_config {
    let publishing_paths = paths.clone();
    thread::Builder::new()
      .name("expri-system-publisher".into())
      .spawn(move || {
        publish_loop(publishing_paths, client_config);
      })?;
  } else {
    write_status(
      paths.root(),
      &json!({
        "status": "disabled", "updated_at_ms": now_ms(), "last_error": null,
      }),
    )?;
  }
  let mut deadline = Instant::now();
  loop {
    let (inventory, sample) = collector.collect()?;
    store.record(inventory, sample)?;
    store.compact(now_ms())?;
    deadline += interval;
    let now = Instant::now();
    if deadline <= now {
      deadline = now + interval;
    }
    thread::sleep(deadline.saturating_duration_since(Instant::now()));
  }
}

fn materialize_loop(paths: StoragePaths) {
  loop {
    if let Ok(Some(mut store)) = LocalStore::open_existing(&paths.profile_database()) {
      loop {
        // Output files can be slow or temporarily unavailable. Their durable
        // outbox is independent of the thread collecting machine observations.
        if store.export_run_captures().is_err() {
          break;
        }
        thread::sleep(Duration::from_millis(250));
      }
    }
    thread::sleep(Duration::from_secs(1));
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

fn publish_loop(paths: StoragePaths, client_config: PathBuf) {
  let mut retry_seconds = 1;
  loop {
    let connected: Result<()> = SystemProfileClient::new(&client_config).and_then(|client| {
      let mut store = LocalStore::open(&paths.profile_database())?;
      save_status(&paths, "connected", None);
      loop {
        match publish_batch(&mut store, &client)? {
          Some(acknowledgement) => {
            write_status(
              paths.root(),
              &json!({
                "status": "published", "updated_at_ms": now_ms(), "last_error": null,
                "machine_id": acknowledgement.machine_id,
                "sample_sequence": acknowledgement.sample_sequence,
                "inventory_revision": acknowledgement.inventory_revision,
              }),
            )?;
            retry_seconds = 1;
            thread::sleep(Duration::from_millis(100));
          }
          None => {
            thread::sleep(Duration::from_secs(5));
          }
        }
      }
    });
    if let Err(error) = connected {
      let detail = safe_publishing_error(&error);
      save_status(&paths, "retrying", Some(detail.clone()));
      eprintln!("System profile publishing will retry: {detail}");
    }
    thread::sleep(Duration::from_secs(retry_seconds));
    retry_seconds = (retry_seconds * 2).min(30);
  }
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

fn save_status(paths: &StoragePaths, status: &str, error: Option<String>) {
  let error = error.map(|error| {
    error
      .chars()
      .filter(|character| !character.is_control())
      .take(1024)
      .collect::<String>()
  });
  if let Err(failure) = write_status(
    paths.root(),
    &json!({
      "status": status, "updated_at_ms": now_ms(), "last_error": error,
    }),
  ) {
    eprintln!("System profile publishing status could not be saved: {failure}");
  }
}

fn write_status(root: &Path, status: &Value) -> Result<()> {
  let destination = root.join("publishing.json");
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
      "system publishing status must be a bounded regular file".into(),
    ));
  }
  let mut bytes = Vec::new();
  std::fs::File::open(path)?
    .take(16 * 1024 + 1)
    .read_to_end(&mut bytes)?;
  if bytes.len() > 16 * 1024 {
    return Err(ExpriError::Message(
      "system publishing status exceeds 16 KiB".into(),
    ));
  }
  Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn saved_publisher_errors_never_include_configuration_or_json_snippets() {
    let secret = "private-machine-token";
    let toml_error = toml::from_str::<Value>(&format!("token = '{secret}' garbage")).unwrap_err();
    let json_error = serde_json::from_str::<u64>(&format!("\"{secret}\"")).unwrap_err();
    for error in [ExpriError::Toml(toml_error), ExpriError::Json(json_error)] {
      let detail = safe_publishing_error(&error);
      assert!(!detail.contains(secret));
      assert!(detail.contains("invalid"));
    }
  }
}
