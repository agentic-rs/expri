use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::Serialize;
use serde_json::Value;

use super::artifacts::{optional_metadata, real_prefix};
use super::message;
use crate::error::Result;

pub(crate) const METADATA_PATHS: [&str; 5] = [
  "run-state.json",
  "snapshot.json",
  "environment/environment-state.json",
  "outputs/params.json",
  crate::run_artifacts::INVENTORY_PATH,
];

#[derive(Serialize)]
pub(crate) struct RunUpdate {
  pub run_id: String,
  pub metadata_revision: Option<String>,
  pub metrics_revision: Option<String>,
  pub stdout_revision: Option<String>,
  pub stderr_revision: Option<String>,
  pub missing: bool,
}

impl RunUpdate {
  pub fn missing(run_id: &str) -> Self {
    Self {
      run_id: run_id.into(),
      metadata_revision: None,
      metrics_revision: None,
      stdout_revision: None,
      stderr_revision: None,
      missing: true,
    }
  }
}

/// Revisions are inexpensive change hints, not content digests. A periodic
/// recovery refresh remains necessary for files with deliberately retained stat
/// metadata and for temporarily unavailable reads.
pub(crate) fn validate_selection(source: &str, run_ids: &[String]) -> Result<()> {
  if run_ids.len() > 8
    || run_ids
      .iter()
      .collect::<std::collections::BTreeSet<_>>()
      .len()
      != run_ids.len()
  {
    return Err(message("select zero to eight distinct run IDs"));
  }
  if source.is_empty() && !run_ids.is_empty() {
    return Err(message("run IDs require a selected source"));
  }
  for run_id in run_ids {
    if run_id.is_empty() || run_id.len() > 256 {
      return Err(message("invalid run ID for update probe"));
    }
  }
  Ok(())
}

pub(crate) fn response(
  catalog_revision: Option<String>,
  source_revision: Option<String>,
  runs: Vec<RunUpdate>,
) -> Result<Value> {
  Ok(serde_json::json!({
    "catalog_revision": catalog_revision,
    "source_revision": source_revision,
    "runs": runs,
  }))
}

pub(crate) fn metadata_revision(revisions: Vec<Option<String>>) -> Result<String> {
  // The order is fixed by METADATA_PATHS, so even missing/present transitions
  // remain detectable without exposing paths or reading metadata contents.
  Ok(serde_json::to_string(&revisions)?)
}

pub(super) fn local_run(runs_dir: &Path, run_id: &str) -> Result<RunUpdate> {
  crate::runs::validate_id(run_id)?;
  let run_dir = runs_dir.join(run_id);
  real_prefix(runs_dir, &run_dir)?;
  let Some(metadata) = optional_metadata(&run_dir)? else {
    return Ok(RunUpdate::missing(run_id));
  };
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message("run directory must be a real directory"));
  }
  let mut metadata_revision = METADATA_PATHS
    .iter()
    .map(|path| fixed_revision(&run_dir, path))
    .collect::<Result<Vec<_>>>()?;
  metadata_revision.push(fixed_revision(&run_dir, "pull-state.json")?);
  // A bounded output inventory detects checkpoint additions/removals and size
  // changes without reading their contents. This is only a refresh hint.
  use std::hash::{Hash, Hasher};
  let inventory = crate::run_artifacts::scan(&run_dir)?;
  let mut hint = std::collections::hash_map::DefaultHasher::new();
  for file in inventory.files {
    file.path.hash(&mut hint);
    file.size.hash(&mut hint);
  }
  inventory.truncated.hash(&mut hint);
  metadata_revision.push(Some(format!("outputs:{:016x}", hint.finish())));
  Ok(RunUpdate {
    run_id: run_id.into(),
    metadata_revision: Some(self::metadata_revision(metadata_revision)?),
    metrics_revision: fixed_revision(&run_dir, "outputs/metrics.jsonl")?,
    stdout_revision: fixed_revision(&run_dir, "logs/stdout.log")?,
    stderr_revision: fixed_revision(&run_dir, "logs/stderr.log")?,
    missing: false,
  })
}

fn fixed_revision(run_dir: &Path, relative: &str) -> Result<Option<String>> {
  let path = run_dir.join(relative);
  real_prefix(run_dir, path.parent().unwrap())?;
  let Some(metadata) = optional_metadata(&path)? else {
    return Ok(None);
  };
  if !metadata.is_file() || metadata.file_type().is_symlink() {
    return Err(message(format!("{relative} must be a regular file")));
  }
  real_prefix(run_dir, path.parent().unwrap())?;
  Ok(Some(stat_revision(&metadata)))
}

fn stat_revision(metadata: &fs::Metadata) -> String {
  let modified = metadata
    .modified()
    .ok()
    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
    .map(|time| time.as_nanos().to_string())
    .unwrap_or_else(|| "unknown".into());
  let revision = format!("stat:{}:{modified}", metadata.len());
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    format!(
      "{revision}:{}:{}:{}:{}",
      metadata.dev(),
      metadata.ino(),
      metadata.ctime(),
      metadata.ctime_nsec()
    )
  }
  #[cfg(not(unix))]
  revision
}
