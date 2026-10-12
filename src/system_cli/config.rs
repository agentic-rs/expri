pub(super) use expri_core::global_config::GlobalConfig;

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

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
}
