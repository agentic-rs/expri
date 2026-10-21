//! Explicit publication generations. A new attempt preserves every acknowledged byte.

use super::*;
use serde_json::json;

impl<S: ObjectStorage> Store<S> {
  pub(in crate::service) fn run_publication(&self, scope: &RunScope) -> ApiResult<Response> {
    validate_scope(scope).map_err(bad)?;
    let db = self.db()?;
    super::super::run_retention::ensure_available(&db, scope, super::super::run_retention::now())?;
    Ok(Response::RunAttempt {
      epoch: publication_epoch(&db, scope)?,
    })
  }
  pub(in crate::service) fn reconcile_run_attempt(
    &self,
    scope: RunScope,
    request_id: String,
    expected_epoch: u64,
  ) -> ApiResult<Response> {
    validate_scope(&scope).map_err(bad)?;
    validate_component(&request_id).map_err(bad)?;
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = gate
      .write()
      .map_err(|_| ApiError::new(503, "run operation unavailable"))?;
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    available_for_continuation(&tx, &scope)?;
    let key =
      serde_json::to_string(&scope).map_err(|_| ApiError::new(500, "cannot encode run scope"))?;
    let committed_epoch = tx
      .query_row(
        "SELECT epoch FROM tracking_attempt_history WHERE scope=?1 AND request_id=?2",
        params![key, request_id],
        |row| row.get::<_, u64>(0),
      )
      .optional()
      .map_err(database)?;
    if let Some(epoch) = committed_epoch {
      if expected_epoch.checked_add(1) != Some(epoch) {
        return Err(ApiError::new(
          409,
          "continuation request belongs to another publication epoch",
        ));
      }
      return Ok(Response::ReconciledRunAttempt { committed_epoch });
    }
    let aborted: Option<u64> = tx
      .query_row(
        "SELECT expected_epoch FROM tracking_attempt_aborts WHERE scope=?1 AND request_id=?2",
        params![key, request_id],
        |row| row.get(0),
      )
      .optional()
      .map_err(database)?;
    if let Some(epoch) = aborted {
      if epoch != expected_epoch {
        return Err(ApiError::new(
          409,
          "continuation request belongs to another publication epoch",
        ));
      }
    } else {
      if publication_epoch(&tx, &scope)? != expected_epoch {
        return Err(ApiError::new(
          409,
          "publication epoch changed before continuation reconciliation",
        ));
      }
      let count: u64 = tx
        .query_row(
          "SELECT COUNT(*) FROM tracking_attempt_aborts WHERE scope=?1",
          [&key],
          |row| row.get(0),
        )
        .map_err(database)?;
      if count >= expri_core::protocol::MAX_TASK_ATTEMPTS as u64 {
        return Err(ApiError::new(
          409,
          "task continuation reconciliation history is full",
        ));
      }
      tx.execute(
        "INSERT INTO tracking_attempt_aborts(scope,request_id,expected_epoch) VALUES(?1,?2,?3)",
        params![key, request_id, expected_epoch],
      )
      .map_err(database)?;
    }
    tx.commit().map_err(database)?;
    Ok(Response::ReconciledRunAttempt {
      committed_epoch: None,
    })
  }
  pub(in crate::service) fn check_run_continuation(&self, scope: &RunScope) -> ApiResult<Response> {
    let db = self.db()?;
    available_for_continuation(&db, scope)?;
    Ok(Response::RunAttempt {
      epoch: publication_epoch(&db, scope)?,
    })
  }
  pub(in crate::service) fn guard_publication_epoch(
    &self,
    request: &Request,
    epoch: u64,
  ) -> ApiResult<()> {
    if !expri_core::service::run_mutation(request) {
      return Ok(());
    }
    let target;
    let scope = match request {
      Request::PutDocument { scope, .. }
      | Request::AppendTracking { scope, .. }
      | Request::AppendStream { scope, .. }
      | Request::SealRun { scope, .. }
      | Request::ImportOutput { scope, .. } => Some(scope),
      Request::BeginUpload { target, .. } | Request::ReferenceFile { target, .. } => match target {
        FileTarget::Run { scope, .. } => Some(scope),
        _ => None,
      },
      Request::PartUrl { upload_id, .. }
      | Request::RecordPart { upload_id, .. }
      | Request::CompleteUpload { upload_id } => {
        target = self.upload_target(upload_id)?;
        match &target {
          FileTarget::Run { scope, .. } => Some(scope),
          _ => None,
        }
      }
      _ => None,
    };
    let Some(scope) = scope else {
      return if epoch == 0 {
        Ok(())
      } else {
        Err(ApiError::new(409, "publication epoch is outside this run"))
      };
    };
    let current = publication_epoch(&*self.db()?, scope)?;
    if current != epoch {
      return Err(ApiError::new(
        409,
        "publication epoch changed; stale publisher mutation refused",
      ));
    }
    Ok(())
  }

