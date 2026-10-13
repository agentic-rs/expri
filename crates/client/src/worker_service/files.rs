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
      || (!directory && metadata.nlink() != 1)
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
  let file = options.open(path)?;
  validate(&file.metadata()?, false)?;
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
