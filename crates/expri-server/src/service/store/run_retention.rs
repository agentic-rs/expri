//! Hosted run archival and durable cleanup after its restore window expires.
use std::io::Read;
use std::sync::TryLockError;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, Utc};

use super::*;
use crate::service::storage::CleanupError;

const RETENTION_SECONDS: i64 = 15 * 24 * 60 * 60;
const STATE_LIMIT: u64 = 64 * 1024;
const TARGET_SCOPE: &str = "json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3";

pub(super) fn now() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs()
    .min(i64::MAX as u64) as i64
}

pub(super) fn initialize(db: &Connection) -> crate::error::Result<()> {
  db.execute_batch("CREATE TABLE IF NOT EXISTS run_retention (
    project_id TEXT NOT NULL, origin TEXT NOT NULL, run_id TEXT NOT NULL,
    archived_at INTEGER NOT NULL, delete_after INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'archived', last_error TEXT,
    PRIMARY KEY(project_id,origin,run_id));
    CREATE INDEX IF NOT EXISTS run_retention_due ON run_retention(status,delete_after);
    CREATE TABLE IF NOT EXISTS run_cleanup_tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT, project_id TEXT NOT NULL, origin TEXT NOT NULL, run_id TEXT NOT NULL,
    kind TEXT NOT NULL, value TEXT NOT NULL, size INTEGER, multipart TEXT NOT NULL DEFAULT '',
    done INTEGER NOT NULL DEFAULT 0, retry_at INTEGER NOT NULL DEFAULT 0,
    last_error TEXT, needs_attention INTEGER NOT NULL DEFAULT 0,
    UNIQUE(project_id,origin,run_id,kind,value,multipart));
    CREATE INDEX IF NOT EXISTS run_cleanup_ready ON run_cleanup_tasks(done,retry_at,id);")
    .map_err(|_| crate::error::ExpriError::Message("cannot initialize run retention".into()))
}

fn scope_params(scope: &RunScope) -> (&str, &str, &str) {
  (&scope.project_id, &scope.origin, &scope.run_id)
}

fn timestamp(value: i64) -> ApiResult<String> {
  DateTime::<Utc>::from_timestamp(value, 0)
    .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
    .ok_or_else(|| ApiError::new(500, "invalid run retention timestamp"))
}

pub(super) fn ensure_available(db: &Connection, scope: &RunScope, at: i64) -> ApiResult<()> {
  validate_scope(scope).map_err(bad)?;
  let unavailable: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM run_retention WHERE project_id=?1 AND origin=?2 AND run_id=?3 AND (status<>'archived' OR delete_after<=?4))", params![scope.project_id,scope.origin,scope.run_id,at], |row| row.get(0)).map_err(database)?;
  if unavailable {
    Err(ApiError::new(
      410,
      "run has expired or been deleted; use a new run identifier",
    ))
  } else {
    Ok(())
  }
}

pub(super) fn ensure_target_available(db: &Connection, target: &FileTarget) -> ApiResult<()> {
  if let FileTarget::Run { scope, .. } = target {
    ensure_available(db, scope, now())?;
  }
  Ok(())
}

pub(super) fn revision(db: &Connection, scope: &RunScope) -> ApiResult<Option<String>> {
  db.query_row("SELECT status||':'||archived_at||':'||delete_after FROM run_retention WHERE project_id=?1 AND origin=?2 AND run_id=?3", scope_params(scope), |row| row.get(0)).optional().map_err(database)
}

fn exists(db: &Connection, scope: &RunScope) -> ApiResult<bool> {
  db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM files WHERE {TARGET_SCOPE} UNION ALL SELECT 1 FROM streams WHERE {TARGET_SCOPE})"), scope_params(scope), |row| row.get(0)).map_err(database)
}

