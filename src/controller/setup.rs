use std::fs;
use std::path::PathBuf;

use crate::config::TargetConfig;
use crate::controller::protocol::{ProtocolPreference, apply_setup_with_preference};
use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result};
use crate::protocol::{SetupRequest, SetupStep};

pub struct SetupOptions {
  pub repo_root: PathBuf,
  pub project_name: Option<String>,
  pub target_name: String,
  pub target: TargetConfig,
  pub steps: Vec<SetupStep>,
  pub control_path: String,
  pub control_persist: String,
  pub dry_run: bool,
  pub force: bool,
  pub verbosity: u8,
  pub quiet: bool,
}

pub fn setup_target(options: SetupOptions) -> Result<()> {
  if options.steps.is_empty() {
    return Err(ExpriError::Message(
      "no setup steps configured in expri.toml".to_string(),
    ));
  }
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
    options.dry_run,
    options.verbosity,
    options.quiet,
  );
  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    eprintln!("setup target: {}", options.target_name);
    eprintln!("repo root: {}", options.repo_root.display());
    eprintln!("setup step count: {}", options.steps.len());
  }

  let _opened_master = remote.open_master()?;
  let inbox = format!("{}/inbox", remote.meta_dir());
  remote.ssh(&format!("mkdir -p {inbox}"))?;
  let request = SetupRequest {
    state_dir: ".expri".to_string(),
    force: options.force,
    steps: options.steps,
  };
  let request_dir = tempfile::Builder::new().prefix("expri-setup-").tempdir()?;
  let request_path = request_dir.path().join("setup-request.json");
  fs::write(&request_path, serde_json::to_string_pretty(&request)?)?;
  remote.upload_file(&request_path, &format!("{inbox}/setup-request.json"))?;
  apply_setup_with_preference(
    &remote,
    ".expri/inbox/setup-request.json",
    preference,
    &node_bin,
  )
}
