//! Interactive password exchange with private, conditional credential persistence.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use clap::Args;
use expri_client::{login, revoke_login, validate_login_url};
use expri_core::service::ClientConfig;
use expri_core::service_auth::{LoginRequest, validate_cli_token};
use expri_core::storage::StoragePaths;
use serde_json::{Value, json};

use crate::error::{ExpriError, Result};
use crate::global_config::{load, replacement_committed, save_server_if_unchanged};

#[derive(Debug, Args)]
pub struct LoginCommand {
  /// Print the structured result for automation.
  #[arg(long)]
  json: bool,
  /// Server URL; defaults to the URL already saved in global configuration.
  #[arg(long)]
  url: Option<String>,
  /// Read one password line from a pipe or file instead of a hidden prompt.
  #[arg(long)]
  password_stdin: bool,
  /// Override EXPRI_STORAGE_DIR and the default ~/.tokn/expri directory.
  #[arg(long)]
  storage_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct LogoutCommand {
  /// Print the structured result for automation.
  #[arg(long)]
  json: bool,
  /// Override EXPRI_STORAGE_DIR and the default ~/.tokn/expri directory.
  #[arg(long)]
  storage_dir: Option<PathBuf>,
}

pub fn run_login(command: LoginCommand, target: Option<&str>, quiet: bool) -> Result<()> {
  current_machine(target)?;
  let paths = StoragePaths::resolve(command.storage_dir.as_deref())?;
  let snapshot = load(&paths)?;
  let previous = snapshot.server.as_ref();
  let url = command
    .url
    .or_else(|| previous.map(|server| server.url.clone()))
    .ok_or_else(|| message("no server URL configured; run expri login --url URL"))?;
  validate_login_url(&url)?;
  let password = password(command.password_stdin)?;
  let credential = login(&url, &password)?;
  drop(password);
  let same_url = previous.is_some_and(|server| same_url(&server.url, &url));
  let server = ClientConfig {
    url: url.clone(),
    token: Some(credential.token.clone()),
    token_env: None,
    project_id: previous
      .filter(|_| same_url)
      .and_then(|server| server.project_id.clone()),
  };
  if let Err(error) = save_server_if_unchanged(&paths, previous, Some(server)) {
    if replacement_committed(&error) {
      return Err(message(
        "API token was saved, but configuration durability could not be confirmed; inspect expri config server show before retrying",
      ));
    }
    if revoke_login(&url, &credential.token).is_err() {
      eprintln!(
        "warning: the unsaved CLI credential could not be revoked; server cleanup is required"
      );
    }
    return Err(error);
  }
  let mut warnings = Vec::new();
  if same_url
    && let Some(previous) = previous
    && let Ok(Some(token)) = configured_token(previous)
    && token != credential.token
    && validate_cli_token(&token).is_ok()
    && let Err(error) = revoke_login(&url, &token)
    && !already_rejected(&error)
  {
    warnings.push("previous CLI credential could not be revoked; it remains valid until revoked");
  }
  print_auth_report(
    &json!({
      "logged_in": true,
      "url": url,
      "config_file": paths.config_file(),
      "expires_at_ms": credential.expires_at_ms,
      "warnings": warnings,
    }),
    command.json,
    quiet,
  )?;
  Ok(())
}

pub fn run_logout(command: LogoutCommand, target: Option<&str>, quiet: bool) -> Result<()> {
  current_machine(target)?;
  let paths = StoragePaths::resolve(command.storage_dir.as_deref())?;
  let snapshot = load(&paths)?;
  let mut warnings = Vec::new();
  if let Some(server) = &snapshot.server
    && let Some(token) = configured_token(server)?
  {
    if validate_cli_token(&token).is_err() {
      return Err(message(
        "configured credential was not issued by expri login; use expri config server unset to remove it locally",
      ));
    }
    if let Err(error) = revoke_login(&server.url, &token) {
      if !already_rejected(&error) {
        return Err(error);
      }
      warnings.push("saved CLI credential was already rejected or revoked");
    }
    let mut connection = server.clone();
    connection.token = None;
    connection.token_env = None;
    save_server_if_unchanged(&paths, Some(server), Some(connection))?;
  }
  print_auth_report(
    &json!({
      "logged_in": false,
      "url": snapshot.server.as_ref().map(|server| &server.url),
      "config_file": paths.config_file(),
      "warnings": warnings,
    }),
    command.json,
    quiet,
  )?;
  Ok(())
}

fn print_auth_report(report: &Value, json: bool, quiet: bool) -> Result<()> {
  if json {
    return crate::output::print_report(report, true);
  }
  if quiet {
    return Ok(());
  }
  let logged_in = report["logged_in"] == true;
  let mut fields = vec![
    vec![
      "Status".into(),
      if logged_in { "Logged in" } else { "Logged out" }.into(),
    ],
    vec![
      "Server".into(),
      report["url"].as_str().unwrap_or("-").into(),
    ],
    vec![
      "Configuration".into(),
      report["config_file"].as_str().unwrap_or("-").into(),
    ],
  ];
  if logged_in {
    fields.push(vec![
      "Validity".into(),
      report["expires_at_ms"]
        .as_i64()
        .map(|expiry| expiry.to_string())
        .unwrap_or_else(|| "Until revoked".into()),
    ]);
  }
  print!(
    "{}",
    crate::output::table(&["FIELD", "VALUE"], &fields, &[16, 80])
  );
  if let Some(warnings) = report["warnings"].as_array() {
    for warning in warnings.iter().filter_map(Value::as_str) {
      eprintln!("warning: {}", crate::output::text(warning));
    }
  }
  Ok(())
}

fn configured_token(server: &ClientConfig) -> Result<Option<String>> {
  if let Some(token) = &server.token {
    Ok(Some(token.clone()))
  } else if let Some(name) = &server.token_env {
    std::env::var(name)
      .map(Some)
      .map_err(|_| message("configured token environment variable is unavailable"))
  } else {
    Ok(None)
  }
}

fn password(from_stdin: bool) -> Result<String> {
  let input = std::io::stdin();
  if from_stdin {
    if input.is_terminal() {
      return Err(message(
        "--password-stdin requires a pipe or redirected password file",
      ));
    }
    return read_password(input.lock());
  }
  if !input.is_terminal() {
    return Err(message(
      "login requires a terminal for its hidden password prompt; use --password-stdin with a pipe or file",
    ));
  }
  let password = rpassword::prompt_password("Dashboard password: ")
    .map_err(|_| message("could not read dashboard password from the terminal"))?;
  validated_password(password)
}

fn read_password(reader: impl Read) -> Result<String> {
  let mut bytes = Vec::new();
  reader.take(259).read_to_end(&mut bytes)?;
  if bytes.ends_with(b"\n") {
    bytes.pop();
    if bytes.ends_with(b"\r") {
      bytes.pop();
    }
  }
  let password = String::from_utf8(bytes)
    .map_err(|_| message("login password must contain 16 to 256 printable ASCII characters"))?;
  validated_password(password)
}

fn validated_password(password: String) -> Result<String> {
  let request = LoginRequest { password };
  request.validate()?;
  Ok(request.password)
}

fn same_url(left: &str, right: &str) -> bool {
  match (url::Url::parse(left), url::Url::parse(right)) {
    (Ok(left), Ok(right)) => {
      left.as_str().trim_end_matches('/') == right.as_str().trim_end_matches('/')
    }
    _ => false,
  }
}

fn already_rejected(error: &ExpriError) -> bool {
  matches!(error, ExpriError::ServiceRejected { status: 401, .. })
}

fn current_machine(target: Option<&str>) -> Result<()> {
  if target.is_some() {
    return Err(message(
      "login and logout run on the current machine; omit --target",
    ));
  }
  Ok(())
}

fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{Cli, Command};
  use clap::Parser;