fn state(db: &Connection, scope: &RunScope, at: i64) -> ApiResult<RunArchival> {
  let row: Option<(String, i64, i64, Option<String>)> = db.query_row("SELECT status,archived_at,delete_after,last_error FROM run_retention WHERE project_id=?1 AND origin=?2 AND run_id=?3", scope_params(scope), |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional().map_err(database)?;
  if let Some((mut status, archived_at, delete_after, last_error)) = row {
    if status == "archived" && delete_after <= at {
      status = "deleting".into();
    }
    let pending_tasks = db.query_row("SELECT COUNT(*) FROM run_cleanup_tasks WHERE project_id=?1 AND origin=?2 AND run_id=?3 AND done=0", scope_params(scope), |row| row.get(0)).map_err(database)?;
    Ok(RunArchival {
      scope: scope.clone(),
      status,
      archived_at: Some(timestamp(archived_at)?),
      delete_after: Some(timestamp(delete_after)?),
      pending_tasks,
      last_error,
    })
  } else if exists(db, scope)? {
    Ok(RunArchival {
      scope: scope.clone(),
      status: "active".into(),
      archived_at: None,
      delete_after: None,
      pending_tasks: 0,
      last_error: None,
    })
  } else {
    Err(ApiError::new(404, "run is missing"))
  }
}

fn refresh(db: &Connection, scope: &RunScope) -> ApiResult<()> {
  record_run_activity(db, scope)?;
  db.execute("INSERT INTO project_change_revisions(project_id,revision) VALUES(?1,1) ON CONFLICT(project_id) DO UPDATE SET revision=revision+1", [&scope.project_id]).map_err(database)?;
  Ok(())
}

impl<S: ObjectStorage> Store<S> {
  pub(super) fn ensure_request_runs(&self, request: &Request) -> ApiResult<()> {
    let mut targets = Vec::new();
    let scope = match request {
      Request::ArchiveRun { .. } | Request::RestoreRun { .. } | Request::RunArchival { .. } => {
        return Ok(());
      }
      Request::BeginUpload { target, .. }
      | Request::GetFile { target }
      | Request::DownloadUrl { target } => {
        targets.push(target.clone());
        None
      }
      Request::ReferenceFile { source, target, .. } => {
        targets.extend([source.clone(), target.clone()]);
        None
      }
      Request::PartUrl { upload_id, .. }
      | Request::RecordPart { upload_id, .. }
      | Request::CompleteUpload { upload_id } => {
        targets.push(self.upload_target(upload_id)?);
        None
      }
      Request::PutDocument { scope, .. }
      | Request::AppendTracking { scope, .. }
      | Request::SealRun { scope, .. }
      | Request::ArchiveStatus { scope }
      | Request::ListFiles { scope }
      | Request::AppendStream { scope, .. }
      | Request::ReadStream { scope, .. } => Some(scope),
      _ => None,
    };
    let db = self.db()?;
    if let Some(scope) = scope {
      ensure_available(&db, scope, now())?;
    }
    for target in targets {
      ensure_target_available(&db, &target)?;
    }
    Ok(())
  }

  pub fn run_archival(&self, scope: &RunScope) -> ApiResult<RunArchival> {
    validate_scope(scope).map_err(bad)?;
    let db = self.db()?;
    projects::ensure_active(&db, &scope.project_id)?;
    state(&db, scope, now())
  }

  pub fn archive_run(&self, scope: &RunScope) -> ApiResult<RunArchival> {
    self.archive_run_at(scope, now())
  }

  fn archive_run_at(&self, scope: &RunScope, at: i64) -> ApiResult<RunArchival> {
    validate_scope(scope).map_err(bad)?;
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = match gate.try_write() {
      Ok(operation) => operation,
      Err(TryLockError::WouldBlock) => {
        return Err(ApiError::new(409, "run operation is busy; retry archival"));
      }
      Err(_) => return Err(ApiError::new(503, "run operation unavailable")),
    };
    {
      let db = self.db()?;
      projects::ensure_active(&db, &scope.project_id)?;
      if revision(&db, scope)?.is_some() {
        ensure_available(&db, scope, at)?;
        return state(&db, scope, at);
      }
    }
    self.require_terminal_state(scope)?;
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    tx.execute("INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after) VALUES(?1,?2,?3,?4,?5)", params![scope.project_id,scope.origin,scope.run_id,at,at.saturating_add(RETENTION_SECONDS)]).map_err(database)?;
    refresh(&tx, scope)?;
    tx.commit().map_err(database)?;
    state(&db, scope, at)
  }

  fn require_terminal_state(&self, scope: &RunScope) -> ApiResult<()> {
    let target = FileTarget::Run {
      scope: scope.clone(),
      path: "run-state.json".into(),
    };
    let record = self.file(&target).map_err(|error| {
      if error.status == 404 {
        ApiError::new(409, "run state is missing")
      } else {
        error
      }
    })?;
    let pending: bool = self.db()?.query_row("SELECT EXISTS(SELECT 1 FROM tracking_versions WHERE target=?1 AND complete=0 AND revision=(SELECT MAX(revision) FROM tracking_versions WHERE target=?1))", [target_json(&target)?], |row| row.get(0)).map_err(database)?;
    if pending {
      return Err(ApiError::new(409, "run state publication is incomplete"));
    }
    if record.size > STATE_LIMIT {
      return Err(ApiError::new(409, "run state exceeds the archival limit"));
    }
    let bytes = match record.storage {
      FileStorage::Tracking { .. } => self.tracking_range(&record, 0, record.size as usize)?,
      FileStorage::Object => {
        let key: String = self
          .db()?
          .query_row(
            "SELECT object_key FROM files WHERE target=?1",
            [target_json(&target)?],
            |row| row.get(0),
          )
          .map_err(database)?;
        let url = self
          .storage
          .presign_get(&key, 60)
          .map_err(|_| ApiError::new(502, "run state storage unavailable"))?;
        let client = reqwest::blocking::Client::builder()
          .redirect(reqwest::redirect::Policy::none())
          .connect_timeout(Duration::from_secs(5))
          .timeout(Duration::from_secs(10))
          .build()
          .map_err(|_| ApiError::new(503, "run state reader unavailable"))?;
        let response = client
          .get(url)
          .send()
          .and_then(|response| response.error_for_status())
          .map_err(|_| ApiError::new(502, "run state storage unavailable"))?;
        let mut bytes = Vec::new();
        response
          .take(STATE_LIMIT + 1)
          .read_to_end(&mut bytes)
          .map_err(|_| ApiError::new(502, "cannot read run state"))?;
        if bytes.len() as u64 != record.size {
          return Err(ApiError::new(409, "run state has an unexpected size"));
        }
        if let Some(sha256) = &record.sha256 {
          use sha2::{Digest, Sha256};
          let digest: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
          if digest != *sha256 {
            return Err(ApiError::new(
              409,
              "run state failed integrity verification",
            ));
          }
        }
        bytes
      }
      FileStorage::Stream => return Err(ApiError::new(409, "run state is not finalized")),
    };
    let state: serde_json::Value =
      serde_json::from_slice(&bytes).map_err(|_| ApiError::new(409, "run state is invalid"))?;
    if state["run_id"]
      .as_str()
      .is_some_and(|run_id| run_id != scope.run_id)
    {
      return Err(ApiError::new(409, "run state belongs to another run"));
    }
    if !matches!(
      state["status"].as_str(),
      Some("completed" | "failed" | "cancelled" | "lost")
    ) {
      return Err(ApiError::new(409, "only finished runs can be archived"));
    }
    Ok(())
  }

  pub fn restore_run(&self, scope: &RunScope) -> ApiResult<RunArchival> {
    self.restore_run_at(scope, now())
  }

  fn restore_run_at(&self, scope: &RunScope, at: i64) -> ApiResult<RunArchival> {
    validate_scope(scope).map_err(bad)?;
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = match gate.try_write() {
      Ok(operation) => operation,
      Err(TryLockError::WouldBlock) => {
        return Err(ApiError::new(
          409,
          "run operation is busy; retry restoration",
        ));
      }
      Err(_) => return Err(ApiError::new(503, "run operation unavailable")),
    };
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    projects::ensure_active(&tx, &scope.project_id)?;
    let selected = state(&tx, scope, at)?;
    if selected.status == "active" {
      return Ok(selected);
    }
    if selected.status != "archived" {
      return Err(ApiError::new(410, "the restore window has expired"));
    }
    tx.execute(
      "DELETE FROM run_retention WHERE project_id=?1 AND origin=?2 AND run_id=?3",
      scope_params(scope),
    )
    .map_err(database)?;
    refresh(&tx, scope)?;
    tx.commit().map_err(database)?;
    state(&db, scope, at)
  }

  pub fn run_retention_cycle(&self) -> ApiResult<bool> {
    self.run_retention_cycle_at(now())
  }

  fn run_retention_cycle_at(&self, at: i64) -> ApiResult<bool> {
    let due: Option<RunScope> = self.db()?.query_row("SELECT project_id,origin,run_id FROM run_retention WHERE status='archived' AND delete_after<=?1 ORDER BY delete_after,project_id,origin,run_id LIMIT 1",[at],|row|Ok(RunScope{project_id:row.get(0)?,origin:row.get(1)?,run_id:row.get(2)?})).optional().map_err(database)?;
    if let Some(scope) = due {
      return self.expire_run(&scope, at);
    }
    self.run_cleanup_cycle(at)
  }

  fn expire_run(&self, scope: &RunScope, at: i64) -> ApiResult<bool> {
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = match gate.try_write() {
      Ok(operation) => operation,
      Err(TryLockError::WouldBlock) => return Ok(false),
      Err(_) => return Err(ApiError::new(503, "run cleanup unavailable")),
    };
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    let changed=tx.execute("UPDATE run_retention SET status='deleting' WHERE project_id=?1 AND origin=?2 AND run_id=?3 AND status='archived' AND delete_after<=?4",params![scope.project_id,scope.origin,scope.run_id,at]).map_err(database)?;
    if changed == 0 {
      return Ok(false);
    }
    // Include replaced objects and incomplete upload sessions, not only the latest file records.
    tx.execute(&format!("INSERT OR IGNORE INTO run_cleanup_tasks(project_id,origin,run_id,kind,value,size) SELECT ?1,?2,?3,'object',object_key,MAX(size) FROM (SELECT object_key,json_extract(record,'$.size') AS size FROM files WHERE {TARGET_SCOPE} AND object_key<>'' UNION ALL SELECT object_key,size FROM uploads WHERE {TARGET_SCOPE} AND complete=1) GROUP BY object_key"),scope_params(scope)).map_err(database)?;
    tx.execute(&format!("INSERT OR IGNORE INTO run_cleanup_tasks(project_id,origin,run_id,kind,value) SELECT ?1,?2,?3,'object',object_key FROM uploads WHERE {TARGET_SCOPE} AND complete=0"),scope_params(scope)).map_err(database)?;
    tx.execute(&format!("INSERT OR IGNORE INTO run_cleanup_tasks(project_id,origin,run_id,kind,value,multipart) SELECT ?1,?2,?3,'multipart',object_key,multipart FROM uploads WHERE {TARGET_SCOPE} AND complete=0 AND multipart IS NOT NULL"),scope_params(scope)).map_err(database)?;
    tx.execute(&format!("INSERT OR IGNORE INTO run_cleanup_tasks(project_id,origin,run_id,kind,value,size) SELECT ?1,?2,?3,'tracking',?3,COALESCE(SUM(size),0) FROM tracking_versions WHERE {TARGET_SCOPE}"),scope_params(scope)).map_err(database)?;
    let encoded =
      serde_json::to_string(scope).map_err(|_| ApiError::new(500, "cannot encode run scope"))?;
    tx.execute("INSERT OR IGNORE INTO run_cleanup_tasks(project_id,origin,run_id,kind,value) SELECT ?1,?2,?3,'archive',CAST(id AS TEXT) FROM result_archives WHERE scope=?4",params![scope.project_id,scope.origin,scope.run_id,encoded]).map_err(database)?;
    tx.execute(
      &format!(
        "DELETE FROM parts WHERE upload_id IN (SELECT upload_id FROM uploads WHERE {TARGET_SCOPE})"
      ),
      scope_params(scope),
    )
    .map_err(database)?;
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
      tx.execute(
        &format!("DELETE FROM {table} WHERE {TARGET_SCOPE}"),
        scope_params(scope),
      )
      .map_err(database)?;
    }
    for table in ["tracking_run_seals", "result_archives"] {
      tx.execute(&format!("DELETE FROM {table} WHERE scope=?1"), [&encoded])
        .map_err(database)?;
    }
    // Other runs and private inputs may reference the same object; keep those objects intact.
    tx.execute("UPDATE run_cleanup_tasks SET done=1,kind='preserved' WHERE project_id=?1 AND origin=?2 AND run_id=?3 AND kind='object' AND (EXISTS(SELECT 1 FROM files WHERE object_key=run_cleanup_tasks.value) OR EXISTS(SELECT 1 FROM uploads WHERE object_key=run_cleanup_tasks.value))",scope_params(scope)).map_err(database)?;
    refresh(&tx, scope)?;
    tx.execute("INSERT INTO project_storage_revisions(project_id,revision) VALUES(?1,1) ON CONFLICT(project_id) DO UPDATE SET revision=revision+1",[&scope.project_id]).map_err(database)?;
    update_cleanup_state(&tx, scope)?;
    tx.commit().map_err(database)?;
    Ok(true)
  }

  fn run_cleanup_cycle(&self, at: i64) -> ApiResult<bool> {
    let task:Option<(i64,RunScope,String,String,String)>=self.db()?.query_row("SELECT id,project_id,origin,run_id,kind,value,multipart FROM run_cleanup_tasks WHERE done=0 AND retry_at<=?1 ORDER BY CASE kind WHEN 'multipart' THEN 0 WHEN 'object' THEN 1 ELSE 2 END,id LIMIT 1",[at],|row|Ok((row.get(0)?,RunScope{project_id:row.get(1)?,origin:row.get(2)?,run_id:row.get(3)?},row.get(4)?,row.get(5)?,row.get(6)?))).optional().map_err(database)?;
    let Some((id, scope, kind, value, multipart)) = task else {
      return Ok(false);
    };
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = match gate.try_write() {
      Ok(operation) => operation,
      Err(TryLockError::WouldBlock) => return Ok(false),
      Err(_) => return Err(ApiError::new(503, "run cleanup unavailable")),
    };
    let ready: bool = self
      .db()?
      .query_row(
        "SELECT done=0 AND retry_at<=?2 FROM run_cleanup_tasks WHERE id=?1",
        params![id, at],
        |row| row.get(0),
      )
      .map_err(database)?;
    if !ready {
      return Ok(true);
    }
    let mut preserved = false;
    let result = match kind.as_str() {
      "object" => {
        let referenced:bool=self.db()?.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE object_key=?1) OR EXISTS(SELECT 1 FROM uploads WHERE object_key=?1)",[&value],|row|row.get(0)).map_err(database)?;
        if referenced {
          preserved = true;
          Ok(())
        } else {
          self.storage.delete_object(&value)
        }
      }
      "multipart" => self.storage.abort_upload(&value, &multipart),
      "tracking" => projects::remove_local_run_tree(&self.directory, &scope),
      "archive" => projects::remove_local_tree(&self.directory, "archives", &value),
      _ => Err(CleanupError::needs_attention(
        "Run cleanup task is invalid; upgrade the service.",
      )),
    };
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    match &result {
      Ok(()) => {
        tx.execute("UPDATE run_cleanup_tasks SET done=1,retry_at=0,last_error=NULL,needs_attention=0,kind=CASE WHEN ?2 THEN 'preserved' ELSE kind END WHERE id=?1",params![id,preserved]).map_err(database)?;
      }
      Err(error) => {
        let delay = if error.is_needs_attention() { 300 } else { 30 };
        tx.execute(
          "UPDATE run_cleanup_tasks SET retry_at=?2,last_error=?3,needs_attention=?4 WHERE id=?1",
          params![
            id,
            at.saturating_add(delay),
            error.to_string(),
            error.is_needs_attention()
          ],
        )
        .map_err(database)?;
      }
    }
    update_cleanup_state(&tx, &scope)?;
    tx.execute("INSERT INTO project_change_revisions(project_id,revision) VALUES(?1,1) ON CONFLICT(project_id) DO UPDATE SET revision=revision+1", [&scope.project_id]).map_err(database)?;
    tx.execute("INSERT INTO project_storage_revisions(project_id,revision) VALUES(?1,1) ON CONFLICT(project_id) DO UPDATE SET revision=revision+1", [&scope.project_id]).map_err(database)?;
    tx.execute(
      "UPDATE dashboard_catalog_revision SET revision=revision+1 WHERE id=1",
      [],
    )
    .map_err(database)?;
    tx.commit().map_err(database)?;
    result
      .map(|()| true)
      .map_err(|error| ApiError::new(502, error.to_string()))
  }
}

fn update_cleanup_state(db: &Connection, scope: &RunScope) -> ApiResult<()> {
  db.execute("UPDATE run_retention SET status=CASE
    WHEN EXISTS(SELECT 1 FROM run_cleanup_tasks WHERE project_id=?1 AND origin=?2 AND run_id=?3 AND done=0 AND needs_attention=1) THEN 'needs_attention'
    WHEN EXISTS(SELECT 1 FROM run_cleanup_tasks WHERE project_id=?1 AND origin=?2 AND run_id=?3 AND done=0) THEN 'deleting'
    ELSE 'deleted' END,
    last_error=(SELECT last_error FROM run_cleanup_tasks WHERE project_id=?1 AND origin=?2 AND run_id=?3 AND done=0 AND last_error IS NOT NULL ORDER BY needs_attention DESC,id LIMIT 1)
    WHERE project_id=?1 AND origin=?2 AND run_id=?3",scope_params(scope)).map_err(database)?;
  Ok(())
}

#[cfg(test)]
mod tests;
