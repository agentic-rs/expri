use std::fs::{self, File, OpenOptions};
use std::path::Path;

use crate::error::{ExpriError, Result};

pub fn worktree_lock(state_dir: &Path) -> Result<File> {
  create_state_directory(state_dir)?;
  let path = state_dir.join("worktree.lock");
  match fs::symlink_metadata(&path) {
    Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
      return Err(ExpriError::Message(format!(
        "checkout lock must be a regular file: {}",
        path.display()
      )));
    }
    Ok(_) => {}
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(source) => return Err(io_context("inspect", &path, source)),
  }
  let file = OpenOptions::new()
    .read(true)
    .write(true)
    .create(true)
    .truncate(false)
    .open(&path)
    .map_err(|source| io_context("open checkout lock", &path, source))?;
  file
    .lock()
    .map_err(|source| io_context("lock checkout", &path, source))?;
  Ok(file)
}

fn create_state_directory(path: &Path) -> Result<()> {
  if let Some(parent) = path.parent()
    && !parent.as_os_str().is_empty()
  {
    create_state_directory(parent)?;
  }
  match fs::create_dir(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
      let metadata =
        fs::symlink_metadata(path).map_err(|source| io_context("inspect", path, source))?;
      if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
      } else {
        Err(ExpriError::Message(format!(
          "run state directory must not be a symlink or file: {}",
          path.display()
        )))
      }
    }
    Err(source) => Err(io_context("create directory", path, source)),
  }
}

fn io_context(action: &'static str, path: &Path, source: std::io::Error) -> ExpriError {
  ExpriError::IoContext {
    action,
    path: path.display().to_string(),
    source,
  }
}

#[cfg(all(test, unix))]
mod tests {
  use std::os::unix::fs::symlink;

  use super::*;

  #[test]
  fn checkout_lock_rejects_symlink_state_directory_and_parents() {
    let root = tempfile::tempdir().expect("root");
    let root = fs::canonicalize(root.path()).expect("root path");
    let outside = tempfile::tempdir().expect("outside");
    symlink(outside.path(), root.join("linked")).expect("state symlink");
    assert!(worktree_lock(&root.join("linked")).is_err());
    assert!(worktree_lock(&root.join("linked/nested/state")).is_err());
    assert!(!outside.path().join("nested").exists());
  }

  #[test]
  fn checkout_lock_rejects_symlink_lock_file() {
    let root = tempfile::tempdir().expect("root");
    let root = fs::canonicalize(root.path()).expect("root path");
    let outside = tempfile::NamedTempFile::new().expect("outside file");
    symlink(outside.path(), root.join("worktree.lock")).expect("lock symlink");
    assert!(worktree_lock(&root).is_err());
  }
}
