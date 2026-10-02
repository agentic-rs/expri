use crate::config::TargetConfig;
use crate::controller::protocol::{ProtocolPreference, apply_environment_with_preference};
use crate::controller::transport::Remote;
use crate::error::Result;
use crate::protocol::EnvironmentCommandRequest;

pub struct EnvironmentOptions {
  pub target: TargetConfig,
  pub control_path: String,
  pub control_persist: String,
  pub verbosity: u8,
  pub quiet: bool,
  pub request: EnvironmentCommandRequest,
}

pub fn execute(options: EnvironmentOptions) -> Result<()> {
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
  )?;
  remote.connect()?;
  apply_environment_with_preference(&remote, &options.request, preference, &node_bin)
}
