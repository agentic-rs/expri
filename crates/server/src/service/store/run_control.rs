use expri_core::experiment_assignment::ExperimentSelection;
use expri_core::resource_requirements::{ResourceRequirements, ResourceStatus};
use expri_core::run_control::{
  ControlAction, ControlCommand, ControlLifecycle, ControlResult, RunControlRequest,
  RunControlResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::*;

mod overlay;
mod requirements;

const ONLINE_MS: i64 = 15_000;
const DELIVERY_MS: i64 = 60_000;

pub(super) fn initialize(db: &Connection) -> crate::error::Result<()> {
  db.execute_batch(
    "CREATE TABLE IF NOT EXISTS run_control_bindings (
      scope TEXT PRIMARY KEY, facts TEXT NOT NULL, last_seen_at_ms INTEGER NOT NULL,
      revision INTEGER NOT NULL DEFAULT 0);
    CREATE TABLE IF NOT EXISTS run_control_commands (
      request_id TEXT PRIMARY KEY, scope TEXT NOT NULL, command TEXT NOT NULL,
      fingerprint TEXT NOT NULL, status TEXT NOT NULL, error TEXT, new_run_id TEXT);
    CREATE INDEX IF NOT EXISTS run_control_commands_scope ON run_control_commands(scope);",
  )
  .map_err(|_| crate::error::ExpriError::Message("cannot initialize run control".into()))?;
  let has_revision = db
    .prepare("PRAGMA table_info(run_control_bindings)")
    .and_then(|mut statement| {
      statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()
    })
    .map_err(|_| crate::error::ExpriError::Message("cannot inspect run control metadata".into()))?
    .iter()
    .any(|name| name == "revision");
  if !has_revision {
    db.execute(
      "ALTER TABLE run_control_bindings ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
      [],
    )
    .map_err(|_| crate::error::ExpriError::Message("cannot upgrade run control metadata".into()))?;
  }
  Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::service) struct ControlSubmission {
  pub project_id: String,
  pub origin: String,
  pub run_id: String,
  pub request_id: String,
  pub binding_id: String,
  pub expected_attempt: u64,
  pub expected_generation: u64,
  pub action: ControlAction,
  pub arguments: Option<Vec<String>>,
  pub checkpoint: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub requirements: Option<ResourceRequirements>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub experiment: Option<ExperimentSelection>,
  pub reason: String,
  pub grace_seconds: u32,
}

impl ControlSubmission {
  fn scope(&self) -> RunScope {
    RunScope {
      project_id: self.project_id.clone(),
      origin: self.origin.clone(),
      run_id: self.run_id.clone(),
    }
  }
}

#[derive(Clone, PartialEq, Deserialize, Serialize)]
struct Facts {
  binding_id: String,
  attempt: u64,
  generation: u64,
  status: String,
  capabilities: Vec<ControlAction>,
  command: Vec<String>,
  checkpoints: Vec<String>,
  #[serde(default)]
  lifecycle: Option<ControlLifecycle>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  requirements: Option<ResourceRequirements>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  resource_status: Option<ResourceStatus>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  manual_paused: Option<bool>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  experiment_assignment_supported: Option<bool>,
}

#[derive(Serialize)]
struct CommandRecord {
  request_id: String,
  action: ControlAction,
  status: String,
  error: Option<String>,
  new_run_id: Option<String>,
}

struct StoredCommand {
  command: ControlCommand,
  fingerprint: String,
  status: String,
  error: Option<String>,
  new_run_id: Option<String>,
}

type CommandRow = (String, String, String, Option<String>, Option<String>);

fn now_ms() -> i64 {
  chrono::Utc::now().timestamp_millis()
}

fn encode(value: &impl Serialize) -> ApiResult<String> {
  serde_json::to_string(value).map_err(|_| ApiError::new(500, "cannot encode run control"))
}

fn decode<T: serde::de::DeserializeOwned>(text: String) -> ApiResult<T> {
  serde_json::from_str(&text).map_err(|_| ApiError::new(500, "invalid stored run control"))
}

fn identifier(value: &str) -> ApiResult<()> {
  validate_component(value).map_err(bad)
}

fn bounded_text(value: &str, limit: usize) -> bool {
  value.len() <= limit
    && !value
      .chars()
      .any(|character| character == '\0' || character.is_control())
}

