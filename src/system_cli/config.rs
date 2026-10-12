use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::{ExpriError, Result};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GlobalConfig {
  #[serde(default)]
  pub system_profile: SystemProfileConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SystemProfileConfig {
  #[serde(default = "default_interval")]
  pub interval_seconds: u64,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PublishingConfig {
  pub client_config: PathBuf,
}

impl GlobalConfig {
  pub fn load(path: &Path) -> Result<Self> {
    let metadata = match std::fs::symlink_metadata(path) {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
      Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 32 * 1024 {
      return Err(ExpriError::Message(
        "global expri config must be a regular file no larger than 32 KiB".into(),
      ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
      .take(32 * 1024 + 1)
      .read_to_end(&mut bytes)?;
    if bytes.len() > 32 * 1024 {
      return Err(ExpriError::Message(
        "global expri config exceeds 32 KiB".into(),
      ));
    }
    let config: Self = toml::from_str(
      std::str::from_utf8(&bytes)
        .map_err(|_| ExpriError::Message("global expri config must be UTF-8".into()))?,
    )?;
    config.interval(None)?;
    if let Some(publish) = &config.system_profile.publish
      && !publish.client_config.is_absolute()
    {
      return Err(ExpriError::Message(
        "system_profile.publish.client_config must be an absolute path on this machine".into(),
      ));
    }
    Ok(config)
  }

  pub fn interval(&self, explicit: Option<u64>) -> Result<Duration> {
    let seconds = explicit.unwrap_or(self.system_profile.interval_seconds);
    if !(1..=60).contains(&seconds) {
      return Err(ExpriError::Message(
        "system profile interval must be between 1 and 60 seconds".into(),
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn missing_configuration_keeps_defaults_without_creating_files() {
    let directory = tempfile::tempdir().unwrap();
    let config = GlobalConfig::load(&directory.path().join("config.toml")).unwrap();
    assert_eq!(config.interval(None).unwrap(), Duration::from_secs(10));
    assert!(config.client_config(None).is_none());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
  }

  #[test]
  fn explicit_interval_and_client_selection_override_global_defaults() {
    let config: GlobalConfig = toml::from_str("[system_profile]\ninterval_seconds=30\n[system_profile.publish]\nclient_config='/etc/expri/machine.toml'\n").unwrap();
    assert_eq!(config.interval(Some(10)).unwrap(), Duration::from_secs(10));
    assert_eq!(
      config.client_config(Some("/tmp/client.toml".into())),
      Some("/tmp/client.toml".into())
    );
    assert!(config.interval(Some(0)).is_err());
  }
}
