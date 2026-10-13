pub(super) use expri_core::global_config::GlobalConfig;

use std::path::PathBuf;
use std::time::Duration;

use expri_core::global_config::read_config_text;
use expri_core::storage::StoragePaths;

use crate::error::{ExpriError, Result};

pub(super) enum PublishingMode {
  Disabled,
  Unconfigured,
  Configured(PathBuf),
}

pub(super) struct CollectionConfig {
  pub interval: Duration,
  pub publishing: PublishingMode,
}

impl CollectionConfig {
  pub fn load(
    paths: &StoragePaths,
    interval_seconds: Option<u64>,
    client_config: Option<PathBuf>,
    no_publish: bool,
  ) -> Result<Self> {
    // Collection uses the machine settings only. Run publication validates its
    // own server binding; unrelated server credentials cannot block readiness.
    let mut config: GlobalConfig = match read_config_text(&paths.config_file())? {
      Some(text) => toml::from_str(&text)
        .map_err(|_| ExpriError::Message("global expri configuration is invalid".into()))?,
      None => GlobalConfig::default(),
    };
    config.server = None;
    config.validate()?;
    let publishing = if no_publish {
      PublishingMode::Disabled
    } else {
      config
        .client_config(client_config)
        .map(PublishingMode::Configured)
        .unwrap_or(PublishingMode::Unconfigured)
    };
    Ok(Self {
      interval: config.interval(interval_seconds)?,
      publishing,
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

  #[test]
  fn server_connection_alone_does_not_enable_continuous_profile_publishing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
      &path,
      "[server]\nurl='https://service.example.invalid'\ntoken='fixture-server-token'\n",
    )
    .unwrap();
    let config = GlobalConfig::load(&path).unwrap();
    assert!(config.server.is_some());
    assert!(config.client_config(None).is_none());
    assert_eq!(config.interval(None).unwrap(), Duration::from_secs(10));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
  }

  #[test]
  fn collector_configuration_distinguishes_unconfigured_and_explicitly_disabled_publishing() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::from_root(directory.path().canonicalize().unwrap()).unwrap();
    assert!(matches!(
      CollectionConfig::load(&paths, None, None, false)
        .unwrap()
        .publishing,
      PublishingMode::Unconfigured
    ));
    assert!(matches!(
      CollectionConfig::load(&paths, Some(1), None, true)
        .unwrap()
        .publishing,
      PublishingMode::Disabled
    ));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
  }

  #[test]
  fn unrelated_invalid_server_binding_does_not_prevent_collection() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::from_root(directory.path().canonicalize().unwrap()).unwrap();
    std::fs::write(
      paths.config_file(),
      "[server]\nurl='https://fixture-secret@host'\ntoken='fixture-token'\ntoken_env='FIXTURE_TOKEN'\n[system_profile]\ninterval_seconds=3\n",
    )
    .unwrap();
    assert!(GlobalConfig::load(&paths.config_file()).is_err());
    let collection = CollectionConfig::load(&paths, None, None, false).unwrap();
    assert_eq!(collection.interval, Duration::from_secs(3));
    assert!(matches!(
      collection.publishing,
      PublishingMode::Unconfigured
    ));
  }

  #[test]
  fn malformed_machine_configuration_still_rejects_startup() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::from_root(directory.path().canonicalize().unwrap()).unwrap();
    for text in [
      "[system_profile]\ninterval_seconds=0\n",
      "[system_profile\n",
    ] {
      std::fs::write(paths.config_file(), text).unwrap();
      assert!(CollectionConfig::load(&paths, None, None, false).is_err());
    }
  }
}
