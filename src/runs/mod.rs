use std::collections::BTreeSet;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, FixedOffset, Utc};
use serde_json::{Value, json};

use crate::error::{ExpriError, Result};
use crate::protocol::RunQueryRequest;

pub const CATALOG_SCRIPT: &str = include_str!("catalog.py");
const METADATA_LIMIT: u64 = 256 * 1024;
const DETAIL_LIMIT: u64 = 16 * 1024 * 1024;
const EXCLUDED_COMPONENTS: [&str; 6] =
  [".venv", ".expri", ".git", ".cache", "cache", "__pycache__"];
const STATUSES: [&str; 7] = [
  "preparing",
  "running",
  "completed",
  "failed",
  "cancelled",
  "lost",
  "unknown",
];
const METADATA_FILES: [&str; 3] = [
  "run-state.json",
  "snapshot.json",
  "environment/environment-state.json",
];

struct Record {
  summary: Value,
  state: Option<Value>,
  started_at: Option<DateTime<FixedOffset>>,
  warnings: Vec<Value>,
}

/// Read the fixed run summary without loading the source or environment inventory.
pub fn summary_directory(runs_dir: &Path, run_id: &str) -> Result<Value> {
  validate_id(run_id)?;
  let runs_dir = std::path::absolute(runs_dir)?;
  let metadata = fs::symlink_metadata(&runs_dir)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message("run catalog directory must be a real directory"));
  }
  if runs_dir
    .parent()
    .is_some_and(|parent| parent.ends_with(".expri"))
  {
    let metadata = fs::symlink_metadata(runs_dir.parent().unwrap())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
      return Err(message(
        "run catalog directory has an unsafe parent directory",
      ));
    }
  }
  let run_dir = fs::canonicalize(runs_dir.parent().unwrap())?
    .join(runs_dir.file_name().unwrap())
    .join(run_id);
  let metadata = fs::symlink_metadata(&run_dir)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message("run directory must be a real directory"));
  }
  let record = record(&run_dir);
  Ok(json!({"run": record.summary, "warnings": record.warnings}))
}

/// Inspect fixed run records without taking leases or reading package inventories.
pub fn query(repo_root: &Path, request: &RunQueryRequest) -> Result<Value> {
  let root = fs::canonicalize(repo_root)?;
  let state_dir = root.join(".expri");
  if let Some(metadata) = optional_metadata(&state_dir)?
    && (!metadata.is_dir() || metadata.file_type().is_symlink())
  {
    return Err(message(
      "run catalog directory has an unsafe parent directory",
    ));
  }
  query_directory(&state_dir.join("runs"), request)
}

/// Inspect a local catalog or downloaded records whose recorded paths are remote.
pub fn query_directory(runs_dir: &Path, request: &RunQueryRequest) -> Result<Value> {
  validate_request(request)?;
  let runs_dir = std::path::absolute(runs_dir)?;
  let present = match optional_metadata(&runs_dir)? {
    Some(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => true,
    Some(_) => return Err(message("run catalog directory must be a real directory")),
    None => false,
  };
  // The repository/cache prefix may use normal filesystem aliases such as /tmp.
  // Owned catalog boundaries and everything selected below them remain unlinked.
  let runs_dir = if present {
    fs::canonicalize(runs_dir.parent().unwrap())?.join(runs_dir.file_name().unwrap())
  } else {
    runs_dir
  };
  if let RunQueryRequest::List {
    task,
    status,
    limit,
  } = request
  {
    if !present {
      return Ok(json!({"runs": [], "warnings": []}));
    }
    return list(&runs_dir, task.as_deref(), status.as_deref(), *limit);
  }
  let run_id = match request {
    RunQueryRequest::Show { run_id } | RunQueryRequest::Files { run_id, .. } => run_id,
    RunQueryRequest::List { .. } => unreachable!(),
  };
  let run_dir = runs_dir.join(run_id);
  if !present || optional_metadata(&run_dir)?.is_none() {
    return Err(message(format!("run is missing: {run_id}")));
  }
  let metadata = fs::symlink_metadata(&run_dir)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message(format!(
      "directory must not be a symlink or file: {}",
      run_dir.display()
    )));
  }
  match request {
    RunQueryRequest::Show { .. } => Ok(show(&run_dir)),
    RunQueryRequest::Files {
      artifacts, metrics, ..
    } => files(&run_dir, artifacts, *metrics),
    RunQueryRequest::List { .. } => unreachable!(),
  }
}

