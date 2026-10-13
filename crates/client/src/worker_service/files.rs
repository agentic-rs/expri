//! Private registry files, bounded reads and crash-safe durable replacements.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use expri_core::error::Result;
use expri_core::storage::StoragePaths;
use serde::Serialize;

use super::error;

pub(super) const LIMIT: u64 = 256 * 1024;

pub(super) fn existing(paths: &StoragePaths) -> Result<Option<PathBuf>> {
  paths.validate_existing()?;
  if !paths.root().try_exists()? {
    return Ok(None);
  }
  let path = paths.root().canonicalize()?.join("worker-service");
  match fs::symlink_metadata(&path) {
    Ok(metadata) => {
      validate(&metadata, true)?;
      Ok(Some(path))
    }
    Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(failure) => Err(failure.into()),
  }
}

pub(super) fn ensure(paths: &StoragePaths) -> Result<PathBuf> {
  paths.ensure_private()?;
  let path = paths.root().canonicalize()?.join("worker-service");
  directory(&path)?;
  Ok(path)
}

pub(super) fn directory(path: &Path) -> Result<()> {
  let mut builder = fs::DirBuilder::new();
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
  }
  match builder.create(path) {
    Ok(()) => sync_parent(path)?,
    Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => {}
    Err(failure) => return Err(failure.into()),
  }
  validate(&fs::symlink_metadata(path)?, true)
}

pub(super) fn validate(metadata: &fs::Metadata, directory: bool) -> Result<()> {
  validate_metadata(metadata, directory, false)
}

fn validate_metadata(metadata: &fs::Metadata, directory: bool, allow_unlinked: bool) -> Result<()> {
  #[cfg(not(unix))]
  let _ = allow_unlinked;
  if metadata.file_type().is_symlink()
    || (directory && !metadata.is_dir())
    || (!directory && !metadata.is_file())
  {
    return Err(error(
      "worker service storage must contain real private directories and regular files",
    ));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if metadata.uid() != unsafe { libc::geteuid() }
      || metadata.permissions().mode() & 0o077 != 0
      || (!directory && metadata.nlink() != 1 && !(allow_unlinked && metadata.nlink() == 0))
    {
      return Err(error(
        "worker service storage must be owned by the current user and private",
      ));
    }
  }
  Ok(())
}

pub(super) fn read(path: &Path) -> Result<Vec<u8>> {
  validate(&fs::symlink_metadata(path)?, false)?;
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  read_open_file(options.open(path)?)
}

fn read_open_file(file: File) -> Result<Vec<u8>> {
  // A durable atomic replacement can unlink this file after we opened it. Its
  // immutable old snapshot remains safe to read; additional hardlinks do not.
  validate_metadata(&file.metadata()?, false, true)?;
  let mut bytes = Vec::new();
  file.take(LIMIT + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > LIMIT {
    return Err(error("worker service metadata exceeds its size limit"));
  }
  Ok(bytes)
}

pub(super) fn write(path: &Path, value: &impl Serialize) -> Result<()> {
  let bytes = serde_json::to_vec(value)?;
  if bytes.len() as u64 + 1 > LIMIT {
    return Err(error("worker service metadata exceeds its size limit"));
  }
  if let Ok(metadata) = fs::symlink_metadata(path) {
    validate(&metadata, false)?;
  }
  let parent = path
    .parent()
    .ok_or_else(|| error("worker service metadata needs a parent"))?;
  validate(&fs::symlink_metadata(parent)?, true)?;
  let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
  temporary.write_all(&bytes)?;
  temporary.write_all(b"\n")?;
  temporary.as_file().sync_all()?;
  if let Ok(metadata) = fs::symlink_metadata(path) {
    validate(&metadata, false)?;
  }
  temporary.persist(path).map_err(|failure| failure.error)?;
  sync_parent(path)
}

pub(super) fn private_file(path: &Path) -> Result<File> {
  expri_core::storage::ensure_private_file(path)
}

fn sync_parent(path: &Path) -> Result<()> {
  #[cfg(unix)]
  File::open(
    path
      .parent()
      .ok_or_else(|| error("worker service path needs a parent"))?,
  )?
  .sync_all()?;
  Ok(())
}

#[cfg(all(test, unix))]
mod tests {
  use std::os::unix::fs::MetadataExt;

  use super::*;

  #[test]
  fn opened_snapshot_survives_atomic_replacement_but_hardlinks_are_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let private = temporary.path().join("private");
    directory(&private).unwrap();
    let path = private.join("status.json");
    write(&path, &serde_json::json!({"ready": false})).unwrap();
    let previous = File::open(&path).unwrap();
    write(&path, &serde_json::json!({"ready": true})).unwrap();
    assert_eq!(previous.metadata().unwrap().nlink(), 0);
    let old: serde_json::Value =
      serde_json::from_slice(&read_open_file(previous).unwrap()).unwrap();
    assert_eq!(old["ready"], false);
    let current: serde_json::Value = serde_json::from_slice(&read(&path).unwrap()).unwrap();
    assert_eq!(current["ready"], true);

    let opened = File::open(&path).unwrap();
    fs::hard_link(&path, private.join("linked.json")).unwrap();
    assert!(read(&path).is_err());
    assert!(read_open_file(opened).is_err());
  }
}
