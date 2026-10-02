use std::collections::BTreeSet;
use std::fs::{self, Metadata};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result};
use crate::lock::LockAttempt;

const OWNER_FILE: &str = ".pull-owner.json";
const RECEIPT_FILE: &str = "pull-state.json";
const EXCLUDED_COMPONENTS: &[&str] = &[".venv", ".expri", ".git", ".cache", "cache", "__pycache__"];

/// Pull a catalog selection without transferring environments or replacing the
/// whole cached run. Artifacts from previous selective pulls remain available.
pub fn pull(
  remote: &Remote,
  repo_root: &Path,
  results_dir: &str,
  target_name: &str,
  selection: &Value,
) -> Result<Value> {
  let runs_dir = cached_runs_dir(repo_root, results_dir, target_name)?;
  let run_id = string_field(selection, "run_id")?;
  validate_component(run_id, "run ID")?;
  let mut id = run_id.bytes();
  if !id.next().is_some_and(|byte| byte.is_ascii_alphanumeric())
    || !id.all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
  {
    return Err(message("catalog returned an invalid run ID"));
  }
  let remote_run_dir = string_field(selection, "run_dir")?;
  validate_remote_run_dir(remote_run_dir, run_id)?;
  let selected_files = selection
    .get("files")
    .and_then(Value::as_array)
    .ok_or_else(|| message("catalog file selection must be an array"))?
    .iter()
    .map(|value| {
      let relative = value
        .as_str()
        .ok_or_else(|| message("catalog file selection must contain strings"))?;
      validate_file(relative)?;
      Ok(relative.to_string())
    })
    .collect::<Result<BTreeSet<_>>>()?;
  let warnings = match selection.get("warnings") {
    Some(Value::Array(warnings)) => warnings.clone(),
    None => Vec::new(),
    _ => return Err(message("catalog warnings must be an array")),
  };
  let destination = runs_dir.join(run_id);
  let owner = json!({
    "schema_version": 1,
    "target_name": target_name,
    "run_id": run_id,
    "remote_run_dir": remote_run_dir,
  });
  validate_destination(&destination, &owner, &selected_files)?;
  let report = json!({
    "run_id": run_id,
    "target_name": target_name,
    "destination": destination,
    "files": selected_files,
    "dry_run": remote.dry_run,
    "warnings": warnings,
  });
  if remote.dry_run {
    return Ok(report);
  }

  let parent = destination.parent().expect("run destination has a parent");
  create_directories(parent)?;
  // Staging shares a filesystem with the destination. A failed transfer cannot
  // overwrite a previously pulled status, log, artifact, or receipt.
  let staging = tempfile::Builder::new()
    .prefix(".pull-")
    .tempdir_in(parent)?;
  let code = staging.path().join("files");
  fs::create_dir(&code)?;
  let mut files_from = tempfile::NamedTempFile::new_in(staging.path())?;
  for relative in &selected_files {
    files_from.write_all(relative.as_bytes())?;
    files_from.write_all(&[0])?;
  }
  files_from.flush()?;
  if !selected_files.is_empty() {
    remote.download_run_files_from(remote_run_dir, &code, files_from.path())?;
  }
  for relative in &selected_files {
    validate_regular_file(&code.join(relative))?;
  }
  validate_destination(&destination, &owner, &selected_files)?;
  match fs::create_dir(&destination) {
    Ok(()) => atomic_json(&destination.join(OWNER_FILE), &owner)?,
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
      validate_owner(&destination, &owner)?;
    }
    Err(error) => return Err(error.into()),
  }
  let lease = crate::lock::try_lock_file(&destination.join(".pull.lock"), true)?;
  let LockAttempt::Acquired(_lease) = lease else {
    return Err(message(
      "another pull is publishing this run; retry after it finishes",
    ));
  };
  validate_destination(&destination, &owner, &selected_files)?;
  // Prepare all parents and validate every target before replacing any file.
  for relative in &selected_files {
    let target = destination.join(relative);
    create_directories(target.parent().expect("selected file has a parent"))?;
    validate_optional_regular_file(&target)?;
    validate_regular_file(&code.join(relative))?;
  }
  for relative in selected_files
    .iter()
    .filter(|relative| *relative != "run-state.json")
  {
    publish_file(&code.join(relative), &destination.join(relative))?;
  }
  if selected_files.contains("run-state.json") {
    publish_file(
      &code.join("run-state.json"),
      &destination.join("run-state.json"),
    )?;
  }
  let receipt = json!({
    "schema_version": 1,
    "target_name": target_name,
    "run_id": run_id,
    "remote_run_dir": remote_run_dir,
    "pulled_at": chrono::Utc::now().to_rfc3339(),
    "selected_files": selected_files,
  });
  atomic_json(&destination.join(RECEIPT_FILE), &receipt)?;
  Ok(report)
}

pub fn validate_component(value: &str, label: &str) -> Result<()> {
  let path = Path::new(value);
  if value.is_empty()
    || value.contains(['/', '\\'])
    || value.chars().any(char::is_control)
    || !matches!(path.components().next(), Some(Component::Normal(_)))
    || path.components().count() != 1
  {
    return Err(message(format!(
      "{label} must be a single safe path component: {value}"
    )));
  }
  Ok(())
}