  pub(in crate::service) fn begin_run_attempt(
    &self,
    scope: RunScope,
    request_id: String,
    expected_epoch: u64,
    attempt: u64,
    documents: BTreeMap<String, u64>,
    streams: BTreeMap<String, u64>,
  ) -> ApiResult<Response> {
    validate_scope(&scope).map_err(bad)?;
    validate_component(&request_id).map_err(bad)?;
    let next_epoch = expected_epoch
      .checked_add(1)
      .filter(|epoch| *epoch <= i64::MAX as u64)
      .ok_or_else(|| ApiError::new(400, "publication epoch exhausted"))?;
    if attempt < 2
      || attempt > expri_core::protocol::MAX_TASK_ATTEMPTS as u64
      || documents.len() > expri_core::service::TRACKING_DOCUMENT_LIMIT
      || streams.len() > TRACKING_STREAM_PATHS.len()
      || documents.keys().any(|path| !document_path(path))
      || streams.keys().any(|path| !stream_path(path))
    {
      return Err(ApiError::new(
        400,
        "invalid task attempt publication snapshot",
      ));
    }
    let fingerprint = serde_json::to_string(&json!({"expected_epoch":expected_epoch,"attempt":attempt,"documents":documents,"streams":streams}))
      .map_err(|_| ApiError::new(500, "cannot encode task attempt"))?;
    // Every normal request holds a shared project gate from its epoch check
    // through its mutation. The exclusive gate makes this CAS a durable fence.
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = gate
      .write()
      .map_err(|_| ApiError::new(503, "run operation unavailable"))?;
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    projects::ensure_active(&tx, &scope.project_id)?;
    available_for_continuation(&tx, &scope)?;
    let key =
      serde_json::to_string(&scope).map_err(|_| ApiError::new(500, "cannot encode run scope"))?;
    let previous: Option<(u64, String)> = tx
      .query_row(
        "SELECT epoch,fingerprint FROM tracking_attempt_history WHERE scope=?1 AND request_id=?2",
        params![key, request_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .optional()
      .map_err(database)?;
    if let Some((epoch, saved)) = previous {
      return if saved == fingerprint {
        Ok(Response::RunAttempt { epoch })
      } else {
        Err(ApiError::new(
          409,
          "continuation request ID was already used with different publication data",
        ))
      };
    }
    let aborted: bool = tx
      .query_row(
        "SELECT EXISTS(SELECT 1 FROM tracking_attempt_aborts WHERE scope=?1 AND request_id=?2)",
        params![key, request_id],
        |row| row.get(0),
      )
      .map_err(database)?;
    if aborted {
      return Err(ApiError::new(
        409,
        "continuation publication was abandoned; use a new request ID",
      ));
    }
    if publication_epoch(&tx, &scope)? != expected_epoch {
      return Err(ApiError::new(
        409,
        "publication epoch changed before task continuation",
      ));
    }
    let mut statement = tx.prepare("SELECT record FROM files WHERE json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3").map_err(database)?;
    let files = statement
      .query_map(
        params![scope.project_id, scope.origin, scope.run_id],
        |row| row.get::<_, String>(0),
      )
      .map_err(database)?
      .map(|row| {
        row.map_err(database).and_then(|raw| {
          serde_json::from_str::<FileRecord>(&raw)
            .map_err(|_| ApiError::new(500, "invalid tracking snapshot"))
        })
      })
      .collect::<ApiResult<Vec<_>>>()?;
    drop(statement);
    let mut actual_documents = BTreeMap::new();
    let mut actual_streams = BTreeMap::new();
    for file in &files {
      if let (FileTarget::Run { path, .. }, FileStorage::Tracking { revision, .. }) =
        (&file.target, &file.storage)
      {
        if document_path(path) {
          actual_documents.insert(path.clone(), *revision);
        }
        if stream_path(path) {
          actual_streams.insert(path.clone(), file.size);
        }
      }
    }
    if actual_documents != documents
      || actual_streams != streams
      || !documents.contains_key("run-state.json")
    {
      return Err(ApiError::new(
        409,
        "published task attempt prefixes changed; synchronize and retry",
      ));
    }
    let incomplete: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tracking_versions v WHERE json_extract(v.target,'$.scope.project_id')=?1 AND json_extract(v.target,'$.scope.origin')=?2 AND json_extract(v.target,'$.scope.run_id')=?3 AND v.document=1 AND v.complete=0 AND v.revision=(SELECT MAX(revision) FROM tracking_versions newer WHERE newer.target=v.target))", params![scope.project_id,scope.origin,scope.run_id], |row| row.get(0)).map_err(database)?;
    if incomplete {
      return Err(ApiError::new(
        409,
        "task attempt document publication is incomplete",
      ));
    }
    let state = files
      .iter()
      .find(|file| matches!(&file.target, FileTarget::Run { path, .. } if path == "run-state.json"))
      .unwrap();
    let FileStorage::Tracking { revision, .. } = state.storage else {
      return Err(ApiError::new(409, "run state is not managed tracking data"));
    };
    let file = open_raw(&self.directory, &state.target, revision, false)?;
    let state_value: Value = serde_json::from_reader(file.take(state.size))
      .map_err(|_| ApiError::new(409, "run state is invalid"))?;
    if state_value["system_capture_id"].is_string() {
      if state_value["system_capture_status"] == "capturing" {
        return Err(ApiError::new(
          409,
          "previous system capture is still draining",
        ));
      }
      run_system::validate_seal(&self.directory, &tx, &files, &state_value)?;
    }
    let snapshot = serde_json::to_string(
      &json!({"attempt":attempt,"documents":documents,"streams":streams,"state":state_value}),
    )
    .map_err(|_| ApiError::new(500, "cannot retain task attempt seal"))?;
    tx.execute("INSERT INTO tracking_attempt_history(scope,request_id,epoch,fingerprint,snapshot) VALUES(?1,?2,?3,?4,?5)",params![key,request_id,next_epoch,fingerprint,snapshot]).map_err(database)?;
    tx.execute("INSERT INTO tracking_run_epochs(scope,epoch) VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET epoch=excluded.epoch",params![key,next_epoch]).map_err(database)?;
    tx.execute("DELETE FROM tracking_run_seals WHERE scope=?1", [&key])
      .map_err(database)?;
    for mut file in files {
      if let (FileTarget::Run { path, .. }, FileStorage::Tracking { sealed, .. }) =
        (&file.target, &mut file.storage)
        && stream_path(path)
      {
        *sealed = false;
        tx.execute(
          "UPDATE tracking_versions SET sealed=0 WHERE target=?1 AND document=0",
          [target_json(&file.target)?],
        )
        .map_err(database)?;
        publish(&tx, &file)?;
      }
    }
    tx.commit().map_err(database)?;
    Ok(Response::RunAttempt { epoch: next_epoch })
  }
}

pub(in crate::service::store) fn publication_epoch(
  db: &Connection,
  scope: &RunScope,
) -> ApiResult<u64> {
  let key =
    serde_json::to_string(scope).map_err(|_| ApiError::new(500, "cannot encode run scope"))?;
  Ok(
    db.query_row(
      "SELECT epoch FROM tracking_run_epochs WHERE scope=?1",
      [key],
      |row| row.get(0),
    )
    .optional()
    .map_err(database)?
    .unwrap_or(0),
  )
}

fn available_for_continuation(db: &Connection, scope: &RunScope) -> ApiResult<()> {
  validate_scope(scope).map_err(bad)?;
  projects::ensure_active(db, &scope.project_id)?;
  super::super::run_retention::ensure_available(db, scope, super::super::run_retention::now())?;
  let archived: bool = db
    .query_row(
      "SELECT EXISTS(SELECT 1 FROM run_retention WHERE project_id=?1 AND origin=?2 AND run_id=?3)",
      params![scope.project_id, scope.origin, scope.run_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  if archived {
    return Err(ApiError::new(
      409,
      "restore this archived run before continuing its task",
    ));
  }
  Ok(())
}
