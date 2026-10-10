use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::{METADATA, OBJECT_BATCH, STREAMS, fs};
use super::{FileRecord, FileStorage, FileTarget, Result, message, validate_target};
use crate::lock::{FileLock, LockAttempt};

const STATE_LIMIT: u64 = 256 * 1024;
const FILE_LIMIT: usize = 64 + METADATA.len() + STREAMS.len();

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedFile {
  record: FileRecord,
  offset: u64,
  verified: bool,
  #[serde(default)]
  modified: Option<(u64, u32)>,
}

fn modified(metadata: &std::fs::Metadata) -> Option<(u64, u32)> {
  metadata
    .modified()
    .ok()?
    .duration_since(std::time::UNIX_EPOCH)
    .ok()
    .map(|time| (time.as_secs(), time.subsec_nanos()))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
  schema_version: u32,
  files: BTreeMap<String, SavedFile>,
}

pub(super) struct Staging {
  pub(super) directory: PathBuf,
  state: State,
  _lease: FileLock,
}

fn private(path: &Path, directory: bool) -> Result<()> {
  if directory {
    fs::directory(path)?;
  } else {
    fs::optional_regular(path)?;
  }
  let metadata = std::fs::symlink_metadata(path)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
      return Err(message(
        "download staging must be private and owned by the current user",
      ));
    }
  }
  if !directory && !metadata.is_file() {
    return Err(message("download staging file must be regular"));
  }
  Ok(())
}

fn record_matches(first: &FileRecord, second: &FileRecord) -> bool {
  first.target == second.target
    && first.sha256 == second.sha256
    && match (&first.storage, &second.storage) {
      (FileStorage::Object, FileStorage::Object) | (FileStorage::Stream, FileStorage::Stream) => {
        first.size == second.size
      }
      (
        FileStorage::Tracking {
          revision: old,
          sealed,
        },
        FileStorage::Tracking { revision: new, .. },
      ) => {
        let FileTarget::Run { path, .. } = &first.target else {
          return false;
        };
        old == new
          && if super::stream_path(path) {
            first.size <= second.size && (!sealed || first.size == second.size)
          } else {
            first.size == second.size
          }
      }
      _ => false,
    }
}

pub(super) fn verified_source(
  record: &FileRecord,
  source: &Path,
) -> Result<Option<std::fs::Metadata>> {
  verified_source_with_cancel(record, source, &mut || Ok(false))
}

