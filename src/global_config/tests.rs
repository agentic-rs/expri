use super::*;

fn paths(directory: &tempfile::TempDir) -> StoragePaths {
  StoragePaths::from_root(directory.path().canonicalize().unwrap().join("global")).unwrap()
}

fn server(token: &str) -> ClientConfig {
  ClientConfig {
    url: "https://expri.example.net".into(),
    token: Some(token.into()),
    token_env: None,
    project_id: Some("vision".into()),
  }
}

#[test]
fn inspection_and_removing_an_absent_connection_create_no_storage() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  assert_eq!(config_path(&paths).unwrap(), paths.config_file());
  assert!(load(&paths).unwrap().server.is_none());
  assert!(save_server(&paths, None).unwrap().server.is_none());
  assert!(!paths.root().exists());
}

#[cfg(unix)]
#[test]
fn readonly_inspection_preserves_modes_bytes_and_directory_entries() {
  use std::os::unix::fs::PermissionsExt;

  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  paths.ensure_private().unwrap();
  let bytes = b"[system_profile]\ninterval_seconds=30\n";
  fs::write(paths.config_file(), bytes).unwrap();
  fs::set_permissions(paths.config_file(), fs::Permissions::from_mode(0o400)).unwrap();
  fs::set_permissions(paths.root(), fs::Permissions::from_mode(0o500)).unwrap();
  assert_eq!(load(&paths).unwrap().system_profile.interval_seconds, 30);
  assert_eq!(fs::read(paths.config_file()).unwrap(), bytes);
  assert_eq!(
    fs::metadata(paths.root()).unwrap().permissions().mode() & 0o777,
    0o500
  );
  assert_eq!(
    fs::metadata(paths.config_file())
      .unwrap()
      .permissions()
      .mode()
      & 0o777,
    0o400
  );
  assert_eq!(fs::read_dir(paths.root()).unwrap().count(), 1);
  fs::set_permissions(paths.root(), fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn conditional_login_save_preserves_system_updates_made_while_authenticating() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  save_server(&paths, Some(server("old-private-token"))).unwrap();
  let mut updated = load(&paths).unwrap();
  let expected = updated.server.clone();
  updated.system_profile.interval_seconds = 30;
  updated.system_profile.publish = Some(expri_core::global_config::PublishingConfig {
    client_config: "/etc/expri/machine.toml".into(),
  });
  fs::write(paths.config_file(), toml::to_string(&updated).unwrap()).unwrap();
  let saved =
    save_server_if_unchanged(&paths, expected.as_ref(), Some(server("new-private-token"))).unwrap();
  assert_eq!(saved.system_profile.interval_seconds, 30);
  assert_eq!(
    saved.system_profile.publish.unwrap().client_config,
    PathBuf::from("/etc/expri/machine.toml")
  );
  assert!(same_server(
    load(&paths).unwrap().server.as_ref(),
    Some(&server("new-private-token"))
  ));
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    for (path, mode) in [
      (paths.root().to_path_buf(), 0o700),
      (paths.config_file(), 0o600),
      (paths.root().join("config.lock"), 0o600),
    ] {
      assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        mode
      );
    }
  }
}

#[test]
fn conditional_save_rejects_every_changed_connection_field_without_revealing_secrets() {
  for field in ["url", "token", "token_env", "project_id", "removed"] {
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(&directory);
    let expected = server("old-private-token");
    save_server(&paths, Some(expected.clone())).unwrap();
    let mut changed = expected.clone();
    match field {
      "url" => changed.url = "https://another.example.net".into(),
      "token" => changed.token = Some("other-private-token".into()),
      "token_env" => {
        changed.token = None;
        changed.token_env = Some("PRIVATE_SERVER_TOKEN_ENV".into());
      }
      "project_id" => changed.project_id = Some("another-project".into()),
      "removed" => {}
      _ => unreachable!(),
    }
    save_server(&paths, (field != "removed").then_some(changed)).unwrap();
    let before = fs::read(paths.config_file()).unwrap();
    let error = save_server_if_unchanged(
      &paths,
      Some(&expected),
      Some(server("issued-private-token")),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), CONNECTION_CHANGED);
    for secret in [
      "old-private-token",
      "other-private-token",
      "issued-private-token",
      "PRIVATE_SERVER_TOKEN_ENV",
      "another-project",
      "another.example.net",
    ] {
      assert!(!error.to_string().contains(secret));
    }
    assert_eq!(fs::read(paths.config_file()).unwrap(), before);
  }
}

