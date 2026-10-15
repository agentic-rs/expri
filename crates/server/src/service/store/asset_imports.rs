//! Reusable output references and a projection of each run's declared asset use.
use serde_json::{Value, json};

use super::*;

const USE_LIMIT: usize = 64;
const RELATIONSHIP_PREVIEW_BYTES: usize = 4 * 1024;

fn bounded_relationships(values: Vec<Value>) -> ApiResult<Vec<Value>> {
  let mut bytes = 2;
  let mut result = Vec::new();
  for value in values {
    let size = serde_json::to_vec(&value)
      .map_err(|_| ApiError::new(500, "cannot encode asset relationship"))?
      .len()
      + 1;
    if bytes + size > RELATIONSHIP_PREVIEW_BYTES {
      break;
    }
    bytes += size;
    result.push(value);
  }
  Ok(result)
}

pub(super) fn initialize(directory: &Path, db: &Connection) -> crate::error::Result<()> {
  let exists: bool = db.query_row(
    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='tracking_asset_uses')",
    [], |row| row.get(0),
  ).map_err(|_| crate::error::ExpriError::Message("cannot inspect asset use storage".into()))?;
  let tx = db
    .unchecked_transaction()
    .map_err(|_| crate::error::ExpriError::Message("cannot initialize asset uses".into()))?;
  tx.execute_batch("CREATE TABLE IF NOT EXISTS tracking_asset_uses (
    target TEXT NOT NULL, path TEXT NOT NULL, input_id TEXT NOT NULL,
    size INTEGER NOT NULL, sha256 TEXT NOT NULL, PRIMARY KEY(target,path));
    CREATE INDEX IF NOT EXISTS tracking_asset_uses_input ON tracking_asset_uses(input_id,size,sha256);
    CREATE INDEX IF NOT EXISTS files_asset_producer ON files(
      json_extract(target,'$.project_id'),
      json_extract(record,'$.provenance.producer.origin'),
      json_extract(record,'$.provenance.producer.run_id'),
      json_extract(record,'$.provenance.producer.path'),
      json_extract(record,'$.size'),json_extract(record,'$.sha256'),target)
      WHERE json_extract(target,'$.kind')='input';")
    .map_err(|_| crate::error::ExpriError::Message("cannot initialize asset use storage".into()))?;
  if !exists {
    let targets = tx.prepare("SELECT target FROM files WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.path')='run-state.json' AND json_extract(record,'$.storage')<>'object'")
      .and_then(|mut statement| statement.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>())
      .map_err(|_| crate::error::ExpriError::Message("cannot inspect existing asset uses".into()))?;
    for raw in targets {
      let target: FileTarget = serde_json::from_str(&raw)?;
      let FileTarget::Run { scope, .. } = &target else {
        continue;
      };
      let value = tracking::captured_document(directory, &tx, scope, "run-state.json")
        .map_err(|error| crate::error::ExpriError::Message(error.message))?;
      index_uses(&tx, &target, &value)
        .map_err(|error| crate::error::ExpriError::Message(error.message))?;
    }
  }
  tx.commit()
    .map_err(|_| crate::error::ExpriError::Message("cannot save existing asset uses".into()))?;
  Ok(())
}

