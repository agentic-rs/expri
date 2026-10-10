//! Safe access to raw tracking files and recovery of acknowledged prefixes.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use super::{
  ApiError, ApiResult, FileRecord, FileStorage, FileTarget, ObjectStorage, Store, bad, database,
  document_path, stream_path, target_json, validate_scope,
};

pub(super) fn recover(directory: &Path, connection: &Connection) -> crate::error::Result<()> {
  let mut statement = connection
    .prepare("SELECT target,revision,size FROM tracking_versions")
    .map_err(|_| {
      crate::error::ExpriError::Message("cannot inspect tracking recovery state".into())
    })?;
  let records = statement
    .query_map([], |row| {
      Ok((
        row.get::<_, String>(0)?,
        row.get::<_, u64>(1)?,
        row.get::<_, u64>(2)?,
      ))
    })
    .map_err(|_| {
      crate::error::ExpriError::Message("cannot inspect tracking recovery state".into())
    })?;
  for record in records {
    let (raw, revision, size) = record.map_err(|_| {
      crate::error::ExpriError::Message("cannot inspect tracking recovery state".into())
    })?;
    let target: FileTarget = serde_json::from_str(&raw)?;
    let file = open_raw_options(directory, &target, revision, false, true).map_err(|_| {
      crate::error::ExpriError::Message("acknowledged tracking file is unavailable".into())
    })?;
    let length = file.metadata()?.len();
    if length < size {
      return Err(crate::error::ExpriError::Message(
        "acknowledged tracking bytes are missing".into(),
      ));
    }
    if length > size {
      file.set_len(size)?;
      file.sync_all()?;
    }
  }
  Ok(())
}

pub(super) fn io_error(_: std::io::Error) -> ApiError {
  ApiError::new(500, "tracking storage operation failed")
}

pub(super) fn raw_path(directory: &Path, target: &FileTarget, revision: u64) -> ApiResult<PathBuf> {
  let FileTarget::Run { scope, path } = target else {
    return Err(ApiError::new(400, "tracking files belong to runs"));
  };
  validate_scope(scope).map_err(bad)?;
  let document = document_path(path);
  if !document && !stream_path(path) {
    return Err(ApiError::new(400, "invalid tracking path"));
  }
  let base = directory
    .join("tracking")
    .join(&scope.project_id)
    .join(&scope.origin)
    .join(&scope.run_id);
  Ok(if document {
    base.join("documents").join(path).join(revision.to_string())
  } else {
    base.join("streams").join(path)
  })
}

pub(super) fn open_raw(
  directory: &Path,
  target: &FileTarget,
  revision: u64,
  create: bool,
) -> ApiResult<File> {
  open_raw_options(directory, target, revision, create, create)
}

fn open_raw_options(
  directory: &Path,
  target: &FileTarget,
  revision: u64,
  create: bool,
  writable: bool,
) -> ApiResult<File> {
  use std::os::fd::{AsRawFd, FromRawFd};
  use std::os::unix::fs::OpenOptionsExt;
  let path = raw_path(directory, target, revision)?;
  let relative = path.strip_prefix(directory).expect("tracking path");
  let mut dir = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
    .open(directory)
    .map_err(io_error)?;
  let parts = relative.components().collect::<Vec<_>>();
  for (index, part) in parts.iter().enumerate() {
    let name = std::ffi::CString::new(part.as_os_str().as_encoded_bytes())
      .map_err(|_| ApiError::new(400, "invalid tracking path"))?;
    let last = index + 1 == parts.len();
    if !last && create {
      let made = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) };
      if made == 0 {
        dir.sync_all().map_err(io_error)?;
      } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
        return Err(io_error(std::io::Error::last_os_error()));
      }
    }
    let flags = if last {
      (if writable {
        libc::O_RDWR
      } else {
        libc::O_RDONLY
      }) | (if create { libc::O_CREAT } else { 0 })
        | libc::O_NOFOLLOW
    } else {
      libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW
    };
    let fd = unsafe {
      libc::openat(
        dir.as_raw_fd(),
        name.as_ptr(),
        flags | libc::O_CLOEXEC,
        0o600,
      )
    };
    if fd < 0 {
      return Err(io_error(std::io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if last {
      use std::os::unix::fs::MetadataExt;
      let metadata = file.metadata().map_err(io_error)?;
      if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(ApiError::new(500, "tracking bytes must be a regular file"));
      }
      if create {
        dir.sync_all().map_err(io_error)?;
      }
      return Ok(file);
    }
    dir = file;
  }
  unreachable!("tracking file has a name")
}

pub(super) fn append_raw(file: &mut File, size: u64, offset: u64, bytes: &[u8]) -> ApiResult<u64> {
  let end = offset
    .checked_add(bytes.len() as u64)
    .filter(|end| *end <= i64::MAX as u64)
    .ok_or_else(|| ApiError::new(413, "tracking offset exceeds its limit"))?;
  if offset > size {
    return Err(ApiError::new(
      409,
      "tracking offset or repeated bytes conflict",
    ));
  }
  if file.metadata().map_err(io_error)?.len() < size {
    return Err(ApiError::new(
      500,
      "acknowledged tracking bytes are missing",
    ));
  }
  file.set_len(size).map_err(io_error)?;
  let overlap = (size - offset).min(bytes.len() as u64) as usize;
  if overlap != 0 {
    file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
    let mut previous = vec![0; overlap];
    file.read_exact(&mut previous).map_err(io_error)?;
    if previous != bytes[..overlap] {
      return Err(ApiError::new(
        409,
        "tracking offset or repeated bytes conflict",
      ));
    }
  }
  if end > size {
    file.seek(SeekFrom::Start(size)).map_err(io_error)?;
    file.write_all(&bytes[overlap..]).map_err(io_error)?;
  }
  file.sync_all().map_err(io_error)?;
  Ok(size.max(end))
}

impl<S: ObjectStorage> Store<S> {
  pub(in crate::service) fn tracking_open(&self, record: &FileRecord) -> ApiResult<File> {
    super::run_retention::ensure_target_available(&*self.db()?, &record.target)?;
    let FileStorage::Tracking { revision, .. } = record.storage else {
      return Err(ApiError::new(400, "file is not in tracking storage"));
    };
    let row: Option<(u64, bool)> = self
      .db()?
      .query_row(
        "SELECT size,complete FROM tracking_versions WHERE target=?1 AND revision=?2",
        params![target_json(&record.target)?, revision],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .optional()
      .map_err(database)?;
    if row.is_none_or(|(size, complete)| size < record.size || !complete) {
      return Err(ApiError::new(409, "tracking revision is unavailable"));
    }
    let file = open_raw(&self.directory, &record.target, revision, false)?;
    if file.metadata().map_err(io_error)?.len() < record.size {
      return Err(ApiError::new(
        500,
        "acknowledged tracking bytes are missing",
      ));
    }
    Ok(file)
  }

  pub(in crate::service) fn tracking_range(
    &self,
    record: &FileRecord,
    offset: u64,
    length: usize,
  ) -> ApiResult<Vec<u8>> {
    if length > 16 * 1024 * 1024
      || offset
        .checked_add(length as u64)
        .is_none_or(|end| end > record.size)
    {
      return Err(ApiError::new(
        413,
        "tracking read exceeds its recorded extent",
      ));
    }
    let mut file = self.tracking_open(record)?;
    file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes).map_err(io_error)?;
    Ok(bytes)
  }
}
