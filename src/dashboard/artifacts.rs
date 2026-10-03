use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;

use serde_json::Value;

use super::message;
use super::preview::projection;
use crate::error::Result;

const CACHE_RECORD_LIMIT: u64 = 256 * 1024;

pub(super) fn optional_metadata(path: &Path) -> std::io::Result<Option<fs::Metadata>> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error),
  }
}

pub(super) fn real_prefix(root: &Path, path: &Path) -> Result<()> {
  let relative = path
    .strip_prefix(root)
    .map_err(|_| message("dashboard path is outside the repository"))?;
  let mut current = root.to_path_buf();
  for component in relative.components() {
    current.push(component);
    let Some(metadata) = optional_metadata(&current)? else {
      break;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
      return Err(message(format!(
        "dashboard directory must be a real directory: {}",
        current.display()
      )));
    }
  }
  Ok(())
}

/// Fixed log/cache files are opened without following links or blocking on FIFOs.
pub(super) fn open_fixed(run_dir: &Path, relative: &str) -> Result<Option<File>> {
  let path = run_dir.join(relative);
  real_prefix(run_dir, path.parent().unwrap())?;
  let Some(initial) = optional_metadata(&path)? else {
    return Ok(None);
  };
  if !initial.is_file() || initial.file_type().is_symlink() {
    return Err(message(format!("{relative} must be a regular file")));
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options.open(&path)?;
  let opened = file.metadata()?;
  if !opened.is_file() {
    return Err(message(format!("{relative} must be a regular file")));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if initial.dev() != opened.dev() || initial.ino() != opened.ino() {
      return Err(message(format!("{relative} changed while opening")));
    }
  }
  real_prefix(run_dir, path.parent().unwrap())?;
  Ok(Some(file))
}

pub(super) fn cache_record(run_dir: &Path, truncated: &mut bool) -> Result<Value> {
  let Some(file) = open_fixed(run_dir, "pull-state.json")? else {
    return Ok(Value::Null);
  };
  let mut bytes = Vec::new();
  file.take(CACHE_RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > CACHE_RECORD_LIMIT {
    return Err(message("pull-state.json exceeds the 256 KiB size limit"));
  }
  let record: Value = serde_json::from_slice(&bytes)?;
  if !record.is_object() {
    return Err(message("pull-state.json must be an object"));
  }
  Ok(projection(
    &record,
    &[
      "pulled_at",
      "selected_files",
      "target_name",
      "remote_run_dir",
    ],
    truncated,
  ))
}
