use std::path::Path;

use serde_json::Value;

use super::super::fs;
use super::Result;

/// Publish initialized directories without ever replacing an existing path.
pub(super) fn publish_directory(from: &Path, to: &Path) -> std::io::Result<()> {
  #[cfg(any(target_os = "linux", target_os = "macos"))]
  {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    #[cfg(target_os = "linux")]
    let result = unsafe {
      libc::syscall(
        libc::SYS_renameat2,
        libc::AT_FDCWD,
        from.as_ptr(),
        libc::AT_FDCWD,
        to.as_ptr(),
        libc::RENAME_NOREPLACE,
      )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
      Ok(())
    } else {
      Err(std::io::Error::last_os_error())
    }
  }
  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  {
    let _ = (from, to);
    Err(std::io::Error::new(
      std::io::ErrorKind::Unsupported,
      "atomic download directory initialization requires Linux or macOS",
    ))
  }
}

/// The caller holds the staging initialization lease or the active transfer lease.
pub(super) fn initialize(
  directory: &Path,
  owner_filename: &str,
  owner: &Value,
  state: Option<&Value>,
) -> Result<()> {
  let parent = directory.parent().expect("download directory has a parent");
  fs::directories(parent)?;
  if fs::inspect(directory)?.is_some() {
    return Ok(());
  }
  let mut builder = tempfile::Builder::new();
  builder.prefix(".service-init-");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    builder.permissions(std::fs::Permissions::from_mode(0o700));
  }
  let temporary = builder.tempdir_in(parent)?;
  checkpoint(directory, "temporary_created");
  fs::atomic_json(&temporary.path().join(owner_filename), owner)?;
  checkpoint(directory, "owner_synced");
  if let Some(state) = state {
    fs::atomic_json(&temporary.path().join("state.json"), state)?;
    checkpoint(directory, "state_synced");
  }
  fs::sync_directory(temporary.path())?;
  match publish_directory(temporary.path(), directory) {
    Ok(()) => fs::sync_directory(parent),
    // A non-cooperating creator may race the lease. The caller validates its
    // ownership; even an empty unowned directory remains untouched.
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
    Err(error) => Err(error.into()),
  }
}

#[cfg(not(test))]
pub(super) fn checkpoint(_directory: &Path, _phase: &str) {}

#[cfg(test)]
pub(super) fn checkpoint(directory: &Path, phase: &str) {
  if std::env::var("EXPRI_TEST_DOWNLOAD_INIT_PHASE").as_deref() != Ok(phase)
    || std::env::var_os("EXPRI_TEST_DOWNLOAD_INIT_TARGET").as_deref() != Some(directory.as_os_str())
  {
    return;
  }
  std::fs::write(directory.parent().unwrap().join("ready"), b"ready").unwrap();
  loop {
    std::thread::sleep(std::time::Duration::from_secs(1));
  }
}