fn validate_argv(arguments: &[String], allow_empty: bool) -> ApiResult<()> {
  let arguments_only = if allow_empty {
    arguments
  } else {
    arguments.get(1..).unwrap_or_default()
  };
  if (!allow_empty && arguments.is_empty())
    || arguments_only.len() > 256
    || arguments_only.iter().map(String::len).sum::<usize>() > 32 * 1024
    || arguments
      .iter()
      .any(|argument| argument.len() > 8192 || argument.contains('\0'))
  {
    return Err(ApiError::new(
      400,
      "task arguments exceed run control limits",
    ));
  }
  Ok(())
}

fn checkpoint(path: &str) -> ApiResult<()> {
  if path.len() > 1024
    || !bounded_text(path, 1024)
    || path.contains('\\')
    || path
      .split('/')
      .any(|part| part.is_empty() || matches!(part, "." | ".."))
  {
    return Err(ApiError::new(
      400,
      "checkpoint must be a bounded relative path",
    ));
  }
  Ok(())
}

fn binding(db: &Connection, scope: &str) -> ApiResult<Option<(Facts, i64)>> {
  let record: Option<(String, i64)> = db
    .query_row(
      "SELECT facts,last_seen_at_ms FROM run_control_bindings WHERE scope=?1",
      [scope],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)?;
  record
    .map(|(facts, at)| Ok((decode(facts)?, at)))
    .transpose()
}

fn command_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRow> {
  Ok((
    row.get(0)?,
    row.get(1)?,
    row.get(2)?,
    row.get(3)?,
    row.get(4)?,
  ))
}

fn stored(record: CommandRow) -> ApiResult<StoredCommand> {
  Ok(StoredCommand {
    command: decode(record.0)?,
    fingerprint: record.1,
    status: record.2,
    error: record.3,
    new_run_id: record.4,
  })
}

fn latest(db: &Connection, scope: &str) -> ApiResult<Option<StoredCommand>> {
  db.query_row(
    "SELECT command,fingerprint,status,error,new_run_id FROM run_control_commands WHERE scope=?1 ORDER BY rowid DESC LIMIT 1",
    [scope], command_row,
  ).optional().map_err(database)?.map(stored).transpose()
}

fn expire(db: &Connection, scope: &str, at: i64) -> ApiResult<()> {
  // Delivery may have actuated even if its acknowledgement was lost. Only an
  // undelivered request can expire without the worker resolving its journal.
  let expired = db.execute(
    "UPDATE run_control_commands SET status='expired',error='worker did not receive command before delivery expired' WHERE scope=?1 AND status='requested' AND json_extract(command,'$.expires_at_ms')<=?2",
    params![scope, at],
  ).map_err(database)?;
  if expired != 0 {
    changed(db, &decode(scope.to_owned())?)?;
  }
  Ok(())
}

pub(super) fn revision(db: &Connection, scope: &RunScope) -> ApiResult<Option<String>> {
  db.query_row(
    "SELECT 'control:'||revision FROM run_control_bindings WHERE scope=?1",
    [encode(scope)?],
    |row| row.get(0),
  )
  .optional()
  .map_err(database)
}

fn changed(db: &Connection, scope: &RunScope) -> ApiResult<()> {
  db.execute(
    "UPDATE run_control_bindings SET revision=revision+1 WHERE scope=?1",
    [encode(scope)?],
  )
  .map_err(database)?;
  record_run_activity(db, scope)?;
  db.execute("INSERT INTO project_change_revisions(project_id,revision) VALUES(?1,1) ON CONFLICT(project_id) DO UPDATE SET revision=revision+1", [&scope.project_id]).map_err(database)?;
  Ok(())
}

