use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};

use super::storage::ObjectStorage;
use super::types::*;

const MAX_PARTS: u64 = 1000;
const MAX_PART_SIZE: u64 = 5 * 1024_u64.pow(3);
const MAX_FILE_SIZE: u64 = MAX_PARTS * MAX_PART_SIZE;
const MAX_ETAG_BYTES: usize = 256;

pub(super) type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Debug)]
pub(super) struct ApiError {
  pub status: u16,
  pub message: String,
}

impl ApiError {
  pub fn new(status: u16, message: impl Into<String>) -> Self {
    Self {
      status,
      message: message.into(),
    }
  }
}

fn database(error: rusqlite::Error) -> ApiError {
  let _ = error;
  ApiError::new(500, "service metadata operation failed")
}

fn bad(error: crate::error::ExpriError) -> ApiError {
  ApiError::new(400, error.to_string())
}

#[derive(Clone)]
struct Upload {
  target: FileTarget,
  size: u64,
  sha256: String,
  key: String,
  multipart: Option<String>,
  part_size: u64,
  sequence: i64,
  complete: bool,
}

pub(super) struct Store<S> {
  connection: Mutex<Connection>,
  // Network calls never hold the database lock. This gate prevents simultaneous
  // begin/complete retries from creating conflicting sessions for one upload.
  upload_gates: Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
  storage: S,
  _lease: crate::lock::FileLock,
}

