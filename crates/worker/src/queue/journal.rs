//! Durable launch intent; a missing supervisor never authorizes rerunning a job.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use expri_core::resource_queue::{AttemptSummary, JobAssignment};
use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};

const MAX_JOURNAL_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stage {
  Claimed,
  /// No launch intent or supervisor exists; retry after a fresh GPU probe.
  WaitingForGpu,
  Prepared,
  LaunchPending,
  Active,
  Terminal,
  Quarantined,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Journal {
  pub assignment: JobAssignment,
  pub attempt: AttemptSummary,
  pub stage: Stage,
  pub repo_root: Option<PathBuf>,
  pub run_dir: Option<PathBuf>,
  pub message: Option<String>,
}

impl Journal {
  pub fn claimed(assignment: JobAssignment) -> Self {
    Self {
      attempt: assignment.attempt.clone(),
      assignment,
      stage: Stage::Claimed,
      repo_root: None,
      run_dir: None,
      message: None,
    }
  }

  pub fn save(&self, directory: &Path) -> Result<()> {
    write_json(&directory.join("journal.json"), self)
  }

  pub fn load(directory: &Path) -> Result<Self> {
    let journal: Self = read_json(&directory.join("journal.json"))?;
    journal.attempt.identity().validate()?;
    journal.assignment.submission.validate()?;
    if directory.file_name().and_then(|name| name.to_str()) != Some(&journal.attempt.attempt_id)
      || journal.assignment.attempt.attempt_id != journal.attempt.attempt_id
      || journal.assignment.job.job_id != journal.attempt.job_id
    {
      return Err(message(
        "queue journal does not match its attempt directory",
      ));
    }
    if let Some(repo) = &journal.repo_root {
      if repo != &directory.join("source") {
        return Err(message(
          "queue journal source directory is not owned by this attempt",
        ));
      }
      real_directory(repo)?;
    }
    if let Some(run) = &journal.run_dir {
      let repo = journal
        .repo_root
        .as_ref()
        .ok_or_else(|| message("run journal has no source directory"))?;
      let run_id = run
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| message("invalid queue run path"))?;
      if crate::jobs::run_directory(repo, run_id)? != *run {
        return Err(message(
          "queue journal run directory is not owned by this attempt",
        ));
      }
    }
    Ok(journal)
  }
}

pub(super) fn private_directory(path: &Path) -> Result<()> {
  match fs::symlink_metadata(path) {
    Ok(_) => real_directory(path)?,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      let mut builder = fs::DirBuilder::new();
      #[cfg(unix)]
      {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
      }
      builder.create(path)?;
      // Persist the attempt directory's parent entry before any launch journal
      // is acknowledged; syncing only the journal's directory is insufficient.
      if let Some(parent) = path.parent() {
        sync_directory(parent)?;
      }
    }
    Err(error) => return Err(error.into()),
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
  }
  Ok(())
}

pub(super) fn real_directory(path: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message(
      "queue storage directories must be real directories without symlinks",
    ));
  }
  Ok(())
}

pub(super) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
  let bytes = serde_json::to_vec_pretty(value)?;
  if bytes.len() as u64 > MAX_JOURNAL_BYTES {
    return Err(message("queue journal exceeds its metadata limit"));
  }
  let parent = path
    .parent()
    .ok_or_else(|| message("queue journal needs a parent"))?;
  real_directory(parent)?;
  if let Ok(metadata) = fs::symlink_metadata(path)
    && (!metadata.is_file() || metadata.file_type().is_symlink())
  {
    return Err(message("queue journal must be a regular file"));
  }
  let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
  temporary.write_all(&bytes)?;
  temporary.as_file().sync_all()?;
  temporary.persist(path).map_err(|error| error.error)?;
  sync_directory(parent)
}

pub(super) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_JOURNAL_BYTES
  {
    return Err(message("queue journal must be a bounded regular file"));
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let mut bytes = Vec::new();
  options
    .open(path)?
    .take(MAX_JOURNAL_BYTES + 1)
    .read_to_end(&mut bytes)?;
  if bytes.len() as u64 > MAX_JOURNAL_BYTES {
    return Err(message("queue journal exceeds its metadata limit"));
  }
  Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
  File::open(path)?.sync_all()?;
  Ok(())
}
#[cfg(not(unix))]
fn sync_directory(_: &Path) -> Result<()> {
  Ok(())
}
fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}
