use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{ExpriError, Result};
use crate::lock::{self, FileLock, LockAttempt};
use crate::protocol::PruneRequest;

#[derive(Debug, Deserialize, Serialize)]
pub struct PruneReport {
  pub apply: bool,
  pub keep_last: usize,
  pub runs: Vec<PruneRun>,
  pub logical_bytes: u64,
  pub pruned_runs: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PruneRun {
  pub run_id: String,
  pub status: Option<String>,
  pub action: String,
  pub reason: String,
  pub logical_bytes: u64,
}

struct Candidate {
  run_dir: PathBuf,
  finished_at: DateTime<FixedOffset>,
  entry: PruneRun,
}

struct FinishedRun {
  status: String,
  finished_at: DateTime<FixedOffset>,
}

/// Remove only owned virtual environments of verifiably finished runs.
///
/// Preview never creates directories, lock files, or audit metadata. Unknown
/// and stale states remain untouched even when their leases are available.
pub fn prune(repo_root: &Path, request: &PruneRequest) -> Result<PruneReport> {
  let root = fs::canonicalize(repo_root)?;
  let mut report = PruneReport {
    apply: request.apply,
    keep_last: request.keep_last,
    runs: Vec::new(),
    logical_bytes: 0,
    pruned_runs: 0,
  };
  let state_dir = root.join(".expri");
  if !real_directory_optional(&state_dir)? {
    return Ok(report);
  }
  let runs_dir = state_dir.join("runs");
  if !real_directory_optional(&runs_dir)? {
    return Ok(report);
  }
  let mut run_dirs = fs::read_dir(&runs_dir)?
    .map(|entry| entry.map(|entry| entry.path()))
    .collect::<std::io::Result<Vec<_>>>()?;
  run_dirs.sort();
  let mut candidates = Vec::new();
  for run_dir in run_dirs {
    let mut entry = PruneRun {
      run_id: run_dir
        .file_name()
        .expect("run directory has a name")
        .to_string_lossy()
        .into_owned(),
      status: None,
      action: "skipped".to_string(),
      reason: String::new(),
      logical_bytes: 0,
    };
    match candidate(&run_dir, request.apply, &mut entry) {
      Ok(Some(candidate)) => candidates.push(candidate),
      Ok(None) => report.runs.push(entry),
      Err(reason) => {
        entry.reason = reason;
        report.runs.push(entry);
      }
    }
  }
  candidates.sort_by(|left, right| {
    right
      .finished_at
      .cmp(&left.finished_at)
      .then_with(|| right.entry.run_id.cmp(&left.entry.run_id))
  });
  for (index, mut candidate) in candidates.into_iter().enumerate() {
    if index < request.keep_last {
      candidate.entry.action = "kept".to_string();
      candidate.entry.reason = "retained by keep_last".to_string();
    } else if !request.apply {
      candidate.entry.action = "preview".to_string();
      candidate.entry.reason = "finished run environment would be removed".to_string();
      report.logical_bytes = report
        .logical_bytes
        .saturating_add(candidate.entry.logical_bytes);
    } else {
      match remove_candidate(&candidate) {
        Ok(audit_error) => {
          candidate.entry.action = "pruned".to_string();
          candidate.entry.reason = "finished run environment removed".to_string();
          report.pruned_runs += 1;
          report.logical_bytes = report
            .logical_bytes
            .saturating_add(candidate.entry.logical_bytes);
          if let Some(error) = audit_error {
            candidate.entry.reason =
              format!("finished run environment removed; could not save prune audit: {error}");
          }
        }
        Err(reason) => candidate.entry.reason = reason,
      }
    }
    report.runs.push(candidate.entry);
  }
  report
    .runs
    .sort_by(|left, right| left.run_id.cmp(&right.run_id));
  Ok(report)
}

fn candidate(
  run_dir: &Path,
  apply: bool,
  entry: &mut PruneRun,
) -> std::result::Result<Option<Candidate>, String> {
  let finished = inspect_finished(run_dir)?;
  entry.status = Some(finished.status);
  if !validate_environment(run_dir)? {
    entry.action = "already_pruned".to_string();
    entry.reason = "run environment is absent".to_string();
    return Ok(None);
  }
  let _run_lock = available_lock(&run_dir.join(".run.lock"), apply)?;
  let _prepare_lock = available_lock(&run_dir.join("environment/.prepare.lock"), apply)?;
  let finished = inspect_finished(run_dir)?;
  entry.status = Some(finished.status);
  if !validate_environment(run_dir)? {
    entry.action = "already_pruned".to_string();
    entry.reason = "run environment is absent".to_string();
    return Ok(None);
  }
  entry.logical_bytes = logical_bytes(&run_dir.join("environment/.venv"))
    .map_err(|error| format!("cannot inspect run environment: {error}"))?;
  Ok(Some(Candidate {
    run_dir: run_dir.to_path_buf(),
    finished_at: finished.finished_at,
    entry: entry.clone(),
  }))
}

fn remove_candidate(candidate: &Candidate) -> std::result::Result<Option<String>, String> {
  let run_dir = &candidate.run_dir;
  let _run_lock = available_lock(&run_dir.join(".run.lock"), true)?;
  let _prepare_lock = available_lock(&run_dir.join("environment/.prepare.lock"), true)?;
  let finished = inspect_finished(run_dir)?;
  if finished.finished_at != candidate.finished_at
    || Some(&finished.status) != candidate.entry.status.as_ref()
  {
    return Err("run state changed during cleanup".to_string());
  }
  if !validate_environment(run_dir)? {
    return Err("run environment changed during cleanup".to_string());
  }
  // Bound descriptor use to one run, then revalidate while both leases are held.
  // Never use a deletion path supplied by a metadata document.
  fs::remove_dir_all(run_dir.join("environment/.venv")).map_err(|error| error.to_string())?;
  Ok(
    write_prune_audit(run_dir, &candidate.entry)
      .err()
      .map(|error| error.to_string()),
  )
}

fn available_lock(path: &Path, create: bool) -> std::result::Result<Option<FileLock>, String> {
  match lock::try_lock_file(path, create).map_err(|error| error.to_string())? {
    LockAttempt::Acquired(file) => Ok(Some(file)),
    LockAttempt::Missing => Ok(None),
    LockAttempt::Busy => Err("run or environment preparation is active".to_string()),
  }
}

fn inspect_finished(run_dir: &Path) -> std::result::Result<FinishedRun, String> {
  real_directory(run_dir).map_err(|_| "run directory is not a real directory".to_string())?;
  let state = read_json(&run_dir.join("run-state.json"))
    .map_err(|_| "missing or invalid run state".to_string())?;
  let id = run_dir.file_name().and_then(|name| name.to_str());
  let code_dir = run_dir.join("code");
  if state.get("run_id").and_then(Value::as_str) != id
    || state.get("code_dir").and_then(Value::as_str) != code_dir.to_str()
  {
    return Err("run state does not match its directory".to_string());
  }
  let status = state
    .get("status")
    .and_then(Value::as_str)
    .unwrap_or_default();
  if !matches!(status, "completed" | "failed") {
    return Err("run has not verifiably finished".to_string());
  }
  let finished_at = state
    .get("finished_at")
    .and_then(Value::as_str)
    .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
    .ok_or_else(|| "missing or invalid run completion time".to_string())?;
  real_directory(&code_dir)
    .map_err(|_| "run code directory is not a real directory".to_string())?;
  Ok(FinishedRun {
    status: status.to_string(),
    finished_at,
  })
}

fn validate_environment(run_dir: &Path) -> std::result::Result<bool, String> {
  let environment_dir = run_dir.join("environment");
  if !real_directory_optional(&environment_dir).map_err(|error| error.to_string())? {
    return Ok(false);
  }
  let environment_path = environment_dir.join(".venv");
  if !real_directory_optional(&environment_path).map_err(|error| error.to_string())? {
    return Ok(false);
  }
  let owner = read_json(&environment_dir.join("owner.json"))
    .map_err(|_| "missing or invalid environment owner".to_string())?;
  if owner.get("schema_version").and_then(Value::as_u64) != Some(1)
    || owner.get("repo_root").and_then(Value::as_str) != run_dir.join("code").to_str()
  {
    return Err("environment owner does not match run code".to_string());
  }
  Ok(true)
}

fn real_directory(path: &Path) -> Result<()> {
  if real_directory_optional(path)? {
    Ok(())
  } else {
    Err(ExpriError::Message(format!(
      "directory is missing: {}",
      path.display()
    )))
  }
}

fn real_directory_optional(path: &Path) -> Result<bool> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
    Ok(_) => Err(ExpriError::Message(format!(
      "directory must not be a symlink or file: {}",
      path.display()
    ))),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(error.into()),
  }
}

