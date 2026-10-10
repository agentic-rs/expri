//! Safe paths for locally cached remote run records.

use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};

use crate::error::{ExpriError, Result};

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

pub fn validate_parents(path: &Path) -> Result<()> {
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

fn optional_metadata(path: &Path) -> Result<Option<Metadata>> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error.into()),
  }
}

fn message(value: impl Into<String>) -> ExpriError {
  ExpriError::Message(value.into())
}