fn snapshot(db: &Connection, scope: &str, at: i64) -> ApiResult<Value> {
  expire(db, scope, at)?;
  let request = latest(db, scope)?.map(|record| CommandRecord {
    request_id: record.command.request_id,
    action: record.command.action,
    status: record.status,
    error: record.error,
    new_run_id: record.new_run_id,
  });
  let Some((facts, last_seen_at_ms)) = binding(db, scope)? else {
    return Ok(
      json!({"supported":false,"online":false,"status":"unknown","attempt":0,
      "generation":0,"binding_id":"","capabilities":[],"command":[],"checkpoints":[],
      "last_seen_at_ms":0,"request":request}),
    );
  };
  let mut snapshot = json!({"supported":true,"online":at.saturating_sub(last_seen_at_ms)<=ONLINE_MS,
    "status":facts.status,"attempt":facts.attempt,"generation":facts.generation,
    "binding_id":facts.binding_id,"capabilities":facts.capabilities,"command":facts.command,
    "checkpoints":facts.checkpoints,"lifecycle":facts.lifecycle,
    "last_seen_at_ms":last_seen_at_ms,"request":request});
  if let Some(requirements) = &facts.requirements {
    snapshot["requirements"] = json!(requirements);
    snapshot["resource_status"] = json!(facts.resource_status);
  }
  if let Some(paused) = facts.manual_paused {
    snapshot["manual_paused"] = json!(paused);
  }
  if let Some(supported) = facts.experiment_assignment_supported {
    snapshot["experiment_assignment_supported"] = json!(supported);
  }
  Ok(snapshot)
}

fn validate_result(result: &ControlResult) -> ApiResult<()> {
  identifier(&result.request_id)?;
  if !matches!(result.status.as_str(), "executing" | "applied" | "failed")
    || result.error.as_ref().is_some_and(|error| {
      error.len() > 4096
        || error
          .chars()
          .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    })
    || (result.status != "failed" && result.error.is_some())
    || (result.status == "executing" && result.new_run_id.is_some())
  {
    return Err(ApiError::new(400, "invalid lifecycle command result"));
  }
  if let Some(run_id) = &result.new_run_id {
    identifier(run_id)?;
  }
  Ok(())
}

fn acknowledge(
  db: &Connection,
  scope: &str,
  facts: &Facts,
  result: &ControlResult,
) -> ApiResult<()> {
  validate_result(result)?;
  let record = db.query_row("SELECT command,fingerprint,status,error,new_run_id FROM run_control_commands WHERE scope=?1 AND request_id=?2", params![scope,result.request_id], command_row).optional().map_err(database)?.map(stored).transpose()?.ok_or_else(|| ApiError::new(409, "no lifecycle command to acknowledge"))?;
  if record.command.binding_id != facts.binding_id {
    return Err(ApiError::new(
      409,
      "lifecycle acknowledgement does not match the current binding and command",
    ));
  }
  if record.status == result.status
    && record.error == result.error
    && record.new_run_id == result.new_run_id
  {
    return Ok(());
  }
  if matches!(record.status.as_str(), "applied" | "failed") {
    return Err(ApiError::new(
      409,
      "lifecycle command already has a final result",
    ));
  }
  if !matches!(record.status.as_str(), "delivered" | "executing") {
    return Err(ApiError::new(
      409,
      "lifecycle command is no longer deliverable",
    ));
  }
  if latest(db, scope)?.is_none_or(|current| current.command.request_id != result.request_id) {
    return Err(ApiError::new(409, "lifecycle command was superseded"));
  }
  if let Some(new_run_id) = &result.new_run_id {
    if record.command.action != ControlAction::Restart {
      return Err(ApiError::new(400, "only restart can report a new run ID"));
    }
    let original: RunScope = decode(scope.to_owned())?;
    if *new_run_id == original.run_id {
      return Err(ApiError::new(400, "restart must report a fresh run ID"));
    }
  }
  if result.status == "applied"
    && record.command.action == ControlAction::Restart
    && result.new_run_id.is_none()
  {
    return Err(ApiError::new(400, "restart must report a fresh run ID"));
  }
  db.execute(
    "UPDATE run_control_commands SET status=?2,error=?3,new_run_id=?4 WHERE request_id=?1",
    params![
      result.request_id,
      result.status,
      result.error,
      result.new_run_id
    ],
  )
  .map_err(database)?;
  changed(db, &decode(scope.to_owned())?)?;
  Ok(())
}