fn read_json(path: &Path) -> Result<Value> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_file() || metadata.file_type().is_symlink() {
    return Err(ExpriError::Message(format!(
      "metadata must be a regular file: {}",
      path.display()
    )));
  }
  Ok(serde_json::from_slice(&fs::read(path)?)?)
}

/// Apparent regular-file sizes, without following package or interpreter links.
/// uv cache hardlinks mean this is not a promise of physical disk reclamation.
fn logical_bytes(path: &Path) -> std::io::Result<u64> {
  let metadata = fs::symlink_metadata(path)?;
  if metadata.file_type().is_symlink() {
    return Ok(0);
  }
  if metadata.is_file() {
    return Ok(metadata.len());
  }
  if !metadata.is_dir() {
    return Err(std::io::Error::other("environment contains a special file"));
  }
  let mut bytes = 0_u64;
  for entry in fs::read_dir(path)? {
    bytes = bytes.saturating_add(logical_bytes(&entry?.path())?);
  }
  Ok(bytes)
}

fn write_prune_audit(run_dir: &Path, entry: &PruneRun) -> Result<()> {
  let directory = run_dir.join("environment");
  let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
  let audit = json!({
    "schema_version": 1,
    "run_id": entry.run_id,
    "status": "pruned",
    "pruned_at": Utc::now().to_rfc3339(),
    "logical_bytes": entry.logical_bytes,
  });
  temporary.write_all(&serde_json::to_vec_pretty(&audit)?)?;
  temporary
    .persist(directory.join("prune-state.json"))
    .map_err(|error| error.error)?;
  Ok(())
}

#[cfg(test)]
mod tests;
