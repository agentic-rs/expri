mod archive;
mod config;
mod context;
mod controller;
mod error;
mod filter;
mod git;
mod node;
mod protocol;
mod shell;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::context::CommandContext;
use crate::controller::download::{DownloadOptions, download_target};
use crate::controller::setup::{SetupOptions, setup_target};
use crate::controller::sync::{SyncOptions, sync_target};
use crate::controller::task::{
  LocalTaskOptions, RemoteTaskOptions, run_local_task, run_remote_task,
};
use crate::error::{ExpriError, Result};
use crate::node::cli::NodeCommand;

#[derive(Debug, Parser)]
#[command(version, about = "Repo-local remote workflow tools")]
struct Cli {
  #[arg(short = 'T', long)]
  target: Option<String>,

  #[arg(short, long, global = true, action = clap::ArgAction::Count)]
  verbose: u8,

  #[arg(short, long, global = true)]
  quiet: bool,

  #[command(subcommand)]
  command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
  Sync(SyncCommand),
  Download(DownloadCommand),
  Setup(SetupCommand),
  Run(RunCommand),
  Node {
    #[command(subcommand)]
    command: NodeCommand,
  },
}

#[derive(Debug, Args)]
struct SyncCommand {
  #[arg(long)]
  config: Option<PathBuf>,

  #[arg(long)]
  repo: Option<PathBuf>,

  #[arg(long)]
  control_path: Option<String>,

  #[arg(long, default_value = "30m")]
  control_persist: String,

  #[arg(long)]
  dry_run: bool,

  #[arg(long)]
  force: bool,

  #[arg(long)]
  pull: bool,

  #[arg(value_name = "PATH", last = true)]
  paths: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct SetupCommand {
  #[arg(long)]
  config: Option<PathBuf>,

  #[arg(long)]
  repo: Option<PathBuf>,

  #[arg(long)]
  control_path: Option<String>,

  #[arg(long, default_value = "30m")]
  control_persist: String,

  #[arg(long)]
  dry_run: bool,

  #[arg(long)]
  force: bool,
}

#[derive(Debug, Args)]
struct DownloadCommand {
  #[arg(long)]
  config: Option<PathBuf>,

  #[arg(long)]
  repo: Option<PathBuf>,

  #[arg(long)]
  control_path: Option<String>,

  #[arg(long, default_value = "30m")]
  control_persist: String,

  #[arg(long)]
  dry_run: bool,

  #[arg(value_name = "NAME", last = true)]
  names: Vec<String>,
}

#[derive(Debug, Args)]
struct RunCommand {
  #[arg(long)]
  config: Option<PathBuf>,

  #[arg(long)]
  repo: Option<PathBuf>,

  #[arg(long)]
  control_path: Option<String>,

  #[arg(long, default_value = "30m")]
  control_persist: String,

  #[arg(long)]
  dry_run: bool,

  #[arg(long)]
  no_sync: bool,

  #[arg(
    value_name = "TASK",
    required = true,
    num_args = 1..,
    trailing_var_arg = true,
    allow_hyphen_values = true
  )]
  task: Vec<String>,
}

fn main() {
  if let Err(error) = run() {
    eprintln!("error: {error}");
    std::process::exit(error.exit_code());
  }
}

fn run() -> Result<()> {
  let cli = Cli::parse();
  match cli.command {
    Command::Sync(command) => run_sync(command, cli.target.as_deref(), cli.verbose, cli.quiet),
    Command::Download(command) => {
      run_download(command, cli.target.as_deref(), cli.verbose, cli.quiet)
    }
    Command::Setup(command) => run_setup(command, cli.target.as_deref(), cli.verbose, cli.quiet),
    Command::Run(command) => run_task(command, cli.target.as_deref(), cli.verbose, cli.quiet),
    Command::Node { command } => {
      if cli.target.is_some() {
        return Err(ExpriError::Message(
          "--target is only valid for controller commands".to_string(),
        ));
      }
      node::cli::run(command)
    }
  }
}

