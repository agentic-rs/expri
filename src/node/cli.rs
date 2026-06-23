use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::error::Result;

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
  Ping,
  Setup(SetupCommand),
  SyncApply(SyncApplyCommand),
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
    NodeCommand::Setup(command) => crate::node::setup::apply_request_file(&command.request),
    NodeCommand::SyncApply(command) => crate::node::sync::apply_request_file(&command.request),
  }
}
