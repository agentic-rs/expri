//! Embedded agent guidance, available without a repository or configuration.

use std::io::{self, Write};

use clap::{Args, Subcommand};

use crate::error::Result;

const OVERVIEW: &str = include_str!("skill/SKILL.md");

#[derive(Debug, Args)]
pub struct SkillCommand {
  #[command(subcommand)]
  topic: Option<SkillTopic>,
}

#[derive(Debug, Subcommand)]
enum SkillTopic {
  /// Select a repository, configure tasks, and choose local or remote execution.
  Config,
  /// Push source and configuration to a worker checkout.
  Push,
  /// Start foreground or detached tasks with isolated code and outputs.
  Run,
  /// Inspect status and logs, cancel detached runs, and fetch worker results.
  Runs,
  /// Record scalar metrics and parameters, then compare experiments.
  Metrics,
  /// Register finalized outputs for background upload.
  Artifacts,
  /// Pin, prepare, and reuse datasets and model files.
  Assets,
  /// Prepare Python environments, check package reuse, and prune old environments.
  Environment,
  /// Publish runs and fetch results through an existing self-hosted service.
  Service,
  /// Collect and publish machine system profiles with global detail/hourly history.
  System,
}

impl SkillTopic {
  fn content(&self) -> &'static str {
    match self {
      Self::Config => include_str!("skill/config.md"),
      Self::Push => include_str!("skill/push.md"),
      Self::Run => include_str!("skill/run.md"),
      Self::Runs => include_str!("skill/runs.md"),
      Self::Metrics => include_str!("skill/metrics.md"),
      Self::Artifacts => include_str!("skill/artifacts.md"),
      Self::Assets => include_str!("skill/assets.md"),
      Self::Environment => include_str!("skill/environment.md"),
      Self::Service => include_str!("skill/service.md"),
      Self::System => include_str!("skill/system.md"),
    }
  }
}

pub fn run(command: SkillCommand) -> Result<()> {
  let content = command.topic.as_ref().map_or(OVERVIEW, SkillTopic::content);
  io::stdout().lock().write_all(content.as_bytes())?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use clap::{CommandFactory, Parser};

  use super::*;
  use crate::{Cli, Command};

  #[test]
  fn overview_routes_to_every_available_topic() {
    let mut cli = Cli::command();
    let skill = cli.find_subcommand_mut("skill").unwrap();
    for topic in skill
      .get_subcommands()
      .filter(|topic| topic.get_name() != "help")
    {
      let route = format!("expri skill {}", topic.get_name());
      assert!(
        OVERVIEW.contains(&format!("`{route}`")),
        "missing route: {route}"
      );
    }
  }

  #[test]
  fn guide_command_examples_match_the_cli() {
    let mut cli = Cli::command();
    let skill = cli.find_subcommand_mut("skill").unwrap();
    let mut guides = vec![OVERVIEW];
    for topic in skill
      .get_subcommands()
      .filter(|topic| topic.get_name() != "help")
    {
      let parsed = Cli::try_parse_from(["expri", "skill", topic.get_name()]).unwrap();
      let Command::Skill(command) = parsed.command else {
        unreachable!();
      };
      guides.push(command.topic.unwrap().content());
    }
    for guide in guides {
      let mut shell_block = false;
      for line in guide.lines() {
        if line.starts_with("```") {
          shell_block = !shell_block && line == "```sh";
          continue;
        }
        if !shell_block {
          continue;
        }
        let Some(arguments) = line
          .strip_prefix("expri ")
          .or_else(|| line.strip_prefix("\"$EXPRI_BIN\" "))
        else {
          continue;
        };
        // Examples use one command per line and no arguments containing spaces.
        let args = std::iter::once("expri").chain(arguments.split_whitespace());
        if let Err(error) = Cli::try_parse_from(args) {
          assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelp,
            "{line}: {error}"
          );
        }
      }
    }
  }
}
