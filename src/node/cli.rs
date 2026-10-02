use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::error::{ExpriError, Result};

pub const UV_ENVIRONMENT_CAPABILITY: &str = "uv-environment-v1";

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
  Ping,
  Capabilities(CapabilitiesCommand),
  PullPrepare,
  Setup(SetupCommand),
  Run(SetupCommand),
  SyncApply(SyncApplyCommand),
}

#[derive(Debug, Args)]
pub struct CapabilitiesCommand {
  #[arg(long)]
  pub has: Option<String>,
}

#[derive(Debug, Args)]
pub struct SetupCommand {
  #[arg(long)]
  pub request: PathBuf,
}

#[derive(Debug, Args)]
pub struct SyncApplyCommand {
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
    NodeCommand::PullPrepare => crate::node::sync::prepare_pull(),
    NodeCommand::Setup(command) => crate::node::setup::apply_request_file(&command.request),
    NodeCommand::Run(command) => crate::node::run::apply_request_file(&command.request),
    NodeCommand::SyncApply(command) => crate::node::sync::apply_request_file(&command.request),
  }
}

fn capabilities(command: CapabilitiesCommand) -> Result<()> {
  if let Some(requested) = command.has {
    if requested != UV_ENVIRONMENT_CAPABILITY {
      return Err(ExpriError::Message(format!(
        "unsupported node capability: {requested}"
      )));
    }
  } else {
    println!(
      "{}",
      serde_json::json!({"capabilities": [UV_ENVIRONMENT_CAPABILITY]})
    );
  }
  Ok(())
}
