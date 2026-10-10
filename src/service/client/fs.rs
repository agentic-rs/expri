use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{ExpriError, Result};

pub(in crate::service) fn message(value: impl Into<String>) -> ExpriError {
  ExpriError::Message(value.into())
}

pub(in crate::service) fn inspect(path: &Path) -> Result<Option<Metadata>> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error.into()),
  }
}

pub(super) fn parents(path: &Path) -> Result<()> {
  for parent in path.ancestors().skip(1) {
    if let Some(metadata) = inspect(parent)?
      && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
      return Err(message("service file has an unsafe parent directory"));
    }
  }
  Ok(())
}

pub(in crate::service) fn directory(path: &Path) -> Result<()> {
  parents(path)?;
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message("service directory must be a real directory"));
  }
  Ok(())
}

pub(in crate::service) fn directories(path: &Path) -> Result<()> {
  if let Some(parent) = path.parent()
    && !parent.as_os_str().is_empty()
  {
    directories(parent)?;
  }
  match fs::create_dir(path) {
    Ok(()) => {
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
      }
      if let Some(parent) = path.parent() {
        sync_directory(parent)?;
      }
      Ok(())
    }
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => directory(path),
    Err(error) => Err(error.into()),
  }
}

pub(in crate::service) fn optional_regular(path: &Path) -> Result<()> {
  parents(path)?;
  if let Some(metadata) = inspect(path)?
    && (!metadata.is_file() || metadata.file_type().is_symlink())
  {
    return Err(message("service file must be a regular file"));
  }
  Ok(())
}

pub(in crate::service) fn sync_directory(path: &Path) -> Result<()> {
  directory(path)?;
  #[cfg(unix)]
  File::open(path)?.sync_all()?;
  Ok(())
}

pub(in crate::service) fn open(path: &Path) -> Result<File> {
  optional_regular(path)?;
  let initial = fs::symlink_metadata(path)?;
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options.open(path)?;
  let current = fs::symlink_metadata(path)?;
  if !file.metadata()?.is_file()
    || !same_identity(&initial, &current)
    || !same_identity(&file.metadata()?, &current)
  {
    return Err(message("service file changed while opening"));
  }
  optional_regular(path)?;
  Ok(file)
}

pub(super) fn same_identity(first: &Metadata, second: &Metadata) -> bool {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    first.dev() == second.dev() && first.ino() == second.ino()
  }
  #[cfg(not(unix))]
  {
    first.created().ok() == second.created().ok()
  }
}

pub(in crate::service) fn unchanged(first: &Metadata, second: &Metadata) -> bool {
  same_identity(first, second)
    && first.len() == second.len()
    && first.modified().ok() == second.modified().ok()
}

pub(in crate::service) fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
  let mut bytes = Vec::new();
  open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > limit {
    return Err(message("service metadata exceeds its size limit"));
  }
  Ok(bytes)
}

pub(in crate::service) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
  optional_regular(path)?;
  let parent = path
    .parent()
    .ok_or_else(|| message("metadata has no parent"))?;
  directory(parent)?;
  let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
  serde_json::to_writer_pretty(&mut temporary, value)?;
  temporary.write_all(b"\n")?;
  temporary.as_file().sync_all()?;
  optional_regular(path)?;
  temporary.persist(path).map_err(|error| error.error)?;
  sync_directory(parent)?;
  Ok(())
}

pub(super) fn digest(file: &mut File, size: u64) -> Result<String> {
  digest_with_cancel(file, size, &mut || Ok(false))
}

pub(in crate::service) fn check_cancelled(
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  if cancelled()? {
    return Err(ExpriError::DownloadCancelled);
  }
  Ok(())
}

pub(in crate::service) fn digest_with_cancel(
  file: &mut File,
  size: u64,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<String> {
  check_cancelled(cancelled)?;
  file.seek(SeekFrom::Start(0))?;
  let mut hash = Sha256::new();
  let mut remaining = size;
  let mut buffer = [0u8; 64 * 1024];
  while remaining > 0 {
    check_cancelled(cancelled)?;
    let length = remaining.min(buffer.len() as u64) as usize;
    let count = file.read(&mut buffer[..length])?;
    if count == 0 {
      return Err(message("service source ended before its recorded size"));
    }
    hash.update(&buffer[..count]);
    remaining -= count as u64;
  }
  check_cancelled(cancelled)?;
  Ok(hex(&hash.finalize()))
}

pub(super) fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