#[test]
fn first_login_cannot_overwrite_a_concurrently_configured_connection() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  let initial = load(&paths).unwrap();
  save_server(&paths, Some(server("concurrent-private-token"))).unwrap();
  let before = fs::read(paths.config_file()).unwrap();
  let error = save_server_if_unchanged(
    &paths,
    initial.server.as_ref(),
    Some(server("issued-private-token")),
  )
  .unwrap_err();
  assert_eq!(error.to_string(), CONNECTION_CHANGED);
  assert_eq!(fs::read(paths.config_file()).unwrap(), before);
  let current = load(&paths).unwrap().server;
  assert!(
    save_server_if_unchanged(&paths, current.as_ref(), None)
      .unwrap()
      .server
      .is_none()
  );
}

#[test]
fn conditional_logout_preserves_url_project_and_system_settings_without_credentials() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  save_server(&paths, Some(server("issued-private-token"))).unwrap();
  let mut existing = load(&paths).unwrap();
  existing.system_profile.interval_seconds = 30;
  fs::write(paths.config_file(), toml::to_string(&existing).unwrap()).unwrap();
  let expected = existing.server.as_ref().unwrap();
  let mut disconnected = expected.clone();
  disconnected.token = None;
  disconnected.token_env = None;
  let saved = save_server_if_unchanged(&paths, Some(expected), Some(disconnected)).unwrap();
  let current = saved.server.as_ref().unwrap();
  assert_eq!(current.url, expected.url);
  assert_eq!(current.project_id, expected.project_id);
  assert!(current.token.is_none() && current.token_env.is_none());
  assert_eq!(saved.system_profile.interval_seconds, 30);
  let reopened = load(&paths).unwrap();
  assert!(same_server(reopened.server.as_ref(), saved.server.as_ref()));
  assert_eq!(reopened.system_profile.interval_seconds, 30);
}

#[test]
fn bounded_encoding_failure_keeps_the_previous_config_and_leaves_no_temporary_file() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  save_server(&paths, Some(server("previous-private-token"))).unwrap();
  let before = fs::read(paths.config_file()).unwrap();
  let error =
    save_server(&paths, Some(server(&"x".repeat(MAX_CONFIG_BYTES as usize)))).unwrap_err();
  assert_eq!(error.to_string(), "global configuration exceeds 32 KiB");
  assert!(!replacement_committed(&error));
  assert_eq!(fs::read(paths.config_file()).unwrap(), before);
  assert_eq!(fs::read_dir(paths.root()).unwrap().count(), 2);
}

#[test]
fn directory_sync_failure_reports_that_the_private_replacement_is_already_committed() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  save_server(&paths, Some(server("previous-private-token"))).unwrap();
  let mut config = load(&paths).unwrap();
  config.server = Some(server("issued-private-token"));
  let encoded = toml::to_string_pretty(&config).unwrap();
  let mut temporary = tempfile::Builder::new()
    .prefix(".config-")
    .tempfile_in(paths.root())
    .unwrap();
  temporary.write_all(encoded.as_bytes()).unwrap();
  temporary.as_file().sync_all().unwrap();
  let error = persist_config_with(paths.root(), &paths.config_file(), temporary, |_| {
    Err(std::io::Error::from(std::io::ErrorKind::Other))
  })
  .unwrap_err();
  assert!(replacement_committed(&error));
  assert!(!error.to_string().contains("issued-private-token"));
  assert_eq!(fs::read(paths.config_file()).unwrap(), encoded.as_bytes());
  assert!(same_server(
    load(&paths).unwrap().server.as_ref(),
    config.server.as_ref()
  ));
  assert_eq!(fs::read_dir(paths.root()).unwrap().count(), 2);
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
      fs::metadata(paths.config_file())
        .unwrap()
        .permissions()
        .mode()
        & 0o777,
      0o600
    );
  }
}

