//! Project accounting, permanent deletion fences, and retryable cleanup work.
use std::sync::TryLockError;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::service::storage::{CleanupError, CleanupResult};

mod local_storage;

const TARGET_PROJECT: &str =
  "COALESCE(json_extract(target,'$.scope.project_id'),json_extract(target,'$.project_id'))";

pub(super) fn initialize(db: &Connection) -> crate::error::Result<()> {
  db.execute_batch("CREATE TABLE IF NOT EXISTS project_change_revisions(project_id TEXT PRIMARY KEY,revision INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS project_deletions(project_id TEXT PRIMARY KEY,revision TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'pending',last_error TEXT);
    CREATE TABLE IF NOT EXISTS project_cleanup_tasks(id INTEGER PRIMARY KEY AUTOINCREMENT,project_id TEXT NOT NULL,kind TEXT NOT NULL,value TEXT NOT NULL,multipart TEXT NOT NULL DEFAULT '',done INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,last_error TEXT,needs_attention INTEGER NOT NULL DEFAULT 0,UNIQUE(project_id,kind,value,multipart));
    CREATE INDEX IF NOT EXISTS project_cleanup_ready ON project_cleanup_tasks(done,retry_at,id);
    CREATE TABLE IF NOT EXISTS dashboard_catalog_revision(id INTEGER PRIMARY KEY CHECK(id=1),revision INTEGER NOT NULL);
    INSERT OR IGNORE INTO dashboard_catalog_revision(id,revision) SELECT 1,COALESCE(MAX(sequence),0) FROM dashboard_run_activity;")
    .map_err(|_| crate::error::ExpriError::Message("cannot initialize project management".into()))?;
  initialize_cleanup_failures(db).map_err(|_| {
    crate::error::ExpriError::Message("cannot initialize project cleanup recovery".into())
  })?;
  for event in ["INSERT", "UPDATE", "DELETE"] {
    let row = if event == "DELETE" { "OLD" } else { "NEW" };
    for (table, expression) in [
      (
        "files",
        "COALESCE(json_extract({row}.target,'$.scope.project_id'),json_extract({row}.target,'$.project_id'))",
      ),
      (
        "uploads",
        "COALESCE(json_extract({row}.target,'$.scope.project_id'),json_extract({row}.target,'$.project_id'))",
      ),
      ("streams", "json_extract({row}.target,'$.scope.project_id')"),
      (
        "tracking_versions",
        "json_extract({row}.target,'$.scope.project_id')",
      ),
      (
        "tracking_run_seals",
        "json_extract({row}.scope,'$.project_id')",
      ),
      (
        "result_archives",
        "json_extract({row}.scope,'$.project_id')",
      ),
    ] {
      let project = expression.replace("{row}", row);
      db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS project_revision_{table}_{event} AFTER {event} ON {table} BEGIN
        INSERT INTO project_change_revisions(project_id,revision) VALUES({project},1) ON CONFLICT(project_id) DO UPDATE SET revision=revision+1; END;"))
        .map_err(|_| crate::error::ExpriError::Message("cannot initialize project revision tracking".into()))?;
      if matches!(table, "files" | "uploads") {
        let keys = if event == "UPDATE" {
          "OLD.object_key,NEW.object_key".to_string()
        } else {
          format!("{row}.object_key")
        };
        db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS shared_project_revision_{table}_{event} AFTER {event} ON {table} BEGIN
          INSERT INTO project_change_revisions(project_id,revision)
          SELECT project_id,1 FROM (SELECT DISTINCT {TARGET_PROJECT} AS project_id FROM files WHERE object_key<>'' AND object_key IN ({keys})
            UNION SELECT DISTINCT {TARGET_PROJECT} FROM uploads WHERE object_key<>'' AND object_key IN ({keys}))
          WHERE project_id IS NOT NULL AND project_id IS NOT {project}
          ON CONFLICT(project_id) DO UPDATE SET revision=revision+1; END;"))
          .map_err(|_| crate::error::ExpriError::Message("cannot initialize shared object revisions".into()))?;
      }
    }
    db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS catalog_revision_{event} AFTER {event} ON dashboard_run_activity BEGIN UPDATE dashboard_catalog_revision SET revision=revision+1 WHERE id=1; END;"))
      .map_err(|_| crate::error::ExpriError::Message("cannot initialize catalog revision tracking".into()))?;
  }
  Ok(())
}

fn initialize_cleanup_failures(db: &Connection) -> rusqlite::Result<()> {
  let columns = db
    .prepare("PRAGMA table_info(project_cleanup_tasks)")?
    .query_map([], |row| row.get::<_, String>(1))?
    .collect::<rusqlite::Result<Vec<_>>>()?;
  let transaction = db.unchecked_transaction()?;
  if !columns.iter().any(|column| column == "last_error") {
    transaction.execute_batch("ALTER TABLE project_cleanup_tasks ADD COLUMN last_error TEXT;
      UPDATE project_cleanup_tasks SET last_error='Cleanup could not complete; check storage and network. Automatic retries continue.' WHERE done=0 AND retry_at>0;")?;
  }
  if !columns.iter().any(|column| column == "needs_attention") {
    transaction.execute_batch(
      "ALTER TABLE project_cleanup_tasks ADD COLUMN needs_attention INTEGER NOT NULL DEFAULT 0;",
    )?;
  }
  transaction.execute_batch(&format!(
    "UPDATE project_deletions SET {};",
    deletion_state_update()
  ))?;
  transaction.commit()
}

fn deletion_state_update() -> &'static str {
  "status=CASE
    WHEN EXISTS(SELECT 1 FROM project_cleanup_tasks WHERE project_id=project_deletions.project_id AND done=0 AND needs_attention=1) THEN 'needs_attention'
    WHEN EXISTS(SELECT 1 FROM project_cleanup_tasks WHERE project_id=project_deletions.project_id AND done=0) THEN 'pending'
    ELSE 'deleted' END,
  last_error=(SELECT last_error FROM project_cleanup_tasks WHERE project_id=project_deletions.project_id AND done=0 AND last_error IS NOT NULL ORDER BY needs_attention DESC,id LIMIT 1)"
}

pub(super) fn ensure_active(db: &Connection, project_id: &str) -> ApiResult<()> {
  validate_component(project_id).map_err(bad)?;
  let deleted: bool = db
    .query_row(
      "SELECT EXISTS(SELECT 1 FROM project_deletions WHERE project_id=?1)",
      [project_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  if deleted {
    Err(ApiError::new(
      410,
      "project has been deleted; use a new project identifier",
    ))
  } else {
    Ok(())
  }
}

fn project_of(target: &FileTarget) -> &str {
  match target {
    FileTarget::Run { scope, .. } => &scope.project_id,
    FileTarget::Input { project_id, .. } => project_id,
  }
}

fn revision(db: &Connection, project_id: &str) -> ApiResult<String> {
  db.query_row(
    "SELECT revision FROM project_change_revisions WHERE project_id=?1",
    [project_id],
    |row| row.get::<_, u64>(0),
  )
  .optional()
  .map_err(database)
  .map(|revision| revision.unwrap_or(0).to_string())
}

fn count_sum(db: &Connection, query: &str, project_id: &str) -> ApiResult<(u64, u64)> {
  db.query_row(query, [project_id], |row| Ok((row.get(0)?, row.get(1)?)))
    .map_err(database)
}

fn stats(directory: &Path, db: &Connection, project_id: &str) -> ApiResult<ProjectStorageStats> {
  let current = format!(
    "SELECT object_key,MAX(json_extract(record,'$.size')) AS size FROM files WHERE {TARGET_PROJECT}=?1 AND json_extract(record,'$.storage')='object' GROUP BY object_key"
  );
  let retained = format!(
    "SELECT object_key,MAX(size) AS size FROM uploads WHERE {TARGET_PROJECT}=?1 AND complete=1 AND object_key NOT IN (SELECT object_key FROM files WHERE {TARGET_PROJECT}=?1 AND json_extract(record,'$.storage')='object') GROUP BY object_key"
  );
  let candidates = format!(
    "SELECT object_key,MAX(size) AS size FROM ({current} UNION ALL {retained}) GROUP BY object_key"
  );
  let protected = outside_references("candidate.object_key");
  let (file_count, logical_bytes) = count_sum(
    db,
    &format!(
      "SELECT COUNT(*),COALESCE(SUM(json_extract(record,'$.size')),0) FROM files WHERE {TARGET_PROJECT}=?1 AND json_extract(record,'$.storage')='object'"
    ),
    project_id,
  )?;
  let (object_count, object_bytes) = count_sum(
    db,
    &format!("SELECT COUNT(*),COALESCE(SUM(size),0) FROM ({current})"),
    project_id,
  )?;
  let (retained_object_count, retained_object_bytes) = count_sum(
    db,
    &format!("SELECT COUNT(*),COALESCE(SUM(size),0) FROM ({retained})"),
    project_id,
  )?;
  let (s3_object_count, s3_storage_bytes) = count_sum(
    db,
    &format!("SELECT COUNT(*),COALESCE(SUM(size),0) FROM ({candidates})"),
    project_id,
  )?;
  let (pending_upload_count, pending_upload_bytes) = count_sum(
    db,
    &format!(
      "SELECT COUNT(*),COALESCE(SUM(size),0) FROM uploads WHERE {TARGET_PROJECT}=?1 AND complete=0"
    ),
    project_id,
  )?;
  let (reclaimable_object_count, reclaimable_object_bytes) = count_sum(
    db,
    &format!(
      "SELECT COUNT(*),COALESCE(SUM(size),0) FROM ({candidates}) AS candidate WHERE NOT ({protected})"
    ),
    project_id,
  )?;
  let tracking_bytes = db.query_row(&format!("SELECT COALESCE(SUM(size),0) FROM (SELECT size FROM tracking_versions WHERE {TARGET_PROJECT}=?1 UNION ALL SELECT size FROM streams WHERE {TARGET_PROJECT}=?1)"), [project_id], |row| row.get(0)).map_err(database)?;
  let local_archive_bytes = local_storage::archive_bytes(directory, db, project_id)?;
  Ok(ProjectStorageStats {
    project_id: project_id.into(),
    revision: revision(db, project_id)?,
    file_count,
    logical_bytes,
    object_count,
    object_bytes,
    shared_reference_count: file_count.saturating_sub(object_count),
    retained_object_count,
    retained_object_bytes,
    s3_object_count: Some(s3_object_count),
    s3_storage_bytes: Some(s3_storage_bytes),
    pending_upload_count,
    pending_upload_bytes,
    tracking_bytes,
    local_archive_bytes: Some(local_archive_bytes),
    local_storage_bytes: Some(tracking_bytes.saturating_add(local_archive_bytes)),
    reclaimable_object_count,
    reclaimable_object_bytes,
  })
}

fn outside_references(key: &str) -> String {
  format!(
    "EXISTS(SELECT 1 FROM files AS foreign_file WHERE foreign_file.object_key={key} AND COALESCE(json_extract(foreign_file.target,'$.scope.project_id'),json_extract(foreign_file.target,'$.project_id')) IS NOT ?1) OR EXISTS(SELECT 1 FROM uploads AS foreign_upload WHERE foreign_upload.object_key={key} AND COALESCE(json_extract(foreign_upload.target,'$.scope.project_id'),json_extract(foreign_upload.target,'$.project_id')) IS NOT ?1)"
  )
}

fn preview(directory: &Path, db: &Connection, project_id: &str) -> ApiResult<ProjectDeletePreview> {
  ensure_active(db, project_id)?;
  let exists: bool = db
    .query_row(
      &format!(
        "SELECT EXISTS(SELECT 1 FROM files WHERE {TARGET_PROJECT}=?1
    UNION ALL SELECT 1 FROM uploads WHERE {TARGET_PROJECT}=?1
    UNION ALL SELECT 1 FROM streams WHERE {TARGET_PROJECT}=?1
    UNION ALL SELECT 1 FROM tracking_versions WHERE {TARGET_PROJECT}=?1
    UNION ALL SELECT 1 FROM result_archives WHERE json_extract(scope,'$.project_id')=?1)"
      ),
      [project_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  if !exists {
    return Err(ApiError::new(404, "project is missing"));
  }
  let stats = stats(directory, db, project_id)?;
  let run_count = db.query_row("SELECT COUNT(*) FROM (SELECT DISTINCT origin,run_id FROM (
    SELECT json_extract(target,'$.scope.origin') AS origin,json_extract(target,'$.scope.run_id') AS run_id FROM files WHERE json_extract(target,'$.scope.project_id')=?1
    UNION ALL SELECT json_extract(target,'$.scope.origin'),json_extract(target,'$.scope.run_id') FROM uploads WHERE json_extract(target,'$.scope.project_id')=?1
    UNION ALL SELECT json_extract(target,'$.scope.origin'),json_extract(target,'$.scope.run_id') FROM streams WHERE json_extract(target,'$.scope.project_id')=?1
    UNION ALL SELECT json_extract(target,'$.scope.origin'),json_extract(target,'$.scope.run_id') FROM tracking_versions WHERE json_extract(target,'$.scope.project_id')=?1
    UNION ALL SELECT json_extract(scope,'$.origin'),json_extract(scope,'$.run_id') FROM result_archives WHERE json_extract(scope,'$.project_id')=?1))", [project_id], |row| row.get(0)).map_err(database)?;
  Ok(ProjectDeletePreview {
    project_id: project_id.into(),
    revision: stats.revision.clone(),
    run_count,
    stats,
  })
}

fn status(db: &Connection, project_id: &str) -> ApiResult<ProjectDeletionStatus> {
  let row: Option<(String, Option<String>)> = db
    .query_row(
      "SELECT status,last_error FROM project_deletions WHERE project_id=?1",
      [project_id],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)?;
  let (status, last_error) =
    row.ok_or_else(|| ApiError::new(404, "project deletion is missing"))?;
  let (pending_tasks, deleted_objects, aborted_uploads) = db.query_row("SELECT COALESCE(SUM(done=0),0),COALESCE(SUM(done=1 AND kind='object'),0),COALESCE(SUM(done=1 AND kind='multipart'),0) FROM project_cleanup_tasks WHERE project_id=?1", [project_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).map_err(database)?;
  Ok(ProjectDeletionStatus {
    project_id: project_id.into(),
    status,
    pending_tasks,
    deleted_objects,
    aborted_uploads,
    last_error,
  })
}

impl<S: ObjectStorage> Store<S> {
  pub(super) fn project_gate(&self, project_id: &str) -> ApiResult<Arc<RwLock<()>>> {
    validate_component(project_id).map_err(bad)?;
    let mut gates = self
      .project_gates
      .lock()
      .map_err(|_| ApiError::new(503, "project operation unavailable"))?;
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(project_id).and_then(Weak::upgrade) {
      return Ok(gate);
    }
    let gate = Arc::new(RwLock::new(()));
    gates.insert(project_id.into(), Arc::downgrade(&gate));
    Ok(gate)
  }

  pub(super) fn request_project(&self, request: &Request) -> ApiResult<Option<String>> {
    let project = match request {
      Request::Capabilities
      | Request::ProjectStorage { .. }
      | Request::PreviewProjectDelete { .. }
      | Request::DeleteProject { .. }
      | Request::ProjectDeletion { .. } => return Ok(None),
      Request::BeginUpload { target, .. }
      | Request::GetFile { target }
      | Request::DownloadUrl { target } => project_of(target),
      Request::ReferenceFile { target, .. } => project_of(target),
      Request::PartUrl { upload_id, .. }
      | Request::RecordPart { upload_id, .. }
      | Request::CompleteUpload { upload_id } => {
        return Ok(Some(project_of(&self.upload_target(upload_id)?).into()));
      }
      Request::PutDocument { scope, .. }
      | Request::AppendTracking { scope, .. }
      | Request::SealRun { scope, .. }
      | Request::ArchiveStatus { scope }
      | Request::ListFiles { scope }
      | Request::AppendStream { scope, .. }
      | Request::ReadStream { scope, .. } => &scope.project_id,
      Request::ListRuns { project_id, .. } => project_id,
    };
    Ok(Some(project.into()))
  }

  pub fn project_storage(&self, project_id: &str) -> ApiResult<ProjectStorageStats> {
    validate_component(project_id).map_err(bad)?;
    stats(&self.directory, &*self.db()?, project_id)
  }

  pub fn preview_project_delete(&self, project_id: &str) -> ApiResult<ProjectDeletePreview> {
    validate_component(project_id).map_err(bad)?;
    preview(&self.directory, &*self.db()?, project_id)
  }

  pub fn project_deletion(&self, project_id: &str) -> ApiResult<ProjectDeletionStatus> {
    validate_component(project_id).map_err(bad)?;
    status(&*self.db()?, project_id)
  }

  pub fn delete_project(
    &self,
    project_id: &str,
    expected_revision: &str,
    confirmation: &str,
  ) -> ApiResult<ProjectDeletionStatus> {
    validate_component(project_id).map_err(bad)?;
    if confirmation != project_id {
      return Err(ApiError::new(400, "confirm the exact project identifier"));
    }
    let gate = self.project_gate(project_id)?;
    let _operation = match gate.try_write() {
      Ok(operation) => operation,
      Err(TryLockError::WouldBlock) => {
        return Err(ApiError::new(
          409,
          "project operation is busy; retry deletion",
        ));
      }
      Err(_) => return Err(ApiError::new(503, "project operation unavailable")),
    };
    let mut db = self.db()?;
    if db
      .query_row(
        "SELECT EXISTS(SELECT 1 FROM project_deletions WHERE project_id=?1)",
        [project_id],
        |row| row.get::<_, bool>(0),
      )
      .map_err(database)?
    {
      return status(&db, project_id);
    }
    let transaction = db.transaction().map_err(database)?;
    let selected = preview(&self.directory, &transaction, project_id)?;
    if selected.revision != expected_revision {
      return Err(ApiError::new(
        409,
        "project changed; review a fresh deletion preview",
      ));
    }
    transaction
      .execute(
        "INSERT INTO project_deletions(project_id,revision) VALUES(?1,?2)",
        params![project_id, expected_revision],
      )
      .map_err(database)?;
    let protected = outside_references("candidate.object_key");
    transaction.execute(&format!("INSERT OR IGNORE INTO project_cleanup_tasks(project_id,kind,value)
      SELECT ?1,'object',object_key FROM (SELECT object_key FROM files WHERE {TARGET_PROJECT}=?1 AND json_extract(record,'$.storage')='object' UNION SELECT object_key FROM uploads WHERE {TARGET_PROJECT}=?1) AS candidate WHERE NOT ({protected})"), [project_id]).map_err(database)?;
    transaction.execute(&format!("INSERT OR IGNORE INTO project_cleanup_tasks(project_id,kind,value,multipart) SELECT ?1,'multipart',object_key,multipart FROM uploads AS candidate WHERE {TARGET_PROJECT}=?1 AND complete=0 AND multipart IS NOT NULL AND NOT ({protected})"), [project_id]).map_err(database)?;
    transaction.execute("INSERT OR IGNORE INTO project_cleanup_tasks(project_id,kind,value) VALUES(?1,'tracking',?1)", [project_id]).map_err(database)?;
    transaction.execute("INSERT OR IGNORE INTO project_cleanup_tasks(project_id,kind,value) SELECT ?1,'archive',CAST(id AS TEXT) FROM result_archives WHERE json_extract(scope,'$.project_id')=?1", [project_id]).map_err(database)?;
    transaction.execute(&format!("DELETE FROM parts WHERE upload_id IN (SELECT upload_id FROM uploads WHERE {TARGET_PROJECT}=?1)"), [project_id]).map_err(database)?;
    for table in [
      "chunks",
      "streams",
      "dashboard_overviews",
      "tracking_metric_rows",
      "tracking_metric_summaries",
      "tracking_metric_state",
      "tracking_versions",
      "files",
      "uploads",
    ] {
      transaction
        .execute(
          &format!("DELETE FROM {table} WHERE {TARGET_PROJECT}=?1"),
          [project_id],
        )
        .map_err(database)?;
    }
    for table in ["tracking_run_seals", "result_archives"] {
      transaction
        .execute(
          &format!("DELETE FROM {table} WHERE json_extract(scope,'$.project_id')=?1"),
          [project_id],
        )
        .map_err(database)?;
    }
    transaction
      .execute(
        "DELETE FROM dashboard_run_activity WHERE project_id=?1",
        [project_id],
      )
      .map_err(database)?;
    transaction
      .execute(
        "DELETE FROM project_storage_revisions WHERE project_id=?1",
        [project_id],
      )
      .map_err(database)?;
    transaction
      .execute(
        "UPDATE dashboard_catalog_revision SET revision=revision+1 WHERE id=1",
        [],
      )
      .map_err(database)?;
    transaction.commit().map_err(database)?;
    status(&db, project_id)
  }

  pub fn project_deletion_cycle(&self) -> ApiResult<bool> {
    let task: Option<(i64,String,String,String,String)> = self.db()?.query_row("SELECT id,project_id,kind,value,multipart FROM project_cleanup_tasks WHERE done=0 AND retry_at<=?1 ORDER BY CASE kind WHEN 'multipart' THEN 0 WHEN 'object' THEN 1 ELSE 2 END,id LIMIT 1", [now()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional().map_err(database)?;
    let Some((id, project_id, kind, value, multipart)) = task else {
      return Ok(false);
    };
    let gate = self.upload_gate(&format!("project-cleanup-{project_id}"))?;
    let _operation = match gate.try_lock() {
      Ok(operation) => operation,
      Err(TryLockError::WouldBlock) => return Ok(false),
      Err(_) => return Err(ApiError::new(503, "project cleanup unavailable")),
    };
    // Another cleanup worker may have completed or delayed this task while the gate was acquired.
    let unavailable: bool = self
      .db()?
      .query_row(
        "SELECT done=1 OR retry_at>?2 FROM project_cleanup_tasks WHERE id=?1",
        params![id, now()],
        |row| row.get(0),
      )
      .map_err(database)?;
    if unavailable {
      return Ok(true);
    }
    let mut preserved = false;
    let result = match kind.as_str() {
      "object" | "multipart" => {
        let protected: bool = self
          .db()?
          .query_row(
            &format!("SELECT {}", outside_references("?2")),
            params![project_id, value],
            |row| row.get(0),
          )
          .map_err(database)?;
        if protected {
          preserved = true;
          Ok(())
        } else if kind == "object" {
          self.storage.delete_object(&value)
        } else {
          self.storage.abort_upload(&value, &multipart)
        }
      }
      "tracking" => remove_local_tree(&self.directory, "tracking", &value),
      "archive" => remove_local_tree(&self.directory, "archives", &value),
      _ => Err(CleanupError::needs_attention(
        "Cleanup task is invalid; check the service configuration or upgrade the service.",
      )),
    };
    let mut db = self.db()?;
    let transaction = db.transaction().map_err(database)?;
    match &result {
      Ok(()) => {
        transaction
          .execute(
            "UPDATE project_cleanup_tasks SET done=1,retry_at=0,last_error=NULL,needs_attention=0,kind=CASE WHEN ?2 THEN 'preserved' ELSE kind END WHERE id=?1",
            params![id, preserved],
          )
          .map_err(database)?;
      }
      Err(error) => {
        let delay = if error.is_needs_attention() { 300 } else { 30 };
        transaction
          .execute(
            "UPDATE project_cleanup_tasks SET retry_at=?2,last_error=?3,needs_attention=?4 WHERE id=?1",
            params![id, now().saturating_add(delay), error.to_string(), error.is_needs_attention()],
          )
          .map_err(database)?;
      }
    }
    transaction
      .execute(
        &format!(
          "UPDATE project_deletions SET {} WHERE project_id=?1",
          deletion_state_update()
        ),
        [&project_id],
      )
      .map_err(database)?;
    transaction.commit().map_err(database)?;
    result
      .map(|()| true)
      .map_err(|error| ApiError::new(502, error.to_string()))
  }
}

fn now() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs()
    .min(i64::MAX as u64) as i64
}

/// Delete only descriptor-relative entries; a swapped parent link cannot escape the service directory.
fn remove_local_tree(directory: &Path, parent: &str, name: &str) -> CleanupResult<()> {
  use std::os::fd::AsRawFd;
  use std::os::unix::fs::OpenOptionsExt;
  validate_component(name).map_err(|_| {
    CleanupError::needs_attention(
      "Cleanup directory is invalid; check the service configuration or upgrade the service.",
    )
  })?;
  let base = fs::OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
    .open(directory)
    .map_err(local_error)?;
  let parent_name = std::ffi::CString::new(parent).expect("fixed cleanup parent");
  let fd = unsafe {
    libc::openat(
      base.as_raw_fd(),
      parent_name.as_ptr(),
      libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
  };
  if fd < 0 {
    let error = std::io::Error::last_os_error();
    return if error.kind() == std::io::ErrorKind::NotFound {
      Ok(())
    } else {
      Err(local_error(error))
    };
  }
  use std::os::fd::FromRawFd;
  let parent = unsafe { fs::File::from_raw_fd(fd) };
  remove_entry(
    parent.as_raw_fd(),
    &std::ffi::CString::new(name).map_err(|_| {
      CleanupError::needs_attention(
        "Cleanup directory is invalid; check the service configuration or upgrade the service.",
      )
    })?,
    0,
  )?;
  parent.sync_all().map_err(local_error)
}

fn local_error(error: std::io::Error) -> CleanupError {
  match error.raw_os_error() {
    Some(libc::EACCES | libc::EPERM) => CleanupError::needs_attention(
      "Local cleanup permission denied; allow the service to remove files in its data directory.",
    ),
    Some(libc::EROFS) => CleanupError::needs_attention(
      "Local cleanup storage is read-only; make the service data directory writable.",
    ),
    Some(libc::ENOTDIR | libc::ELOOP | libc::EINVAL | libc::ENAMETOOLONG) => {
      CleanupError::needs_attention(
        "Local cleanup directory is invalid; check the service data directory configuration.",
      )
    }
    _ => CleanupError::retryable(
      "Local cleanup could not complete; check disk availability. Automatic retries continue.",
    ),
  }
}

fn remove_entry(
  parent: std::os::fd::RawFd,
  name: &std::ffi::CStr,
  depth: usize,
) -> CleanupResult<()> {
  if depth > 64 {
    return Err(CleanupError::needs_attention(
      "Local cleanup exceeds its directory depth limit; reduce nesting in the service data directory.",
    ));
  }
  let fd = unsafe {
    libc::openat(
      parent,
      name.as_ptr(),
      libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
  };
  if fd < 0 {
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
      return Ok(());
    }
    if matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) {
      let removed = unsafe { libc::unlinkat(parent, name.as_ptr(), 0) };
      return if removed == 0
        || std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
      {
        Ok(())
      } else {
        Err(local_error(std::io::Error::last_os_error()))
      };
    }
    return Err(local_error(error));
  }
  let entries = unsafe { libc::fdopendir(fd) };
  if entries.is_null() {
    let error = std::io::Error::last_os_error();
    unsafe {
      libc::close(fd);
    }
    return Err(local_error(error));
  }
  let result = (|| {
    loop {
      let entry = unsafe { libc::readdir(entries) };
      if entry.is_null() {
        break;
      }
      let child = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
      if child.to_bytes() != b"." && child.to_bytes() != b".." {
        remove_entry(fd, child, depth + 1)?;
      }
    }
    let removed = unsafe { libc::unlinkat(parent, name.as_ptr(), libc::AT_REMOVEDIR) };
    if removed == 0 || std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
      Ok(())
    } else {
      Err(local_error(std::io::Error::last_os_error()))
    }
  })();
  unsafe {
    libc::closedir(entries);
  }
  result
}

#[cfg(test)]
mod tests;