  #[test]
  fn password_line_preserves_spaces_and_rejects_extra_lines_without_echoing() {
    assert_eq!(
      read_password(&b"  fixture password 123  \r\n"[..]).unwrap(),
      "  fixture password 123  "
    );
    assert_eq!(read_password(&vec![b'x'; 256][..]).unwrap().len(), 256);
    for bytes in [
      vec![b'x'; 257],
      b"must-not-leak\nsecond-line".to_vec(),
      vec![0xff],
      Vec::new(),
    ] {
      let error = read_password(bytes.as_slice()).unwrap_err();
      assert!(!error.to_string().contains("must-not-leak"));
    }
  }

  #[test]
  fn cli_accepts_safe_inputs_without_password_arguments_and_rejects_remote_targets_first() {
    for args in [
      vec!["expri", "login"],
      vec![
        "expri",
        "login",
        "--url",
        "https://example.net",
        "--password-stdin",
      ],
      vec!["expri", "logout", "--storage-dir", "/missing/global"],
    ] {
      Cli::try_parse_from(args).unwrap();
    }
    assert!(Cli::try_parse_from(["expri", "login", "--password", "must-not-leak"]).is_err());
    for operation in ["login", "logout"] {
      let cli = Cli::try_parse_from([
        "expri",
        "-T",
        "remote",
        operation,
        "--storage-dir",
        "/missing/global",
      ])
      .unwrap();
      let result = match cli.command {
        Command::Login(command) => run_login(command, cli.target.as_deref(), false),
        Command::Logout(command) => run_logout(command, cli.target.as_deref(), false),
        _ => unreachable!(),
      };
      assert!(result.unwrap_err().to_string().contains("current machine"));
    }
  }

  #[test]
  fn credential_cleanup_only_contacts_the_same_normalized_server() {
    assert!(same_url(
      "https://EXAMPLE.net:443/api/",
      "https://example.net/api"
    ));
    assert!(!same_url(
      "https://example.net/api",
      "https://example.net/other"
    ));
    assert!(!same_url("http://example.net", "https://example.net"));
    assert!(!same_url(
      "https://old.example.net",
      "https://new.example.net"
    ));
  }
}
