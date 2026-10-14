//! Shared machine configuration. Loading never creates files or resolves secrets.

use std::fs::{self, Metadata, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};
use crate::service::ClientConfig;

pub const MAX_CONFIG_BYTES: u64 = 32 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub server: Option<ClientConfig>,
  #[serde(default)]
  pub system_profile: SystemProfileConfig,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub queue_worker: Option<QueueWorkerConfig>,
}

/// Execution is explicitly enabled only by this machine-local declaration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueWorkerConfig {
  pub worker_id: String,
  pub project_ids: Vec<String>,
  #[serde(default)]
  pub capacity: QueueWorkerCapacityConfig,
}

/// Omitted fields use the machine's detected execution budget.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueWorkerCapacityConfig {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cpus: Option<u32>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub memory_bytes: Option<u64>,
  /// An explicit empty list opts out of GPU execution on this machine.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub gpus: Option<Vec<crate::resource_queue::WorkerGpu>>,
}

/// Fully resolved machine-local execution settings, ready for registration.
#[derive(Clone, Debug)]
pub struct ResolvedQueueWorkerConfig {
  pub worker_id: String,
  pub project_ids: Vec<String>,
  pub capacity: crate::resource_queue::WorkerCapacity,
}

impl QueueWorkerConfig {
  pub fn validate(&self) -> Result<()> {
    validate_worker_identity(&self.worker_id, &self.project_ids)?;
    validate_worker_capacity(&crate::resource_queue::WorkerCapacity {
      cpus: self.capacity.cpus.unwrap_or(1),
      memory_bytes: self.capacity.memory_bytes.unwrap_or(1),
      gpus: self.capacity.gpus.clone().unwrap_or_default(),
    })
  }

  pub fn needs_detection(&self) -> bool {
    self.capacity.cpus.is_none()
      || self.capacity.memory_bytes.is_none()
      || self.capacity.gpus.is_none()
  }

  pub fn resolve_capacity(
    &self,
    inferred: crate::resource_queue::WorkerCapacity,
  ) -> Result<ResolvedQueueWorkerConfig> {
    self.validate()?;
    let resolved = ResolvedQueueWorkerConfig {
      worker_id: self.worker_id.clone(),
      project_ids: self.project_ids.clone(),
      capacity: crate::resource_queue::WorkerCapacity {
        cpus: self.capacity.cpus.unwrap_or(inferred.cpus),
        memory_bytes: self.capacity.memory_bytes.unwrap_or(inferred.memory_bytes),
        gpus: self.capacity.gpus.clone().unwrap_or(inferred.gpus),
      },
    };
    resolved.validate()?;
    Ok(resolved)
  }
}

impl ResolvedQueueWorkerConfig {
  pub fn validate(&self) -> Result<()> {
    validate_worker_identity(&self.worker_id, &self.project_ids)?;
    validate_worker_capacity(&self.capacity)
  }
}

fn validate_worker_identity(worker_id: &str, project_ids: &[String]) -> Result<()> {
  crate::identifiers::validate_component(worker_id)?;
  if project_ids.is_empty() || project_ids.len() > 64 {
    return Err(invalid(
      "queue_worker.project_ids requires an explicit nonempty project allowlist",
    ));
  }
  let mut projects = std::collections::BTreeSet::new();
  for project in project_ids {
    crate::identifiers::validate_component(project)?;
    if !projects.insert(project) {
      return Err(invalid(
        "queue_worker.project_ids contains a duplicate project",
      ));
    }
  }
  Ok(())
}

