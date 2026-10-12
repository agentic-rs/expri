use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use clap::{Args, Subcommand};
use expri_core::global_config::GlobalConfig;
use expri_core::service::ClientConfig;
use expri_core::storage::StoragePaths;
use serde_json::{Value, json};

use crate::error::{ExpriError, Result};
use crate::global_config::{load, save_server};

const TOKEN_LIMIT: usize = 4096;

#[derive(Debug, Args)]
pub struct ConfigCommand {
  /// Print the structured result for automation.
  #[arg(long, global = true)]
  json: bool,
  /// Override EXPRI_STORAGE_DIR and the default ~/.tokn/expri directory.
  #[arg(long, global = true)]
  storage_dir: Option<PathBuf>,
  #[command(subcommand)]
  command: ConfigSubcommand,
}

#[derive(Debug, Subcommand)]
enum ConfigSubcommand {
  /// Configure the default server connection for this machine.
  Server {
    #[command(subcommand)]
    command: ServerCommand,
  },
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
  /// Save a server URL and token in the private global configuration.
  Set {
    #[arg(long)]
    url: String,
    /// Read the token from stdin; its value is never printed.
    #[arg(long, required = true)]
    token_stdin: bool,
  },
  /// Show the configured URL and credential source, keeping the token hidden.
  Show,
  /// Remove the default server connection, preserving other global settings.
  Unset,
}

pub fn run(command: ConfigCommand, target: Option<&str>, quiet: bool) -> Result<()> {
  if target.is_some() {
    return Err(message(
      "global configuration belongs to the current machine; omit -T",
    ));
  }
  let paths = StoragePaths::resolve(command.storage_dir.as_deref())?;
  paths.validate_existing()?;
  let json = command.json;
  let ConfigSubcommand::Server { command } = command.command;
  let report = match command {
    ServerCommand::Show => {
      let config = load(&paths)?;
      report(&paths, &config)
    }
    ServerCommand::Set { url, token_stdin } => {
      debug_assert!(token_stdin);
      let input = std::io::stdin();
      if input.is_terminal() {
        return Err(message(
          "--token-stdin requires a pipe or redirected token file",
        ));
      }
      let token = read_token(input.lock())?;
      let server = ClientConfig {
        url,
        token: Some(token),
        token_env: None,
        project_id: None,
      };
      server.validate()?;
      let config = save_server(&paths, Some(server))?;
      report(&paths, &config)
    }
    ServerCommand::Unset => {
      let config = save_server(&paths, None)?;
      report(&paths, &config)
    }
  };
  if json || !quiet {
    crate::output::print_report(&report, json)?;
  }
  Ok(())
}

fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}

fn read_token(reader: impl Read) -> Result<String> {
  let mut bytes = Vec::new();
  reader
    .take((TOKEN_LIMIT + 3) as u64)
    .read_to_end(&mut bytes)?;
  if bytes.ends_with(b"\n") {
    bytes.pop();
    if bytes.ends_with(b"\r") {
      bytes.pop();
    }
  }
  if bytes.is_empty() || bytes.len() > TOKEN_LIMIT || !bytes.iter().all(u8::is_ascii_graphic) {
    return Err(message(
      "server token must be one nonempty ASCII line without spaces, at most 4096 bytes",
    ));
  }
  String::from_utf8(bytes).map_err(|_| message("server token must be UTF-8"))
}

fn report(paths: &StoragePaths, config: &GlobalConfig) -> Value {
  json!({
    "config_file": paths.config_file(),
    "configured": config.server.is_some(),
    "server": config.server.as_ref().map(|server| json!({
      "url": server.url,
      "token_configured": server.token.is_some() || server.token_env.is_some(),
      "token_source": if server.token.is_some() {
        Some("config")
      } else if server.token_env.is_some() {
        Some("environment")
      } else {
        None
      },
      "token_env": server.token_env,
    })),
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn token_input_is_bounded_and_diagnostics_do_not_echo_it() {
    assert_eq!(read_token(&b"secret\r\n"[..]).unwrap(), "secret");
    for bytes in [
      Vec::new(),
      b"secret\nsecond-line".to_vec(),
      b"secret with spaces".to_vec(),
      vec![b'x'; TOKEN_LIMIT + 1],
      vec![0xff],
    ] {
      assert!(
        !read_token(bytes.as_slice())
          .unwrap_err()
          .to_string()
          .contains("secret")
      );
    }
  }

  #[test]
  fn updates_preserve_system_settings_and_hide_tokens_in_reports() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("global");
    let paths = StoragePaths::from_root(root.clone()).unwrap();
    paths.ensure_private().unwrap();
    std::fs::write(paths.config_file(), "[system_profile]\ninterval_seconds=30\n[system_profile.publish]\nclient_config='/etc/expri/machine.toml'\n").unwrap();
    let config = save_server(
      &paths,
      Some(ClientConfig {
        url: "https://expri.example.net".into(),
        token: Some("must-not-appear".into()),
        token_env: None,
        project_id: None,
      }),
    )
    .unwrap();
    assert_eq!(config.system_profile.interval_seconds, 30);
    assert_eq!(
      config
        .system_profile
        .publish
        .as_ref()
        .unwrap()
        .client_config,
      PathBuf::from("/etc/expri/machine.toml")
    );
    let output = report(&paths, &config).to_string();
    assert!(!output.contains("must-not-appear"));
    assert_eq!(
      GlobalConfig::load(&paths.config_file())
        .unwrap()
        .server
        .unwrap()
        .token
        .as_deref(),
      Some("must-not-appear")
    );
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
      );
      assert_eq!(
        std::fs::metadata(paths.config_file())
          .unwrap()
          .permissions()
          .mode()
          & 0o777,
        0o600
      );
    }
    let config = save_server(&paths, None).unwrap();
    assert!(config.server.is_none());
    assert_eq!(config.system_profile.interval_seconds, 30);
  }

  #[test]
  fn removing_an_absent_connection_does_not_create_storage() {
    let directory = tempfile::tempdir().unwrap();
    let paths =
      StoragePaths::from_root(directory.path().canonicalize().unwrap().join("absent")).unwrap();
    assert!(save_server(&paths, None).unwrap().server.is_none());
    assert!(!paths.root().exists());
  }

  #[test]
  fn url_only_connection_reports_no_credential_source() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::from_root(directory.path().canonicalize().unwrap()).unwrap();
    let config = GlobalConfig {
      server: Some(ClientConfig {
        url: "https://expri.example.net".into(),
        token: None,
        token_env: None,
        project_id: Some("vision".into()),
      }),
      ..GlobalConfig::default()
    };
    let output = report(&paths, &config);
    assert_eq!(output["configured"], true);
    assert_eq!(output["server"]["token_configured"], false);
    assert!(output["server"]["token_source"].is_null());
    assert!(output["server"]["token_env"].is_null());
  }
}
