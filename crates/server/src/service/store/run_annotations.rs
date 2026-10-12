//! User-managed names and tags stay separate from worker-owned run documents.
use std::collections::BTreeSet;
use std::sync::TryLockError;

use super::*;

const NICKNAME_CHAR_LIMIT: usize = 128;
const NICKNAME_BYTE_LIMIT: usize = 512;
const TAG_LIMIT: usize = 16;
const TAG_CHAR_LIMIT: usize = 48;
const TAG_BYTE_LIMIT: usize = 192;

pub(super) fn initialize(db: &Connection) -> crate::error::Result<()> {
  db.execute_batch(
    "CREATE TABLE IF NOT EXISTS run_annotations (
      project_id TEXT NOT NULL, origin TEXT NOT NULL, run_id TEXT NOT NULL,
      nickname TEXT, tags TEXT NOT NULL, revision INTEGER NOT NULL,
      PRIMARY KEY(project_id,origin,run_id));",
  )
  .map_err(|_| crate::error::ExpriError::Message("cannot initialize run annotations".into()))
}

pub(super) fn revision(db: &Connection, scope: &RunScope) -> ApiResult<Option<String>> {
  db.query_row(
    "SELECT 'annotations:'||revision FROM run_annotations WHERE project_id=?1 AND origin=?2 AND run_id=?3",
    params![scope.project_id, scope.origin, scope.run_id],
    |row| row.get(0),
  )
  .optional()
  .map_err(database)
}

fn ensure_run(db: &Connection, scope: &RunScope) -> ApiResult<()> {
  projects::ensure_active(db, &scope.project_id)?;
  run_retention::ensure_available(db, scope, run_retention::now())?;
  let exists: bool = db
    .query_row(
      "SELECT EXISTS(SELECT 1 FROM (SELECT target FROM files UNION ALL SELECT target FROM streams)
      WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1
      AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3)",
      params![scope.project_id, scope.origin, scope.run_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  if !exists {
    return Err(ApiError::new(404, "run is missing"));
  }
  Ok(())
}

fn read(db: &Connection, scope: &RunScope) -> ApiResult<RunAnnotations> {
  let row: Option<(Option<String>, String)> = db
    .query_row(
      "SELECT nickname,tags FROM run_annotations WHERE project_id=?1 AND origin=?2 AND run_id=?3",
      params![scope.project_id, scope.origin, scope.run_id],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)?;
  let (nickname, tags) = match row {
    Some((nickname, tags)) => (
      nickname,
      serde_json::from_str(&tags)
        .map_err(|_| ApiError::new(500, "invalid stored run annotations"))?,
    ),
    None => (None, Vec::new()),
  };
  Ok(RunAnnotations {
    scope: scope.clone(),
    nickname,
    tags,
  })
}

fn normalize(
  scope: &RunScope,
  nickname: Option<String>,
  tags: Vec<String>,
) -> ApiResult<RunAnnotations> {
  let nickname = nickname
    .map(|nickname| {
      if nickname.chars().any(char::is_control) {
        return Err(ApiError::new(
          400,
          "nickname must not contain control characters",
        ));
      }
      let nickname = nickname.trim();
      if nickname.len() > NICKNAME_BYTE_LIMIT || nickname.chars().count() > NICKNAME_CHAR_LIMIT {
        return Err(ApiError::new(
          400,
          "nickname must contain at most 128 characters",
        ));
      }
      Ok((!nickname.is_empty()).then(|| nickname.to_string()))
    })
    .transpose()?
    .flatten();
  if tags.len() > TAG_LIMIT {
    return Err(ApiError::new(400, "select at most 16 tags"));
  }
  let mut seen = BTreeSet::new();
  let mut normalized_tags = Vec::with_capacity(tags.len());
  for tag in tags {
    if tag.chars().any(char::is_control) {
      return Err(ApiError::new(
        400,
        "tags must not contain control characters",
      ));
    }
    let tag = tag.trim();
    if tag.is_empty() || tag.len() > TAG_BYTE_LIMIT || tag.chars().count() > TAG_CHAR_LIMIT {
      return Err(ApiError::new(
        400,
        "each tag must contain 1 to 48 characters",
      ));
    }
    if seen.insert(tag.to_string()) {
      normalized_tags.push(tag.to_string());
    }
  }
  Ok(RunAnnotations {
    scope: scope.clone(),
    nickname,
    tags: normalized_tags,
  })
}

impl<S: ObjectStorage> Store<S> {
  /// Dashboard readers can show cleanup status after the edit window expires.
  /// Their catalog/detail selection already establishes the requested run scope.
  pub(in crate::service) fn dashboard_run_annotations(
    &self,
    scope: &RunScope,
  ) -> ApiResult<RunAnnotations> {
    validate_scope(scope).map_err(bad)?;
    let db = self.db()?;
    projects::ensure_active(&db, &scope.project_id)?;
    read(&db, scope)
  }

  pub fn run_annotations(&self, scope: &RunScope) -> ApiResult<RunAnnotations> {
    validate_scope(scope).map_err(bad)?;
    let db = self.db()?;
    ensure_run(&db, scope)?;
    read(&db, scope)
  }

  pub fn set_run_annotations(
    &self,
    scope: &RunScope,
    nickname: Option<String>,
    tags: Vec<String>,
  ) -> ApiResult<RunAnnotations> {
    validate_scope(scope).map_err(bad)?;
    let annotations = normalize(scope, nickname, tags)?;
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = match gate.try_read() {
      Ok(operation) => operation,
      Err(TryLockError::WouldBlock) => {
        return Err(ApiError::new(
          409,
          "project operation is busy; retry saving run annotations",
        ));
      }
      Err(_) => return Err(ApiError::new(503, "project operation unavailable")),
    };
    let mut db = self.db()?;
    let transaction = db.transaction().map_err(database)?;
    ensure_run(&transaction, scope)?;
    if read(&transaction, scope)? == annotations {
      return Ok(annotations);
    }
    let tags = serde_json::to_string(&annotations.tags)
      .map_err(|_| ApiError::new(500, "cannot encode run annotations"))?;
    transaction.execute(
      "INSERT INTO run_annotations(project_id,origin,run_id,nickname,tags,revision) VALUES(?1,?2,?3,?4,?5,1)
        ON CONFLICT(project_id,origin,run_id) DO UPDATE SET nickname=excluded.nickname,tags=excluded.tags,revision=run_annotations.revision+1",
      params![scope.project_id, scope.origin, scope.run_id, annotations.nickname, tags],
    ).map_err(database)?;
    record_run_activity(&transaction, scope)?;
    transaction
      .execute(
        "INSERT INTO project_change_revisions(project_id,revision) VALUES(?1,1)
        ON CONFLICT(project_id) DO UPDATE SET revision=revision+1",
        [&scope.project_id],
      )
      .map_err(database)?;
    transaction.commit().map_err(database)?;
    Ok(annotations)
  }
}

#[cfg(test)]
mod tests;
