use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::types::{FileTarget, UploadState};
use super::fs::message;
use super::{RECORD_LIMIT, fs};
use crate::error::Result;
use crate::lock::{FileLock, LockAttempt};

#[derive(Serialize, Deserialize)]
pub(super) struct SavedFile {
  pub(super) target: FileTarget,
  pub(super) size: u64,
  pub(super) sha256: String,
  pub(super) snapshot: Option<String>,
  pub(super) upload: UploadState,
}

#[derive(Serialize, Deserialize)]
pub(super) struct QueueState {
  schema_version: u32,
  owner: Value,
  pub(super) files: BTreeMap<String, SavedFile>,
  pub(super) streams: BTreeMap<String, u64>,
}

pub(super) struct Queue {
  pub(super) directory: PathBuf,
  pub(super) state: QueueState,
  _lease: FileLock,
}

impl Queue {
  pub(super) fn new(directory: PathBuf, owner: Value) -> Result<Self> {
    fs::directories(&directory)?;
    let LockAttempt::Acquired(lease) =
      crate::lock::try_lock_file(&directory.join(".sync.lock"), true)?
    else {
      return Err(message("another service sync is using this queue"));
    };
    let path = directory.join("queue.json");
    let state = if fs::inspect(&path)?.is_some() {
      let state: QueueState = serde_json::from_slice(&fs::read_bounded(&path, RECORD_LIMIT)?)?;
      if state.schema_version != 1 || state.owner != owner {
        return Err(message(
          "service sync queue belongs to another endpoint or run",
        ));
      }
      state
    } else {
      QueueState {
        schema_version: 1,
        owner,
        files: BTreeMap::new(),
        streams: BTreeMap::new(),
      }
    };
    let queue = Self {
      directory,
      state,
      _lease: lease,
    };
    queue.save()?;
    Ok(queue)
  }

  pub(super) fn save(&self) -> Result<()> {
    fs::atomic_json(&self.directory.join("queue.json"), &self.state)
  }

  pub(super) fn allocate(&self) -> Result<tempfile::NamedTempFile> {
    Ok(
      tempfile::Builder::new()
        .prefix("upload-")
        .rand_bytes(24)
        .tempfile_in(&self.directory)?,
    )
  }
}