fn validate_request(request: &RunQueryRequest) -> Result<()> {
  match request {
    RunQueryRequest::List { task, status, .. } => {
      if let Some(task) = task
        && task.trim().is_empty()
      {
        return Err(message("task filter must be a nonempty string"));
      }
      if let Some(status) = status
        && !STATUSES.contains(&status.as_str())
      {
        return Err(message(format!("invalid run status: {status}")));
      }
    }
    RunQueryRequest::Show { run_id } => validate_id(run_id)?,
    RunQueryRequest::Files {
      run_id, artifacts, ..
    } => {
      validate_id(run_id)?;
      for artifact in artifacts {
        validate_artifact(artifact)?;
      }
    }
  }
  Ok(())
}

pub(crate) fn validate_id(run_id: &str) -> Result<()> {
  let mut bytes = run_id.bytes();
  if !bytes
    .next()
    .is_some_and(|byte| byte.is_ascii_alphanumeric())
    || !bytes.all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
  {
    return Err(message(format!("invalid run ID: {run_id}")));
  }
  Ok(())
}

fn validate_artifact(artifact: &str) -> Result<&str> {
  let normalized = artifact.strip_suffix('/').unwrap_or(artifact);
  let parts: Vec<_> = normalized.split('/').collect();
  if !matches!(parts[0], "code" | "outputs")
    || parts.iter().any(|part| matches!(*part, "" | "." | ".."))
    || artifact.contains('\\')
    || artifact.chars().any(char::is_control)
    || parts.iter().any(|part| EXCLUDED_COMPONENTS.contains(part))
  {
    return Err(message(format!("invalid artifact path: {artifact}")));
  }
  Ok(normalized)
}

fn list(
  runs_dir: &Path,
  task: Option<&str>,
  status: Option<&str>,
  limit: Option<usize>,
) -> Result<Value> {
  let mut paths = fs::read_dir(runs_dir)?
    .map(|entry| entry.map(|entry| entry.path()))
    .collect::<std::io::Result<Vec<_>>>()?;
  paths.sort();
  let mut warnings = Vec::new();
  let mut records = Vec::new();
  for path in paths {
    let Some(run_id) = path.file_name().and_then(|name| name.to_str()) else {
      continue;
    };
    if validate_id(run_id).is_err() {
      continue;
    }
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() {
      warn(
        &mut warnings,
        run_id,
        "run directory must be a real directory",
      );
      continue;
    }
    if !metadata.is_dir() {
      continue;
    }
    let record = record(&path);
    warnings.extend(record.warnings);
    if task.is_some_and(|task| record.summary["task"].as_str() != Some(task))
      || status.is_some_and(|status| record.summary["status"].as_str() != Some(status))
    {
      continue;
    }
    records.push((record.started_at, run_id.to_string(), record.summary));
  }
  records.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
  let runs: Vec<_> = records
    .into_iter()
    .take(limit.unwrap_or(usize::MAX))
    .map(|(_, _, summary)| summary)
    .collect();
  Ok(json!({"runs": runs, "warnings": warnings}))
}

fn show(run_dir: &Path) -> Value {
  let mut record = record(run_dir);
  let snapshot = optional_json(run_dir, "snapshot.json", &mut record.warnings, true);
  let environment = optional_json(
    run_dir,
    "environment/environment-state.json",
    &mut record.warnings,
    true,
  );
  json!({
    "run": record.summary, "state": record.state, "snapshot": snapshot,
    "environment": environment, "warnings": record.warnings,
  })
}