impl<S: ObjectStorage> Store<S> {
  pub fn open(directory: &Path, storage: S) -> crate::error::Result<Self> {
    fs::create_dir_all(directory)?;
    let directory = fs::canonicalize(directory)?;
    let lease = match crate::lock::try_lock_file(&directory.join(".service.lock"), true)? {
      crate::lock::LockAttempt::Acquired(lease) => lease,
      _ => {
        return Err(crate::error::ExpriError::Message(
          "service data directory is already in use".into(),
        ));
      }
    };
    let path = directory.join("metadata.sqlite3");
    if fs::symlink_metadata(&path)
      .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
    {
      return Err(crate::error::ExpriError::Message(
        "service metadata must be a regular file".into(),
      ));
    }
    #[cfg(unix)]
    {
      use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
      match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
      {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
      }
      fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    let connection = Connection::open(&path)
      .map_err(|_| crate::error::ExpriError::Message("cannot open service metadata".into()))?;
    connection
      .busy_timeout(Duration::from_secs(5))
      .map_err(|_| crate::error::ExpriError::Message("cannot configure service metadata".into()))?;
    let version: i64 = connection
      .pragma_query_value(None, "user_version", |row| row.get(0))
      .map_err(|_| {
        crate::error::ExpriError::Message("cannot read service metadata version".into())
      })?;
    if version > 1 {
      return Err(crate::error::ExpriError::Message(
        "service metadata schema is newer than this binary".into(),
      ));
    }
    connection
      .execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
      CREATE TABLE IF NOT EXISTS uploads (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT, upload_id TEXT NOT NULL UNIQUE,
        target TEXT NOT NULL, size INTEGER NOT NULL, sha256 TEXT NOT NULL,
        object_key TEXT NOT NULL, multipart TEXT, part_size INTEGER NOT NULL,
        complete INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS parts (
        upload_id TEXT NOT NULL, part_number INTEGER NOT NULL, etag TEXT NOT NULL,
        PRIMARY KEY(upload_id, part_number));
      CREATE TABLE IF NOT EXISTS files (
        target TEXT PRIMARY KEY, record TEXT NOT NULL, object_key TEXT NOT NULL,
        sequence INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS streams (
        target TEXT PRIMARY KEY, size INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS chunks (
        target TEXT NOT NULL, offset INTEGER NOT NULL, data BLOB NOT NULL,
        PRIMARY KEY(target, offset));
      PRAGMA user_version=1;",
      )
      .map_err(|_| {
        crate::error::ExpriError::Message("cannot initialize service metadata".into())
      })?;
    Ok(Self {
      connection: Mutex::new(connection),
      upload_gates: Mutex::new(BTreeMap::new()),
      storage,
      _lease: lease,
    })
  }

  fn db(&self) -> ApiResult<MutexGuard<'_, Connection>> {
    self
      .connection
      .lock()
      .map_err(|_| ApiError::new(503, "service metadata unavailable"))
  }

  fn upload_gate(&self, id: &str) -> ApiResult<Arc<Mutex<()>>> {
    validate_component(id).map_err(bad)?;
    let mut gates = self
      .upload_gates
      .lock()
      .map_err(|_| ApiError::new(503, "upload operation unavailable"))?;
    gates.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = gates.get(id).and_then(Weak::upgrade) {
      return Ok(gate);
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(id.into(), Arc::downgrade(&gate));
    Ok(gate)
  }

  fn upload(&self, id: &str) -> ApiResult<Upload> {
    validate_component(id).map_err(bad)?;
    let row = self.db()?.query_row(
      "SELECT target,size,sha256,object_key,multipart,part_size,sequence,complete FROM uploads WHERE upload_id=?1",
      [id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
    ).optional().map_err(database)?.ok_or_else(|| ApiError::new(404, "upload is missing"))?;
    Ok(Upload {
      target: serde_json::from_str(&row.0)
        .map_err(|_| ApiError::new(500, "invalid stored upload"))?,
      size: row.1,
      sha256: row.2,
      key: row.3,
      multipart: row.4,
      part_size: row.5,
      sequence: row.6,
      complete: row.7,
    })
  }

  pub fn upload_target(&self, id: &str) -> ApiResult<FileTarget> {
    Ok(self.upload(id)?.target)
  }

  fn state(&self, id: &str, upload: &Upload) -> ApiResult<UploadState> {
    let db = self.db()?;
    let mut statement = db
      .prepare("SELECT part_number,etag FROM parts WHERE upload_id=?1 ORDER BY part_number")
      .map_err(database)?;
    let parts = statement
      .query_map([id], |row| {
        Ok(CompletedPart {
          part_number: row.get(0)?,
          etag: row.get(1)?,
        })
      })
      .map_err(database)?
      .collect::<std::result::Result<Vec<_>, _>>()
      .map_err(database)?;
    Ok(UploadState {
      upload_id: id.into(),
      part_size: upload.part_size,
      parts,
      complete: upload.complete,
    })
  }

  pub fn execute(&self, request: Request) -> ApiResult<Response> {
    match request {
      Request::BeginUpload {
        upload_id,
        target,
        size,
        sha256,
      } => self.begin(&upload_id, target, size, sha256),
      Request::PartUrl {
        upload_id,
        part_number,
      } => {
        let upload = self.upload(&upload_id)?;
        check_part(&upload, part_number)?;
        if upload.complete {
          return Err(ApiError::new(409, "upload is already complete"));
        }
        let multipart = upload
          .multipart
          .ok_or_else(|| ApiError::new(409, "upload initialization is incomplete; retry begin"))?;
        let url = self
          .storage
          .presign_part(&upload.key, &multipart, part_number, 900)
          .map_err(|_| ApiError::new(502, "object storage unavailable"))?;
        Ok(Response::Url { url })
      }
      Request::RecordPart { upload_id, part } => self.record_part(&upload_id, part),
      Request::CompleteUpload { upload_id } => self.complete(&upload_id),
      Request::ListFiles { scope } => self.list_files(&scope),
      Request::ListRuns { project_id, origin } => self.list_runs(&project_id, &origin),
      Request::GetFile { target } => Ok(Response::File {
        file: self.file(&target)?,
      }),
      Request::DownloadUrl { target } => {
        validate_target(&target).map_err(bad)?;
        let key = self
          .db()?
          .query_row(
            "SELECT object_key FROM files WHERE target=?1",
            [target_json(&target)?],
            |row| row.get::<_, String>(0),
          )
          .optional()
          .map_err(database)?
          .ok_or_else(|| ApiError::new(404, "completed object is missing"))?;
        Ok(Response::Url {
          url: self
            .storage
            .presign_get(&key, 900)
            .map_err(|_| ApiError::new(502, "object storage unavailable"))?,
        })
      }
      Request::AppendStream {
        scope,
        path,
        offset,
        data_base64,
      } => self.append(scope, path, offset, &data_base64),
      Request::ReadStream {
        scope,
        path,
        offset,
        limit,
      } => self.read_stream(scope, path, offset, limit),
    }
  }

  fn begin(&self, id: &str, target: FileTarget, size: u64, sha256: String) -> ApiResult<Response> {
    validate_component(id).map_err(bad)?;
    validate_target(&target).map_err(bad)?;
    if sha256.len() != 64
      || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
      || size > MAX_FILE_SIZE
    {
      return Err(ApiError::new(400, "invalid artifact size or SHA256"));
    }
    let sha256 = sha256.to_ascii_lowercase();
    let gate = self.upload_gate(id)?;
    let _gate = gate
      .lock()
      .map_err(|_| ApiError::new(503, "upload operation unavailable"))?;
    match self.upload(id) {
      Ok(upload) => {
        if upload.target != target || upload.size != size || upload.sha256 != sha256 {
          return Err(ApiError::new(
            409,
            "upload ID is already assigned to different bytes",
          ));
        }
      }
      Err(error) if error.status == 404 => {
        if let FileTarget::Input { .. } = target {
          match self.file(&target) {
            Ok(file) if file.size != size || file.sha256.as_deref() != Some(sha256.as_str()) => {
              return Err(ApiError::new(
                409,
                "input ID is immutable; choose a new version",
              ));
            }
            Ok(_) => {}
            Err(error) if error.status == 404 => {}
            Err(error) => return Err(error),
          }
        }
        self.db()?.execute("INSERT INTO uploads(upload_id,target,size,sha256,object_key,part_size) VALUES(?1,?2,?3,?4,?5,?6)",
          params![id, target_json(&target)?, size, sha256, object_key(&target, id), part_size(size)]).map_err(database)?;
      }
      Err(error) => return Err(error),
    }
    let mut upload = self.upload(id)?;
    if !upload.complete && upload.multipart.is_none() {
      let multipart = self
        .storage
        .begin_upload(&upload.key, "application/octet-stream")
        .map_err(|_| ApiError::new(502, "object storage unavailable; retry begin"))?;
      self
        .db()?
        .execute(
          "UPDATE uploads SET multipart=?2 WHERE upload_id=?1",
          params![id, multipart],
        )
        .map_err(database)?;
      upload = self.upload(id)?;
    }
    Ok(Response::Upload {
      upload: self.state(id, &upload)?,
    })
  }

  fn record_part(&self, id: &str, part: CompletedPart) -> ApiResult<Response> {
    let gate = self.upload_gate(id)?;
    let _gate = gate
      .lock()
      .map_err(|_| ApiError::new(503, "upload operation unavailable"))?;
    let upload = self.upload(id)?;
    check_part(&upload, part.part_number)?;
    if part.etag.is_empty()
      || part.etag.len() > MAX_ETAG_BYTES
      || part.etag.chars().any(char::is_control)
    {
      return Err(ApiError::new(400, "invalid storage part receipt"));
    }
    if upload.complete {
      return Ok(Response::Upload {
        upload: self.state(id, &upload)?,
      });
    }
    if upload.multipart.is_none() {
      return Err(ApiError::new(409, "retry upload initialization"));
    }
    self.db()?.execute("INSERT INTO parts(upload_id,part_number,etag) VALUES(?1,?2,?3) ON CONFLICT(upload_id,part_number) DO UPDATE SET etag=excluded.etag", params![id, part.part_number, part.etag]).map_err(database)?;
    Ok(Response::Upload {
      upload: self.state(id, &upload)?,
    })
  }

  fn complete(&self, id: &str) -> ApiResult<Response> {
    let gate = self.upload_gate(id)?;
    let _gate = gate
      .lock()
      .map_err(|_| ApiError::new(503, "upload operation unavailable"))?;
    let upload = self.upload(id)?;
    let state = self.state(id, &upload)?;
    if upload.complete {
      return Ok(Response::File {
        file: self.file(&upload.target)?,
      });
    }
    if state.parts.len() != expected_parts(&upload) as usize
      || state
        .parts
        .iter()
        .enumerate()
        .any(|(index, part)| part.part_number != index as u32 + 1)
    {
      return Err(ApiError::new(409, "upload parts are incomplete"));
    }
    let multipart = upload
      .multipart
      .as_ref()
      .ok_or_else(|| ApiError::new(409, "retry upload initialization"))?;
    let metadata = match self
      .storage
      .head(&upload.key)
      .map_err(|_| ApiError::new(502, "object storage unavailable"))?
    {
      Some(metadata) => metadata,
      None => match self
        .storage
        .complete_upload(&upload.key, multipart, &state.parts)
      {
        Ok(metadata) => metadata,
        Err(_) => self
          .storage
          .head(&upload.key)
          .ok()
          .flatten()
          .ok_or_else(|| ApiError::new(502, "object completion failed; retry completion"))?,
      },
    };
    if metadata.size != upload.size {
      return Err(ApiError::new(
        409,
        "completed object has an unexpected size",
      ));
    }
    let target = target_json(&upload.target)?;
    let record = FileRecord {
      target: upload.target.clone(),
      size: upload.size,
      sha256: Some(upload.sha256.clone()),
      storage: FileStorage::Object,
    };
    let mut db = self.db()?;
    let transaction = db.transaction().map_err(database)?;
    if let FileTarget::Input { .. } = upload.target {
      let existing: Option<String> = transaction
        .query_row(
          "SELECT record FROM files WHERE target=?1",
          [&target],
          |row| row.get(0),
        )
        .optional()
        .map_err(database)?;
      if let Some(existing) = existing {
        let existing: FileRecord = serde_json::from_str(&existing)
          .map_err(|_| ApiError::new(500, "invalid stored artifact"))?;
        if existing.size != upload.size || existing.sha256 != record.sha256 {
          return Err(ApiError::new(409, "input ID is immutable"));
        }
      }
    }
    let stream_size: Option<u64> = transaction
      .query_row(
        "SELECT size FROM streams WHERE target=?1",
        [&target],
        |row| row.get(0),
      )
      .optional()
      .map_err(database)?;
    if stream_size.is_some_and(|size| size != upload.size) {
      return Err(ApiError::new(
        409,
        "stream changed while its object was uploading; retry with current bytes",
      ));
    }
    let published = transaction.execute("INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,?3,?4) ON CONFLICT(target) DO UPDATE SET record=excluded.record,object_key=excluded.object_key,sequence=excluded.sequence WHERE files.sequence < excluded.sequence",
      params![target, serde_json::to_string(&record).map_err(|_| ApiError::new(500, "cannot encode artifact"))?, upload.key, upload.sequence]).map_err(database)?;
    if published != 0 {
      // Publish the immutable object and retire its live copy in one durable transaction.
      transaction
        .execute("DELETE FROM chunks WHERE target=?1", [&target])
        .map_err(database)?;
      transaction
        .execute("DELETE FROM streams WHERE target=?1", [&target])
        .map_err(database)?;
    }
    transaction
      .execute("UPDATE uploads SET complete=1 WHERE upload_id=?1", [id])
      .map_err(database)?;
    transaction.commit().map_err(database)?;
    drop(db);
    Ok(Response::File {
      file: self.file(&upload.target)?,
    })
  }

  fn file(&self, target: &FileTarget) -> ApiResult<FileRecord> {
    validate_target(target).map_err(bad)?;
    let encoded = target_json(target)?;
    let db = self.db()?;
    let object: Option<String> = db
      .query_row(
        "SELECT record FROM files WHERE target=?1",
        [&encoded],
        |row| row.get(0),
      )
      .optional()
      .map_err(database)?;
    if let Some(object) = object {
      return serde_json::from_str(&object)
        .map_err(|_| ApiError::new(500, "invalid stored artifact"));
    }
    let size: Option<u64> = db
      .query_row(
        "SELECT size FROM streams WHERE target=?1",
        [&encoded],
        |row| row.get(0),
      )
      .optional()
      .map_err(database)?;
    Ok(FileRecord {
      target: target.clone(),
      size: size.ok_or_else(|| ApiError::new(404, "artifact is missing"))?,
      sha256: None,
      storage: FileStorage::Stream,
    })
  }

  fn list_files(&self, scope: &RunScope) -> ApiResult<Response> {
    validate_scope(scope).map_err(bad)?;
    let db = self.db()?;
    let filter = "json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3";
    let mut files = BTreeMap::new();
    let mut statement = db
      .prepare(&format!(
        "SELECT record FROM files WHERE {filter} LIMIT 1001"
      ))
      .map_err(database)?;
    for row in statement
      .query_map(
        params![scope.project_id, scope.origin, scope.run_id],
        |row| row.get::<_, String>(0),
      )
      .map_err(database)?
    {
      let file: FileRecord = serde_json::from_str(&row.map_err(database)?)
        .map_err(|_| ApiError::new(500, "invalid stored artifact"))?;
      files.insert(target_json(&file.target)?, file);
    }
    let mut statement = db
      .prepare(&format!(
        "SELECT target,size FROM streams WHERE {filter} LIMIT 1001"
      ))
      .map_err(database)?;
    for row in statement
      .query_map(
        params![scope.project_id, scope.origin, scope.run_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
      )
      .map_err(database)?
    {
      let (target, size) = row.map_err(database)?;
      if let std::collections::btree_map::Entry::Vacant(entry) = files.entry(target) {
        let record = FileRecord {
          target: serde_json::from_str(entry.key())
            .map_err(|_| ApiError::new(500, "invalid stored stream"))?,
          size,
          sha256: None,
          storage: FileStorage::Stream,
        };
        entry.insert(record);
      }
    }
    if files.len() > 1000 {
      return Err(ApiError::new(
        413,
        "run exceeds the 1000-file listing limit",
      ));
    }
    Ok(Response::Files {
      files: files.into_values().collect(),
    })
  }

  fn list_runs(&self, project: &str, origin: &str) -> ApiResult<Response> {
    validate_component(project).map_err(bad)?;
    validate_component(origin).map_err(bad)?;
    let db = self.db()?;
    let mut statement = db.prepare("SELECT DISTINCT json_extract(target,'$.scope.run_id') AS run_id FROM (SELECT target FROM files UNION SELECT target FROM streams) WHERE json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 ORDER BY run_id DESC LIMIT 501").map_err(database)?;
    let runs = statement
      .query_map(params![project, origin], |row| row.get::<_, String>(0))
      .map_err(database)?
      .map(|row| {
        row.map(|run_id| RunScope {
          project_id: project.into(),
          origin: origin.into(),
          run_id,
        })
      })
      .collect::<std::result::Result<Vec<_>, _>>()
      .map_err(database)?;
    if runs.len() > 500 {
      return Err(ApiError::new(
        413,
        "origin exceeds the 500-run listing limit",
      ));
    }
    Ok(Response::Runs { runs })
  }

  fn append(
    &self,
    scope: RunScope,
    path: String,
    offset: u64,
    encoded_data: &str,
  ) -> ApiResult<Response> {
    let target = stream_target(scope, path)?;
    if encoded_data.len() > STREAM_BATCH.div_ceil(3) * 4 {
      return Err(ApiError::new(413, "stream batch exceeds 64 KiB"));
    }
    let data = base64::engine::general_purpose::STANDARD
      .decode(encoded_data)
      .map_err(|_| ApiError::new(400, "invalid stream encoding"))?;
    if data.len() > STREAM_BATCH
      || offset
        .checked_add(data.len() as u64)
        .is_none_or(|end| end > i64::MAX as u64)
    {
      return Err(ApiError::new(
        413,
        "stream offset or batch exceeds its limit",
      ));
    }
    let encoded = target_json(&target)?;
    let mut db = self.db()?;
    let transaction = db.transaction().map_err(database)?;
    let finalized: bool = transaction
      .query_row(
        "SELECT EXISTS(SELECT 1 FROM files WHERE target=?1)",
        [&encoded],
        |row| row.get(0),
      )
      .map_err(database)?;
    if finalized {
      return Err(ApiError::new(409, "stream has been finalized as an object"));
    }
    transaction
      .execute(
        "INSERT OR IGNORE INTO streams(target,size) VALUES(?1,0)",
        [&encoded],
      )
      .map_err(database)?;
    let size: u64 = transaction
      .query_row(
        "SELECT size FROM streams WHERE target=?1",
        [&encoded],
        |row| row.get(0),
      )
      .map_err(database)?;
    let end = offset + data.len() as u64;
    if offset > size {
      return Err(ApiError::new(
        409,
        "stream offset or repeated bytes conflict",
      ));
    }
    let overlap = (size - offset).min(data.len() as u64) as usize;
    if overlap != 0 && read_bytes(&transaction, &encoded, offset, overlap)? != data[..overlap] {
      return Err(ApiError::new(
        409,
        "stream offset or repeated bytes conflict",
      ));
    }
    if end > size {
      transaction
        .execute(
          "INSERT INTO chunks(target,offset,data) VALUES(?1,?2,?3)",
          params![encoded, size, &data[overlap..]],
        )
        .map_err(database)?;
      transaction
        .execute(
          "UPDATE streams SET size=?2 WHERE target=?1",
          params![encoded, end],
        )
        .map_err(database)?;
    }
    transaction.commit().map_err(database)?;
    Ok(Response::Acknowledged { offset: end })
  }

  fn read_stream(
    &self,
    scope: RunScope,
    path: String,
    offset: u64,
    limit: usize,
  ) -> ApiResult<Response> {
    let target = target_json(&stream_target(scope, path)?)?;
    if limit > STREAM_BATCH {
      return Err(ApiError::new(413, "stream reads are limited to 64 KiB"));
    }
    let db = self.db()?;
    let size: Option<u64> = db
      .query_row(
        "SELECT size FROM streams WHERE target=?1",
        [&target],
        |row| row.get(0),
      )
      .optional()
      .map_err(database)?;
    let size = match size {
      Some(size) => size,
      None => {
        let finalized: bool = db
          .query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE target=?1)",
            [&target],
            |row| row.get(0),
          )
          .map_err(database)?;
        return Err(if finalized {
          ApiError::new(409, "stream finalized; refresh the file catalog")
        } else {
          ApiError::new(404, "stream is missing")
        });
      }
    };
    if offset > size {
      return Err(ApiError::new(409, "stream offset exceeds available bytes"));
    }
    let data = read_bytes(
      &db,
      &target,
      offset,
      limit.min(usize::try_from(size - offset).unwrap_or(usize::MAX)),
    )?;
    Ok(Response::Stream {
      offset,
      total_size: size,
      data_base64: base64::engine::general_purpose::STANDARD.encode(data),
    })
  }
}

fn object_key(target: &FileTarget, upload_id: &str) -> String {
  match target {
    FileTarget::Run { scope, .. } => format!(
      "projects/{}/runs/{}/{}/objects/{upload_id}",
      scope.project_id, scope.origin, scope.run_id
    ),
    FileTarget::Input {
      project_id,
      input_id,
    } => format!("projects/{project_id}/inputs/{input_id}/objects/{upload_id}"),
  }
}

fn target_json(target: &FileTarget) -> ApiResult<String> {
  serde_json::to_string(target).map_err(|_| ApiError::new(500, "cannot encode artifact target"))
}

fn stream_target(scope: RunScope, path: String) -> ApiResult<FileTarget> {
  validate_scope(&scope).map_err(bad)?;
  if !stream_path(&path) {
    return Err(ApiError::new(
      400,
      "only metrics and saved logs support streaming",
    ));
  }
  Ok(FileTarget::Run { scope, path })
}

fn part_size(size: u64) -> u64 {
  let mib = 1024 * 1024;
  (size.div_ceil(MAX_PARTS).div_ceil(mib) * mib).max(8 * mib)
}

fn expected_parts(upload: &Upload) -> u32 {
  upload.size.div_ceil(upload.part_size).max(1) as u32
}

fn check_part(upload: &Upload, number: u32) -> ApiResult<()> {
  if number == 0 || number > expected_parts(upload) {
    return Err(ApiError::new(400, "part number is outside this upload"));
  }
  Ok(())
}

fn read_bytes(db: &Connection, target: &str, offset: u64, length: usize) -> ApiResult<Vec<u8>> {
  let end = offset.saturating_add(length as u64);
  let mut result = Vec::with_capacity(length);
  let mut statement = db.prepare("SELECT offset,data FROM chunks WHERE target=?1 AND offset<?3 AND offset+length(data)>?2 ORDER BY offset").map_err(database)?;
  for row in statement
    .query_map(params![target, offset, end], |row| {
      Ok((row.get::<_, u64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })
    .map_err(database)?
  {
    let (start, data) = row.map_err(database)?;
    let first = offset.saturating_sub(start) as usize;
    let last = ((end - start) as usize).min(data.len());
    result.extend_from_slice(&data[first..last]);
  }
  if result.len() != length {
    return Err(ApiError::new(500, "stored stream has a missing byte range"));
  }
  Ok(result)
}

#[cfg(test)]
pub(super) mod tests {
  use std::sync::{Arc, Mutex};

  use super::*;
  use crate::service::storage::ObjectMetadata;

  #[derive(Default)]
  struct Backend {
    sessions: BTreeMap<String, String>,
    sizes: BTreeMap<String, u64>,
    objects: BTreeMap<String, u64>,
    starts: usize,
    lose_completion: bool,
  }

  #[derive(Clone, Default)]
  pub struct MockStorage(Arc<Mutex<Backend>>);

  impl MockStorage {
    fn stage(&self, id: &str, size: u64) {
      let mut backend = self.0.lock().unwrap();
      let key = backend
        .sessions
        .keys()
        .find(|key| key.ends_with(&format!("/objects/{id}")))
        .unwrap()
        .clone();
      backend.sizes.insert(key, size);
    }
  }

  impl ObjectStorage for MockStorage {
    fn begin_upload(&self, key: &str, _: &str) -> crate::error::Result<String> {
      let mut backend = self.0.lock().unwrap();
      backend.starts += 1;
      let id = format!("multipart-{}", backend.starts);
      backend.sessions.insert(key.into(), id.clone());
      Ok(id)
    }
    fn presign_part(
      &self,
      key: &str,
      id: &str,
      number: u32,
      _: u32,
    ) -> crate::error::Result<String> {
      assert_eq!(self.0.lock().unwrap().sessions.get(key).unwrap(), id);
      Ok(format!("https://storage.invalid/{key}?part={number}"))
    }
    fn complete_upload(
      &self,
      key: &str,
      id: &str,
      _: &[CompletedPart],
    ) -> crate::error::Result<ObjectMetadata> {
      let mut backend = self.0.lock().unwrap();
      assert_eq!(backend.sessions.remove(key).unwrap(), id);
      let size = *backend.sizes.get(key).unwrap();
      backend.objects.insert(key.into(), size);
      if backend.lose_completion {
        backend.lose_completion = false;
        return Err(crate::error::ExpriError::Message(
          "lost completion acknowledgement".into(),
        ));
      }
      Ok(ObjectMetadata { size })
    }
    fn head(&self, key: &str) -> crate::error::Result<Option<ObjectMetadata>> {
      Ok(
        self
          .0
          .lock()
          .unwrap()
          .objects
          .get(key)
          .map(|size| ObjectMetadata { size: *size }),
      )
    }
    fn presign_get(&self, key: &str, _: u32) -> crate::error::Result<String> {
      Ok(format!("https://storage.invalid/{key}"))
    }
  }

  pub fn scope() -> RunScope {
    RunScope {
      project_id: "project".into(),
      origin: "worker".into(),
      run_id: "run-1".into(),
    }
  }

  fn append_request(offset: u64, bytes: &[u8]) -> Request {
    Request::AppendStream {
      scope: scope(),
      path: "outputs/metrics.jsonl".into(),
      offset,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
  }

  #[test]
  fn data_directory_rejects_a_second_service_until_the_lease_is_released() {
    let directory = tempfile::tempdir().unwrap();
    let first = Store::open(directory.path(), MockStorage::default()).unwrap();
    let error = Store::open(directory.path(), MockStorage::default())
      .err()
      .unwrap();
    assert!(error.to_string().contains("already in use"));
    drop(first);
    Store::open(directory.path(), MockStorage::default()).unwrap();
  }

  #[test]
  fn largest_upload_and_escaped_receipts_fit_the_api_limits() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let target = FileTarget::Run {
      scope: scope(),
      path: "outputs/model.pt".into(),
    };
    let begin = |id: &str, size| Request::BeginUpload {
      upload_id: id.into(),
      target: target.clone(),
      size,
      sha256: "a".repeat(64),
    };
    let Response::Upload { upload } = store.execute(begin("largest", MAX_FILE_SIZE)).unwrap()
    else {
      panic!("upload response");
    };
    assert_eq!(upload.part_size, MAX_PART_SIZE);
    assert_eq!(MAX_FILE_SIZE.div_ceil(upload.part_size), MAX_PARTS);
    assert_eq!(part_size(MAX_FILE_SIZE - 1), MAX_PART_SIZE);
    assert_eq!(part_size(0), 8 * 1024 * 1024);
    assert_eq!(
      store
        .execute(begin("too-large", MAX_FILE_SIZE + 1))
        .unwrap_err()
        .status,
      400
    );
    assert_eq!(
      store
        .execute(Request::RecordPart {
          upload_id: "largest".into(),
          part: CompletedPart {
            part_number: 1,
            etag: "a".repeat(MAX_ETAG_BYTES + 1),
          },
        })
        .unwrap_err()
        .status,
      400
    );
    let response = Response::Upload {
      upload: UploadState {
        upload_id: "a".repeat(96),
        part_size: MAX_PART_SIZE,
        parts: (1..=MAX_PARTS as u32)
          .map(|part_number| CompletedPart {
            part_number,
            etag: "\\\"".repeat(MAX_ETAG_BYTES / 2),
          })
          .collect(),
        complete: false,
      },
    };
    assert!(serde_json::to_vec(&response).unwrap().len() < MAX_REQUEST);
  }

  #[test]
  fn stream_retries_compare_bytes_and_survive_restart_with_bounded_reads() {
    let directory = tempfile::tempdir().unwrap();
    {
      let store = Store::open(directory.path(), MockStorage::default()).unwrap();
      assert!(matches!(
        store.execute(append_request(0, b"first\n")).unwrap(),
        Response::Acknowledged { offset: 6 }
      ));
      assert!(matches!(
        store.execute(append_request(6, b"second\n")).unwrap(),
        Response::Acknowledged { offset: 13 }
      ));
      assert!(matches!(
        store.execute(append_request(0, b"first\n")).unwrap(),
        Response::Acknowledged { offset: 6 }
      ));
      assert_eq!(
        store
          .execute(append_request(0, b"wrong\n"))
          .unwrap_err()
          .status,
        409
      );
      assert_eq!(
        store
          .execute(append_request(14, b"gap"))
          .unwrap_err()
          .status,
        409
      );
    }
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let response = store
      .execute(Request::ReadStream {
        scope: scope(),
        path: "outputs/metrics.jsonl".into(),
        offset: 4,
        limit: 6,
      })
      .unwrap();
    match response {
      Response::Stream {
        offset,
        total_size,
        data_base64,
      } => {
        assert_eq!((offset, total_size), (4, 13));
        assert_eq!(
          base64::engine::general_purpose::STANDARD
            .decode(data_base64)
            .unwrap(),
          b"t\nseco"
        );
      }
      _ => panic!("stream response"),
    }
    assert_eq!(
      store
        .execute(Request::ReadStream {
          scope: scope(),
          path: "outputs/metrics.jsonl".into(),
          offset: 0,
          limit: STREAM_BATCH + 1
        })
        .unwrap_err()
        .status,
      413
    );
    assert!(
      matches!(store.execute(Request::ListFiles { scope: scope() }).unwrap(), Response::Files { files } if files.len() == 1 && matches!(files[0].storage, FileStorage::Stream))
    );
  }

  #[test]
  fn lost_short_append_ack_can_resume_after_growth_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    {
      let store = Store::open(directory.path(), MockStorage::default()).unwrap();
      store.execute(append_request(0, b"first\n")).unwrap();
      // The sender did not receive this acknowledgement and its file then grew.
    }
    {
      let store = Store::open(directory.path(), MockStorage::default()).unwrap();
      assert!(matches!(
        store
          .execute(append_request(0, b"first\nsecond\n"))
          .unwrap(),
        Response::Acknowledged { offset: 13 }
      ));
      assert_eq!(
        store
          .execute(append_request(0, b"wrong\nsecond\nthird\n"))
          .unwrap_err()
          .status,
        409
      );
      assert!(matches!(
        store
          .execute(append_request(0, b"first\nsecond\n"))
          .unwrap(),
        Response::Acknowledged { offset: 13 }
      ));
      assert_eq!(
        store
          .db()
          .unwrap()
          .query_row("SELECT COUNT(*) FROM chunks", [], |row| row
            .get::<_, u64>(0))
          .unwrap(),
        2
      );
    }
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let Response::Stream {
      offset,
      total_size,
      data_base64,
    } = store
      .execute(Request::ReadStream {
        scope: scope(),
        path: "outputs/metrics.jsonl".into(),
        offset: 0,
        limit: STREAM_BATCH,
      })
      .unwrap()
    else {
      panic!("stream response");
    };
    assert_eq!((offset, total_size), (0, 13));
    assert_eq!(
      base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .unwrap(),
      b"first\nsecond\n"
    );
  }

  #[test]
  fn multipart_receipts_resume_after_restart_and_lost_completion_ack() {
    let directory = tempfile::tempdir().unwrap();
    let backend = MockStorage::default();
    let target = FileTarget::Run {
      scope: scope(),
      path: "outputs/model.pt".into(),
    };
    let size = 8 * 1024 * 1024 + 1;
    let begin = Request::BeginUpload {
      upload_id: "upload-1".into(),
      target,
      size,
      sha256: "a".repeat(64),
    };
    {
      let store = Store::open(directory.path(), backend.clone()).unwrap();
      store.execute(begin.clone()).unwrap();
      store
        .execute(Request::RecordPart {
          upload_id: "upload-1".into(),
          part: CompletedPart {
            part_number: 1,
            etag: "opaque-1".into(),
          },
        })
        .unwrap();
      assert_eq!(
        store
          .execute(Request::CompleteUpload {
            upload_id: "upload-1".into()
          })
          .unwrap_err()
          .status,
        409
      );
    }
    let store = Store::open(directory.path(), backend.clone()).unwrap();
    assert!(
      matches!(store.execute(begin).unwrap(), Response::Upload { upload } if upload.parts.len() == 1 && !upload.complete)
    );
    assert_eq!(backend.0.lock().unwrap().starts, 1);
    store
      .execute(Request::RecordPart {
        upload_id: "upload-1".into(),
        part: CompletedPart {
          part_number: 2,
          etag: "opaque-2".into(),
        },
      })
      .unwrap();
    backend.stage("upload-1", size);
    backend.0.lock().unwrap().lose_completion = true;
    assert!(
      matches!(store.execute(Request::CompleteUpload { upload_id: "upload-1".into() }).unwrap(), Response::File { file } if file.size == size && file.sha256.as_deref() == Some("a".repeat(64).as_str()))
    );
    assert!(matches!(
      store
        .execute(Request::CompleteUpload {
          upload_id: "upload-1".into()
        })
        .unwrap(),
      Response::File { .. }
    ));
  }

  #[test]
  fn finalized_streams_cannot_grow_and_input_ids_cannot_change_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let backend = MockStorage::default();
    let store = Store::open(directory.path(), backend.clone()).unwrap();
    store.execute(append_request(0, b"row\n")).unwrap();
    for (id, target, size) in [
      (
        "stream",
        FileTarget::Run {
          scope: scope(),
          path: "outputs/metrics.jsonl".into(),
        },
        4,
      ),
      (
        "input",
        FileTarget::Input {
          project_id: "project".into(),
          input_id: "dataset-v1".into(),
        },
        10,
      ),
    ] {
      store
        .execute(Request::BeginUpload {
          upload_id: id.into(),
          target,
          size,
          sha256: "b".repeat(64),
        })
        .unwrap();
      store
        .execute(Request::RecordPart {
          upload_id: id.into(),
          part: CompletedPart {
            part_number: 1,
            etag: "part".into(),
          },
        })
        .unwrap();
      backend.stage(id, size);
      store
        .execute(Request::CompleteUpload {
          upload_id: id.into(),
        })
        .unwrap();
    }
    assert_eq!(
      store
        .execute(append_request(4, b"late\n"))
        .unwrap_err()
        .status,
      409
    );
    assert_eq!(
      store
        .execute(Request::BeginUpload {
          upload_id: "different".into(),
          target: FileTarget::Input {
            project_id: "project".into(),
            input_id: "dataset-v1".into()
          },
          size: 11,
          sha256: "c".repeat(64)
        })
        .unwrap_err()
        .status,
      409
    );
    assert!(
      matches!(store.execute(Request::ListFiles { scope: scope() }).unwrap(), Response::Files { files } if files.len() == 1 && matches!(files[0].storage, FileStorage::Object))
    );
    drop(store);
    let store = Store::open(directory.path(), backend).unwrap();
    for table in ["streams", "chunks"] {
      assert_eq!(
        store
          .db()
          .unwrap()
          .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, u64>(0)
          })
          .unwrap(),
        0,
        "finalized data remained in {table}"
      );
    }
    assert_eq!(
      store
        .execute(Request::ReadStream {
          scope: scope(),
          path: "outputs/metrics.jsonl".into(),
          offset: 0,
          limit: STREAM_BATCH,
        })
        .unwrap_err()
        .status,
      409
    );
    assert!(matches!(
      store
        .execute(Request::CompleteUpload {
          upload_id: "stream".into()
        })
        .unwrap(),
      Response::File { file } if file.size == 4 && matches!(file.storage, FileStorage::Object)
    ));
  }
}