pub(super) fn verified_source_with_cancel(
  record: &FileRecord,
  source: &Path,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<Option<std::fs::Metadata>> {
  fs::check_cancelled(cancelled)?;
  if !matches!(record.storage, FileStorage::Object) || fs::inspect(source)?.is_none() {
    return Ok(None);
  }
  let mut input = fs::open(source)?;
  let initial = input.metadata()?;
  if initial.len() != record.size {
    return Ok(None);
  }
  let digest = fs::digest_with_cancel(&mut input, record.size, cancelled)?;
  if !fs::unchanged(&initial, &input.metadata()?)
    || !fs::unchanged(&initial, &std::fs::symlink_metadata(source)?)
  {
    return Err(message(
      "local download changed while verifying its contents",
    ));
  }
  Ok((Some(digest.as_str()) == record.sha256.as_deref()).then_some(initial))
}

impl Staging {
  pub(super) fn open(directory: PathBuf, owner: &Value) -> Result<Self> {
    let parent = directory.parent().expect("staging has a parent");
    fs::directories(parent)?;
    let LockAttempt::Acquired(_initialization) =
      crate::lock::try_lock_file(&parent.join(".pull-init.lock"), true)?
    else {
      return Err(crate::error::ExpriError::DownloadBusy { initializing: true });
    };
    super::initialization::initialize(
      &directory,
      "owner.json",
      owner,
      Some(&serde_json::json!({"schema_version":1,"files":{}})),
    )?;
    private(&directory, true)?;
    let owner_path = directory.join("owner.json");
    private(&owner_path, false)
      .map_err(|_| message("refusing to reuse unowned or unsafe download staging"))?;
    let actual: Value = serde_json::from_slice(&fs::read_bounded(&owner_path, 16 * 1024)?)?;
    if actual != *owner {
      return Err(message(
        "download staging belongs to another service source or run",
      ));
    }
    let LockAttempt::Acquired(lease) =
      crate::lock::try_lock_file(&directory.join(".pull.lock"), true)?
    else {
      return Err(crate::error::ExpriError::DownloadBusy {
        initializing: false,
      });
    };
    let state_path = directory.join("state.json");
    private(&state_path, false)
      .map_err(|_| message("download staging has no safe progress record"))?;
    let state = serde_json::from_slice::<State>(&fs::read_bounded(&state_path, STATE_LIMIT)?)?;
    if state.schema_version != 1 || state.files.len() > FILE_LIMIT {
      return Err(message("invalid download progress record"));
    }
    for (path, saved) in &state.files {
      validate_target(&saved.record.target)?;
      let owned = match &saved.record.target {
        FileTarget::Run {
          scope,
          path: returned,
        } => returned == path && serde_json::to_value(scope)? == owner["scope"],
        FileTarget::Input { .. } => {
          path == "input" && serde_json::to_value(&saved.record.target)? == owner["target"]
        }
      };
      if !owned
        || saved.offset > saved.record.size
        || (saved.verified && saved.offset != saved.record.size)
        || (matches!(saved.record.storage, FileStorage::Object)
          && saved.offset != saved.record.size
          && !saved.offset.is_multiple_of(OBJECT_BATCH))
      {
        return Err(message(
          "download progress contains an inconsistent file record",
        ));
      }
      super::validate_record(&saved.record)?;
    }
    let staging = Self {
      directory,
      state,
      _lease: lease,
    };
    fs::directories(&staging.directory.join("files"))?;
    staging.save()?;
    Ok(staging)
  }

  fn save(&self) -> Result<()> {
    if serde_json::to_vec_pretty(&self.state)?.len() as u64 + 1 > STATE_LIMIT {
      return Err(message("download progress exceeds its size limit"));
    }
    fs::atomic_json(&self.directory.join("state.json"), &self.state)
  }

  pub(super) fn path(&self, path: &str) -> PathBuf {
    self.directory.join("files").join(path)
  }

  pub(super) fn verified(&self, path: &str) -> bool {
    self
      .state
      .files
      .get(path)
      .is_some_and(|saved| saved.verified)
  }

  pub(super) fn ensure_verified(&self, path: &str) -> Result<()> {
    let saved = self
      .state
      .files
      .get(path)
      .ok_or_else(|| message("download has no saved receipt"))?;
    private(&self.path(path), false)?;
    let metadata = std::fs::symlink_metadata(self.path(path))?;
    if !saved.verified
      || metadata.len() != saved.record.size
      || modified(&metadata) != saved.modified
    {
      return Err(message(
        "verified download changed before publication; previous files were retained",
      ));
    }
    Ok(())
  }

  /// Reuse a matching local object without sending its bytes over the network.
  /// The caller must hold the destination lease while this shared inode is used.
  pub(super) fn reuse(&mut self, path: &str, record: &FileRecord, source: &Path) -> Result<bool> {
    #[cfg(unix)]
    if let Some(metadata) = fs::inspect(source)? {
      use std::os::unix::fs::MetadataExt;
      if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        // Staging is private; an existing public file cannot be its shared inode.
        return Ok(false);
      }
    }
    let Some(initial) = verified_source(record, source)? else {
      return Ok(false);
    };
    self.state.files.insert(
      path.into(),
      SavedFile {
        record: record.clone(),
        offset: 0,
        verified: false,
        modified: None,
      },
    );
    self.save()?;
    self.remove_data(path)?;
    let data_path = self.path(path);
    fs::directories(data_path.parent().unwrap())?;
    std::fs::hard_link(source, &data_path)?;
    fs::sync_directory(data_path.parent().unwrap())?;
    let output = fs::open(&data_path)?;
    if !fs::unchanged(&initial, &output.metadata()?) {
      self.reset(path)?;
      return Err(message("local download changed while saving its receipt"));
    }
    self.acknowledge(path, record.size, true, &output)?;
    Ok(true)
  }

  pub(super) fn select(&mut self, selected: &BTreeMap<String, FileRecord>) -> Result<()> {
    if selected.len() > FILE_LIMIT {
      return Err(message("too many selected download files"));
    }
    let removed = self
      .state
      .files
      .keys()
      .filter(|path| !selected.contains_key(*path))
      .cloned()
      .collect::<Vec<_>>();
    for path in &removed {
      self.state.files.remove(path);
    }
    self.save()?;
    super::initialization::checkpoint(&self.directory, "selection_pruned");
    for path in removed {
      self.remove_data(&path)?;
    }
    Ok(())
  }

  fn remove_data(&self, path: &str) -> Result<()> {
    let path = self.path(path);
    if fs::inspect(&path)?.is_some() {
      private(&path, false)?;
      std::fs::remove_file(&path)?;
      fs::sync_directory(path.parent().unwrap())?;
    }
    Ok(())
  }

  pub(super) fn prepare(
    &mut self,
    path: &str,
    record: &FileRecord,
    resumable: bool,
  ) -> Result<(File, u64)> {
    let reuse = resumable
      && self
        .state
        .files
        .get(path)
        .is_some_and(|saved| record_matches(&saved.record, record));
    if !reuse {
      // Unlink rather than truncate: a previous publication may share this inode.
      self.state.files.insert(
        path.into(),
        SavedFile {
          record: record.clone(),
          offset: 0,
          verified: false,
          modified: None,
        },
      );
      self.save()?;
      super::initialization::checkpoint(&self.directory, "record_reset");
      self.remove_data(path)?;
    } else {
      let saved = self.state.files.get_mut(path).unwrap();
      saved.record = record.clone();
      saved.verified &= saved.offset == record.size;
      self.save()?;
    }
    let offset = self.state.files[path].offset;
    let data_path = self.path(path);
    fs::directories(data_path.parent().unwrap())?;
    let file = if fs::inspect(&data_path)?.is_none() {
      if offset > 0 || self.state.files[path].verified {
        return Err(message("acknowledged download data is missing"));
      }
      let mut options = OpenOptions::new();
      options.read(true).write(true).create_new(true);
      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
      }
      let file = options.open(&data_path)?;
      file.sync_all()?;
      fs::sync_directory(data_path.parent().unwrap())?;
      file
    } else {
      private(&data_path, false)?;
      let initial = std::fs::symlink_metadata(&data_path)?;
      let mut options = OpenOptions::new();
      options.read(true).write(true);
      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
      }
      let file = options.open(&data_path)?;
      let current = std::fs::symlink_metadata(&data_path)?;
      if !fs::same_identity(&initial, &current) || !fs::same_identity(&file.metadata()?, &current) {
        return Err(message("download staging changed while opening"));
      }
      private(&data_path, false)?;
      file
    };
    let metadata = file.metadata()?;
    if metadata.len() < offset {
      return Err(message("acknowledged download data is truncated"));
    }
    if matches!(record.storage, FileStorage::Tracking { .. })
      && metadata.len() == offset
      && self.state.files[path].modified.is_some()
      && self.state.files[path].modified != modified(&metadata)
    {
      // Tracking records need no remote hash, but an edited cached hardlink must
      // not be mistaken for the acknowledged server prefix.
      drop(file);
      self.reset(path)?;
      return self.prepare(path, record, resumable);
    }
    #[cfg(unix)]
    {
      use std::os::unix::fs::MetadataExt;
      if metadata.nlink() > 1
        && (!self.state.files[path].verified || offset < record.size || metadata.len() != offset)
      {
        if matches!(record.storage, FileStorage::Tracking { .. }) {
          // The previous cache remains visible while its acknowledged prefix is
          // copied to a private inode. Only the new inode receives appended bytes.
          let mut source = file;
          let mut temporary = tempfile::NamedTempFile::new_in(data_path.parent().unwrap())?;
          source.seek(SeekFrom::Start(0))?;
          let copied = std::io::copy(&mut source.by_ref().take(offset), &mut temporary)?;
          if copied != offset || !fs::unchanged(&metadata, &source.metadata()?) {
            return Err(message("cached tracking prefix changed while detaching"));
          }
          temporary.as_file().sync_all()?;
          private(&data_path, false)?;
          let detached = temporary.persist(&data_path).map_err(|error| error.error)?;
          fs::sync_directory(data_path.parent().unwrap())?;
          self.acknowledge(path, offset, false, &detached)?;
          return self.prepare(path, record, resumable);
        }
        // A cache publication shares only complete verified data. Never trim or
        // extend a shared inode if an interrupted reset left partial progress.
        drop(file);
        self.reset(path)?;
        return self.prepare(path, record, resumable);
      }
    }
    let mut file = file;
    if metadata.len() != offset {
      file.set_len(offset)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    Ok((file, offset))
  }

  pub(super) fn acknowledge(
    &mut self,
    path: &str,
    offset: u64,
    verified: bool,
    file: &File,
  ) -> Result<()> {
    file.sync_all()?;
    let saved = self
      .state
      .files
      .get_mut(path)
      .expect("prepared file has a record");
    saved.offset = offset;
    saved.verified = verified;
    saved.modified = modified(&file.metadata()?);
    self.save()
  }

  pub(super) fn reset(&mut self, path: &str) -> Result<()> {
    let saved = self
      .state
      .files
      .get_mut(path)
      .expect("prepared file has a record");
    saved.offset = 0;
    saved.verified = false;
    saved.modified = None;
    self.save()?;
    self.remove_data(path)
  }

  pub(super) fn clear(&mut self) -> Result<()> {
    let paths = self.state.files.keys().cloned().collect::<Vec<_>>();
    self.state.files.clear();
    self.save()?;
    for path in paths {
      self.remove_data(&path)?;
    }
    Ok(())
  }

  pub(super) fn retain_tracking(&mut self) -> Result<()> {
    if !self
      .state
      .files
      .values()
      .any(|saved| matches!(saved.record.storage, FileStorage::Tracking { .. }))
    {
      return self.clear();
    }
    let removed = self
      .state
      .files
      .iter()
      .filter(|(_, saved)| !matches!(saved.record.storage, FileStorage::Tracking { .. }))
      .map(|(path, _)| path.clone())
      .collect::<Vec<_>>();
    for path in &removed {
      self.state.files.remove(path);
    }
    self.save()?;
    for path in removed {
      self.remove_data(&path)?;
    }
    Ok(())
  }
}