impl<S: ObjectStorage> Store<S> {
  pub(super) fn poll_run_control_batch(
    &self,
    requests: Vec<RunControlRequest>,
  ) -> ApiResult<Response> {
    use expri_core::run_control::{ControlPollResult, MAX_CONTROL_POLLS};
    if requests.is_empty() || requests.len() > MAX_CONTROL_POLLS {
      return Err(ApiError::new(
        400,
        "run control batch must contain 1 to 64 polls",
      ));
    }
    let size = serde_json::to_vec(&Request::RunControlBatch {
      requests: requests.clone(),
    })
    .map_err(|_| ApiError::new(400, "invalid run control batch"))?
    .len();
    if size > MAX_REQUEST {
      return Err(ApiError::new(
        413,
        "run control batch exceeds request limit",
      ));
    }
    let mut reply_bytes = 0;
    let responses = requests
      .into_iter()
      .map(|request| {
        let mut result = match self.execute(Request::RunControl { request }) {
          Ok(Response::RunControl { response }) => ControlPollResult {
            response: Some(response),
            error: None,
          },
          _ => ControlPollResult {
            response: None,
            error: Some("run control poll unavailable".into()),
          },
        };
        let mut bytes = serde_json::to_vec(&result).map_or(MAX_REQUEST, |value| value.len());
        if reply_bytes + bytes > MAX_REQUEST - 4096 {
          // Acknowledgements are retained. Withheld commands remain durable
          // and are delivered later instead of poisoning the whole batch.
          if let Some(response) = &mut result.response {
            response.command = None;
          }
          bytes = serde_json::to_vec(&result).map_or(4096, |value| value.len());
        }
        reply_bytes += bytes;
        result
      })
      .collect();
    Ok(Response::RunControlBatch { responses })
  }

  pub(super) fn poll_run_control(
    &self,
    request: RunControlRequest,
  ) -> ApiResult<RunControlResponse> {
    self.poll_run_control_at(request, now_ms())
  }

  fn poll_run_control_at(
    &self,
    request: RunControlRequest,
    at: i64,
  ) -> ApiResult<RunControlResponse> {
    let RunControlRequest::Poll {
      scope,
      binding_id,
      attempt,
      generation,
      status,
      capabilities,
      command,
      checkpoints,
      lifecycle,
      requirements,
      resource_status,
      manual_paused,
      experiment_assignment_supported,
      result,
    } = request;
    validate_scope(&scope).map_err(bad)?;
    identifier(&binding_id)?;
    if attempt == 0
      || attempt > i64::MAX as u64
      || generation > i64::MAX as u64
      || !matches!(
        status.as_str(),
        "preparing"
          | "pending"
          | "running"
          | "waiting"
          | "paused"
          | "completed"
          | "failed"
          | "cancelled"
          | "lost"
      )
      || capabilities.len() > 6
      || checkpoints.len() > 100
    {
      return Err(ApiError::new(400, "invalid lifecycle worker facts"));
    }
    validate_argv(&command, false)?;
    requirements::validate_facts(
      requirements.as_ref(),
      resource_status.as_deref(),
      &capabilities,
    )?;
    for path in &checkpoints {
      checkpoint(path)?;
    }
    if let Some(lifecycle) = &lifecycle {
      let start = chrono::DateTime::parse_from_rfc3339(&lifecycle.started_at)
        .map_err(|_| ApiError::new(400, "invalid lifecycle start timestamp"))?;
      if lifecycle.started_at.len() > 64
        || lifecycle.finished_at.as_ref().is_some_and(|finish| {
          finish.len() > 64
            || chrono::DateTime::parse_from_rfc3339(finish).map_or(true, |finish| finish < start)
        })
      {
        return Err(ApiError::new(400, "invalid lifecycle finish timestamp"));
      }
    }
    let scope_json = encode(&scope)?;
    let facts = Facts {
      binding_id,
      attempt,
      generation,
      status,
      capabilities,
      command,
      checkpoints,
      lifecycle,
      requirements,
      resource_status: resource_status.map(|status| *status),
      manual_paused,
      experiment_assignment_supported,
    };
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    projects::ensure_active(&tx, &scope.project_id)?;
    run_retention::ensure_available(&tx, &scope, at / 1000)?;
    resource_queue::ensure_direct_run(&tx, &scope)?;
    let previous_binding = binding(&tx, &scope_json)?;
    let facts_changed = previous_binding
      .as_ref()
      .is_none_or(|(previous, _)| previous != &facts);
    if let Some((previous, last_seen)) = previous_binding {
      if previous.binding_id != facts.binding_id {
        if at.saturating_sub(last_seen) <= ONLINE_MS {
          return Err(ApiError::new(409, "another worker binding is still online"));
        }
        if result.is_some() {
          return Err(ApiError::new(
            409,
            "new worker binding cannot acknowledge previous commands",
          ));
        }
        if tx.execute("UPDATE run_control_commands SET status='failed',error='worker binding changed' WHERE scope=?1 AND status IN ('requested','delivered','executing')", [&scope_json]).map_err(database)? != 0 { changed(&tx, &scope)?; }
      } else if facts.attempt < previous.attempt || facts.generation < previous.generation {
        return Err(ApiError::new(409, "stale lifecycle worker facts"));
      }
    } else if result.is_some() {
      return Err(ApiError::new(
        409,
        "unregistered worker cannot acknowledge commands",
      ));
    }
    if let Some(result) = &result {
      acknowledge(&tx, &scope_json, &facts, result)?;
    }
    expire(&tx, &scope_json, at)?;
    tx.execute("INSERT INTO run_control_bindings(scope,facts,last_seen_at_ms) VALUES(?1,?2,?3) ON CONFLICT(scope) DO UPDATE SET facts=excluded.facts,last_seen_at_ms=excluded.last_seen_at_ms", params![scope_json,encode(&facts)?,at]).map_err(database)?;
    if facts_changed {
      changed(&tx, &scope)?;
    }
    let mut outgoing = None;
    if let Some(record) = latest(&tx, &scope_json)?
      && matches!(
        record.status.as_str(),
        "requested" | "delivered" | "executing"
      )
    {
      if record.command.binding_id == facts.binding_id
        // Continue can advance the attempt before its final acknowledgement.
        // Replaying an executing command consults the worker's durable journal.
        && (record.status == "executing"
          || (record.command.expected_attempt == facts.attempt
            && record.command.expected_generation == facts.generation))
      {
        if record.status == "requested" {
          tx.execute(
            "UPDATE run_control_commands SET status='delivered' WHERE request_id=?1",
            [&record.command.request_id],
          )
          .map_err(database)?;
          changed(&tx, &scope)?;
        }
        let acknowledged_execution = result.as_ref().is_some_and(|result| {
          result.status == "executing" && result.request_id == record.command.request_id
        });
        if !acknowledged_execution {
          outgoing = Some(record.command);
        }
      } else {
        tx.execute("UPDATE run_control_commands SET status='failed',error='run attempt or generation changed' WHERE request_id=?1", [&record.command.request_id]).map_err(database)?;
        changed(&tx, &scope)?;
      }
    }
    tx.commit().map_err(database)?;
    Ok(RunControlResponse { command: outgoing })
  }