fn record(run_dir: &Path) -> Record {
  let run_id = run_dir.file_name().unwrap().to_string_lossy();
  let mut warnings = Vec::new();
  let state = optional_json(run_dir, "run-state.json", &mut warnings, true);
  let mut record = record_from_state(&run_id, state);
  warnings.append(&mut record.warnings);
  record.warnings = warnings;
  record
}

/// Apply the same catalog validation to locally saved and hosted run states.
pub(crate) fn summary_from_state(run_id: &str, state: Option<Value>) -> Value {
  let record = record_from_state(run_id, state);
  json!({"run": record.summary, "state": record.state, "warnings": record.warnings})
}

fn record_from_state(run_id: &str, state: Option<Value>) -> Record {
  let mut warnings = Vec::new();
  let mut summary = json!({
    "run_id": run_id, "task": null, "status": "unknown", "started_at": null,
    "finished_at": null, "exit_code": null, "schema_version": 0,
  });
  let mut started_at = None;
  if let Some(state) = &state {
    let mut validate = || {
      let Some(object) = state.as_object() else {
        warn(
          &mut warnings,
          run_id,
          "run-state.json must contain a JSON object",
        );
        return;
      };
      let schema = match object.get("schema_version") {
        None => 0,
        Some(value) => match value.as_u64() {
          Some(schema) => schema,
          None => {
            summary["schema_version"] = Value::Null;
            warn(
              &mut warnings,
              run_id,
              "run-state.json has invalid schema_version",
            );
            return;
          }
        },
      };
      summary["schema_version"] = json!(schema);
      if schema > 1 {
        warn(
          &mut warnings,
          run_id,
          format!("run-state.json uses unsupported schema_version {schema}"),
        );
        return;
      }
      if object.get("run_id").and_then(Value::as_str) != Some(run_id) {
        warn(
          &mut warnings,
          run_id,
          "run-state.json run_id does not match its directory",
        );
        return;
      }
      if let Some(task) = object
        .get("task")
        .and_then(Value::as_str)
        .filter(|task| !task.trim().is_empty())
      {
        summary["task"] = json!(task);
      } else {
        field_warning(&mut warnings, run_id, "task");
      }
      if let Some(status) = object
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| *status != "unknown" && STATUSES.contains(status))
      {
        summary["status"] = json!(status);
      } else {
        field_warning(&mut warnings, run_id, "status");
      }
      started_at = object.get("started_at").and_then(parse_timestamp);
      if started_at.is_some() {
        summary["started_at"] = object["started_at"].clone();
      } else {
        field_warning(&mut warnings, run_id, "started_at");
      }
      let terminal = matches!(
        summary["status"].as_str(),
        Some("completed" | "failed" | "cancelled")
      );
      if object
        .get("finished_at")
        .and_then(parse_timestamp)
        .is_some()
      {
        summary["finished_at"] = object["finished_at"].clone();
      } else if object.contains_key("finished_at") || terminal {
        field_warning(&mut warnings, run_id, "finished_at");
      }
      if let Some(exit_code) = object
        .get("exit_code")
        .and_then(Value::as_i64)
        .filter(|code| i32::try_from(*code).is_ok())
      {
        summary["exit_code"] = json!(exit_code);
      } else if object.contains_key("exit_code") || terminal {
        field_warning(&mut warnings, run_id, "exit_code");
      }
    };
    validate();
  }
  Record {
    summary,
    state,
    started_at,
    warnings,
  }
}

fn parse_timestamp(value: &Value) -> Option<DateTime<FixedOffset>> {
  let text = value.as_str()?;
  if !matches!(text.as_bytes().get(10), Some(b'T' | b't')) {
    return None;
  }
  let parsed = DateTime::parse_from_rfc3339(text).ok()?;
  let utc = parsed.with_timezone(&Utc);
  if !(1..=9999).contains(&parsed.year())
    || !(1..=9999).contains(&utc.year())
    || parsed.timestamp_subsec_nanos() >= 1_000_000_000
  {
    return None;
  }
  Some(parsed)
}