fn validate_worker_capacity(capacity: &crate::resource_queue::WorkerCapacity) -> Result<()> {
  use crate::resource_queue::GpuVendor;
  capacity.validate()?;
  for gpu in &capacity.gpus {
    if gpu.vendor == GpuVendor::Apple && gpu.memory_bytes.is_some() {
      return Err(invalid(
        "queue_worker Apple GPU memory is shared; omit dedicated memory_bytes",
      ));
    }
  }
  Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemProfileConfig {
  #[serde(default = "default_interval")]
  pub interval_seconds: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub publish: Option<PublishingConfig>,
}

fn default_interval() -> u64 {
  10
}

impl Default for SystemProfileConfig {
  fn default() -> Self {
    Self {
      interval_seconds: default_interval(),
      publish: None,
    }
  }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishingConfig {
  pub client_config: PathBuf,
}

impl GlobalConfig {
  pub fn load(path: &Path) -> Result<Self> {
    let Some(text) = read_config_text(path)? else {
      return Ok(Self::default());
    };
    let config: Self =
      toml::from_str(&text).map_err(|_| invalid("global expri configuration is invalid"))?;
    config.validate()?;
    Ok(config)
  }

  pub fn validate(&self) -> Result<()> {
    self.interval(None)?;
    if let Some(worker) = &self.queue_worker {
      worker.validate()?;
    }
    if let Some(server) = &self.server {
      if server.token.is_none() && server.token_env.is_none() {
        server.validate_url()?;
      } else {
        server.validate()?;
      }
    }
    if let Some(publish) = &self.system_profile.publish
      && !publish.client_config.is_absolute()
    {
      return Err(invalid(
        "system_profile.publish.client_config must be an absolute path on this machine",
      ));
    }
    Ok(())
  }

  pub fn interval(&self, explicit: Option<u64>) -> Result<Duration> {
    let seconds = explicit.unwrap_or(self.system_profile.interval_seconds);
    if !(1..=60).contains(&seconds) {
      return Err(invalid(
        "system profile interval must be between 1 and 60 seconds",
      ));
    }
    Ok(Duration::from_secs(seconds))
  }

  pub fn client_config(&self, explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| {
      self
        .system_profile
        .publish
        .as_ref()
        .map(|publish| publish.client_config.clone())
    })
  }
}

/// Read a bounded regular configuration file without modifying it.
///
/// Parent aliases are canonicalized, supporting trusted external HOME aliases.
/// Callers using managed storage must first validate its owned directory boundary.
/// The selected file itself must never be a symlink, including during opening.
pub fn read_config_text(path: &Path) -> Result<Option<String>> {
  let initial = match fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(error.into()),
  };
  regular(&initial)?;
  let parent = path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .unwrap_or_else(|| Path::new("."));
  let physical = fs::canonicalize(parent)?.join(
    path
      .file_name()
      .ok_or_else(|| invalid("expri configuration needs a filename"))?,
  );
  for parent in physical.ancestors().skip(1) {
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
      return Err(invalid(
        "expri configuration has an unsafe parent directory",
      ));
    }
  }
  let before = fs::symlink_metadata(&physical)?;
  regular(&before)?;
  if !same_identity(&initial, &before) {
    return Err(invalid("expri configuration changed while opening"));
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options.open(&physical)?;
  let opened = file.metadata()?;
  regular(&opened)?;
  let current = fs::symlink_metadata(path)?;
  regular(&current)?;
  if !same_identity(&initial, &opened) || !same_identity(&initial, &current) {
    return Err(invalid("expri configuration changed while opening"));
  }
  let mut bytes = Vec::new();
  (&file).take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > MAX_CONFIG_BYTES {
    return Err(invalid("expri configuration exceeds 32 KiB"));
  }
  let after = file.metadata()?;
  let current = fs::symlink_metadata(path)?;
  regular(&current)?;
  if !same_identity(&initial, &current)
    || opened.len() != after.len()
    || opened.modified().ok() != after.modified().ok()
  {
    return Err(invalid("expri configuration changed while reading"));
  }
  String::from_utf8(bytes)
    .map(Some)
    .map_err(|_| invalid("expri configuration must be UTF-8"))
}

fn regular(metadata: &Metadata) -> Result<()> {
  if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_CONFIG_BYTES {
    return Err(invalid(
      "expri configuration must be a regular file no larger than 32 KiB",
    ));
  }
  Ok(())
}

fn same_identity(first: &Metadata, second: &Metadata) -> bool {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    first.dev() == second.dev() && first.ino() == second.ino()
  }
  #[cfg(not(unix))]
  {
    first
      .created()
      .ok()
      .zip(second.created().ok())
      .is_some_and(|(first, second)| first == second)
  }
}

