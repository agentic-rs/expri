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

  #[command(subcommand)]
  command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
  Sync(SyncCommand),
  Download(DownloadCommand),
  Setup(SetupCommand),
  Task(TaskCommand),
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

  #[arg(short, long, action = clap::ArgAction::Count)]
  verbose: u8,

  #[arg(short, long)]
  quiet: bool,

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

  #[arg(short, long, action = clap::ArgAction::Count)]
  verbose: u8,

  #[arg(short, long)]
  quiet: bool,
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

  #[arg(short, long, action = clap::ArgAction::Count)]
  verbose: u8,

  #[arg(short, long)]
  quiet: bool,

  #[arg(value_name = "NAME", last = true)]
  names: Vec<String>,
}

#[derive(Debug, Args)]
struct TaskCommand {
  name: String,

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

  #[arg(short, long, action = clap::ArgAction::Count)]
  verbose: u8,

  #[arg(short, long)]
  quiet: bool,

  #[arg(value_name = "ARG", last = true)]
  args: Vec<String>,
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
    Command::Sync(command) => run_sync(command, cli.target.as_deref()),
    Command::Download(command) => run_download(command, cli.target.as_deref()),
    Command::Setup(command) => run_setup(command, cli.target.as_deref()),
    Command::Task(command) => run_task(command, cli.target.as_deref()),
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

fn run_sync(command: SyncCommand, target: Option<&str>) -> Result<()> {
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
    verbosity: command.verbose,
    quiet: command.quiet,
  })
}

fn run_download(command: DownloadCommand, target: Option<&str>) -> Result<()> {
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
    verbosity: command.verbose,
    quiet: command.quiet,
  })
}

fn run_setup(command: SetupCommand, target: Option<&str>) -> Result<()> {
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
    verbosity: command.verbose,
    quiet: command.quiet,
  })
}

fn run_task(command: TaskCommand, target: Option<&str>) -> Result<()> {
  let context = CommandContext::load(command.config, command.repo)?;
  let task = context.config.task(&command.name)?;
  if target.is_some() {
    let context = context.into_target(target, command.control_path)?;
    return run_remote_task(RemoteTaskOptions {
      repo_root: context.repo_root,
      project_name: context.project_name,
      target_name: context.target_name,
      target: context.target,
      control_path: context.control_path,
      control_persist: command.control_persist,
      name: command.name,
      task,
      args: command.args,
      dry_run: command.dry_run,
      verbosity: command.verbose,
      quiet: command.quiet,
    });
  }

  run_local_task(LocalTaskOptions {
    repo_root: context.repo_root,
    project_name: context.project_name,
    name: command.name,
    task,
    args: command.args,
    dry_run: command.dry_run,
    verbosity: command.verbose,
    quiet: command.quiet,
  })
}