fn run_sync(command: SyncCommand, target: Option<&str>, verbosity: u8, quiet: bool) -> Result<()> {
  let context = CommandContext::load(command.config, command.repo)?
    .into_target(target, command.control_path)?;
  let sync = context.config.sync_rules()?;

  sync_target(SyncOptions {
    repo_root: context.repo_root,
    project_name: context.project_name,
    target_name: context.target_name,
    target: context.target,
    sync,
    control_path: context.control_path,
    control_persist: command.control_persist,
    dry_run: command.dry_run,
    force: command.force,
    pull: command.pull,
    paths: command.paths,
    verbosity,
    quiet,
  })
}

fn run_download(
  command: DownloadCommand,
  target: Option<&str>,
  verbosity: u8,
  quiet: bool,
) -> Result<()> {
  let context = CommandContext::load(command.config, command.repo)?
    .into_target(target, command.control_path)?;
  let results_dir = context.config.download_results_dir();
  let mappings = context.config.download_mappings();

  download_target(DownloadOptions {
    repo_root: context.repo_root,
    project_name: context.project_name,
    target_name: context.target_name,
    target: context.target,
    results_dir,
    mappings,
    names: command.names,
    control_path: context.control_path,
    control_persist: command.control_persist,
    dry_run: command.dry_run,
    verbosity,
    quiet,
  })
}

fn run_setup(
  command: SetupCommand,
  target: Option<&str>,
  verbosity: u8,
  quiet: bool,
) -> Result<()> {
  let context = CommandContext::load(command.config, command.repo)?
    .into_target(target, command.control_path)?;
  let steps = context.config.setup_steps();

  setup_target(SetupOptions {
    repo_root: context.repo_root,
    project_name: context.project_name,
    target_name: context.target_name,
    target: context.target,
    steps,
    control_path: context.control_path,
    control_persist: command.control_persist,
    dry_run: command.dry_run,
    force: command.force,
    verbosity,
    quiet,
  })
}

fn run_task(command: RunCommand, target: Option<&str>, verbosity: u8, quiet: bool) -> Result<()> {
  let context = CommandContext::load(command.config, command.repo)?;
  let mut task_parts = command.task.into_iter();
  let name = task_parts
    .next()
    .expect("clap requires at least one task argument");
  let args = task_parts.collect::<Vec<_>>();
  let task = context.config.task(&name)?;
  if target.is_some() {
    let context = context.into_target(target, command.control_path)?;
    if !command.no_sync {
      let sync = context.config.sync_rules()?;
      sync_target(SyncOptions {
        repo_root: context.repo_root.clone(),
        project_name: context.project_name.clone(),
        target_name: context.target_name.clone(),
        target: context.target.clone(),
        sync,
        control_path: context.control_path.clone(),
        control_persist: command.control_persist.clone(),
        dry_run: command.dry_run,
        force: false,
        pull: false,
        paths: Vec::new(),
        verbosity,
        quiet,
      })?;
    }
    return run_remote_task(RemoteTaskOptions {
      repo_root: context.repo_root,
      project_name: context.project_name,
      target_name: context.target_name,
      target: context.target,
      control_path: context.control_path,
      control_persist: command.control_persist,
      name,
      task,
      args,
      dry_run: command.dry_run,
      verbosity,
      quiet,
    });
  }

  run_local_task(LocalTaskOptions {
    repo_root: context.repo_root,
    project_name: context.project_name,
    name,
    task,
    args,
    dry_run: command.dry_run,
    verbosity,
    quiet,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn run_options_before_name_belong_to_expri() {
    let cli = Cli::try_parse_from([
      "expri",
      "run",
      "--dry-run",
      "--no-sync",
      "train",
      "--model",
      "tiny",
    ])
    .unwrap();

    let Command::Run(command) = cli.command else {
      panic!("expected run command");
    };

    assert_eq!(command.task[0], "train");
    assert!(command.dry_run);
    assert!(command.no_sync);
    assert_eq!(command.task[1..], ["--model", "tiny"]);
  }

  #[test]
  fn run_options_after_name_are_task_args() {
    let cli = Cli::try_parse_from([
      "expri",
      "run",
      "train",
      "--dry-run",
      "--config",
      "task-config.toml",
    ])
    .unwrap();

    let Command::Run(command) = cli.command else {
      panic!("expected run command");
    };

    assert_eq!(command.task[0], "train");
    assert!(!command.dry_run);
    assert!(command.config.is_none());
    assert_eq!(
      command.task[1..],
      ["--dry-run", "--config", "task-config.toml"]
    );
  }
}
