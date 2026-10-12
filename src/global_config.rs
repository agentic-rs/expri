//! Private machine configuration writes shared by setup and authentication.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use expri_core::global_config::{GlobalConfig, MAX_CONFIG_BYTES};
use expri_core::lock::{self, LockAttempt};
use expri_core::service::ClientConfig;
use expri_core::storage::{StoragePaths, ensure_private_file};

use crate::error::{ExpriError, Result};

const CONNECTION_CHANGED: &str = "global server configuration changed during authentication; retry";
const REPLACEMENT_SYNC_ACTION: &str = "sync global configuration after replacement";

/// A replacement is already visible; revoking its new credential would leave
/// the saved connection unusable even though the write itself succeeded.
pub(crate) fn replacement_committed(error: &ExpriError) -> bool {
  matches!(error, ExpriError::IoContext { action, .. } if *action == REPLACEMENT_SYNC_ACTION)
}

pub(crate) fn config_path(paths: &StoragePaths) -> Result<PathBuf> {
  paths.validate_existing()?;
  // Canonicalize trusted external parent aliases while retaining the filename;
  // the bounded reader rejects a config-file symlink without following it.
  if paths.root().exists() {
    Ok(fs::canonicalize(paths.root())?.join("config.toml"))
  } else {
    Ok(paths.config_file())
  }
}

pub(crate) fn load(paths: &StoragePaths) -> Result<GlobalConfig> {
  GlobalConfig::load(&config_path(paths)?)
}

pub(crate) fn save_server(
  paths: &StoragePaths,
  server: Option<ClientConfig>,
) -> Result<GlobalConfig> {
  update_server(paths, None, server)
}

pub(crate) fn save_server_if_unchanged(
  paths: &StoragePaths,
  expected: Option<&ClientConfig>,
  server: Option<ClientConfig>,
) -> Result<GlobalConfig> {
  update_server(paths, Some(expected), server)
}

fn update_server(
  paths: &StoragePaths,
  expected: Option<Option<&ClientConfig>>,
  server: Option<ClientConfig>,
) -> Result<GlobalConfig> {
  if let Some(server) = &server {
    if server.token.is_none() && server.token_env.is_none() {
      server.validate_url()?;
    } else {
      server.validate()?;
    }
  }
  let existing = load(paths)?;
  // Unsetting an absent connection keeps inspection-like no-write semantics.
  // Conditional writes must always compare while holding the shared lock.
  if expected.is_none() && server.is_none() && existing.server.is_none() {
    return Ok(existing);
  }
  paths.ensure_private()?;
  let root = fs::canonicalize(paths.root())?;
  let path = root.join("config.toml");
  let lock_path = root.join("config.lock");
  let lock_file = ensure_private_file(&lock_path)?;
  let _lease = match lock::try_lock_file(&lock_path, false)? {
    LockAttempt::Acquired(lease) => lease,
    LockAttempt::Busy => return Err(message("global configuration is being updated; retry")),
    LockAttempt::Missing => return Err(message("global configuration lock disappeared; retry")),
  };
  validate_lock(&lock_file, &lock_path)?;
  let mut config = GlobalConfig::load(&path)?;
  if let Some(expected) = expected
    && !same_server(config.server.as_ref(), expected)
  {
    return Err(message(CONNECTION_CHANGED));
  }
  if server.is_none() && config.server.is_none() {
    return Ok(config);
  }
  config.server = server;
  config.validate()?;
  let encoded =
    toml::to_string_pretty(&config).map_err(|_| message("cannot encode global configuration"))?;
  if encoded.len() as u64 > MAX_CONFIG_BYTES {
    return Err(message("global configuration exceeds 32 KiB"));
  }
  let mut temporary = tempfile::Builder::new()
    .prefix(".config-")
    .tempfile_in(&root)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    temporary
      .as_file()
      .set_permissions(fs::Permissions::from_mode(0o600))?;
  }
  temporary.write_all(encoded.as_bytes())?;
  temporary.as_file().sync_all()?;
  persist_config(&root, &path, temporary)?;
  Ok(config)
}

fn persist_config(root: &Path, path: &Path, temporary: tempfile::NamedTempFile) -> Result<()> {
  persist_config_with(root, path, temporary, sync_config_directory)
}

fn persist_config_with(
  root: &Path,
  path: &Path,
  temporary: tempfile::NamedTempFile,
  sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
  // Recheck the destination after writing. An introduced symlink, invalid file,
  // or non-regular destination must never be replaced with credential material.
  GlobalConfig::load(path)?;
  temporary.persist(path).map_err(|error| error.error)?;
  sync_directory(root).map_err(|source| ExpriError::IoContext {
    action: REPLACEMENT_SYNC_ACTION,
    path: root.display().to_string(),
    source,
  })?;
  Ok(())
}

fn sync_config_directory(root: &Path) -> std::io::Result<()> {
  #[cfg(unix)]
  {
    File::open(root)?.sync_all()
  }
  #[cfg(not(unix))]
  {
    let _ = root;
    Ok(())
  }
}

fn same_server(left: Option<&ClientConfig>, right: Option<&ClientConfig>) -> bool {
  match (left, right) {
    (None, None) => true,
    (Some(left), Some(right)) => {
      left.url == right.url
        && left.token == right.token
        && left.token_env == right.token_env
        && left.project_id == right.project_id
    }
    _ => false,
  }
}

fn validate_lock(file: &File, path: &Path) -> Result<()> {
  let opened = file.metadata()?;
  let current = fs::symlink_metadata(path)?;
  if !opened.is_file() || !current.is_file() || current.file_type().is_symlink() {
    return Err(message("global configuration lock changed; retry"));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if opened.nlink() != 1
      || current.nlink() != 1
      || opened.dev() != current.dev()
      || opened.ino() != current.ino()
    {
      return Err(message("global configuration lock changed; retry"));
    }
  }
  Ok(())
}

fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests;
