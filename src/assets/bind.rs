use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::Descriptor;
use crate::error::{ExpriError, Result};

/// Pin verified cache bytes so an atomic cache replacement cannot change a run.
pub fn bind_verified(
  source: &Path,
  binding: &Path,
  descriptor: &Descriptor,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  descriptor.validate()?;
  check_cancelled(cancelled)?;
  let parent = binding
    .parent()
    .filter(|path| !path.as_os_str().is_empty())
    .unwrap_or(Path::new("."));
  directories(parent)?;
  optional_regular(binding)?;
  if inspect(binding)?.is_some() {
    return Err(message(
      "asset binding already exists; remove it explicitly before downloading or use a new run directory",
    ));
  }
  let mut file = open(source)?;
  let initial = file.metadata()?;
  if initial.len() != descriptor.size {
    return Err(message("verified asset changed before binding"));
  }
  protect_read_only(&file)?;
  let protected = file.metadata()?;
  if !unchanged(&initial, &protected) || !unchanged(&protected, &open(source)?.metadata()?) {
    return Err(message("verified asset changed before binding"));
  }
  check_cancelled(cancelled)?;
  match fs::hard_link(source, binding) {
    Ok(()) => {
      let bound = open(binding)?;
      let bound_metadata = bound.metadata()?;
      if !unchanged(&protected, &bound_metadata)
        || !unchanged(&protected, &file.metadata()?)
        || !unchanged(&protected, &open(source)?.metadata()?)
      {
        // Never remove another process's replacement while cleaning our own link.
        if inspect(binding)?.is_some_and(|current| same_identity(&current, &bound_metadata)) {
          fs::remove_file(binding)?;
          sync_directory(parent)?;
        }
        return Err(message("verified asset changed while binding"));
      }
    }
    Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
      copy_verified(&mut file, binding, descriptor, cancelled)?;
      if !unchanged(&protected, &file.metadata()?)
        || !unchanged(&protected, &open(source)?.metadata()?)
      {
        return Err(message("verified asset changed while binding"));
      }
    }
    Err(error) => return Err(error.into()),
  }
  sync_directory(parent)
}

fn protect_read_only(file: &File) -> Result<()> {
  let mut permissions = file.metadata()?.permissions();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    if permissions.mode() & 0o7777 == 0o400 {
      return Ok(());
    }
    permissions.set_mode(0o400);
  }
  #[cfg(not(unix))]
  {
    if permissions.readonly() {
      return Ok(());
    }
    permissions.set_readonly(true);
  }
  file.set_permissions(permissions)?;
  file.sync_all()?;
  Ok(())
}

/// Verify during the necessary cross-filesystem copy, then publish atomically.
fn copy_verified(
  source: &mut File,
  binding: &Path,
  descriptor: &Descriptor,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  check_cancelled(cancelled)?;
  let parent = binding
    .parent()
    .filter(|path| !path.as_os_str().is_empty())
    .unwrap_or(Path::new("."));
  directory(parent)?;
  let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
  let mut remaining = descriptor.size;
  let mut digest = Sha256::new();
  let mut buffer = [0u8; 64 * 1024];
  while remaining > 0 {
    check_cancelled(cancelled)?;
    let length = remaining.min(buffer.len() as u64) as usize;
    source.read_exact(&mut buffer[..length])?;
    temporary.write_all(&buffer[..length])?;
    digest.update(&buffer[..length]);
    remaining -= length as u64;
  }
  let actual = digest
    .finalize()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect::<String>();
  if actual != descriptor.sha256 || source.read(&mut [0u8; 1])? != 0 {
    return Err(message(
      "asset copy did not match its verified SHA256 digest",
    ));
  }
  check_cancelled(cancelled)?;
  protect_read_only(temporary.as_file())?;
  temporary.as_file().sync_all()?;
  optional_regular(binding)?;
  temporary
    .persist_noclobber(binding)
    .map_err(|error| error.error)?;
  sync_directory(parent)
}

pub(crate) fn message(value: impl Into<String>) -> ExpriError {
  ExpriError::Message(value.into())
}

pub(crate) fn check_cancelled(cancelled: &mut dyn FnMut() -> Result<bool>) -> Result<()> {
  if cancelled()? {
    return Err(ExpriError::DownloadCancelled);
  }
  Ok(())
}

pub(crate) fn inspect(path: &Path) -> Result<Option<Metadata>> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error.into()),
  }
}

pub(crate) fn parents(path: &Path) -> Result<()> {
  for parent in path
    .ancestors()
    .skip(1)
    .filter(|parent| !parent.as_os_str().is_empty())
  {
    if let Some(metadata) = inspect(parent)?
      && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
      return Err(message("asset path has an unsafe parent directory"));
    }
  }
  Ok(())
}

pub(crate) fn directory(path: &Path) -> Result<()> {
  parents(path)?;
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message("asset directory must be a real directory"));
  }
  Ok(())
}

pub(crate) fn directories(path: &Path) -> Result<()> {
  if path.as_os_str().is_empty() {
    return Ok(());
  }
  if let Some(parent) = path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
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
      if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
      {
        sync_directory(parent)?;
      }
      Ok(())
    }
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => directory(path),
    Err(error) => Err(error.into()),
  }
}

pub(crate) fn optional_regular(path: &Path) -> Result<()> {
  parents(path)?;
  if let Some(metadata) = inspect(path)?
    && (!metadata.is_file() || metadata.file_type().is_symlink())
  {
    return Err(message(
      "asset file must be a regular file without symlinks",
    ));
  }
  Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
  directory(path)?;
  #[cfg(unix)]
  File::open(path)?.sync_all()?;
  Ok(())
}

pub(crate) fn open(path: &Path) -> Result<File> {
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
    return Err(message("asset file changed while opening"));
  }
  optional_regular(path)?;
  Ok(file)
}

fn same_identity(first: &Metadata, second: &Metadata) -> bool {
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

pub(crate) fn unchanged(first: &Metadata, second: &Metadata) -> bool {
  same_identity(first, second)
    && first.len() == second.len()
    && first.modified().ok() == second.modified().ok()
}

#[cfg(test)]
#[path = "bind_tests.rs"]
mod tests;