  pub(in crate::service) fn run_control(&self, scope: &RunScope) -> ApiResult<Value> {
    validate_scope(scope).map_err(bad)?;
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = gate
      .read()
      .map_err(|_| ApiError::new(503, "project operation unavailable"))?;
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    projects::ensure_active(&tx, &scope.project_id)?;
    run_retention::ensure_available(&tx, scope, at_seconds())?;
    let mut result = snapshot(&tx, &encode(scope)?, now_ms())?;
    if resource_queue::is_queued_run(&tx, scope)? {
      result["supported"] = json!(false);
      result["capabilities"] = json!([]);
    }
    tx.commit().map_err(database)?;
    Ok(result)
  }

  pub(in crate::service) fn submit_run_control(
    &self,
    selection: ControlSubmission,
  ) -> ApiResult<Value> {
    self.submit_run_control_at(selection, now_ms())
  }

  fn submit_run_control_at(&self, selection: ControlSubmission, at: i64) -> ApiResult<Value> {
    let scope = selection.scope();
    validate_scope(&scope).map_err(bad)?;
    identifier(&selection.request_id)?;
    identifier(&selection.binding_id)?;
    if selection.reason.trim().is_empty()
      || !bounded_text(&selection.reason, 1024)
      || !(1..=3600).contains(&selection.grace_seconds)
    {
      return Err(ApiError::new(
        400,
        "invalid lifecycle reason or grace period",
      ));
    }
    if let Some(arguments) = &selection.arguments {
      validate_argv(arguments, true)?;
    }
    if let Some(path) = &selection.checkpoint {
      checkpoint(path)?;
    }
    if !matches!(
      selection.action,
      ControlAction::Continue | ControlAction::Restart
    ) && (selection.arguments.is_some() || selection.checkpoint.is_some())
    {
      return Err(ApiError::new(
        400,
        "only continue or restart accepts task arguments",
      ));
    }
    if selection.action == ControlAction::Restart && selection.checkpoint.is_some() {
      return Err(ApiError::new(400, "restart does not accept a checkpoint"));
    }
    if let Some(experiment) = &selection.experiment {
      if selection.action != ControlAction::Restart {
        return Err(ApiError::new(
          400,
          "only restart accepts an experiment selection",
        ));
      }
      experiment.validate().map_err(bad)?;
    }
    match (&selection.action, &selection.requirements) {
      (ControlAction::SetRequirements, Some(requirements)) => {
        requirements.validate().map_err(bad)?
      }
      (ControlAction::SetRequirements, None) => {
        return Err(ApiError::new(
          400,
          "set_requirements requires a disk requirements policy",
        ));
      }
      (_, Some(_)) => {
        return Err(ApiError::new(
          400,
          "only set_requirements accepts requirements",
        ));
      }
      (_, None) => {}
    }
    let fingerprint = encode(&selection)?;
    let scope_json = encode(&scope)?;
    let gate = self.project_gate(&scope.project_id)?;
    let _operation = gate
      .read()
      .map_err(|_| ApiError::new(503, "project operation unavailable"))?;
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    projects::ensure_active(&tx, &scope.project_id)?;
    run_retention::ensure_available(&tx, &scope, at / 1000)?;
    resource_queue::ensure_direct_run(&tx, &scope)?;
    let existing = tx.query_row("SELECT command,fingerprint,status,error,new_run_id FROM run_control_commands WHERE request_id=?1", [&selection.request_id], command_row).optional().map_err(database)?.map(stored).transpose()?;
    if let Some(existing) = existing {
      if existing.fingerprint != fingerprint {
        return Err(ApiError::new(
          409,
          "lifecycle request ID was already used for different arguments",
        ));
      }
      let result = snapshot(&tx, &scope_json, at)?;
      tx.commit().map_err(database)?;
      return Ok(result);
    }
    expire(&tx, &scope_json, at)?;
    let (facts, last_seen) = binding(&tx, &scope_json)?
      .ok_or_else(|| ApiError::new(409, "run worker does not support website lifecycle"))?;
    if at.saturating_sub(last_seen) > ONLINE_MS {
      return Err(ApiError::new(409, "run worker is offline"));
    }
    if facts.binding_id != selection.binding_id
      || facts.attempt != selection.expected_attempt
      || facts.generation != selection.expected_generation
    {
      return Err(ApiError::new(
        409,
        "run attempt, generation, or worker binding changed; refresh before retrying",
      ));
    }
    if !facts.capabilities.contains(&selection.action) {
      return Err(ApiError::new(
        409,
        "worker does not support this action in the current run state",
      ));
    }
    if selection.experiment.is_some() && facts.experiment_assignment_supported != Some(true) {
      return Err(ApiError::new(
        409,
        "experiment assignment requires a current publishing worker authorized to manage experiments",
      ));
    }
    if let Some(experiment) = &selection.experiment {
      experiments::validate_assignment_selection(&tx, &scope.project_id, experiment)?;
    }
    if selection.action == ControlAction::Continue
      && selection
        .checkpoint
        .as_ref()
        .is_none_or(|path| !facts.checkpoints.contains(path))
    {
      return Err(ApiError::new(
        400,
        "continue requires one of the worker's recorded checkpoints",
      ));
    }
    if latest(&tx, &scope_json)?.is_some_and(|record| {
      matches!(
        record.status.as_str(),
        "requested" | "delivered" | "executing"
      )
    }) {
      return Err(ApiError::new(
        409,
        "another lifecycle command is in progress",
      ));
    }
    let command = ControlCommand {
      request_id: selection.request_id,
      binding_id: selection.binding_id,
      expected_attempt: selection.expected_attempt,
      expected_generation: selection.expected_generation,
      action: selection.action,
      arguments: selection.arguments,
      checkpoint: selection.checkpoint,
      requirements: selection.requirements,
      experiment: selection.experiment,
      reason: selection.reason,
      grace_seconds: selection.grace_seconds,
      expires_at_ms: at.saturating_add(DELIVERY_MS),
    };
    tx.execute("INSERT INTO run_control_commands(request_id,scope,command,fingerprint,status) VALUES(?1,?2,?3,?4,'requested')", params![command.request_id,scope_json,encode(&command)?,fingerprint]).map_err(database)?;
    changed(&tx, &scope)?;
    let result = snapshot(&tx, &scope_json, at)?;
    tx.commit().map_err(database)?;
    Ok(result)
  }
}

fn at_seconds() -> i64 {
  now_ms() / 1000
}

#[cfg(test)]
mod tests;
