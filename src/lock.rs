use std::fs::{self, File, OpenOptions};
use std::path::Path;

use crate::error::{ExpriError, Result};

pub struct FileLock {
  file: File,
}

impl Drop for FileLock {
  fn drop(&mut self) {
    // A concurrently spawned child can briefly inherit the file description.
    // Release the lease before close rather than waiting for its last copy.
    let _ = self.file.unlock();
  }
}

pub enum LockAttempt {
  Acquired(FileLock),
  Busy,
  Missing,
}

/// Hold this lease from run preparation until its terminal state is published.
pub fn run_lock(run_dir: &Path) -> Result<FileLock> {
  let file =
    open_lock_file(&run_dir.join(".run.lock"), true)?.expect("creating a lock file returns a file");
  file
    .lock()
    .map_err(|source| io_context("lock run", run_dir, source))?;
  Ok(FileLock { file })
}

/// Preview callers pass `create = false` so inspecting a run never creates files.
pub fn try_lock_file(path: &Path, create: bool) -> Result<LockAttempt> {
  let Some(file) = open_lock_file(path, create)? else {
    return Ok(LockAttempt::Missing);
  };
  match file.try_lock() {
    Ok(()) => Ok(LockAttempt::Acquired(FileLock { file })),
    Err(std::fs::TryLockError::WouldBlock) => Ok(LockAttempt::Busy),
    Err(std::fs::TryLockError::Error(source)) => Err(io_context("lock file", path, source)),
  }
}

fn open_lock_file(path: &Path, create: bool) -> Result<Option<File>> {
  if let Some(parent) = path.parent() {
    validate_directory(parent)?;
  }
  let initial = match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Some(metadata),
    Ok(_) => {
      return Err(ExpriError::Message(format!(
        "lock must be a regular file: {}",
        path.display()
      )));
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
    Err(source) => return Err(io_context("inspect", path, source)),
  };
  let file = if initial.is_some() {
    OpenOptions::new().read(true).write(true).open(path)
  } else if create {
    match OpenOptions::new()
      .read(true)
      .write(true)
      .create_new(true)
      .open(path)
    {
      Ok(file) => Ok(file),
      Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
        return open_lock_file(path, create);
      }
      Err(error) => Err(error),
    }
  } else {
    return Ok(None);
  }
  .map_err(|source| io_context("open lock", path, source))?;
  let metadata = file
    .metadata()
    .map_err(|source| io_context("inspect open lock", path, source))?;
  let current = fs::symlink_metadata(path).map_err(|source| io_context("inspect", path, source))?;
  if !metadata.is_file() || !current.is_file() || current.file_type().is_symlink() {
    return Err(ExpriError::Message(format!(
      "lock must be a regular file: {}",
      path.display()
    )));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if metadata.dev() != current.dev() || metadata.ino() != current.ino() {
      return Err(ExpriError::Message(format!(
        "lock changed while opening: {}",
        path.display()
      )));
    }
  }
  if let Some(parent) = path.parent() {
    validate_directory(parent)?;
  }
  Ok(Some(file))
}

fn validate_directory(path: &Path) -> Result<()> {
  if let Some(parent) = path.parent()
    && !parent.as_os_str().is_empty()
  {
    validate_directory(parent)?;
  }
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

pub fn worktree_lock(state_dir: &Path) -> Result<FileLock> {
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
  Ok(FileLock { file })
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
  fn leases_release_while_an_inherited_file_description_remains_open() {
    let root = tempfile::tempdir().expect("root");
    let root = fs::canonicalize(root.path()).expect("root path");
    for lease_kind in ["run", "prepare", "worktree"] {
      let directory = root.join(lease_kind);
      fs::create_dir(&directory).expect("lease directory");
      let (lease, path) = match lease_kind {
        "run" => (
          run_lock(&directory).expect("run lease"),
          directory.join(".run.lock"),
        ),
        "prepare" => {
          let path = directory.join(".prepare.lock");
          let LockAttempt::Acquired(lease) = try_lock_file(&path, true).expect("prepare lease")
          else {
            panic!("new prepare lease should be available");
          };
          (lease, path)
        }
        _ => (
          worktree_lock(&directory).expect("worktree lease"),
          directory.join("worktree.lock"),
        ),
      };
      // A descriptor copied by fork refers to the same open file description.
      let inherited = lease.file.try_clone().expect("inherited lease descriptor");
      assert!(
        matches!(try_lock_file(&path, false).unwrap(), LockAttempt::Busy),
        "{lease_kind} lease must protect its scope"
      );
      drop(lease);
      let LockAttempt::Acquired(released) = try_lock_file(&path, false).expect("released lease")
      else {
        panic!("{lease_kind} lease survived its owning scope");
      };
      drop(released);
      drop(inherited);
    }
  }

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
