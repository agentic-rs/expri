use std::path::PathBuf;

use crate::config::TargetConfig;
use crate::controller::protocol::{ProtocolPreference, query_runs_with_preference};
use crate::controller::transport::Remote;
use crate::error::Result;
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
  )?;
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
