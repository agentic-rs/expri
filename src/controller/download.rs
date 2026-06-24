use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::{DownloadMapping, TargetConfig};
use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result};
use crate::shell;

pub struct DownloadOptions {
  pub repo_root: PathBuf,
  pub project_name: Option<String>,
  pub target_name: String,
  pub target: TargetConfig,
  pub results_dir: String,
  pub mappings: Vec<DownloadMapping>,
  pub ignore: Vec<String>,
  pub names: Vec<String>,
  pub control_path: String,
  pub control_persist: String,
  pub dry_run: bool,
  pub verbosity: u8,
  pub quiet: bool,
}

pub fn download_target(options: DownloadOptions) -> Result<()> {
  if options.mappings.is_empty() {
    return Err(ExpriError::Message(
      "no download mappings configured in expri.toml".to_string(),
    ));
  }

  let mappings = selected_mappings(options.mappings, &options.names)?;
  let results_dir = safe_relative_path(&options.results_dir, "download results_dir")?;
  validate_ignore_patterns(&options.ignore)?;
  let local_root = options
    .repo_root
    .join(results_dir)
    .join(&options.target_name);
  let remote = Remote::new(
    options.target,
    options.control_path,
    options.control_persist,
    options.dry_run,
    options.verbosity,
    options.quiet,
  );

  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    eprintln!("download target: {}", options.target_name);
    eprintln!("repo root: {}", options.repo_root.display());
    eprintln!("results root: {}", local_root.display());
    for pattern in &options.ignore {
      eprintln!("ignore: {pattern}");
    }
  }

  let _opened_master = remote.open_master()?;
  for mapping in mappings {
    let remote_path = safe_relative_path(&mapping.remote_path, "download remote path")?;
    let local_path = safe_relative_path(&mapping.local_path, "download local path")?;
    let source = join_remote_path(&remote.remote_dir, &remote_path);
    let destination = local_root.join(local_path);
    if options.verbosity > 0 && !options.quiet {
      eprintln!(
        "download {}: {} -> {}",
        mapping.name,
        source,
        destination.display()
      );
    }
    if !remote_path_exists(&remote, &remote_path)? {
      if !options.quiet {
        eprintln!("skip {}: remote path does not exist", mapping.name);
      }
      continue;
    }
    if !options.dry_run {
      fs::create_dir_all(&destination).map_err(|source| ExpriError::IoContext {
        action: "create directory",
        path: destination.display().to_string(),
        source,
      })?;
    }
    remote.download_dir_with_excludes(&source, &destination, &options.ignore)?;
  }
  Ok(())
}

fn selected_mappings(
  mappings: Vec<DownloadMapping>,
  names: &[String],
) -> Result<Vec<DownloadMapping>> {
  let mut by_name = BTreeMap::new();
  for mapping in mappings {
    by_name.insert(mapping.name.clone(), mapping);
  }
  if names.is_empty() {
    return Ok(by_name.into_values().collect());
  }

  let mut missing = Vec::new();
  let mut selected = Vec::new();
  let mut seen = BTreeSet::new();
  for name in names {
    if !seen.insert(name) {
      continue;
    }
    match by_name.get(name) {
      Some(mapping) => selected.push(mapping.clone()),
      None => missing.push(name.clone()),
    }
  }
  if !missing.is_empty() {
    return Err(ExpriError::Message(format!(
      "unknown download mapping(s): {}",
      missing.join(", ")
    )));
  }
  Ok(selected)
}

fn safe_relative_path(value: &str, label: &str) -> Result<PathBuf> {
  let path = PathBuf::from(value);
  if value.is_empty()
    || path.is_absolute()
    || path
      .components()
      .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
  {
    return Err(ExpriError::Message(format!(
      "{label} must be a relative path inside the repo/results root: {value}"
    )));
  }
  Ok(path)
}

fn validate_ignore_patterns(patterns: &[String]) -> Result<()> {
  for pattern in patterns {
    let path = PathBuf::from(pattern);
    if pattern.is_empty()
      || path.is_absolute()
      || path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
      return Err(ExpriError::Message(format!(
        "download ignore pattern must be relative and stay inside the mapping root: {pattern}"
      )));
    }
  }
  Ok(())
}

fn join_remote_path(remote_dir: &str, relative_path: &Path) -> String {
  let relative = relative_path.to_string_lossy();
  let remote_dir = remote_dir.trim_end_matches('/');
  format!("{remote_dir}/{relative}")
}

fn remote_path_exists(remote: &Remote, relative_path: &Path) -> Result<bool> {
  remote.ssh_success(&format!(
    "cd {} && [ -e {} ]",
    remote.quoted_remote_dir(),
    shell::quote(relative_path.to_string_lossy())
  ))
}
