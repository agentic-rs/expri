//! Read-only selection of global or explicitly supplied service client settings.

use std::path::{Path, PathBuf};

use expri_core::global_config::{GlobalConfig, read_config_text};
use expri_core::service::ClientConfig;
use expri_core::storage::StoragePaths;

use crate::error::{ExpriError, Result};

const NO_SERVER: &str = "no server configured; run expri login --url URL, or pass --config PATH";
const NO_CREDENTIAL: &str = "no API credential configured; run expri login";

/// Load either a global `[server]` table or a legacy flat client file.
/// Environment credentials are validated by name but are not resolved here.
pub fn load_client_config(path: &Path) -> Result<ClientConfig> {
  let text = read_config_text(path)?
    .ok_or_else(|| message("service client configuration file is missing"))?;
  let table: toml::Table =
    toml::from_str(&text).map_err(|_| message("service client configuration is invalid"))?;
  let config = if table.is_empty()
    || table.contains_key("server")
    || table.contains_key("system_profile")
    || table.contains_key("queue_worker")
  {
    let global: GlobalConfig =
      toml::from_str(&text).map_err(|_| message("global expri configuration is invalid"))?;
    global.validate()?;
    let server = global.server.ok_or_else(|| message(NO_SERVER))?;
    if server.token.is_none() && server.token_env.is_none() {
      return Err(message(NO_CREDENTIAL));
    }
    server
  } else {
    toml::from_str(&text).map_err(|_| message("service client configuration is invalid"))?
  };
  config.validate()?;
  Ok(config)
}

/// Select a client file without creating storage or changing permissions.
/// An explicit file bypasses HOME, EXPRI_STORAGE_DIR, and global configuration.
pub fn resolve_client_config(explicit: Option<&Path>) -> Result<PathBuf> {
  if let Some(path) = explicit {
    return Ok(path.to_path_buf());
  }
  resolve_default_client_config(&StoragePaths::resolve(None)?)
}

fn resolve_default_client_config(paths: &StoragePaths) -> Result<PathBuf> {
  paths.validate_existing()?;
  let path = match std::fs::canonicalize(paths.root()) {
    Ok(root) => root.join("config.toml"),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => paths.config_file(),
    Err(error) => return Err(error.into()),
  };
  let config = GlobalConfig::load(&path)?;
  paths.validate_existing()?;
  let server = config.server.ok_or_else(|| message(NO_SERVER))?;
  if server.token.is_none() && server.token_env.is_none() {
    return Err(message(NO_CREDENTIAL));
  }
  Ok(path)
}

fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;

  fn directory() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
  }

  #[test]
  fn both_global_and_legacy_files_load_without_resolving_environment_credentials() {
    let directory = directory();
    let path = directory.path().join("client.toml");
    for contents in [
      "url='https://example.net'\ntoken_env='EXPRI_TEST_UNSET_TOKEN'\nproject_id='vision'\n",
      "[server]\nurl='https://example.net'\ntoken_env='EXPRI_TEST_UNSET_TOKEN'\nproject_id='vision'\n[system_profile]\ninterval_seconds=30\n",
    ] {
      fs::write(&path, contents).unwrap();
      let config = load_client_config(&path).unwrap();
      assert_eq!(config.url, "https://example.net");
      assert_eq!(config.project_id.as_deref(), Some("vision"));
      assert_eq!(config.token_env.as_deref(), Some("EXPRI_TEST_UNSET_TOKEN"));
      assert!(config.token.is_none());
    }
    fs::write(
      &path,
      "[server]\nurl='https://example.net'\ntoken='stored-secret'\n",
    )
    .unwrap();
    let config = load_client_config(&path).unwrap();
    assert_eq!(config.token.as_deref(), Some("stored-secret"));
    assert!(!format!("{config:?}").contains("stored-secret"));
  }

  #[test]
  fn missing_server_and_invalid_file_errors_are_clear_and_secret_safe() {
    let directory = directory();
    let path = directory.path().join("config.toml");
    for contents in ["", "[system_profile]\ninterval_seconds=10\n"] {
      fs::write(&path, contents).unwrap();
      let error = load_client_config(&path).unwrap_err().to_string();
      assert_eq!(error, NO_SERVER);
    }
    for contents in [
      "url='https://example.net'\ntoken='must-not-leak'\ntoken_env='EXPRI_MUST_NOT_LEAK'\n",
      "[server]\nurl='https://must-not-leak@host'\ntoken='must-not-leak'\n",
      "[server]\nurl='https://host'\ntoken=must-not-leak\n",
      "url='https://host'\ntoken='must-not-leak'\nunknown='must-not-leak'\n",
    ] {
      fs::write(&path, contents).unwrap();
      let error = load_client_config(&path).unwrap_err();
      assert!(!format!("{error:?} {error}").contains("must-not-leak"));
      assert!(!error.to_string().contains("EXPRI_MUST_NOT_LEAK"));
    }
    fs::remove_file(&path).unwrap();
    assert!(
      load_client_config(&path)
        .unwrap_err()
        .to_string()
        .contains("file is missing")
    );
  }

  #[test]
  fn global_url_only_configuration_requires_login_without_changing_the_file() {
    let directory = directory();
    let root = directory.path().canonicalize().unwrap();
    let config = root.join("config.toml");
    let contents = "[server]\nurl='https://example.net'\n[system_profile]\ninterval_seconds=20\n";
    fs::write(&config, contents).unwrap();
    assert_eq!(
      load_client_config(&config).unwrap_err().to_string(),
      NO_CREDENTIAL
    );
    let paths = StoragePaths::from_root(root.clone()).unwrap();
    assert_eq!(
      resolve_default_client_config(&paths)
        .unwrap_err()
        .to_string(),
      NO_CREDENTIAL
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), contents);
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    fs::write(&config, "url='https://example.net'\n").unwrap();
    assert!(
      load_client_config(&config)
        .unwrap_err()
        .to_string()
        .contains("exactly one")
    );
  }

  #[test]
  fn explicit_selection_bypasses_global_resolution_and_never_loads_the_file() {
    let directory = directory();
    let path = directory.path().join("absent/client.toml");
    assert_eq!(resolve_client_config(Some(&path)).unwrap(), path);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    // Even empty and relative explicit paths are selected without consulting globals.
    assert_eq!(
      resolve_client_config(Some(Path::new(""))).unwrap(),
      PathBuf::new()
    );
    assert_eq!(
      resolve_client_config(Some(Path::new("client.toml"))).unwrap(),
      PathBuf::from("client.toml")
    );
  }

  #[test]
  fn default_selection_requires_nested_server_without_creating_storage() {
    let directory = directory();
    let root = directory.path().join("absent");
    let paths = StoragePaths::from_root(root.clone()).unwrap();
    assert_eq!(
      resolve_default_client_config(&paths)
        .unwrap_err()
        .to_string(),
      NO_SERVER
    );
    assert!(!root.exists());
    fs::create_dir(&root).unwrap();
    let config = root.join("config.toml");
    fs::write(
      &config,
      "url='https://example.net'\ntoken='stored-secret'\n",
    )
    .unwrap();
    assert!(resolve_default_client_config(&paths).is_err());
    fs::write(&config, "[server]\nurl='https://example.net'\ntoken='stored-secret'\n[system_profile]\ninterval_seconds=20\n").unwrap();
    assert_eq!(
      resolve_default_client_config(&paths).unwrap(),
      fs::canonicalize(&config).unwrap()
    );
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
  }

  #[test]
  fn storage_override_has_precedence_over_home_configuration() {
    let directory = directory();
    let home = directory.path().join("home");
    let override_root = directory.path().join("override");
    fs::create_dir_all(home.join(".tokn/expri")).unwrap();
    fs::create_dir(&override_root).unwrap();
    fs::write(
      home.join(".tokn/expri/config.toml"),
      "invalid home configuration",
    )
    .unwrap();
    fs::write(
      override_root.join("config.toml"),
      "[server]\nurl='https://override.example.net'\ntoken='stored-secret'\n",
    )
    .unwrap();
    let paths =
      StoragePaths::resolve_with(None, Some(&override_root), Some(&home), directory.path())
        .unwrap();
    let selected = resolve_default_client_config(&paths).unwrap();
    assert_eq!(
      selected,
      fs::canonicalize(override_root.join("config.toml")).unwrap()
    );
    assert_eq!(
      load_client_config(&selected).unwrap().url,
      "https://override.example.net"
    );
    let relative =
      StoragePaths::resolve_with(None, Some(Path::new("override")), None, directory.path())
        .unwrap();
    assert_eq!(resolve_default_client_config(&relative).unwrap(), selected);
  }

  #[cfg(unix)]
  #[test]
  fn trusted_home_alias_is_supported_but_owned_boundary_and_file_links_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = directory();
    let home = directory.path().join("real-home");
    let linked_home = directory.path().join("linked-home");
    fs::create_dir_all(home.join(".tokn/expri")).unwrap();
    symlink(&home, &linked_home).unwrap();
    let config = home.join(".tokn/expri/config.toml");
    fs::write(
      &config,
      "[server]\nurl='https://example.net'\ntoken='stored-secret'\n",
    )
    .unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o400)).unwrap();
    let paths =
      StoragePaths::resolve_with(None, None, Some(&linked_home), directory.path()).unwrap();
    assert_eq!(
      resolve_default_client_config(&paths).unwrap(),
      fs::canonicalize(&config).unwrap()
    );
    assert_eq!(
      fs::metadata(&config).unwrap().permissions().mode() & 0o777,
      0o400
    );
    fs::rename(&config, config.with_extension("real")).unwrap();
    symlink(config.with_extension("real"), &config).unwrap();
    assert!(resolve_default_client_config(&paths).is_err());
    fs::remove_file(&config).unwrap();
    fs::rename(home.join(".tokn"), home.join("real-tokn")).unwrap();
    symlink(home.join("real-tokn"), home.join(".tokn")).unwrap();
    assert!(resolve_default_client_config(&paths).is_err());
  }
}
