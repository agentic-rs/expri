use std::path::PathBuf;

use crate::config::TargetConfig;
use crate::controller::protocol::{ProtocolPreference, query_runs_with_preference};
use crate::controller::transport::Remote;
use crate::error::Result;
use crate::metrics::MetricFiles;
use crate::protocol::RunQueryRequest;

pub struct RunOptions {
  pub repo_root: PathBuf,
  pub target_name: String,
  pub target: TargetConfig,
  pub results_dir: String,
  pub control_path: String,
  pub control_persist: String,
  pub verbosity: u8,
  pub quiet: bool,
  pub dry_run: bool,
  pub request: RunQueryRequest,
}

pub struct MetricsFetchOptions {
  pub repo_root: PathBuf,
  pub target_name: String,
  pub target: TargetConfig,
  pub results_dir: String,
  pub control_path: String,
  pub control_persist: String,
  pub verbosity: u8,
  pub quiet: bool,
  pub run_ids: Vec<String>,
}

pub struct FetchedMetrics {
  pub runs_dir: PathBuf,
  pub selections: Vec<MetricSelection>,
}

pub struct MetricSelection {
  pub run_id: String,
  pub files: MetricFiles,
  pub warnings: Vec<serde_json::Value>,
}

pub fn execute(options: RunOptions) -> Result<serde_json::Value> {
  let preference = ProtocolPreference::parse(options.target.protocol.as_deref())?;
  let node_bin = options
    .target
    .node_bin
    .clone()
    .unwrap_or_else(|| "expri".to_string());
  // Even a pull preview queries the remote catalog, so it can show the actual file selection.
  let mut remote = Remote::new(
    options.target,
    options.control_path,
    options.control_persist,
    false,
    options.verbosity,
    options.quiet,
  )?
  .with_diagnostic_stdout(true);
  remote.connect()?;
  let report = query_runs_with_preference(&remote, &options.request, preference, &node_bin)?;
  if matches!(options.request, RunQueryRequest::Files { .. }) {
    remote.dry_run = options.dry_run;
    super::run_pull::pull(
      &remote,
      &options.repo_root,
      &options.results_dir,
      &options.target_name,
      &report,
    )
  } else {
    Ok(report)
  }
}

/// Fetch fixed metric/parameter paths through the existing staged run pull.
/// Presence reflects this remote selection, rather than older retained cache files.
pub fn fetch_metrics(options: MetricsFetchOptions) -> Result<FetchedMetrics> {
  let runs_dir = super::run_pull::cached_runs_dir(
    &options.repo_root,
    &options.results_dir,
    &options.target_name,
  )?;
  let preference = ProtocolPreference::parse(options.target.protocol.as_deref())?;
  let node_bin = options
    .target
    .node_bin
    .clone()
    .unwrap_or_else(|| "expri".to_string());
  let remote = Remote::new(
    options.target,
    options.control_path,
    options.control_persist,
    false,
    options.verbosity,
    options.quiet,
  )?
  .with_diagnostic_stdout(true);
  remote.connect()?;
  let mut selections = Vec::with_capacity(options.run_ids.len());
  for run_id in options.run_ids {
    let request = RunQueryRequest::Files {
      run_id: run_id.clone(),
      artifacts: Vec::new(),
      metrics: true,
    };
    let selection = query_runs_with_preference(&remote, &request, preference, &node_bin)?;
    let report = super::run_pull::pull(
      &remote,
      &options.repo_root,
      &options.results_dir,
      &options.target_name,
      &selection,
    )?;
    let selected = |path: &str| {
      report["files"]
        .as_array()
        .is_some_and(|files| files.iter().any(|file| file.as_str() == Some(path)))
    };
    selections.push(MetricSelection {
      run_id,
      files: MetricFiles {
        metrics: selected("outputs/metrics.jsonl"),
        params: selected("outputs/params.json"),
      },
      warnings: report["warnings"].as_array().cloned().unwrap_or_default(),
    });
  }
  Ok(FetchedMetrics {
    runs_dir,
    selections,
  })
}