fn digest_valid(value: &str) -> bool {
  value.len() == 64
    && value
      .bytes()
      .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn workspace_path(value: &str) -> bool {
  !value.is_empty()
    && value.len() <= 1024
    && !value.contains('\\')
    && !value.chars().any(char::is_control)
    && value
      .split('/')
      .all(|part| !matches!(part, "" | "." | ".."))
}

pub(super) fn index_uses(db: &Connection, target: &FileTarget, value: &Value) -> ApiResult<()> {
  let FileTarget::Run { scope, path } = target else {
    return Ok(());
  };
  if path != "run-state.json" {
    return Ok(());
  }
  let mut uses = BTreeMap::new();
  for asset in value["assets"]
    .as_array()
    .into_iter()
    .flatten()
    .take(USE_LIMIT)
  {
    let source = &asset["source"];
    if source["project_id"]
      .as_str()
      .is_some_and(|project| project != scope.project_id)
    {
      continue;
    }
    let input = asset["availability"]["input_id"].as_str().or_else(|| {
      (source["kind"] == "expri")
        .then(|| source["input_id"].as_str())
        .flatten()
    });
    let (Some(input), Some(path), Some(size), Some(sha256)) = (
      input,
      asset["path"].as_str(),
      asset["size"].as_u64(),
      asset["sha256"].as_str(),
    ) else {
      continue;
    };
    if validate_component(input).is_err()
      || !workspace_path(path)
      || !digest_valid(sha256)
      || size > i64::MAX as u64
    {
      continue;
    }
    uses.insert(
      path.to_string(),
      (input.to_string(), size, sha256.to_string()),
    );
  }
  let key = target_json(target)?;
  let previous: BTreeMap<String, (String, u64, String)> = db
    .prepare("SELECT path,input_id,size,sha256 FROM tracking_asset_uses WHERE target=?1")
    .map_err(database)?
    .query_map([&key], |row| {
      Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
    })
    .map_err(database)?
    .collect::<rusqlite::Result<_>>()
    .map_err(database)?;
  if previous == uses {
    return Ok(());
  }
  db.execute("DELETE FROM tracking_asset_uses WHERE target=?1", [&key])
    .map_err(database)?;
  for (path, (input, size, sha256)) in uses {
    db.execute(
      "INSERT INTO tracking_asset_uses(target,path,input_id,size,sha256) VALUES(?1,?2,?3,?4,?5)",
      params![key, path, input, size, sha256],
    )
    .map_err(database)?;
  }
  db.execute("INSERT INTO project_storage_revisions(project_id,revision) VALUES(?1,1) ON CONFLICT(project_id) DO UPDATE SET revision=revision+1", [&scope.project_id]).map_err(database)?;
  Ok(())
}

fn scalar_fields(value: &Value, names: &[&str]) -> Value {
  if !value.is_object() {
    return Value::Null;
  }
  Value::Object(
    names
      .iter()
      .filter_map(|name| {
        let value = value.get(*name)?;
        if value.is_null()
          || value.is_boolean()
          || value.is_number()
          || value
            .as_str()
            .is_some_and(|text| text.len() <= 512 && !text.chars().any(char::is_control))
        {
          Some((name.to_string(), value.clone()))
        } else {
          None
        }
      })
      .collect(),
  )
}

fn capture(
  directory: &Path,
  db: &Connection,
  scope: &RunScope,
  producer: OutputProducer,
  file: &FileRecord,
) -> ApiResult<AssetProvenance> {
  let run = tracking::captured_document(directory, db, scope, "run-state.json")?;
  let inventory =
    tracking::captured_document(directory, db, scope, crate::run_artifacts::INVENTORY_PATH)?;
  let attempt = registered_attempt(&inventory, &producer.path, file);
  let entries = run["task_attempts"]["attempts"].as_array();
  let continued = run["publication_epoch"]
    .as_u64()
    .is_some_and(|epoch| epoch > 0)
    || run.get("task_attempts").is_some_and(|history| {
      history["current_attempt"] != 1
        || entries.is_none_or(|entries| entries.len() != 1 || entries[0]["attempt"] != 1)
    });
  let recorded =
    attempt.and_then(|attempt| entries?.iter().find(|entry| entry["attempt"] == attempt));
  // An unregistered artifact has no proven relationship to the latest task.
  // Preserve only its exact run/path identity once multiple attempts exist.
  let known = recorded.is_some() || (!continued && attempt.is_none_or(|attempt| attempt == 1));
  let (state, snapshot, environment) = if known {
    let snapshot_path = attempt
      .filter(|attempt| *attempt > 1)
      .map(|attempt| format!("outputs/attempts/{attempt}/snapshot.json"))
      .unwrap_or_else(|| "snapshot.json".into());
    let environment_path = attempt
      .filter(|attempt| *attempt > 1)
      .map(|attempt| format!("outputs/attempts/{attempt}/environment-state.json"))
      .unwrap_or_else(|| "environment/environment-state.json".into());
    (
      recorded.unwrap_or(&run).clone(),
      tracking::captured_document(directory, db, scope, &snapshot_path)?,
      tracking::captured_document(directory, db, scope, &environment_path)?,
    )
  } else {
    (Value::Null, Value::Null, Value::Null)
  };
  // Never retain command arguments, filesystem paths, inventories, URLs, or arbitrary metadata.
  let mut safe_snapshot = scalar_fields(&snapshot, &["schema_version", "created_at"]);
  if safe_snapshot.is_object() {
    safe_snapshot["source"] = scalar_fields(
      &snapshot["source"],
      &[
        "kind",
        "git_head",
        "checkout_manifest_sha256",
        "sync_state_sha256",
      ],
    );
  }
  let mut safe_state = scalar_fields(
    &state,
    &[
      "schema_version",
      "task",
      "status",
      "started_at",
      "finished_at",
      "exit_code",
      "task_exit_code",
    ],
  );
  if known
    && let Some(attempt) = attempt
    && safe_state.is_object()
  {
    safe_state["task_attempt"] = json!(attempt);
  }
  Ok(AssetProvenance {
    producer,
    captured_at: chrono::Utc::now().to_rfc3339(),
    state: safe_state,
    snapshot: safe_snapshot,
    environment: scalar_fields(
      &environment,
      &[
        "schema_version",
        "fingerprint",
        "lock_sha256",
        "pyproject_sha256",
        "install_project",
      ],
    ),
  })
}

fn registered_attempt(inventory: &Value, path: &str, file: &FileRecord) -> Option<u64> {
  let entries = inventory["files"].as_array()?;
  if inventory["schema_version"] != 1 || entries.len() > crate::run_artifacts::FILE_LIMIT {
    return None;
  }
  let mut matching = entries.iter().filter(|entry| {
    entry["path"] == path
      && entry["size"].as_u64() == Some(file.size)
      && entry["sha256"].as_str() == file.sha256.as_deref()
  });
  let entry = matching.next()?;
  if matching.next().is_some() {
    return None;
  }
  entry["task_attempt"]
    .as_u64()
    .filter(|attempt| *attempt > 0 && *attempt <= expri_core::protocol::MAX_TASK_ATTEMPTS as u64)
}

impl<S: ObjectStorage> Store<S> {
  pub(in crate::service) fn producer_run_available(
    &self,
    project_id: &str,
    producer: &OutputProducer,
  ) -> ApiResult<bool> {
    let scope = RunScope {
      project_id: project_id.into(),
      origin: producer.origin.clone(),
      run_id: producer.run_id.clone(),
    };
    if validate_scope(&scope).is_err()
      || crate::run_artifacts::validate_path(&producer.path).is_err()
    {
      return Ok(false);
    }
    let db = self.db()?;
    if run_retention::ensure_available(&db, &scope, run_retention::now()).is_err() {
      return Ok(false);
    }
    db.query_row(
      "SELECT EXISTS(SELECT 1 FROM files WHERE target=?1)",
      [target_json(&FileTarget::Run {
        scope,
        path: "run-state.json".into(),
      })?],
      |row| row.get(0),
    )
    .map_err(database)
  }

  pub(super) fn import_output(
    &self,
    scope: RunScope,
    path: String,
    input_id: Option<String>,
    size: u64,
    sha256: String,
  ) -> ApiResult<Response> {
    validate_scope(&scope).map_err(bad)?;
    crate::run_artifacts::validate_path(&path).map_err(bad)?;
    if !digest_valid(&sha256) {
      return Err(ApiError::new(400, "invalid output SHA256"));
    }
    if let Some(input_id) = &input_id {
      validate_component(input_id).map_err(bad)?;
    }
    let producer = OutputProducer {
      origin: scope.origin.clone(),
      run_id: scope.run_id.clone(),
      path: path.clone(),
    };
    let source = FileTarget::Run {
      scope: scope.clone(),
      path,
    };
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    // The durable input is sufficient to finish a retry even after source-run retention.
    let existing: Option<String> = if let Some(input_id) = &input_id {
      tx.query_row(
        "SELECT record FROM files WHERE target=?1",
        [target_json(&FileTarget::Input {
          project_id: scope.project_id.clone(),
          input_id: input_id.clone(),
        })?],
        |row| row.get(0),
      )
      .optional()
      .map_err(database)?
    } else {
      tx.query_row("SELECT record FROM files WHERE json_extract(target,'$.kind')='input' AND json_extract(target,'$.project_id')=?1 AND json_extract(record,'$.provenance.producer.origin')=?2 AND json_extract(record,'$.provenance.producer.run_id')=?3 AND json_extract(record,'$.provenance.producer.path')=?4 AND json_extract(record,'$.size')=?5 AND json_extract(record,'$.sha256')=?6 ORDER BY target LIMIT 1", params![scope.project_id,producer.origin,producer.run_id,producer.path,size,sha256], |row| row.get(0)).optional().map_err(database)?
    };
    if let Some(raw) = existing {
      let record: FileRecord =
        serde_json::from_str(&raw).map_err(|_| ApiError::new(500, "invalid stored asset"))?;
      if !matches!(record.storage, FileStorage::Object)
        || record.size != size
        || record.sha256.as_deref() != Some(&sha256)
        || record
          .provenance
          .as_ref()
          .is_none_or(|value| value.producer != producer)
      {
        return Err(ApiError::new(
          409,
          "input ID is already bound to another output",
        ));
      }
      return Ok(Response::File { file: record });
    }
    run_retention::ensure_target_available(&tx, &source)?;
    let (raw, key, sequence): (String, String, i64) = tx
      .query_row(
        "SELECT record,object_key,sequence FROM files WHERE target=?1",
        [target_json(&source)?],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
      )
      .optional()
      .map_err(database)?
      .ok_or_else(|| ApiError::new(404, "completed source output is missing"))?;
    let mut record: FileRecord =
      serde_json::from_str(&raw).map_err(|_| ApiError::new(500, "invalid stored output"))?;
    if record.target != source
      || !matches!(record.storage, FileStorage::Object)
      || record.size != size
      || record.sha256.as_deref() != Some(&sha256)
    {
      return Err(ApiError::new(
        409,
        "source output does not match the pinned identity",
      ));
    }
    let input_id = match input_id {
      Some(input_id) => input_id,
      None => tx
        .query_row("SELECT 'asset-'||lower(hex(randomblob(16)))", [], |row| {
          row.get(0)
        })
        .map_err(database)?,
    };
    let target = FileTarget::Input {
      project_id: scope.project_id.clone(),
      input_id,
    };
    let target_key = target_json(&target)?;
    let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM uploads WHERE target=?1 AND complete=0) OR EXISTS(SELECT 1 FROM streams WHERE target=?1)", [&target_key], |row| row.get(0)).map_err(database)?;
    if pending {
      return Err(ApiError::new(409, "input has an active transfer"));
    }
    record.target = target;
    record.provenance = Some(capture(&self.directory, &tx, &scope, producer, &record)?);
    tx.execute(
      "INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,?3,?4)",
      params![
        target_key,
        serde_json::to_string(&record)
          .map_err(|_| ApiError::new(500, "cannot encode imported asset"))?,
        key,
        sequence
      ],
    )
    .map_err(database)?;
    record_storage_publication(&tx, &record.target)?;
    tx.commit().map_err(database)?;
    Ok(Response::File { file: record })
  }

  pub(in crate::service) fn dashboard_asset_relationships(
    &self,
    record: &FileRecord,
  ) -> ApiResult<Value> {
    let db = self.db()?;
    match &record.target {
      FileTarget::Input {
        project_id,
        input_id,
      } => {
        let producer = record.provenance.as_ref().map(|value| {
          let scope = RunScope { project_id: project_id.clone(), origin: value.producer.origin.clone(), run_id: value.producer.run_id.clone() };
          let available = run_retention::ensure_available(&db, &scope, run_retention::now()).is_ok()
            && db.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE target=?1)", [target_json(&FileTarget::Run { scope, path: "run-state.json".into() })?], |row| row.get::<_, bool>(0)).map_err(database)?;
          Ok::<_, ApiError>(json!({"origin":value.producer.origin,"run_id":value.producer.run_id,"path":value.producer.path,"run_available":available}))
        }).transpose()?;
        let selection = "FROM tracking_asset_uses WHERE input_id=?1 AND size=?2 AND sha256=?3 AND json_extract(target,'$.scope.project_id')=?4 AND NOT EXISTS(SELECT 1 FROM run_retention WHERE project_id=?4 AND origin=json_extract(target,'$.scope.origin') AND run_id=json_extract(target,'$.scope.run_id') AND (status<>'archived' OR delete_after<=?5))";
        let used_by_count: usize = db
          .query_row(
            &format!("SELECT COUNT(*) {selection}"),
            params![
              input_id,
              record.size,
              record.sha256,
              project_id,
              run_retention::now()
            ],
            |row| row.get(0),
          )
          .map_err(database)?;
        let mut statement = db.prepare(&format!("SELECT json_extract(target,'$.scope.origin'),json_extract(target,'$.scope.run_id'),path {selection} ORDER BY target,path LIMIT 16")).map_err(database)?;
        let used_by = statement.query_map(params![input_id,record.size,record.sha256,project_id,run_retention::now()], |row| Ok(json!({"origin":row.get::<_,String>(0)?,"run_id":row.get::<_,String>(1)?,"path":row.get::<_,String>(2)?}))).map_err(database)?.collect::<rusqlite::Result<Vec<_>>>().map_err(database)?;
        let used_by = bounded_relationships(used_by)?;
        Ok(json!({"producer":producer,"used_by":used_by,"used_by_count":used_by_count}))
      }
      FileTarget::Run { scope, path } => {
        let selection = "FROM files WHERE json_extract(target,'$.kind')='input' AND json_extract(target,'$.project_id')=?1 AND json_extract(record,'$.provenance.producer.origin')=?2 AND json_extract(record,'$.provenance.producer.run_id')=?3 AND json_extract(record,'$.provenance.producer.path')=?4 AND json_extract(record,'$.size')=?5 AND json_extract(record,'$.sha256')=?6";
        let params = params![
          scope.project_id,
          scope.origin,
          scope.run_id,
          path,
          record.size,
          record.sha256
        ];
        let count: usize = db
          .query_row(&format!("SELECT COUNT(*) {selection}"), params, |row| {
            row.get(0)
          })
          .map_err(database)?;
        let assets = db
          .prepare(&format!(
            "SELECT json_extract(target,'$.input_id') {selection} ORDER BY target LIMIT 16"
          ))
          .map_err(database)?
          .query_map(params, |row| {
            Ok(json!({"input_id":row.get::<_,String>(0)?}))
          })
          .map_err(database)?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(database)?;
        let assets = bounded_relationships(assets)?;
        Ok(json!({"assets":assets,"assets_count":count}))
      }
    }
  }
}

#[cfg(test)]
mod tests;
