//! Private repository context stays outside committed asset descriptors.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Source, assets, checkout_lease, git_ignored, message};
use crate::error::Result;

pub(super) fn client_config(root: &Path, explicit: Option<&Path>) -> Result<Option<PathBuf>> {
  let local = root.join(".expri/config.toml");
  assets::optional_regular(&local)?;
  let selected = if let Some(path) = explicit {
    Some(std::path::absolute(path)?)
  } else if local.exists() {
    Some(local.clone())
  } else {
    let config = root.join("expri.toml");
    if config.exists() {
      crate::config::Config::load(&config)?
        .local_service()?
        .map(|service| service.client_config)
    } else {
      None
    }
  };
  let selected_is_local = selected
    .as_ref()
    .and_then(|path| fs::canonicalize(path).ok())
    .zip(fs::canonicalize(&local).ok())
    .is_some_and(|(selected, local)| selected == local);
  if selected_is_local {
    let tracked = Command::new("git")
      .current_dir(root)
      .args([
        "--literal-pathspecs",
        "ls-files",
        "--error-unmatch",
        "--",
        ".expri/config.toml",
      ])
      .output()?;
    if tracked.status.success() {
      return Err(message(
        "repository service settings must be Git-ignored; remove .expri/config.toml from the Git index",
      ));
    }
    ignore_private_settings(root)?;
  }
  Ok(selected)
}

fn ignore_private_settings(root: &Path) -> Result<()> {
  let _lease = checkout_lease(root)?;
  if git_ignored(root, Path::new(".expri/config.toml"))? {
    return Ok(());
  }
  let path = root.join(".gitignore");
  assets::optional_regular(&path)?;
  let mut contents = if path.exists() {
    fs::read_to_string(&path)?
  } else {
    String::new()
  };
  if contents.lines().last() == Some("/.expri/") {
    return Ok(());
  }
  if !contents.is_empty() && !contents.ends_with('\n') {
    contents.push('\n');
  }
  contents.push_str("/.expri/\n");
  let mut temporary = tempfile::NamedTempFile::new_in(root)?;
  std::io::Write::write_all(&mut temporary, contents.as_bytes())?;
  temporary.as_file().sync_all()?;
  assets::optional_regular(&path)?;
  temporary.persist(&path).map_err(|error| error.error)?;
  assets::sync_directory(root)
}

pub(super) fn project_context(config: Option<&Path>) -> Result<(&Path, String)> {
  let path = config.ok_or_else(|| {
    message("private assets require .expri/config.toml or --client-config with a project_id")
  })?;
  let canonical = fs::canonicalize(path)?;
  let config = expri_client::load_client_config(&canonical)?;
  let endpoint = url::Url::parse(&config.url).map_err(|_| message("invalid client service URL"))?;
  // Asset transfer namespaces identify the service by its origin, so a path
  // prefix is not interchangeable with the root endpoint.
  if endpoint.path() != "/" {
    return Err(message(
      "client service URL must be an HTTP(S) root without credentials or query parameters",
    ));
  }
  let project_id = config.project_id.ok_or_else(|| {
    message("private assets require project_id in the selected client configuration")
  })?;
  expri_core::service::validate_component(&project_id)?;
  Ok((path, project_id))
}

pub(super) fn parse_source(value: &str) -> Result<Source> {
  let source = if let Some(value) = value.strip_prefix("hf://") {
    let (repo_type, value) = if let Some(value) = value.strip_prefix("datasets/") {
      ("dataset", value)
    } else {
      ("model", value.strip_prefix("models/").unwrap_or(value))
    };
    let mut parts = value.splitn(3, '/');
    let namespace = parts.next().unwrap_or("");
    let repository = parts.next().unwrap_or("");
    let filename = parts.next().unwrap_or("");
    let (repository, revision) = repository.split_once('@').unwrap_or((repository, "main"));
    Source::HuggingFace {
      repo_id: format!("{namespace}/{repository}"),
      repo_type: repo_type.into(),
      filename: filename.into(),
      revision: revision.into(),
      requested_revision: Some(revision.into()),
    }
  } else if value.starts_with("expri://") {
    let parts = relative_parts(value)?;
    if parts.len() != 2 || parts[0] != "inputs" {
      return Err(message(
        "private input reference must be expri://./inputs/input-id; use assets import for a run output",
      ));
    }
    Source::Expri {
      input_id: parts[1].clone(),
    }
  } else {
    Source::Url { url: value.into() }
  };
  source.validate()?;
  Ok(source)
}

pub(super) fn parse_output(value: &str) -> Result<assets::Producer> {
  let parts = relative_parts(value)?;
  if parts.len() < 4 || parts[2] != "outputs" {
    return Err(message(
      "run output reference must be expri://./origin/run-id/outputs/file",
    ));
  }
  expri_core::service::validate_component(&parts[0])?;
  expri_core::service::validate_component(&parts[1])?;
  let path = parts[2..].join("/");
  expri_runs::run_artifacts::validate_path(&path)?;
  Ok(assets::Producer {
    origin: parts[0].clone(),
    run_id: parts[1].clone(),
    path,
  })
}

fn relative_parts(value: &str) -> Result<Vec<String>> {
  let path = value.strip_prefix("expri://./").ok_or_else(|| {
    message("private references must use expri://./ with the repository's local service settings")
  })?;
  if path.is_empty() || path.contains(['?', '#', '\\']) || path.len() > 4096 {
    return Err(message("invalid repository-relative asset reference"));
  }
  path
    .split('/')
    .map(|part| {
      let part = decode_segment(part)?;
      if part.is_empty() || matches!(part.as_str(), "." | "..") || part.contains(['/', '\\']) {
        return Err(message("asset reference contains an unsafe path component"));
      }
      Ok(part)
    })
    .collect()
}

fn decode_segment(value: &str) -> Result<String> {
  let mut bytes = Vec::with_capacity(value.len());
  let mut input = value.bytes();
  while let Some(byte) = input.next() {
    if byte == b'%' {
      let high = input.next().and_then(|byte| (byte as char).to_digit(16));
      let low = input.next().and_then(|byte| (byte as char).to_digit(16));
      let (Some(high), Some(low)) = (high, low) else {
        return Err(message("asset reference contains invalid URL escaping"));
      };
      bytes.push((high * 16 + low) as u8);
    } else {
      bytes.push(byte);
    }
  }
  String::from_utf8(bytes).map_err(|_| message("asset reference must contain UTF-8 paths"))
}