/// Resolve the trusted repository first, then reject cache paths that traverse
/// symlinks. This remains read-only even when no records have been pulled yet.
pub fn cached_runs_dir(repo_root: &Path, results_dir: &str, target_name: &str) -> Result<PathBuf> {
  validate_component(target_name, "target name")?;
  let results_dir = validate_results_dir(results_dir)?;
  let path = fs::canonicalize(repo_root)?
    .join(results_dir)
    .join(target_name)
    .join("runs");
  validate_parents(&path)?;
  if let Some(metadata) = optional_metadata(&path)?
    && (!metadata.is_dir() || metadata.file_type().is_symlink())
  {
    return Err(message("pulled run cache must be a real directory"));
  }
  Ok(path)
}

pub fn validate_results_dir(value: &str) -> Result<PathBuf> {
  let path = Path::new(value);
  if value.is_empty()
    || value.contains('\\')
    || value.chars().any(char::is_control)
    || path.is_absolute()
    || path
      .components()
      .any(|component| !matches!(component, Component::Normal(_)))
    || value.split('/').any(|part| matches!(part, "" | "." | ".."))
  {
    return Err(message(
      "results_dir must be a relative directory inside the repository",
    ));
  }
  Ok(path.to_path_buf())
}

fn validate_remote_run_dir(value: &str, run_id: &str) -> Result<()> {
  // This is a Unix target path, regardless of the controller's host platform.
  let parts: Vec<_> = value.split('/').collect();
  if !value.starts_with('/')
    || value.contains('\\')
    || value.chars().any(char::is_control)
    || parts.len() < 4
    || parts[1..]
      .iter()
      .any(|part| matches!(*part, "" | "." | ".."))
    || parts[parts.len() - 3..] != [".expri", "runs", run_id]
  {
    return Err(message(
      "catalog run_dir must be an absolute .expri/runs/<run_id> directory",
    ));
  }
  Ok(())
}

fn validate_file(relative: &str) -> Result<()> {
  let parts: Vec<_> = relative.split('/').collect();
  if relative.contains('\\')
    || relative.chars().any(char::is_control)
    || parts
      .iter()
      .any(|part| matches!(*part, "" | "." | "..") || EXCLUDED_COMPONENTS.contains(part))
    || !(matches!(
      relative,
      "run-state.json" | "snapshot.json" | "environment/environment-state.json"
    ) || (parts.len() > 1 && matches!(parts[0], "logs" | "outputs" | "code")))
  {
    return Err(message(format!(
      "catalog selected an unsafe or unsupported file: {relative}"
    )));
  }
  Ok(())
}

fn validate_destination(destination: &Path, owner: &Value, files: &BTreeSet<String>) -> Result<()> {
  validate_parents(destination)?;
  if let Some(metadata) = optional_metadata(destination)? {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
      return Err(message("pulled run destination must be a real directory"));
    }
    validate_owner(destination, owner)?;
    validate_optional_regular_file(&destination.join(RECEIPT_FILE))?;
    for relative in files {
      validate_optional_regular_file(&destination.join(relative))?;
    }
  }
  Ok(())
}

fn validate_owner(destination: &Path, expected: &Value) -> Result<()> {
  let path = destination.join(OWNER_FILE);
  validate_regular_file(&path)
    .map_err(|_| message("refusing to update an unowned pulled run directory"))?;
  let metadata = fs::symlink_metadata(&path)?;
  if metadata.len() > 16 * 1024 {
    return Err(message("pulled run ownership record is too large"));
  }
  let actual: Value = serde_json::from_slice(&fs::read(&path)?)?;
  if actual != *expected {
    return Err(message(
      "pulled run destination belongs to a different target or remote run",
    ));
  }
  Ok(())
}

fn validate_parents(path: &Path) -> Result<()> {
  for parent in path.ancestors().skip(1) {
    if let Some(metadata) = optional_metadata(parent)?
      && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
      return Err(message(format!(
        "pull destination has an unsafe parent: {}",
        parent.display()
      )));
    }
  }
  Ok(())
}

fn validate_regular_file(path: &Path) -> Result<()> {
  validate_parents(path)?;
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_file() || metadata.file_type().is_symlink() {
    return Err(message(format!(
      "pulled file must be a regular file: {}",
      path.display()
    )));
  }
  Ok(())
}

fn validate_optional_regular_file(path: &Path) -> Result<()> {
  validate_parents(path)?;
  if optional_metadata(path)?.is_some() {
    validate_regular_file(path)?;
  }
  Ok(())
}

fn create_directories(path: &Path) -> Result<()> {
  if let Some(parent) = path.parent()
    && !parent.as_os_str().is_empty()
  {
    create_directories(parent)?;
  }
  match fs::create_dir(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
      let metadata = fs::symlink_metadata(path)?;
      if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
      } else {
        Err(message("pull destination parent must be a real directory"))
      }
    }
    Err(error) => Err(error.into()),
  }
}

fn publish_file(source: &Path, destination: &Path) -> Result<()> {
  validate_regular_file(source)?;
  validate_optional_regular_file(destination)?;
  fs::rename(source, destination)?;
  Ok(())
}

fn atomic_json(path: &Path, value: &Value) -> Result<()> {
  validate_optional_regular_file(path)?;
  let mut temporary = tempfile::NamedTempFile::new_in(path.parent().expect("metadata parent"))?;
  temporary.write_all(&serde_json::to_vec_pretty(value)?)?;
  temporary.persist(path).map_err(|error| error.error)?;
  Ok(())
}

fn optional_metadata(path: &Path) -> Result<Option<Metadata>> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error.into()),
  }
}

fn string_field<'a>(selection: &'a Value, name: &str) -> Result<&'a str> {
  selection
    .get(name)
    .and_then(Value::as_str)
    .ok_or_else(|| message(format!("catalog selection has no string {name}")))
}

fn message(value: impl Into<String>) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(all(test, unix))]
mod tests;