fn invalid(message: &str) -> ExpriError {
  ExpriError::Message(message.into())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource_queue::{GpuVendor, WorkerCapacity, WorkerGpu};

  fn detected_capacity() -> WorkerCapacity {
    WorkerCapacity {
      cpus: 8,
      memory_bytes: 16 * 1024_u64.pow(3),
      gpus: vec![WorkerGpu {
        device_id: "GPU-a".into(),
        model: "NVIDIA GPU".into(),
        memory_bytes: Some(8 * 1024_u64.pow(3)),
        vendor: GpuVendor::Nvidia,
        selector: Some("GPU-a".into()),
      }],
    }
  }

  fn worker_config(capacity: &str) -> QueueWorkerConfig {
    let config: GlobalConfig = toml::from_str(&format!(
      "[queue_worker]\nworker_id='worker-a'\nproject_ids=['vision']\n{capacity}"
    ))
    .unwrap();
    config.queue_worker.unwrap()
  }

  #[test]
  fn worker_capacity_can_be_omitted_and_resolves_only_when_requested() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let contents = "[queue_worker]\nworker_id='worker-a'\nproject_ids=['vision']\n";
    fs::write(&path, contents).unwrap();
    let config = GlobalConfig::load(&path).unwrap();
    let worker = config.queue_worker.unwrap();
    assert!(worker.needs_detection());
    assert!(worker.capacity.cpus.is_none());
    assert!(worker.capacity.memory_bytes.is_none());
    assert!(worker.capacity.gpus.is_none());
    let resolved = worker.resolve_capacity(detected_capacity()).unwrap();
    assert_eq!(resolved.worker_id, "worker-a");
    assert_eq!(resolved.project_ids, ["vision"]);
    assert_eq!(resolved.capacity, detected_capacity());
    assert_eq!(fs::read_to_string(path).unwrap(), contents);
  }

  #[test]
  fn worker_partial_overrides_replace_only_explicit_fields() {
    let worker = worker_config("[queue_worker.capacity]\ncpus=32\n");
    worker.validate().unwrap();
    assert!(worker.needs_detection());
    let resolved = worker.resolve_capacity(detected_capacity()).unwrap();
    assert_eq!(resolved.capacity.cpus, 32);
    assert_eq!(
      resolved.capacity.memory_bytes,
      detected_capacity().memory_bytes
    );
    assert_eq!(resolved.capacity.gpus, detected_capacity().gpus);

    let worker = worker_config("[queue_worker.capacity]\nmemory_bytes=4096\n");
    let resolved = worker.resolve_capacity(detected_capacity()).unwrap();
    assert_eq!(resolved.capacity.cpus, 8);
    assert_eq!(resolved.capacity.memory_bytes, 4096);
    assert_eq!(resolved.capacity.gpus, detected_capacity().gpus);
  }

  #[test]
  fn worker_explicit_empty_gpus_disable_gpu_inference_and_survive_roundtrip() {
    let worker = worker_config("[queue_worker.capacity]\ncpus=4\nmemory_bytes=4096\ngpus=[]\n");
    assert!(!worker.needs_detection());
    let roundtrip: QueueWorkerConfig = toml::from_str(&toml::to_string(&worker).unwrap()).unwrap();
    assert_eq!(roundtrip.capacity.gpus, Some(vec![]));
    assert!(!roundtrip.needs_detection());
    let resolved = roundtrip.resolve_capacity(detected_capacity()).unwrap();
    assert_eq!(resolved.capacity.cpus, 4);
    assert_eq!(resolved.capacity.memory_bytes, 4096);
    assert!(resolved.capacity.gpus.is_empty());

    let omitted = worker_config("[queue_worker.capacity]\ncpus=4\nmemory_bytes=4096\n");
    let roundtrip: QueueWorkerConfig = toml::from_str(&toml::to_string(&omitted).unwrap()).unwrap();
    assert!(roundtrip.capacity.gpus.is_none());
    assert!(roundtrip.needs_detection());
    assert_eq!(
      roundtrip
        .resolve_capacity(detected_capacity())
        .unwrap()
        .capacity
        .gpus,
      detected_capacity().gpus
    );
  }

  #[test]
  fn legacy_worker_capacity_with_gpu_tables_keeps_its_explicit_budget() {
    let worker = worker_config(
      "[queue_worker.capacity]\ncpus=4\nmemory_bytes=4096\n\
      [[queue_worker.capacity.gpus]]\ndevice_id='GPU-b'\nmodel='NVIDIA GPU'\n\
      vendor='nvidia'\nselector='GPU-b'\nmemory_bytes=8192\n",
    );
    worker.validate().unwrap();
    assert!(!worker.needs_detection());
    let resolved = worker.resolve_capacity(detected_capacity()).unwrap();
    assert_eq!(resolved.capacity.cpus, 4);
    assert_eq!(resolved.capacity.memory_bytes, 4096);
    assert_eq!(resolved.capacity.gpus[0].device_id, "GPU-b");
    assert_eq!(resolved.capacity.gpus[0].memory_bytes, Some(8192));
    let roundtrip: QueueWorkerConfig = toml::from_str(&toml::to_string(&worker).unwrap()).unwrap();
    assert_eq!(
      roundtrip
        .resolve_capacity(detected_capacity())
        .unwrap()
        .capacity,
      resolved.capacity
    );
  }

  #[test]
  fn worker_invalid_overrides_fail_even_without_detection() {
    for capacity in [
      "[queue_worker.capacity]\ncpus=0\n",
      "[queue_worker.capacity]\ncpus=1000001\n",
      "[queue_worker.capacity]\nmemory_bytes=0\n",
      "[queue_worker.capacity]\n[[queue_worker.capacity.gpus]]\n\
       device_id='GPU-a'\nmodel='NVIDIA GPU'\nvendor='nvidia'\n",
      "[queue_worker.capacity]\n[[queue_worker.capacity.gpus]]\n\
       device_id='GPU-a'\nmodel='Apple GPU'\nvendor='apple'\nmemory_bytes=8192\n",
    ] {
      let worker = worker_config(capacity);
      assert!(worker.validate().is_err(), "accepted {capacity}");
      assert!(worker.resolve_capacity(detected_capacity()).is_err());
    }
  }

  #[test]
  fn worker_resolved_budget_must_be_valid_but_overridden_detection_is_ignored() {
    let omitted = worker_config("");
    let mut inferred = detected_capacity();
    inferred.cpus = 0;
    assert!(omitted.resolve_capacity(inferred.clone()).is_err());
    inferred.cpus = 8;
    inferred.memory_bytes = 0;
    assert!(omitted.resolve_capacity(inferred.clone()).is_err());
    inferred.memory_bytes = 8192;
    inferred.gpus[0].selector = None;
    assert!(omitted.resolve_capacity(inferred).is_err());

    let explicit = worker_config("[queue_worker.capacity]\ncpus=4\nmemory_bytes=4096\ngpus=[]\n");
    explicit
      .resolve_capacity(WorkerCapacity {
        cpus: 0,
        memory_bytes: 0,
        gpus: vec![],
      })
      .unwrap();
  }

  #[test]
  fn automatic_capacity_still_requires_explicit_valid_worker_identity_and_projects() {
    for identity in [
      "worker_id='worker-a'\nproject_ids=[]\n",
      "worker_id='worker-a'\nproject_ids=['vision', 'vision']\n",
      "worker_id='../worker-a'\nproject_ids=['vision']\n",
    ] {
      let config: GlobalConfig = toml::from_str(&format!("[queue_worker]\n{identity}")).unwrap();
      assert!(config.validate().is_err());
    }
  }

  #[test]
  fn missing_configuration_keeps_defaults_without_creating_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("absent/config.toml");
    let config = GlobalConfig::load(&path).unwrap();
    assert_eq!(config.interval(None).unwrap(), Duration::from_secs(10));
    assert!(config.server.is_none());
    assert!(config.client_config(None).is_none());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
  }

  #[test]
  fn server_and_system_settings_roundtrip_together() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let publish = directory.path().join("client.toml");
    let explicit = directory.path().join("explicit.toml");
    fs::write(&path, format!("[server]\nurl='https://example.net'\ntoken='must-not-leak'\n[system_profile]\ninterval_seconds=30\n[system_profile.publish]\nclient_config={:?}\n", publish.to_str().unwrap())).unwrap();
    let config = GlobalConfig::load(&path).unwrap();
    assert_eq!(
      config.server.as_ref().unwrap().token.as_deref(),
      Some("must-not-leak")
    );
    assert_eq!(config.interval(None).unwrap(), Duration::from_secs(30));
    assert_eq!(config.interval(Some(10)).unwrap(), Duration::from_secs(10));
    assert_eq!(config.client_config(Some(explicit.clone())), Some(explicit));
    assert_eq!(config.client_config(None), Some(publish));
    let roundtrip: GlobalConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
    roundtrip.validate().unwrap();
    assert_eq!(roundtrip.system_profile.interval_seconds, 30);
    assert!(!format!("{config:?}").contains("must-not-leak"));
  }

  #[test]
  fn url_only_server_survives_logout_without_becoming_an_authenticated_client() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
      &path,
      "[server]\nurl='https://example.net'\n[system_profile]\ninterval_seconds=30\n",
    )
    .unwrap();
    let config = GlobalConfig::load(&path).unwrap();
    let server = config.server.as_ref().unwrap();
    assert_eq!(server.url, "https://example.net");
    assert!(server.token.is_none());
    assert!(server.token_env.is_none());
    assert!(server.validate().is_err());
    config.validate().unwrap();
    let restored: GlobalConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
    restored.validate().unwrap();
    assert_eq!(restored.system_profile.interval_seconds, 30);
    fs::write(&path, "[server]\nurl='https://must-not-leak@host'\n").unwrap();
    let error = GlobalConfig::load(&path).unwrap_err();
    assert!(!error.to_string().contains("must-not-leak"));
  }

  #[test]
  fn malformed_or_invalid_configuration_never_echoes_contents() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for contents in [
      "[server]\nurl='https://must-not-leak@host'\ntoken='must-not-leak'\n",
      "[server]\nurl='https://host'\ntoken='must-not-leak'\ntoken_env='MUST_NOT_LEAK'\n",
      "[server]\nurl='https://host'\ntoken=must-not-leak\n",
      "[system_profile]\ninterval_seconds=0\n",
      "[system_profile.publish]\nclient_config='must-not-leak'\n",
      "[server]\nurl='https://host'\ntoken='must-not-leak'\nunknown='must-not-leak'\n",
    ] {
      fs::write(&path, contents).unwrap();
      let error = GlobalConfig::load(&path).unwrap_err();
      assert!(!format!("{error:?} {error}").contains("must-not-leak"));
      assert!(!error.to_string().contains("MUST_NOT_LEAK"));
    }
  }

  #[test]
  fn nonregular_oversized_and_nonutf8_files_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    assert!(GlobalConfig::load(directory.path()).is_err());
    let path = directory.path().join("config.toml");
    fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
    assert!(GlobalConfig::load(&path).is_err());
    fs::write(&path, [0xff]).unwrap();
    assert_eq!(
      GlobalConfig::load(&path).unwrap_err().to_string(),
      "expri configuration must be UTF-8"
    );
  }

  #[cfg(unix)]
  #[test]
  fn loading_readonly_files_preserves_permissions_and_rejects_file_links() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "[system_profile]\ninterval_seconds=20\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    assert_eq!(
      GlobalConfig::load(&path)
        .unwrap()
        .system_profile
        .interval_seconds,
      20
    );
    assert_eq!(
      fs::metadata(&path).unwrap().permissions().mode() & 0o777,
      0o400
    );
    let linked = directory.path().join("linked.toml");
    symlink(&path, &linked).unwrap();
    assert!(GlobalConfig::load(&linked).is_err());
    let dangling = directory.path().join("dangling.toml");
    symlink(directory.path().join("absent"), &dangling).unwrap();
    assert!(GlobalConfig::load(&dangling).is_err());
  }

  #[cfg(unix)]
  #[test]
  fn external_parent_alias_is_supported_and_fifo_is_rejected() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("config.toml"), "").unwrap();
    let linked = directory.path().join("linked");
    symlink(&real, &linked).unwrap();
    assert!(GlobalConfig::load(&linked.join("config.toml")).is_ok());
    let fifo = directory.path().join("fifo");
    let c_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    assert!(GlobalConfig::load(&fifo).is_err());
  }
}
