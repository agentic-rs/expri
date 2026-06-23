use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::error::Result;

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
  Ping,
  SyncApply(SyncApplyCommand),
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
    NodeCommand::SyncApply(command) => crate::node::sync::apply_request_file(&command.request),
  }
}
