//! Count registered archive payloads without walking directories or following links.
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;

use super::*;

pub(super) fn archive_bytes(directory: &Path, db: &Connection, project_id: &str) -> ApiResult<u64> {
  let mut statement = db
    .prepare("SELECT id FROM result_archives WHERE json_extract(scope,'$.project_id')=?1")
    .map_err(database)?;
  let ids = statement
    .query_map([project_id], |row| row.get::<_, i64>(0))
    .map_err(database)?;
  let base = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
    .open(directory)
    .map_err(local_error)?;
  let Some(archives) = child(&base, "archives", true)? else {
    return Ok(0);
  };
  let mut bytes = 0u64;
  for id in ids {
    let id = id.map_err(database)?.to_string();
    let Some(archive) = child(&archives, &id, true)? else {
      continue;
    };
    let Some(file) = child(&archive, "result.zip", false)? else {
      continue;
    };
    let metadata = file.metadata().map_err(local_error)?;
    if metadata.is_file() {
      bytes = bytes.saturating_add(metadata.len());
    }
  }
  Ok(bytes)
}

fn child(parent: &File, name: &str, directory: bool) -> ApiResult<Option<File>> {
  let name = std::ffi::CString::new(name).expect("fixed archive name or integer id");
  let flags = libc::O_RDONLY
    | libc::O_NOFOLLOW
    | libc::O_CLOEXEC
    | libc::O_NONBLOCK
    | if directory { libc::O_DIRECTORY } else { 0 };
  let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
  if fd < 0 {
    let error = std::io::Error::last_os_error();
    return if error.kind() == std::io::ErrorKind::NotFound
      || matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP))
    {
      Ok(None)
    } else {
      Err(local_error(error))
    };
  }
  Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

fn local_error(_: std::io::Error) -> ApiError {
  ApiError::new(500, "cannot inspect project local storage")
}