#[test]
fn busy_configuration_lock_prevents_lost_updates_and_releases_for_retry() {
  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  save_server(&paths, Some(server("previous-private-token"))).unwrap();
  let before = fs::read(paths.config_file()).unwrap();
  let LockAttempt::Acquired(lease) =
    lock::try_lock_file(&paths.root().join("config.lock"), false).unwrap()
  else {
    panic!("new lock")
  };
  let error = save_server(&paths, Some(server("replacement-private-token"))).unwrap_err();
  assert_eq!(
    error.to_string(),
    "global configuration is being updated; retry"
  );
  assert_eq!(fs::read(paths.config_file()).unwrap(), before);
  drop(lease);
  save_server(&paths, Some(server("replacement-private-token"))).unwrap();
}

#[cfg(unix)]
#[test]
fn symlink_and_hardlink_locks_do_not_modify_external_files() {
  use std::os::unix::fs::{PermissionsExt, symlink};

  for alias in ["symlink", "hardlink"] {
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(&directory);
    paths.ensure_private().unwrap();
    let external = directory.path().join("external");
    fs::write(&external, b"external-file-contents").unwrap();
    fs::set_permissions(&external, fs::Permissions::from_mode(0o644)).unwrap();
    let lock_path = paths.root().join("config.lock");
    if alias == "symlink" {
      symlink(&external, &lock_path).unwrap();
    } else {
      fs::hard_link(&external, &lock_path).unwrap();
    }
    assert!(save_server(&paths, Some(server("private-token"))).is_err());
    assert_eq!(fs::read(&external).unwrap(), b"external-file-contents");
    assert_eq!(
      fs::metadata(&external).unwrap().permissions().mode() & 0o777,
      0o644
    );
    assert!(load(&paths).unwrap().server.is_none());
    assert!(!paths.config_file().exists());
  }
}

#[cfg(unix)]
#[test]
fn destination_recheck_rejects_a_late_symlink_and_removes_private_temporary_bytes() {
  use std::os::unix::fs::symlink;

  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  paths.ensure_private().unwrap();
  let external = directory.path().join("external");
  fs::write(&external, b"external-file-contents").unwrap();
  let mut temporary = tempfile::Builder::new()
    .prefix(".config-")
    .tempfile_in(paths.root())
    .unwrap();
  temporary.write_all(b"private-issued-token").unwrap();
  let temporary_path = temporary.path().to_path_buf();
  symlink(&external, paths.config_file()).unwrap();
  let error = persist_config(paths.root(), &paths.config_file(), temporary).unwrap_err();
  assert!(!error.to_string().contains("private-issued-token"));
  assert!(!temporary_path.exists());
  assert_eq!(fs::read(external).unwrap(), b"external-file-contents");
  assert!(
    fs::symlink_metadata(paths.config_file())
      .unwrap()
      .file_type()
      .is_symlink()
  );
}

#[cfg(unix)]
#[test]
fn atomic_replacement_of_a_hardlinked_config_preserves_the_external_inode() {
  use std::os::unix::fs::PermissionsExt;

  let directory = tempfile::tempdir().unwrap();
  let paths = paths(&directory);
  paths.ensure_private().unwrap();
  let external = directory.path().join("external.toml");
  let previous = b"[system_profile]\ninterval_seconds=30\n";
  fs::write(&external, previous).unwrap();
  fs::set_permissions(&external, fs::Permissions::from_mode(0o644)).unwrap();
  fs::hard_link(&external, paths.config_file()).unwrap();
  save_server(&paths, Some(server("private-issued-token"))).unwrap();
  assert_eq!(fs::read(external.clone()).unwrap(), previous);
  assert_eq!(
    fs::metadata(external).unwrap().permissions().mode() & 0o777,
    0o644
  );
  assert_eq!(load(&paths).unwrap().system_profile.interval_seconds, 30);
  assert!(load(&paths).unwrap().server.is_some());
}
