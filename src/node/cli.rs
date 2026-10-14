use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::error::{ExpriError, Result};

pub const UV_ENVIRONMENT_CAPABILITY: &str = "uv-environment-v1";
pub const ENVIRONMENT_MAINTENANCE_CAPABILITY: &str = "env-maintenance-v1";
pub const RUN_RECORDS_CAPABILITY: &str = "run-records-v1";
pub const RUN_METRICS_CAPABILITY: &str = "run-metrics-v1";
pub const DURABLE_RUNS_CAPABILITY: &str = "durable-runs-v1";
pub const RUN_PUBLISHING_CAPABILITY: &str = "run-publishing-v1";
pub const SMOKE_RUNS_CAPABILITY: &str = "smoke-runs-v1";
#[cfg(unix)]
pub const WORKER_SERVICE_CAPABILITY: &str = "worker-service-v1";
pub const ASSETS_CAPABILITY: &str = "assets-v2";

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
  Ping,
  Capabilities(CapabilitiesCommand),
  Setup(SetupCommand),
  Run(SetupCommand),
  #[command(hide = true)]
  RunWorker(RunWorkerCommand),
  Env(RequestCommand),
  Runs(RequestCommand),
  Jobs(RequestCommand),
  PushApply(PushApplyCommand),
}

#[derive(Debug, Args)]
pub struct RunWorkerCommand {
  #[arg(long)]
  pub run_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct CapabilitiesCommand {
  #[arg(long)]
  pub has: Option<String>,
}

#[derive(Debug, Args)]
pub struct RequestCommand {
  #[arg(
    long,
    required_unless_present = "request_stdin",
    conflicts_with = "request_stdin"
  )]
  pub request: Option<PathBuf>,
  #[arg(long)]
  pub request_stdin: bool,
}

#[derive(Debug, Args)]
pub struct SetupCommand {
  #[arg(long)]
  pub request: PathBuf,
}

#[derive(Debug, Args)]
pub struct PushApplyCommand {
  #[arg(long)]
  pub request: PathBuf,
}

pub fn run(command: NodeCommand) -> Result<()> {
  match command {
    NodeCommand::Ping => {
      println!("ok");
      Ok(())
    }
    NodeCommand::Capabilities(command) => capabilities(command),
    NodeCommand::Setup(command) => crate::node::setup::apply_request_file(&command.request),
    NodeCommand::Run(command) => {
      let request: crate::protocol::RunRequest =
        serde_json::from_slice(&std::fs::read(command.request)?)?;
      let root = std::env::current_dir()?;
      if let Some(receipt) = crate::node::run::execute_request_at_with_initializer(
        &request,
        &root,
        initialize_worker_service,
      )? {
        println!("{receipt}");
      }
      Ok(())
    }
    NodeCommand::RunWorker(command) => crate::node::run::worker(&command.run_dir),
    NodeCommand::Env(command) => {
      if let Some(path) = command.request {
        crate::node::environment::apply_request_file(&path)
      } else {
        crate::node::environment::apply_request_stdin()
      }
    }
    NodeCommand::Runs(command) => {
      if let Some(path) = command.request {
        crate::node::runs::apply_request_file(&path)
      } else {
        crate::node::runs::apply_request_stdin()
      }
    }
    NodeCommand::Jobs(command) => {
      if let Some(path) = command.request {
        crate::node::jobs::apply_request_file(&path)
      } else {
        crate::node::jobs::apply_request_stdin()
      }
    }
    NodeCommand::PushApply(command) => crate::node::sync::apply_request_file(&command.request),
  }
}

pub(crate) fn initialize_worker_service() -> Result<()> {
  let paths = expri_core::storage::StoragePaths::resolve(None)?;
  expri_client::worker_service::ensure_collection_ready(&paths)?;
  Ok(())
}

fn capabilities(command: CapabilitiesCommand) -> Result<()> {
  let available = [
    UV_ENVIRONMENT_CAPABILITY,
    ENVIRONMENT_MAINTENANCE_CAPABILITY,
    RUN_RECORDS_CAPABILITY,
    DURABLE_RUNS_CAPABILITY,
    RUN_METRICS_CAPABILITY,
    RUN_PUBLISHING_CAPABILITY,
    SMOKE_RUNS_CAPABILITY,
    #[cfg(unix)]
    WORKER_SERVICE_CAPABILITY,
    ASSETS_CAPABILITY,
    expri_core::run_system::RUN_SYSTEM_CAPABILITY,
  ];
  if let Some(requested) = command.has {
    if !available.contains(&requested.as_str()) {
      return Err(ExpriError::Message(format!(
        "unsupported node capability: {requested}"
      )));
    }
  } else {
    println!("{}", serde_json::json!({"capabilities": available}));
  }
  Ok(())
}