fn field_warning(warnings: &mut Vec<Value>, run_id: &str, field: &str) {
  warn(
    warnings,
    run_id,
    format!("run-state.json has invalid or missing {field}"),
  );
}

fn optional_json(
  run_dir: &Path,
  relative: &str,
  warnings: &mut Vec<Value>,
  missing: bool,
) -> Option<Value> {
  match read_json(run_dir, relative) {
    Ok(Some(value)) if relative != "run-state.json" || value.is_null() => {
      if value.is_object() {
        Some(value)
      } else {
        warn(
          warnings,
          &run_dir.file_name().unwrap().to_string_lossy(),
          format!("{relative} must contain a JSON object"),
        );
        None
      }
    }
    Ok(Some(value)) => Some(value),
    Ok(None) => {
      if missing {
        warn(
          warnings,
          &run_dir.file_name().unwrap().to_string_lossy(),
          format!("{relative} is missing"),
        );
      }
      None
    }
    Err(error) => {
      warn(
        warnings,
        &run_dir.file_name().unwrap().to_string_lossy(),
        error,
      );
      None
    }
  }
}

fn read_json(run_dir: &Path, relative: &str) -> std::result::Result<Option<Value>, String> {
  let limit = if relative == "run-state.json" {
    METADATA_LIMIT
  } else {
    DETAIL_LIMIT
  };
  let Some(path) = safe_path(run_dir, relative)? else {
    return Ok(None);
  };
  let Some(metadata) =
    optional_metadata(&path).map_err(|_| format!("{relative} could not be read"))?
  else {
    return Ok(None);
  };
  if !metadata.is_file() || metadata.file_type().is_symlink() {
    return Err(format!("{relative} must be a regular file"));
  }
  if metadata.len() > limit {
    return Err(format!("{relative} exceeds the metadata size limit"));
  }
  let file = File::open(&path).map_err(|_| format!("{relative} could not be read"))?;
  let opened = file
    .metadata()
    .map_err(|_| format!("{relative} could not be read"))?;
  if !opened.is_file() || !same_file(&metadata, &opened) {
    return Err(format!("{relative} could not be read"));
  }
  safe_path(run_dir, relative)?;
  let mut contents = Vec::new();
  file
    .take(limit + 1)
    .read_to_end(&mut contents)
    .map_err(|_| format!("{relative} could not be read"))?;
  if contents.len() as u64 > limit {
    return Err(format!("{relative} exceeds the metadata size limit"));
  }
  serde_json::from_slice(&contents)
    .map(Some)
    .map_err(|_| format!("{relative} contains invalid JSON"))
}

fn same_file(expected: &Metadata, opened: &Metadata) -> bool {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    expected.dev() == opened.dev() && expected.ino() == opened.ino()
  }
  #[cfg(not(unix))]
  {
    expected.len() == opened.len() && expected.modified().ok() == opened.modified().ok()
  }
}

fn safe_path(run_dir: &Path, relative: &str) -> std::result::Result<Option<PathBuf>, String> {
  let mut current = run_dir.to_path_buf();
  let parts: Vec<_> = relative.split('/').collect();
  for part in &parts[..parts.len() - 1] {
    current.push(part);
    let Some(metadata) =
      optional_metadata(&current).map_err(|_| format!("{relative} could not be read"))?
    else {
      return Ok(None);
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
      return Err(format!("{relative} has an unsafe parent directory"));
    }
  }
  Ok(Some(current.join(parts.last().unwrap())))
}

