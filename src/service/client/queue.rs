use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::types::{ArchiveRecord, FileTarget, UploadState};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Protocol {
  Legacy,
  TrackingV1,
}

#[derive(Serialize, Deserialize)]
pub(super) struct SavedDocument {
  pub(super) revision: u64,
  pub(super) size: u64,
  pub(super) sha256: String,
  pub(super) snapshot: Option<String>,
  pub(super) offset: u64,
  pub(super) complete: bool,
}

#[derive(Serialize, Deserialize)]
pub(super) struct QueueState {
  schema_version: u32,
  owner: Value,
  pub(super) files: BTreeMap<String, SavedFile>,
  pub(super) streams: BTreeMap<String, u64>,
  #[serde(default)]
  pub(super) protocol: Option<Protocol>,
  #[serde(default)]
  pub(super) documents: BTreeMap<String, SavedDocument>,
  #[serde(default)]
  pub(super) archive: Option<ArchiveRecord>,
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
      let mut state = state;
      if state.protocol.is_none() && (!state.files.is_empty() || !state.streams.is_empty()) {
        state.protocol = Some(Protocol::Legacy);
      }
      state
    } else {
      QueueState {
        schema_version: 1,
        owner,
        files: BTreeMap::new(),
        streams: BTreeMap::new(),
        protocol: None,
        documents: BTreeMap::new(),
        archive: None,
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
