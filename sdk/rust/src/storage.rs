use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Result, invalid};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Directory(File);

impl Directory {
  pub fn open(path: &Path, create: bool) -> Result<Self> {
    if create {
      std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    }
    let file = OpenOptions::new()
      .read(true)
      .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
      .open(path)?;
    Ok(Self(file))
  }

  pub fn child(&self, name: &str) -> Result<Self> {
    let name = c_name(name)?;
    // SAFETY: name is NUL terminated and the directory descriptor is live.
    if unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
      let error = io::Error::last_os_error();
      if error.kind() != io::ErrorKind::AlreadyExists {
        return Err(error.into());
      }
    } else {
      self.0.sync_all()?;
    }
    let descriptor = unsafe {
      libc::openat(
        self.0.as_raw_fd(),
        name.as_ptr(),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
      )
    };
    if descriptor < 0 {
      return Err(io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a newly owned descriptor.
    Ok(Self(unsafe { File::from_raw_fd(descriptor) }))
  }

  pub fn file(&self, name: &str, flags: i32, missing: bool) -> Result<Option<File>> {
    let name = c_name(name)?;
    let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let create_or_open = flags & libc::O_CREAT != 0 && flags & libc::O_EXCL == 0;
    let mut descriptor = unsafe {
      libc::openat(
        self.0.as_raw_fd(),
        name.as_ptr(),
        flags | if create_or_open { libc::O_EXCL } else { 0 },
        0o600,
      )
    };
    if descriptor < 0
      && create_or_open
      && io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists
    {
      descriptor = unsafe {
        libc::openat(
          self.0.as_raw_fd(),
          name.as_ptr(),
          flags & !libc::O_CREAT,
          0o600,
        )
      };
    }
    if descriptor < 0 {
      let error = io::Error::last_os_error();
      if missing && error.kind() == io::ErrorKind::NotFound {
        return Ok(None);
      }
      return Err(error.into());
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if !file.metadata()?.is_file() {
      return Err(invalid(
        "SDK output must be a regular file, without a symlink",
      ));
    }
    Ok(Some(file))
  }

  pub fn read(&self, name: &str, limit: usize) -> Result<Option<Vec<u8>>> {
    let Some(file) = self.file(name, libc::O_RDONLY, true)? else {
      return Ok(None);
    };
    if file.metadata()?.len() > limit as u64 {
      return Err(invalid(format!("{name} exceeds its metadata limit")));
    }
    let mut raw = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut raw)?;
    if raw.len() > limit {
      return Err(invalid(format!("{name} exceeds its metadata limit")));
    }
    Ok(Some(raw))
  }

  /// Publish complete bytes, either atomically replacing a locked journal or
  /// creating immutable parameters without overwriting another writer.
  pub fn publish(&self, target: &str, raw: &[u8], replace: bool) -> Result<bool> {
    let (name, mut file) = self.temporary()?;
    let operation = (|| {
      file.write_all(raw)?;
      file.sync_all()?;
      let source = c_name(&name)?;
      let destination = c_name(target)?;
      let result = if replace {
        unsafe {
          libc::renameat(
            self.0.as_raw_fd(),
            source.as_ptr(),
            self.0.as_raw_fd(),
            destination.as_ptr(),
          )
        }
      } else {
        unsafe {
          libc::linkat(
            self.0.as_raw_fd(),
            source.as_ptr(),
            self.0.as_raw_fd(),
            destination.as_ptr(),
            0,
          )
        }
      };
      if result < 0 {
        let error = io::Error::last_os_error();
        if !replace && error.kind() == io::ErrorKind::AlreadyExists {
          return Ok(false);
        }
        return Err(error.into());
      }
      self.0.sync_all()?;
      Ok(true)
    })();
    // Removing a renamed source is harmless; preserve the actual write error.
    let source = c_name(&name)?;
    unsafe { libc::unlinkat(self.0.as_raw_fd(), source.as_ptr(), 0) };
    operation
  }

  fn temporary(&self) -> Result<(String, File)> {
    for _ in 0..64 {
      let counter = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
      let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| invalid("system clock predates the Unix epoch"))?
        .as_nanos();
      let name = format!(".sdk-{}-{time}-{counter}.tmp", std::process::id());
      match self.file(&name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, false) {
        Ok(Some(file)) => return Ok((name, file)),
        Err(crate::Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => continue,
        Err(error) => return Err(error),
        Ok(None) => unreachable!(),
      }
    }
    Err(invalid("cannot create an exclusive SDK metadata file"))
  }
}

fn c_name(name: &str) -> Result<CString> {
  if name.is_empty() || name.contains('/') || matches!(name, "." | "..") {
    return Err(invalid(
      "SDK metadata filename must be one safe path component",
    ));
  }
  CString::new(name).map_err(|_| invalid("SDK filename contains a NUL byte"))
}

pub(crate) struct Lease<'a>(&'a File);

impl<'a> Lease<'a> {
  pub fn exclusive(file: &'a File) -> Result<Self> {
    loop {
      if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        return Ok(Self(file));
      }
      let error = io::Error::last_os_error();
      if error.kind() != io::ErrorKind::Interrupted {
        return Err(error.into());
      }
    }
  }
}

impl Drop for Lease<'_> {
  fn drop(&mut self) {
    unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
  }
}
