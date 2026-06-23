use clap::Subcommand;

use crate::error::Result;

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
  Ping,
}

pub fn run(command: NodeCommand) -> Result<()> {
  match command {
    NodeCommand::Ping => {
      println!("ok");
      Ok(())
    }
  }
}
