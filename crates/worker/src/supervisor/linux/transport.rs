use super::{CAPABILITY, MAX_BINARY_BYTES, REQUEST_LIMIT, REQUIRED_SEALS, message};
use expri_core::error::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Descriptor {
  pub(super) fd: i32,
  pub(super) dev: u64,
  pub(super) ino: u64,
}

impl Descriptor {
  pub(super) fn from_file(file: &impl AsRawFd) -> io::Result<Self> {
    let fd = file.as_raw_fd();
    let metadata = descriptor_metadata(fd)?;
    Ok(Self {
      fd,
      dev: metadata.st_dev,
      ino: metadata.st_ino,
    })
  }

  pub(super) fn validate(&self, mode: libc::mode_t) -> io::Result<()> {
    let metadata = descriptor_metadata(self.fd)?;
    if self.fd < 3
      || metadata.st_dev != self.dev
      || metadata.st_ino != self.ino
      || metadata.st_mode & libc::S_IFMT != mode
    {
      return Err(io::Error::other(
        "inherited supervisor descriptor has changed identity",
      ));
    }
    set_cloexec(self.fd, true)
  }
}

pub(super) fn pinned_target(path: &Path) -> Result<(File, String)> {
  let path = fs::canonicalize(path)?;
  let mut source = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
    .open(path)?;
  let metadata = source.metadata()?;
  if !metadata.is_file()
    || metadata.permissions().mode() & 0o111 == 0
    || metadata.len() > MAX_BINARY_BYTES
  {
    return Err(message(
      "supervisor target must be a regular executable no larger than 256 MiB",
    ));
  }
  let mut bytes = Vec::new();
  Read::by_ref(&mut source)
    .take(MAX_BINARY_BYTES + 1)
    .read_to_end(&mut bytes)?;
  if bytes.len() as u64 > MAX_BINARY_BYTES || !bytes.starts_with(b"\x7fELF") {
    return Err(message(
      "supervisor target must be a native Linux ELF executable",
    ));
  }
  let digest = hex_digest(&bytes);
  Ok((sealed_memfd("expri-supervisor-executable", &bytes)?, digest))
}

pub(super) fn preflight(target: &File) -> Result<()> {
  let path = format!("/proc/self/fd/{}", target.as_raw_fd());
  let mut output = tempfile::tempfile()?;
  let mut child = Command::new(path)
    .args(["node", "capabilities"])
    .stdin(Stdio::null())
    .stdout(output.try_clone()?)
    .stderr(Stdio::null())
    .spawn()?;
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    if let Some(status) = child.try_wait()? {
      if !status.success() {
        return Err(message(
          "target executable failed its supervisor capability check",
        ));
      }
      output.seek(SeekFrom::Start(0))?;
      let mut bytes = Vec::new();
      output.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
      let supported = bytes.len() <= 64 * 1024
        && serde_json::from_slice::<Value>(&bytes)
          .ok()
          .and_then(|value| {
            value["capabilities"].as_array().map(|capabilities| {
              capabilities
                .iter()
                .any(|capability| capability.as_str() == Some(CAPABILITY))
            })
          })
          .unwrap_or(false);
      return if supported {
        Ok(())
      } else {
        Err(message(
          "target executable does not advertise the live supervisor handoff protocol in its capability response",
        ))
      };
    }
    if Instant::now() >= deadline || output.metadata()?.len() > 64 * 1024 {
      let _ = child.kill();
      let _ = child.wait();
      return Err(message(
        "supervisor target capability check timed out or exceeded its response limit",
      ));
    }
    thread::sleep(Duration::from_millis(25));
  }
}

pub(super) fn exec_target(target: &File, handoff_fd: i32) -> Result<()> {
  let arguments = [
    cstring(b"expri")?,
    cstring(b"node")?,
    cstring(b"resume-run-worker")?,
    cstring(b"--handoff-fd")?,
    cstring(handoff_fd.to_string().as_bytes())?,
  ];
  let mut argv: Vec<_> = arguments.iter().map(|arg| arg.as_ptr()).collect();
  argv.push(std::ptr::null());
  let environment: Vec<_> = std::env::vars_os()
    .map(|(key, value)| {
      let mut bytes = key.as_bytes().to_vec();
      bytes.push(b'=');
      bytes.extend_from_slice(value.as_bytes());
      CString::new(bytes)
    })
    .collect::<std::result::Result<_, _>>()
    .map_err(io::Error::other)?;
  let mut envp: Vec<_> = environment.iter().map(|item| item.as_ptr()).collect();
  envp.push(std::ptr::null());
  // The sealed ELF checked above is exactly the image installed by this exec.
  unsafe {
    libc::fexecve(target.as_raw_fd(), argv.as_ptr(), envp.as_ptr());
  }
  Err(io::Error::last_os_error().into())
}