fn files(run_dir: &Path, artifacts: &[String], metrics: bool) -> Result<Value> {
  let mut selected = BTreeSet::new();
  let mut warnings = Vec::new();
  for relative in METADATA_FILES {
    collect_files(
      run_dir,
      relative,
      &mut selected,
      &mut warnings,
      false,
      false,
    )?;
  }
  if metrics {
    for relative in ["outputs/metrics.jsonl", "outputs/params.json"] {
      let warnings_before = warnings.len();
      collect_files(
        run_dir,
        relative,
        &mut selected,
        &mut warnings,
        false,
        false,
      )?;
      if !selected.contains(relative) && warnings.len() == warnings_before {
        warn(
          &mut warnings,
          &run_dir.file_name().unwrap().to_string_lossy(),
          format!("{relative} is missing"),
        );
      }
    }
  } else {
    collect_files(run_dir, "logs", &mut selected, &mut warnings, false, true)?;
  }
  for artifact in artifacts {
    collect_files(
      run_dir,
      validate_artifact(artifact)?,
      &mut selected,
      &mut warnings,
      true,
      true,
    )?;
  }
  Ok(
    json!({"run_id": run_dir.file_name().unwrap().to_string_lossy(), "run_dir": run_dir, "files": selected, "warnings": warnings}),
  )
}

fn collect_files(
  run_dir: &Path,
  relative: &str,
  files: &mut BTreeSet<String>,
  warnings: &mut Vec<Value>,
  explicit: bool,
  recurse: bool,
) -> Result<()> {
  if relative
    .split('/')
    .any(|part| EXCLUDED_COMPONENTS.contains(&part))
  {
    warn(
      warnings,
      &run_dir.file_name().unwrap().to_string_lossy(),
      format!("{relative} is excluded from artifact selection"),
    );
    return Ok(());
  }
  if relative.chars().any(char::is_control) || relative.contains('\\') {
    let error = format!("invalid run file path: {relative}");
    if explicit {
      return Err(message(error));
    }
    warn(
      warnings,
      &run_dir.file_name().unwrap().to_string_lossy(),
      error,
    );
    return Ok(());
  }
  let collect = (|| -> std::result::Result<(), String> {
    let path = safe_path(run_dir, relative)?;
    let metadata = path
      .as_ref()
      .map(|path| optional_metadata(path))
      .transpose()
      .map_err(|error| error.to_string())?
      .flatten();
    let (Some(path), Some(metadata)) = (path, metadata) else {
      if explicit {
        return Err(format!("artifact is missing: {relative}"));
      }
      return Ok(());
    };
    if relative == "logs" && !explicit && !metadata.is_dir() {
      return Err("logs must be a real directory".to_string());
    }
    if metadata.is_file() && !metadata.file_type().is_symlink() {
      files.insert(relative.to_string());
    } else if metadata.is_dir() && !metadata.file_type().is_symlink() && recurse {
      let mut entries = fs::read_dir(path)
        .map_err(|error| error.to_string())?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
      entries.sort();
      for name in entries {
        let name = name
          .to_str()
          .ok_or_else(|| "run file name is not valid UTF-8".to_string())?;
        collect_files(
          run_dir,
          &format!("{relative}/{name}"),
          files,
          warnings,
          explicit,
          true,
        )
        .map_err(|error| error.to_string())?;
      }
    } else {
      return Err(if recurse {
        format!("{relative} must not be a symlink or special file")
      } else {
        format!("{relative} must be a regular file")
      });
    }
    Ok(())
  })();
  if let Err(error) = collect {
    if explicit {
      return Err(message(error));
    }
    warn(
      warnings,
      &run_dir.file_name().unwrap().to_string_lossy(),
      error,
    );
  }
  Ok(())
}

fn optional_metadata(path: &Path) -> std::io::Result<Option<Metadata>> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error),
  }
}

fn warn(warnings: &mut Vec<Value>, run_id: &str, message: impl Into<String>) {
  warnings.push(json!({"run_id": run_id, "message": message.into()}));
}

fn message(message: impl Into<String>) -> ExpriError {
  ExpriError::Message(message.into())
}

#[cfg(test)]
mod tests;
