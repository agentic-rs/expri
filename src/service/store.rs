use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::time::Duration;

use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};

use super::storage::ObjectStorage;
use super::types::*;

mod archive;
mod dashboard_storage;
mod projects;
mod references;
mod tracking;

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
  directory: PathBuf,
  connection: Mutex<Connection>,
  // Network calls never hold the database lock. This gate prevents simultaneous
  // begin/complete retries from creating conflicting sessions for one upload.
  upload_gates: Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
  project_gates: Mutex<BTreeMap<String, Weak<RwLock<()>>>>,
  storage: S,
  _lease: crate::lock::FileLock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DashboardSource {
  pub project_id: String,
  pub origin: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DashboardProjectSource {
  pub project_id: String,
  pub origin: Option<String>,
}

pub(super) struct DashboardPage<T> {
  pub items: Vec<T>,
  pub total_count: usize,
  pub legacy_order: bool,
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
    if version > 3 {
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
      CREATE TABLE IF NOT EXISTS dashboard_overviews (
        target TEXT PRIMARY KEY, version TEXT NOT NULL, record TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS dashboard_run_activity (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
        project_id TEXT NOT NULL, origin TEXT NOT NULL, run_id TEXT NOT NULL,
        legacy_order INTEGER NOT NULL DEFAULT 0,
        UNIQUE(project_id,origin,run_id));
      CREATE TABLE IF NOT EXISTS project_storage_revisions (
        project_id TEXT PRIMARY KEY, revision INTEGER NOT NULL);
      CREATE INDEX IF NOT EXISTS dashboard_activity_source ON dashboard_run_activity(project_id,origin,sequence);
      CREATE INDEX IF NOT EXISTS dashboard_files_scope ON files(json_extract(target,'$.kind'),json_extract(target,'$.scope.project_id'),json_extract(target,'$.scope.origin'),json_extract(target,'$.scope.run_id'));
      CREATE INDEX IF NOT EXISTS dashboard_storage_inputs ON files(json_extract(target,'$.kind'),json_extract(target,'$.project_id'),sequence DESC);
      CREATE INDEX IF NOT EXISTS dashboard_storage_outputs ON files(json_extract(target,'$.kind'),json_extract(target,'$.scope.project_id'),sequence DESC);
      CREATE INDEX IF NOT EXISTS dashboard_streams_scope ON streams(json_extract(target,'$.kind'),json_extract(target,'$.scope.project_id'),json_extract(target,'$.scope.origin'),json_extract(target,'$.scope.run_id'));
      INSERT OR IGNORE INTO dashboard_run_activity(project_id,origin,run_id,legacy_order)
        SELECT json_extract(target,'$.scope.project_id'),json_extract(target,'$.scope.origin'),json_extract(target,'$.scope.run_id'),1
        FROM (SELECT target,sequence FROM files UNION ALL SELECT target,0 AS sequence FROM streams)
        WHERE json_extract(target,'$.kind')='run'
        GROUP BY json_extract(target,'$.scope.project_id'),json_extract(target,'$.scope.origin'),json_extract(target,'$.scope.run_id')
        ORDER BY MAX(sequence),json_extract(target,'$.scope.run_id');
      ",
      )
      .map_err(|_| {
        crate::error::ExpriError::Message("cannot initialize service metadata".into())
      })?;
    tracking::initialize(&connection)?;
    tracking::recover(&directory, &connection)?;
    archive::initialize(&connection)
      .map_err(|_| crate::error::ExpriError::Message("cannot initialize archive storage".into()))?;
    projects::initialize(&connection)?;
    // Older binaries must not ignore project tombstones and resurrect deleted data.
    connection
      .pragma_update(None, "user_version", 3)
      .map_err(|_| {
        crate::error::ExpriError::Message("cannot upgrade service metadata version".into())
      })?;
    Ok(Self {
      directory,
      connection: Mutex::new(connection),
      upload_gates: Mutex::new(BTreeMap::new()),
      project_gates: Mutex::new(BTreeMap::new()),
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

  /// Discover only run scopes. Private inputs and incomplete uploads are not
  /// dashboard sources, and database pagination never materializes the catalog.
  pub fn dashboard_sources(
    &self,
    limit: usize,
    offset: usize,
  ) -> ApiResult<DashboardPage<DashboardSource>> {
    dashboard_page_bounds(limit, offset)?;
    let db = self.db()?;
    let selection = "SELECT DISTINCT json_extract(target,'$.scope.project_id') AS project_id, json_extract(target,'$.scope.origin') AS origin FROM (SELECT target FROM files UNION SELECT target FROM streams) WHERE json_extract(target,'$.kind')='run'";
    let total_count = db
      .query_row(&format!("SELECT COUNT(*) FROM ({selection})"), [], |row| {
        row.get(0)
      })
      .map_err(database)?;
    let mut statement = db
      .prepare(&format!(
        "{selection} ORDER BY project_id,origin LIMIT ?1 OFFSET ?2"
      ))
      .map_err(database)?;
    let rows = statement
      .query_map(params![limit, offset], |row| {
        Ok(DashboardSource {
          project_id: row.get(0)?,
          origin: row.get(1)?,
        })
      })
      .map_err(database)?;
    let mut items = Vec::with_capacity(limit);
    for row in rows {
      let source = row.map_err(database)?;
      for component in [&source.project_id, &source.origin] {
        validate_component(component)
          .map_err(|_| ApiError::new(500, "invalid stored run scope"))?;
      }
      items.push(source);
    }
    Ok(DashboardPage {
      items,
      total_count,
      legacy_order: false,
    })
  }

  /// A source may exceed the sync CLI's original 500-run convenience listing.
  /// Dashboard readers select a bounded, deterministic page directly in SQLite.
  #[cfg(test)]
  pub fn dashboard_runs(
    &self,
    project_id: &str,
    origin: &str,
    limit: usize,
    offset: usize,
  ) -> ApiResult<DashboardPage<RunScope>> {
    self.dashboard_project_runs(project_id, Some(origin), limit, offset)
  }

  /// Select one project window across all published origins, without per-machine
  /// fanout. An explicit machine selects its own window before other filters.
  pub(in crate::service) fn dashboard_project_runs(
    &self,
    project_id: &str,
    origin: Option<&str>,
    limit: usize,
    offset: usize,
  ) -> ApiResult<DashboardPage<RunScope>> {
    validate_component(project_id).map_err(bad)?;
    if let Some(origin) = origin {
      validate_component(origin).map_err(bad)?;
    }
    dashboard_page_bounds(limit, offset)?;
    let db = self.db()?;
    let selection = "SELECT DISTINCT json_extract(target,'$.scope.origin') AS origin,json_extract(target,'$.scope.run_id') AS run_id FROM (SELECT target FROM files UNION SELECT target FROM streams) WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 AND (?2 IS NULL OR json_extract(target,'$.scope.origin')=?2)";
    let total_count = db
      .query_row(
        &format!("SELECT COUNT(*) FROM ({selection})"),
        params![project_id, origin],
        |row| row.get(0),
      )
      .map_err(database)?;
    let mut statement = db
      .prepare(&format!(
        "SELECT discovered.origin,discovered.run_id, COALESCE(activity.legacy_order,1) FROM ({selection}) AS discovered LEFT JOIN dashboard_run_activity AS activity ON activity.project_id=?1 AND activity.origin=discovered.origin AND activity.run_id=discovered.run_id ORDER BY COALESCE(activity.sequence,0) DESC,discovered.run_id DESC,discovered.origin ASC LIMIT ?3 OFFSET ?4"
      ))
      .map_err(database)?;
    let rows = statement
      .query_map(params![project_id, origin, limit, offset], |row| {
        Ok((
          RunScope {
            project_id: project_id.into(),
            origin: row.get(0)?,
            run_id: row.get(1)?,
          },
          row.get::<_, bool>(2)?,
        ))
      })
      .map_err(database)?;
    let mut items = Vec::with_capacity(limit);
    let mut legacy_order = false;
    for row in rows {
      let (scope, legacy) = row.map_err(database)?;
      legacy_order |= legacy;
      validate_scope(&scope).map_err(|_| ApiError::new(500, "invalid stored run scope"))?;
      items.push(scope);
    }
    Ok(DashboardPage {
      items,
      total_count,
      legacy_order,
    })
  }

  pub fn dashboard_artifact(&self, scope: &RunScope, path: &str) -> ApiResult<Option<FileRecord>> {
    let target = FileTarget::Run {
      scope: scope.clone(),
      path: path.into(),
    };
    match self.file(&target) {
      Ok(record) => Ok(Some(record)),
      Err(error) if error.status == 404 => Ok(None),
      Err(error) => Err(error),
    }
  }

  /// Catalog only completed output objects. No checkpoint bytes or object
  /// storage requests are needed, and pending uploads never claim availability.
  pub fn dashboard_output_objects(&self, scope: &RunScope) -> ApiResult<(Vec<FileRecord>, bool)> {
    validate_scope(scope).map_err(bad)?;
    let db = self.db()?;
    let mut statement = db.prepare("SELECT record FROM files WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3 AND substr(json_extract(target,'$.path'),1,8)='outputs/' AND json_extract(target,'$.path')<>?4 AND json_extract(record,'$.storage')='object' ORDER BY target LIMIT ?5").map_err(database)?;
    let records = statement
      .query_map(
        params![
          scope.project_id,
          scope.origin,
          scope.run_id,
          crate::run_artifacts::INVENTORY_PATH,
          crate::run_artifacts::FILE_LIMIT + 1
        ],
        |row| row.get::<_, String>(0),
      )
      .map_err(database)?;
    let mut files = Vec::new();
    let mut scanned = 0;
    for record in records {
      scanned += 1;
      let file: FileRecord = serde_json::from_str(&record.map_err(database)?)
        .map_err(|_| ApiError::new(500, "invalid stored artifact"))?;
      let FileTarget::Run {
        scope: stored_scope,
        path,
      } = &file.target
      else {
        return Err(ApiError::new(500, "invalid stored artifact scope"));
      };
      if stored_scope != scope {
        return Err(ApiError::new(500, "invalid stored artifact scope"));
      }
      if !matches!(file.storage, FileStorage::Object) {
        continue;
      }
      if crate::run_artifacts::validate_path(path).is_ok() {
        files.push(file);
      }
    }
    let truncated = scanned > crate::run_artifacts::FILE_LIMIT;
    files.truncate(crate::run_artifacts::FILE_LIMIT);
    Ok((files, truncated))
  }

  pub fn dashboard_attachment_url(
    &self,
    target: &FileTarget,
    disposition: &str,
  ) -> ApiResult<(String, u64)> {
    validate_target(target).map_err(bad)?;
    let (key, raw) = self
      .db()?
      .query_row(
        "SELECT object_key,record FROM files WHERE target=?1",
        [target_json(target)?],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
      )
      .optional()
      .map_err(database)?
      .ok_or_else(|| ApiError::new(404, "completed object is missing"))?;
    let record: FileRecord =
      serde_json::from_str(&raw).map_err(|_| ApiError::new(500, "invalid stored artifact"))?;
    if record.target != *target || !matches!(record.storage, FileStorage::Object) {
      return Err(ApiError::new(500, "invalid stored artifact scope"));
    }
    // Capture the object key and its size together before signing. Replacing a
    // published path cannot mix one version's URL with another's size.
    let url = self
      .storage
      .presign_get_attachment(&key, 900, disposition)
      .map_err(|_| ApiError::new(502, "object storage unavailable"))?;
    Ok((url, record.size))
  }

  pub fn dashboard_archive_attachment(&self, scope: &RunScope) -> ApiResult<(String, u64)> {
    validate_scope(scope).map_err(bad)?;
    let target = FileTarget::Run {
      scope: scope.clone(),
      path: "result.zip".into(),
    };
    let (status, key, raw): (String, Option<String>, Option<String>) = self.db()?.query_row(
      "SELECT a.status,f.object_key,f.record FROM result_archives a LEFT JOIN files f ON f.target=?2 WHERE a.scope=?1 ORDER BY a.id DESC LIMIT 1",
      params![serde_json::to_string(scope).map_err(|_|ApiError::new(500,"cannot encode archive scope"))?, target_json(&target)?],
      |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(database)?
      .ok_or_else(||ApiError::new(404,"result archive is not available"))?;
    if status != "archived" {
      return Err(ApiError::new(409, "result archive is not ready"));
    }
    let raw = raw.ok_or_else(|| ApiError::new(404, "result archive is missing"))?;
    let record: FileRecord =
      serde_json::from_str(&raw).map_err(|_| ApiError::new(500, "invalid archive record"))?;
    if record.target != target || !matches!(record.storage, FileStorage::Object) {
      return Err(ApiError::new(500, "invalid archive record"));
    }
    let url = self
      .storage
      .presign_get_attachment(
        &key.ok_or_else(|| ApiError::new(404, "result archive is missing"))?,
        900,
        &crate::dashboard::artifacts::disposition("result.zip"),
      )
      .map_err(|_| ApiError::new(502, "object storage unavailable"))?;
    Ok((url, record.size))
  }

  pub fn dashboard_run_exists(&self, scope: &RunScope) -> ApiResult<bool> {
    validate_scope(scope).map_err(bad)?;
    self.db()?.query_row("SELECT EXISTS(SELECT 1 FROM (SELECT target FROM files UNION ALL SELECT target FROM streams) WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3)", params![scope.project_id, scope.origin, scope.run_id], |row| row.get(0)).map_err(database)
  }

  /// A bounded poll reads only publication sequences and saved stream lengths.
  /// It neither parses artifacts nor contacts object storage. One database lock
  /// keeps source activity and the requested artifact hints in the same view.
  pub fn dashboard_updates(
    &self,
    source: Option<&DashboardSource>,
    run_ids: &[String],
  ) -> ApiResult<serde_json::Value> {
    use crate::dashboard::updates;

    updates::validate_selection(if source.is_some() { "selected" } else { "" }, run_ids)
      .map_err(bad)?;
    if let Some(source) = source {
      validate_component(&source.project_id).map_err(bad)?;
      validate_component(&source.origin).map_err(bad)?;
    }
    for run_id in run_ids {
      validate_component(run_id).map_err(bad)?;
    }
    let runs: Vec<_> = source
      .map(|source| {
        run_ids
          .iter()
          .map(|run_id| {
            (
              run_id.clone(),
              RunScope {
                project_id: source.project_id.clone(),
                origin: source.origin.clone(),
                run_id: run_id.clone(),
              },
            )
          })
          .collect()
      })
      .unwrap_or_default();
    self.dashboard_revision_updates(
      source.map(|source| source.project_id.as_str()),
      source.map(|source| source.origin.as_str()),
      &runs,
    )
  }

  pub(in crate::service) fn dashboard_project_updates(
    &self,
    project_id: &str,
    runs: &[(String, RunScope)],
  ) -> ApiResult<serde_json::Value> {
    self.dashboard_revision_updates(Some(project_id), None, runs)
  }

  fn dashboard_revision_updates(
    &self,
    project_id: Option<&str>,
    origin: Option<&str>,
    selections: &[(String, RunScope)],
  ) -> ApiResult<serde_json::Value> {
    use crate::dashboard::updates::{self, RunUpdate};
    let keys: Vec<_> = selections.iter().map(|(key, _)| key.clone()).collect();
    updates::validate_selection(if project_id.is_some() { "selected" } else { "" }, &keys)
      .map_err(bad)?;
    if let Some(project_id) = project_id {
      validate_component(project_id).map_err(bad)?;
    }
    if let Some(origin) = origin {
      validate_component(origin).map_err(bad)?;
    }
    for (_, scope) in selections {
      validate_scope(scope).map_err(bad)?;
      if Some(scope.project_id.as_str()) != project_id
        || origin.is_some_and(|origin| scope.origin != origin)
      {
        return Err(ApiError::new(
          400,
          "run selection is outside the dashboard source",
        ));
      }
    }
    let db = self.db()?;
    let catalog_revision = db
      .query_row(
        "SELECT revision FROM dashboard_catalog_revision WHERE id=1",
        [],
        |row| row.get::<_, i64>(0),
      )
      .map_err(database)?
      .to_string();
    let source_revision = project_id
      .map(|project_id| {
        db.query_row(
          "SELECT COALESCE(MAX(sequence),0) FROM dashboard_run_activity WHERE project_id=?1 AND (?2 IS NULL OR origin=?2)",
          params![project_id, origin],
          |row| row.get::<_, i64>(0),
        )
        .map(|sequence| sequence.to_string())
        .map_err(database)
      })
      .transpose()?;
    let storage_revision = project_id
      .map(|project_id| {
        db.query_row(
          "SELECT revision FROM project_storage_revisions WHERE project_id=?1",
          [project_id],
          |row| row.get::<_, i64>(0),
        )
        .optional()
        .map(|revision| revision.unwrap_or(0).to_string())
        .map_err(database)
      })
      .transpose()?;
    let mut runs = Vec::with_capacity(selections.len());
    for (run_id, scope) in selections {
      let exists: bool = db
          .query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3 UNION ALL SELECT 1 FROM streams WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3)",
            params![scope.project_id, scope.origin, scope.run_id],
            |row| row.get(0),
          )
          .map_err(database)?;
      if !exists {
        runs.push(RunUpdate::missing(run_id));
        continue;
      }
      let mut metadata_revision = updates::METADATA_PATHS
        .iter()
        .map(|path| dashboard_artifact_revision(&db, scope, path))
        .collect::<ApiResult<Vec<_>>>()?;
      let archive: Option<(i64, String)> = db
        .query_row(
          "SELECT id,status FROM result_archives WHERE scope=?1 ORDER BY id DESC LIMIT 1",
          [serde_json::to_string(&scope)
            .map_err(|_| ApiError::new(500, "cannot encode archive scope"))?],
          |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(database)?;
      metadata_revision.push(archive.map(|(id, status)| format!("archive:{id}:{status}")));
      runs.push(RunUpdate {
        run_id: run_id.clone(),
        metadata_revision: Some(updates::metadata_revision(metadata_revision).map_err(bad)?),
        metrics_revision: dashboard_artifact_revision(&db, scope, "outputs/metrics.jsonl")?,
        stdout_revision: dashboard_artifact_revision(&db, scope, "logs/stdout.log")?,
        stderr_revision: dashboard_artifact_revision(&db, scope, "logs/stderr.log")?,
        missing: false,
      });
    }
    let mut reply =
      updates::response(Some(catalog_revision), source_revision, runs).map_err(bad)?;
    reply["storage_revision"] = serde_json::to_value(storage_revision)
      .map_err(|_| ApiError::new(500, "cannot encode storage revision"))?;
    Ok(reply)
  }

  pub fn dashboard_stream_range(
    &self,
    scope: &RunScope,
    path: &str,
    offset: u64,
    limit: usize,
  ) -> ApiResult<(Vec<u8>, u64)> {
    let target = target_json(&stream_target(scope.clone(), path.into())?)?;
    if limit > STREAM_BATCH {
      return Err(ApiError::new(413, "stream reads are limited to 64 KiB"));
    }
    let db = self.db()?;
    let size = db
      .query_row(
        "SELECT size FROM streams WHERE target=?1",
        [&target],
        |row| row.get::<_, u64>(0),
      )
      .optional()
      .map_err(database)?
      .ok_or_else(|| ApiError::new(409, "stream changed; refresh the run"))?;
    if offset > size {
      return Err(ApiError::new(409, "stream changed; refresh the run"));
    }
    let length = limit.min(usize::try_from(size - offset).unwrap_or(usize::MAX));
    Ok((read_bytes(&db, &target, offset, length)?, size))
  }

  pub fn dashboard_download_url(&self, target: &FileTarget) -> ApiResult<String> {
    match self.execute(Request::DownloadUrl {
      target: target.clone(),
    })? {
      Response::Url { url } => Ok(url),
      _ => Err(ApiError::new(500, "invalid object storage response")),
    }
  }

  /// This disposable cache stores normalized previews, never private inputs or
  /// full inventories. Its version is the existing completed artifact digest.
  pub fn dashboard_cached_overview(
    &self,
    scope: &RunScope,
    version: &str,
  ) -> ApiResult<Option<serde_json::Value>> {
    self.dashboard_cached_record(scope, "run-state.json", version, 16 * 1024)
  }

  pub(in crate::service) fn dashboard_cached_scalars(
    &self,
    scope: &RunScope,
    path: &str,
    version: &str,
  ) -> ApiResult<Option<serde_json::Value>> {
    if !matches!(path, "outputs/params.json" | "outputs/metrics.jsonl") {
      return Err(ApiError::new(400, "invalid scalar cache artifact"));
    }
    self.dashboard_cached_record(scope, path, version, 64 * 1024)
  }

  fn dashboard_cached_record(
    &self,
    scope: &RunScope,
    path: &str,
    version: &str,
    limit: usize,
  ) -> ApiResult<Option<serde_json::Value>> {
    validate_scope(scope).map_err(bad)?;
    let key = target_json(&FileTarget::Run {
      scope: scope.clone(),
      path: path.into(),
    })?;
    let raw: Option<String> = self
      .db()?
      .query_row(
        "SELECT record FROM dashboard_overviews WHERE target=?1 AND version=?2",
        params![key, version],
        |row| row.get(0),
      )
      .optional()
      .map_err(database)?;
    Ok(
      raw
        .filter(|raw| raw.len() <= limit)
        .and_then(|raw| serde_json::from_str(&raw).ok()),
    )
  }

  pub fn dashboard_cache_overview(
    &self,
    scope: &RunScope,
    version: &str,
    record: &serde_json::Value,
  ) -> ApiResult<()> {
    self.dashboard_cache_record(scope, "run-state.json", version, record, 16 * 1024)
  }

  /// Reuse the disposable overview cache for compact table data. Large
  /// projections remain readable but are not retained in this cache.
  pub(in crate::service) fn dashboard_cache_scalars(
    &self,
    scope: &RunScope,
    path: &str,
    version: &str,
    record: &serde_json::Value,
  ) -> ApiResult<()> {
    if !matches!(path, "outputs/params.json" | "outputs/metrics.jsonl") {
      return Err(ApiError::new(400, "invalid scalar cache artifact"));
    }
    if serde_json::to_vec(record)
      .map_err(|_| ApiError::new(500, "cannot encode scalar columns"))?
      .len()
      > 64 * 1024
    {
      return Ok(());
    }
    self.dashboard_cache_record(scope, path, version, record, 64 * 1024)
  }

  fn dashboard_cache_record(
    &self,
    scope: &RunScope,
    path: &str,
    version: &str,
    record: &serde_json::Value,
    limit: usize,
  ) -> ApiResult<()> {
    validate_scope(scope).map_err(bad)?;
    if version.len() > 96 {
      return Err(ApiError::new(400, "invalid overview version"));
    }
    let raw = serde_json::to_string(record)
      .map_err(|_| ApiError::new(500, "cannot encode run overview"))?;
    if raw.len() > limit {
      return Err(ApiError::new(413, "run overview exceeds its cache limit"));
    }
    let key = target_json(&FileTarget::Run {
      scope: scope.clone(),
      path: path.into(),
    })?;
    let mut db = self.db()?;
    let transaction = db.transaction().map_err(database)?;
    projects::ensure_active(&transaction, &scope.project_id)?;
    transaction
      .execute(
        "INSERT OR REPLACE INTO dashboard_overviews(target,version,record) VALUES(?1,?2,?3)",
        params![key, version, raw],
      )
      .map_err(database)?;
    transaction
      .execute("DELETE FROM dashboard_overviews WHERE rowid NOT IN (SELECT rowid FROM dashboard_overviews ORDER BY rowid DESC LIMIT 4096)", [])
      .map_err(database)?;
    transaction.commit().map_err(database)
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
    let project = self.request_project(&request)?;
    let gate = project
      .as_deref()
      .map(|project| self.project_gate(project))
      .transpose()?;
    let _operation = gate
      .as_ref()
      .map(|gate| {
        gate
          .read()
          .map_err(|_| ApiError::new(503, "project operation unavailable"))
      })
      .transpose()?;
    if let Some(project) = &project {
      projects::ensure_active(&*self.db()?, project)?;
    }
    match request {
      Request::ProjectStorage { project_id } => Ok(Response::ProjectStorage {
        stats: self.project_storage(&project_id)?,
      }),
      Request::PreviewProjectDelete { project_id } => Ok(Response::ProjectDeletePreview {
        preview: self.preview_project_delete(&project_id)?,
      }),
      Request::DeleteProject {
        project_id,
        revision,
        confirmation,
      } => Ok(Response::ProjectDeletion {
        deletion: self.delete_project(&project_id, &revision, &confirmation)?,
      }),
      Request::ProjectDeletion { project_id } => Ok(Response::ProjectDeletion {
        deletion: self.project_deletion(&project_id)?,
      }),
      Request::Capabilities => Ok(Response::Capabilities {
        features: vec![
          "tracking-v1".into(),
          "file-references-v1".into(),
          "project-storage-management-v1".into(),
        ],
      }),
      Request::PutDocument {
        scope,
        path,
        revision,
        offset,
        total_size,
        data_base64,
      } => self.put_document(scope, path, revision, offset, total_size, &data_base64),
      Request::AppendTracking {
        scope,
        path,
        offset,
        data_base64,
      } => self.append_tracking(scope, path, offset, &data_base64),
      Request::SealRun {
        scope,
        documents,
        streams,
        incomplete,
      } => Ok(Response::Archive {
        archive: self.seal_tracking(scope, documents, streams, incomplete)?,
      }),
      Request::ArchiveStatus { scope } => Ok(Response::Archive {
        archive: self.archive_status(&scope)?,
      }),
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
      Request::ReferenceFile {
        source,
        target,
        size,
        sha256,
      } => self.reference_file(source, target, size, sha256),
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
    self.reject_managed_tracking(&target)?;
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
    self.reject_managed_tracking(&upload.target)?;
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
    self.reject_managed_tracking(&upload.target)?;
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
    tracking::reject_managed(&transaction, &upload.target)?;
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
    let current_archive = archive::can_publish(&transaction, id, &upload.target)?;
    let published = if current_archive {
      transaction.execute("INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,?3,?4) ON CONFLICT(target) DO UPDATE SET record=excluded.record,object_key=excluded.object_key,sequence=excluded.sequence WHERE files.sequence < excluded.sequence",
      params![target, serde_json::to_string(&record).map_err(|_| ApiError::new(500, "cannot encode artifact"))?, upload.key, upload.sequence]).map_err(database)?
    } else {
      0
    };

    if published != 0 {
      // Publish the immutable object and retire its live copy in one durable transaction.
      transaction
        .execute("DELETE FROM chunks WHERE target=?1", [&target])
        .map_err(database)?;
      transaction
        .execute("DELETE FROM streams WHERE target=?1", [&target])
        .map_err(database)?;
      if let FileTarget::Run { scope, .. } = &upload.target {
        record_run_activity(&transaction, scope)?;
      }
      record_storage_publication(&transaction, &upload.target)?;
    }
    transaction
      .execute("UPDATE uploads SET complete=1 WHERE upload_id=?1", [id])
      .map_err(database)?;
    transaction.commit().map_err(database)?;
    drop(db);
    Ok(Response::File {
      file: if current_archive {
        self.file(&upload.target)?
      } else {
        record
      },
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
    let initialized = transaction
      .execute(
        "INSERT OR IGNORE INTO streams(target,size) VALUES(?1,0)",
        [&encoded],
      )
      .map_err(database)?
      != 0;
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
    if initialized || end > size {
      let FileTarget::Run { scope, .. } = &target else {
        unreachable!("stream targets are runs")
      };
      record_run_activity(&transaction, scope)?;
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
    if let Some(file) = self.tracking_file(&FileTarget::Run {
      scope: scope.clone(),
      path: path.clone(),
    })? {
      if limit > STREAM_BATCH {
        return Err(ApiError::new(413, "stream reads are limited to 64 KiB"));
      }
      if offset > file.size {
        return Err(ApiError::new(409, "stream offset exceeds available bytes"));
      }
      let bytes = self.tracking_range(&file, offset, limit.min((file.size - offset) as usize))?;
      return Ok(Response::Stream {
        offset,
        total_size: file.size,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
      });
    }
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

fn dashboard_page_bounds(limit: usize, offset: usize) -> ApiResult<()> {
  if !(1..=1000).contains(&limit) || i64::try_from(offset).is_err() {
    return Err(ApiError::new(400, "invalid dashboard pagination"));
  }
  Ok(())
}

fn dashboard_artifact_revision(
  db: &Connection,
  scope: &RunScope,
  path: &str,
) -> ApiResult<Option<String>> {
  let target = target_json(&FileTarget::Run {
    scope: scope.clone(),
    path: path.into(),
  })?;
  let object = db
    .query_row(
      "SELECT sequence FROM files WHERE target=?1",
      [&target],
      |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(database)?;
  if let Some(sequence) = object {
    return Ok(Some(format!("object:{sequence}")));
  }
  db.query_row(
    "SELECT size FROM streams WHERE target=?1",
    [&target],
    |row| row.get::<_, u64>(0),
  )
  .optional()
  .map(|size| size.map(|size| format!("stream:{size}")))
  .map_err(database)
}

fn record_run_activity(db: &Connection, scope: &RunScope) -> ApiResult<()> {
  db.execute("INSERT OR REPLACE INTO dashboard_run_activity(project_id,origin,run_id,legacy_order) VALUES(?1,?2,?3,0)", params![scope.project_id,scope.origin,scope.run_id]).map_err(database)?;
  Ok(())
}

/// Upload IDs are allocated at begin, so their sequence cannot represent the
/// order in which files become visible. Advance this counter at publication.
fn record_storage_publication(db: &Connection, target: &FileTarget) -> ApiResult<()> {
  let project_id = match target {
    FileTarget::Input { project_id, .. } => Some(project_id.as_str()),
    FileTarget::Run { scope, path }
      if path != crate::run_artifacts::INVENTORY_PATH
        && crate::run_artifacts::validate_path(path).is_ok() =>
    {
      Some(scope.project_id.as_str())
    }
    _ => None,
  };
  if let Some(project_id) = project_id {
    db.execute(
      "INSERT INTO project_storage_revisions(project_id,revision) VALUES(?1,1) ON CONFLICT(project_id) DO UPDATE SET revision=project_storage_revisions.revision+1",
      [project_id],
    ).map_err(database)?;
  }
  Ok(())
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
    pub(crate) fn stage(&self, id: &str, size: u64) {
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

  #[test]
  fn dashboard_project_windows_aggregate_before_paging_and_machine_filters_choose_their_own_window()
  {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let mut db = store.db().unwrap();
    let tx = db.transaction().unwrap();
    for index in 0..650 {
      let scope = RunScope {
        project_id: "project".into(),
        origin: if index % 2 == 0 {
          "worker-a"
        } else {
          "worker-b"
        }
        .into(),
        run_id: format!("run-{:04}", index / 2),
      };
      let target = FileTarget::Run {
        scope: scope.clone(),
        path: "logs/stdout.log".into(),
      };
      tx.execute(
        "INSERT INTO streams(target,size) VALUES(?1,0)",
        [target_json(&target).unwrap()],
      )
      .unwrap();
      tx.execute("INSERT INTO dashboard_run_activity(project_id,origin,run_id,legacy_order) VALUES(?1,?2,?3,0)",params![scope.project_id,scope.origin,scope.run_id]).unwrap();
    }
    let outside = FileTarget::Run {
      scope: RunScope {
        project_id: "other".into(),
        origin: "worker-a".into(),
        run_id: "run-0324".into(),
      },
      path: "logs/stdout.log".into(),
    };
    tx.execute(
      "INSERT INTO streams(target,size) VALUES(?1,0)",
      [target_json(&outside).unwrap()],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(db);
    let project = store
      .dashboard_project_runs("project", None, 500, 0)
      .unwrap();
    assert_eq!(project.total_count, 650);
    assert_eq!(project.items.len(), 500);
    assert_eq!(project.items[0].origin, "worker-b");
    assert_eq!(project.items[0].run_id, "run-0324");
    assert_eq!(project.items.last().unwrap().run_id, "run-0075");
    assert!(!project.legacy_order);
    let machine = store
      .dashboard_project_runs("project", Some("worker-a"), 500, 0)
      .unwrap();
    assert_eq!(machine.total_count, 325);
    assert_eq!(machine.items.last().unwrap().run_id, "run-0000");
    assert!(
      machine
        .items
        .iter()
        .all(|scope| scope.origin == "worker-a" && scope.project_id == "project")
    );
    let older = store
      .dashboard_project_runs("project", None, 500, 500)
      .unwrap();
    assert_eq!(older.items.len(), 150);
    let outside = RunScope {
      project_id: "other".into(),
      origin: "worker-a".into(),
      run_id: "run-0324".into(),
    };
    assert_eq!(
      store
        .dashboard_project_updates("project", &[("worker-a:run-0324".into(), outside)])
        .unwrap_err()
        .status,
      400
    );
  }

  #[test]
  fn dashboard_scope_pages_are_distinct_bounded_and_exclude_private_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let db = store.db().unwrap();
    for index in 0..1205 {
      let scope = RunScope {
        project_id: "project".into(),
        origin: "worker".into(),
        run_id: format!("run-{index:04}"),
      };
      for path in ["logs/stdout.log", "outputs/metrics.jsonl"] {
        let target = target_json(&FileTarget::Run {
          scope: scope.clone(),
          path: path.into(),
        })
        .unwrap();
        db.execute("INSERT INTO streams(target,size) VALUES(?1,0)", [target])
          .unwrap();
      }
    }
    let input = FileTarget::Input {
      project_id: "private-project".into(),
      input_id: "private-dataset".into(),
    };
    let input_record = FileRecord {
      target: input.clone(),
      size: 999_000,
      sha256: Some("a".repeat(64)),
      storage: FileStorage::Object,
    };
    db.execute(
      "INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,'private-object',1)",
      params![
        target_json(&input).unwrap(),
        serde_json::to_string(&input_record).unwrap()
      ],
    )
    .unwrap();
    let second = FileTarget::Run {
      scope: RunScope {
        project_id: "other-project".into(),
        origin: "rental".into(),
        run_id: "other-run".into(),
      },
      path: "logs/stderr.log".into(),
    };
    db.execute(
      "INSERT INTO streams(target,size) VALUES(?1,0)",
      [target_json(&second).unwrap()],
    )
    .unwrap();
    drop(db);
    let sources = store.dashboard_sources(1, 0).unwrap();
    assert_eq!(sources.total_count, 2);
    assert_eq!(sources.items.len(), 1);
    assert_eq!(sources.items[0].project_id, "other-project");
    let sources = store.dashboard_sources(1, 1).unwrap();
    assert_eq!(sources.items[0].project_id, "project");
    let first = store.dashboard_runs("project", "worker", 1000, 0).unwrap();
    assert_eq!(first.total_count, 1205);
    assert_eq!(first.items.len(), 1000);
    assert_eq!(first.items[0].run_id, "run-1204");
    let rest = store
      .dashboard_runs("project", "worker", 1000, 1000)
      .unwrap();
    assert_eq!(rest.items.len(), 205);
    assert_eq!(rest.items[0].run_id, "run-0204");
    assert_eq!(rest.items[204].run_id, "run-0000");
    assert!(
      store
        .dashboard_runs("project", "rental", 10, 0)
        .unwrap()
        .items
        .is_empty()
    );
    assert!(store.dashboard_sources(0, 0).is_err());
    assert!(store.dashboard_sources(1001, 0).is_err());
    assert!(store.dashboard_runs("../project", "worker", 1, 0).is_err());
  }

  #[test]
  fn dashboard_stream_ranges_read_only_the_requested_extent() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    store
      .execute(append_request(0, b"first\nsecond\n"))
      .unwrap();
    let (bytes, size) = store
      .dashboard_stream_range(&scope(), "outputs/metrics.jsonl", 6, 6)
      .unwrap();
    assert_eq!(bytes, b"second");
    assert_eq!(size, 13);
    assert_eq!(
      store
        .dashboard_stream_range(&scope(), "outputs/metrics.jsonl", 0, STREAM_BATCH + 1)
        .unwrap_err()
        .status,
      413
    );
    assert_eq!(
      store
        .dashboard_stream_range(&scope(), "outputs/metrics.jsonl", 14, 1)
        .unwrap_err()
        .status,
      409
    );
    assert!(
      store
        .dashboard_artifact(&scope(), "run-state.json")
        .unwrap()
        .is_none()
    );
  }

  #[test]
  fn dashboard_update_hints_track_scoped_activity_and_missing_membership() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let source = DashboardSource {
      project_id: "project".into(),
      origin: "worker".into(),
    };
    let ids = vec!["run-1".into(), "not-yet-synced".into()];
    let empty = store.dashboard_updates(Some(&source), &ids).unwrap();
    assert_eq!(empty["source_revision"], "0");
    assert!(
      empty["runs"]
        .as_array()
        .unwrap()
        .iter()
        .all(|run| run["missing"] == true)
    );
    store
      .execute(append_request(0, b"unparsed metrics"))
      .unwrap();
    let first = store.dashboard_updates(Some(&source), &ids).unwrap();
    assert_eq!(first["runs"][0]["metrics_revision"], "stream:16");
    let mut other = scope();
    other.origin = "other-worker".into();
    store
      .execute(Request::AppendStream {
        scope: other,
        path: "logs/stdout.log".into(),
        offset: 0,
        data_base64: base64::engine::general_purpose::STANDARD.encode(b"private to other scope"),
      })
      .unwrap();
    let scoped = store.dashboard_updates(Some(&source), &ids).unwrap();
    assert_ne!(scoped["catalog_revision"], first["catalog_revision"]);
    assert_eq!(scoped["source_revision"], first["source_revision"]);
    assert_eq!(scoped["runs"], first["runs"]);
    let target = target_json(&FileTarget::Run {
      scope: scope(),
      path: "outputs/metrics.jsonl".into(),
    })
    .unwrap();
    store
      .db()
      .unwrap()
      .execute("DELETE FROM streams WHERE target=?1", [&target])
      .unwrap();
    let deleted = store.dashboard_updates(Some(&source), &ids).unwrap();
    assert_eq!(deleted["runs"][0]["missing"], true);
    assert_eq!(
      deleted["runs"][0]["metrics_revision"],
      serde_json::Value::Null
    );
    assert!(
      store
        .dashboard_updates(Some(&source), &["run-1".into(), "run-1".into()])
        .is_err()
    );
    assert!(
      store
        .dashboard_updates(Some(&source), &["..".into()])
        .is_err()
    );
  }

  #[test]
  fn storage_revision_tracks_publication_order_overwrites_and_ignores_suppressed_uploads() {
    let directory = tempfile::tempdir().unwrap();
    let backend = MockStorage::default();
    let store = Store::open(directory.path(), backend.clone()).unwrap();
    let source = DashboardSource {
      project_id: "project".into(),
      origin: "worker".into(),
    };
    {
      let revision = || {
        store.dashboard_updates(Some(&source), &[]).unwrap()["storage_revision"]
          .as_str()
          .unwrap()
          .parse::<u64>()
          .unwrap()
      };
      let begin = |id: &str, path: &str| {
        store
          .execute(Request::BeginUpload {
            upload_id: id.into(),
            target: FileTarget::Run {
              scope: scope(),
              path: path.into(),
            },
            size: 1,
            sha256: "a".repeat(64),
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
        backend.stage(id, 1);
      };
      let complete = |id: &str| {
        store
          .execute(Request::CompleteUpload {
            upload_id: id.into(),
          })
          .unwrap();
      };
      assert_eq!(revision(), 0);
      begin("older-start", "outputs/early.pt");
      begin("newer-start", "outputs/later.pt");
      complete("newer-start");
      assert_eq!(revision(), 1);
      complete("older-start");
      assert_eq!(
        revision(),
        2,
        "late completion must advance the project revision"
      );
      begin("replace-existing", "outputs/early.pt");
      begin("new-high-sequence", "outputs/new.pt");
      complete("new-high-sequence");
      assert_eq!(revision(), 3);
      complete("replace-existing");
      assert_eq!(
        revision(),
        4,
        "an overwrite older than another upload must refresh Storage"
      );
      complete("replace-existing");
      assert_eq!(
        revision(),
        4,
        "idempotent completion must not advance revision"
      );
      begin("suppressed-old", "outputs/late.pt");
      begin("winning-new", "outputs/late.pt");
      complete("winning-new");
      assert_eq!(revision(), 5);
      complete("suppressed-old");
      assert_eq!(
        revision(),
        5,
        "an upload that did not publish must not advance revision"
      );
      begin("inventory", crate::run_artifacts::INVENTORY_PATH);
      complete("inventory");
      assert_eq!(
        revision(),
        5,
        "the internal artifact manifest is not a Storage row"
      );
    }
    drop(store);
    let reopened = Store::open(directory.path(), backend).unwrap();
    assert_eq!(
      reopened.dashboard_updates(Some(&source), &[]).unwrap()["storage_revision"],
      "5"
    );
  }

  #[test]
  fn dashboard_overview_cache_is_versioned_persistent_and_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let backend = MockStorage::default();
    let store = Store::open(directory.path(), backend.clone()).unwrap();
    let record =
      serde_json::json!({"run": {"run_id": "run-one", "status": "running"}, "warnings": []});
    store
      .dashboard_cache_overview(&scope(), "old-digest", &record)
      .unwrap();
    assert_eq!(
      store
        .dashboard_cached_overview(&scope(), "old-digest")
        .unwrap(),
      Some(record.clone())
    );
    assert!(
      store
        .dashboard_cached_overview(&scope(), "new-digest")
        .unwrap()
        .is_none()
    );
    assert_eq!(
      store
        .dashboard_cache_overview(
          &scope(),
          "new-digest",
          &serde_json::json!({"oversized": "x".repeat(16 * 1024)})
        )
        .unwrap_err()
        .status,
      413
    );
    {
      let mut db = store.db().unwrap();
      let transaction = db.transaction().unwrap();
      for index in 0..4096 {
        transaction
          .execute(
            "INSERT INTO dashboard_overviews(target,version,record) VALUES(?1,'old','{}')",
            [format!("cache-{index}")],
          )
          .unwrap();
      }
      transaction.commit().unwrap();
    }
    store
      .dashboard_cache_overview(&scope(), "new-digest", &record)
      .unwrap();
    assert_eq!(
      store
        .db()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM dashboard_overviews", [], |row| row
          .get::<_, usize>(
          0
        ))
        .unwrap(),
      4096
    );
    drop(store);
    let reopened = Store::open(directory.path(), backend).unwrap();
    assert_eq!(
      reopened
        .dashboard_cached_overview(&scope(), "new-digest")
        .unwrap(),
      Some(record)
    );
    assert!(
      reopened
        .dashboard_cached_overview(&scope(), "old-digest")
        .unwrap()
        .is_none()
    );
  }

  #[test]
  fn dashboard_activity_order_uses_received_updates_and_duplicate_appends_do_not_bump_it() {
    let directory = tempfile::tempdir().unwrap();
    let backend = MockStorage::default();
    let store = Store::open(directory.path(), backend.clone()).unwrap();
    let append = |run_id: &str, offset, data: &[u8]| Request::AppendStream {
      scope: RunScope {
        project_id: "project".into(),
        origin: "worker".into(),
        run_id: run_id.into(),
      },
      path: "outputs/metrics.jsonl".into(),
      offset,
      data_base64: base64::engine::general_purpose::STANDARD.encode(data),
    };
    store.execute(append("run-zzz", 0, b"x")).unwrap();
    store.execute(append("run-aaa", 0, b"x")).unwrap();
    let latest = store.dashboard_runs("project", "worker", 1, 0).unwrap();
    assert_eq!(latest.items[0].run_id, "run-aaa");
    assert!(!latest.legacy_order);
    store.execute(append("run-zzz", 0, b"x")).unwrap();
    assert_eq!(
      store
        .dashboard_runs("project", "worker", 1, 0)
        .unwrap()
        .items[0]
        .run_id,
      "run-aaa"
    );
    store.execute(append("run-zzz", 1, b"y")).unwrap();
    assert_eq!(
      store
        .dashboard_runs("project", "worker", 1, 0)
        .unwrap()
        .items[0]
        .run_id,
      "run-zzz"
    );
    drop(store);
    let reopened = Store::open(directory.path(), backend).unwrap();
    let latest = reopened.dashboard_runs("project", "worker", 1, 0).unwrap();
    assert_eq!(latest.items[0].run_id, "run-zzz");
    assert!(!latest.legacy_order);
  }
}