pub(super) fn sealed_memfd(name: &str, bytes: &[u8]) -> Result<File> {
  let name = cstring(name.as_bytes())?;
  let fd =
    unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
  if fd < 0 {
    return Err(io::Error::last_os_error().into());
  }
  let mut file = unsafe { File::from_raw_fd(fd) };
  file.write_all(bytes)?;
  file.seek(SeekFrom::Start(0))?;
  if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, REQUIRED_SEALS) } < 0 {
    return Err(io::Error::last_os_error().into());
  }
  Ok(file)
}

pub(super) fn set_cloexec(fd: i32, value: bool) -> io::Result<()> {
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
  if flags < 0
    || unsafe {
      libc::fcntl(
        fd,
        libc::F_SETFD,
        if value {
          flags | libc::FD_CLOEXEC
        } else {
          flags & !libc::FD_CLOEXEC
        },
      )
    } < 0
  {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

pub(super) struct Inheritance(Vec<(i32, i32)>);

impl Inheritance {
  pub(super) fn new(descriptors: &[i32]) -> io::Result<Self> {
    let mut inherited = Self(Vec::new());
    for &fd in descriptors {
      let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
      if flags < 0 {
        return Err(io::Error::last_os_error());
      }
      inherited.0.push((fd, flags));
      set_cloexec(fd, false)?;
    }
    Ok(inherited)
  }
}

impl Drop for Inheritance {
  fn drop(&mut self) {
    for &(fd, flags) in &self.0 {
      unsafe {
        libc::fcntl(fd, libc::F_SETFD, flags);
      }
    }
  }
}

pub(super) fn cstring(bytes: &[u8]) -> io::Result<CString> {
  CString::new(bytes).map_err(io::Error::other)
}

pub(super) fn descriptor_metadata(fd: i32) -> io::Result<libc::stat> {
  let mut stat = unsafe { std::mem::zeroed() };
  if unsafe { libc::fstat(fd, &mut stat) } < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(stat)
}

pub(super) fn validate_named_descriptor(descriptor: &Descriptor, path: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_file()
    || metadata.file_type().is_symlink()
    || metadata.dev() != descriptor.dev
    || metadata.ino() != descriptor.ino
  {
    return Err(message("inherited task log no longer matches its run"));
  }
  Ok(())
}

pub(super) fn process_start(pid: u32) -> io::Result<u64> {
  let bytes = fs::read(format!("/proc/{pid}/stat"))?;
  let close = bytes
    .iter()
    .rposition(|byte| *byte == b')')
    .ok_or_else(|| io::Error::other("invalid process identity"))?;
  let suffix = std::str::from_utf8(&bytes[close + 1..]).map_err(io::Error::other)?;
  suffix
    .split_whitespace()
    .nth(19)
    .and_then(|field| field.parse().ok())
    .ok_or_else(|| io::Error::other("missing process start identity"))
}

pub(super) fn cancel_requested(run_dir: &Path) -> io::Result<bool> {
  match fs::symlink_metadata(run_dir.join(".cancel-request")) {
    Ok(metadata)
      if metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == 0 =>
    {
      Ok(true)
    }
    Ok(_) => Err(io::Error::other(
      "cancellation request must be an empty regular file",
    )),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(error),
  }
}

pub(super) fn read_json(path: &Path, limit: usize) -> io::Result<Value> {
  let mut file = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
    .open(path)?;
  if !file.metadata()?.is_file() {
    return Err(io::Error::other("supervisor record must be a regular file"));
  }
  let mut bytes = Vec::new();
  Read::by_ref(&mut file)
    .take((limit + 1) as u64)
    .read_to_end(&mut bytes)?;
  if bytes.len() > limit {
    return Err(io::Error::other(
      "supervisor record exceeds its metadata limit",
    ));
  }
  serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub(super) fn write_json(path: &Path, value: &Value) -> Result<()> {
  if let Ok(metadata) = fs::symlink_metadata(path)
    && (!metadata.is_file() || metadata.file_type().is_symlink())
  {
    return Err(message("supervisor request must be a regular file"));
  }
  let parent = path
    .parent()
    .ok_or_else(|| message("supervisor request needs a run directory"))?;
  let bytes = serde_json::to_vec(value)?;
  if bytes.len() > REQUEST_LIMIT {
    return Err(message("supervisor request exceeds its size limit"));
  }
  let mut file = tempfile::NamedTempFile::new_in(parent)?;
  file.write_all(&bytes)?;
  file.as_file().sync_all()?;
  file.persist(path).map_err(|error| error.error)?;
  File::open(parent)?.sync_all()?;
  Ok(())
}

pub(super) fn hex_digest(bytes: &[u8]) -> String {
  Sha256::digest(bytes)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}
