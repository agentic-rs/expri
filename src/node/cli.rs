use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::error::{ExpriError, Result};

pub const UV_ENVIRONMENT_CAPABILITY: &str = "uv-environment-v1";
pub const ENVIRONMENT_MAINTENANCE_CAPABILITY: &str = "env-maintenance-v1";
pub const RUN_RECORDS_CAPABILITY: &str = "run-records-v1";
pub const RUN_METRICS_CAPABILITY: &str = "run-metrics-v1";
pub const DURABLE_RUNS_CAPABILITY: &str = "durable-runs-v1";
pub const SUPERVISOR_RESTART_CAPABILITY: &str = "run-supervisor-restart-v1";
pub const TASK_CONTINUATION_CAPABILITY: &str = "run-task-continuation-v1";
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
  #[command(hide = true)]
  ResumeRunWorker(ResumeRunWorkerCommand),
  #[command(hide = true)]
  ContinueWorker(ContinueWorkerCommand),
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
pub struct ResumeRunWorkerCommand {
  #[arg(long, value_parser = clap::value_parser!(i32).range(3..))]
  pub handoff_fd: i32,
}

#[derive(Debug, Args)]
pub struct ContinueWorkerCommand {
  #[arg(long)]
  pub run_dir: PathBuf,
  #[arg(long)]
  pub request_id: String,
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
    NodeCommand::ResumeRunWorker(command) => resume_run_worker(command.handoff_fd),
    NodeCommand::ContinueWorker(command) => {
      expri_worker::continuation::worker(&command.run_dir, &command.request_id)
    }
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

#[cfg(target_os = "linux")]
fn resume_run_worker(handoff_fd: i32) -> Result<()> {
  expri_worker::supervisor::resume_from_fd(handoff_fd)
}

#[cfg(not(target_os = "linux"))]
fn resume_run_worker(_: i32) -> Result<()> {
  Err(ExpriError::Message(
    "live supervisor restart requires a native Linux training machine".into(),
  ))
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
    #[cfg(target_os = "linux")]
    SUPERVISOR_RESTART_CAPABILITY,
    #[cfg(target_os = "linux")]
    TASK_CONTINUATION_CAPABILITY,
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

#[cfg(test)]
mod tests {
  use clap::Parser;

  use super::*;

  #[test]
  fn resume_worker_requires_an_explicit_handoff_descriptor_above_standard_streams() {
    let cli =
      crate::Cli::try_parse_from(["expri", "node", "resume-run-worker", "--handoff-fd", "7"])
        .unwrap();
    let crate::Command::Node {
      command: NodeCommand::ResumeRunWorker(command),
    } = cli.command
    else {
      panic!("expected resume worker command");
    };
    assert_eq!(command.handoff_fd, 7);
    for arguments in [
      vec!["expri", "node", "resume-run-worker"],
      vec!["expri", "node", "resume-run-worker", "--handoff-fd=-1"],
      vec!["expri", "node", "resume-run-worker", "--handoff-fd=0"],
      vec!["expri", "node", "resume-run-worker", "--handoff-fd=1"],
      vec!["expri", "node", "resume-run-worker", "--handoff-fd=2"],
    ] {
      assert!(crate::Cli::try_parse_from(arguments).is_err());
    }
  }

  #[test]
  fn live_restart_capability_is_advertised_only_on_linux() {
    let supported = capabilities(CapabilitiesCommand {
      has: Some(SUPERVISOR_RESTART_CAPABILITY.into()),
    });
    assert_eq!(supported.is_ok(), cfg!(target_os = "linux"));
  }
}
