mod archive;
mod config;
mod controller;
mod error;
mod filter;
mod git;
mod node;
mod protocol;
mod shell;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::Config;
use crate::controller::setup::{SetupOptions, setup_target};
use crate::controller::sync::{SyncOptions, sync_target};
use crate::error::{ExpriError, Result};
use crate::node::cli::NodeCommand;

#[derive(Debug, Parser)]
#[command(version, about = "Repo-local remote workflow tools")]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
  Sync(SyncCommand),
  Setup(SetupCommand),
  Node {
    #[command(subcommand)]
    command: NodeCommand,
  },
}

#[derive(Debug, Args)]
struct SyncCommand {
  target: Option<String>,

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
struct SetupCommand {
  target: Option<String>,

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

fn main() {
  if let Err(error) = run() {
    eprintln!("error: {error}");
    std::process::exit(error.exit_code());
  }
}

fn run() -> Result<()> {
  let cli = Cli::parse();
  match cli.command {
    Command::Sync(command) => run_sync(command),
    Command::Setup(command) => run_setup(command),
    Command::Node { command } => node::cli::run(command),
  }
}

fn run_sync(command: SyncCommand) -> Result<()> {
  let config_path = command
    .config
    .unwrap_or_else(|| PathBuf::from("expri.toml"));
  let config_path = if config_path.is_absolute() {
    config_path
  } else {
    std::env::current_dir()?.join(config_path)
  };
  let config = Config::load(&config_path)?;
  let project_name = config.project_name().map(str::to_string);
  let repo_root = match command.repo {
    Some(path) if path.is_absolute() => path,
    Some(path) => std::env::current_dir()?.join(path),
    None => config_path
      .parent()
      .ok_or_else(|| ExpriError::Message("config path has no parent".to_string()))?
      .to_path_buf(),
  };
  let target_name = config.resolve_target_name(command.target.as_deref())?;
  let target = config.target(&target_name)?;
  let sync = config.sync_rules()?;
  let control_path = command
    .control_path
    .or_else(|| config.ssh.as_ref().and_then(|ssh| ssh.control_path.clone()))
    .unwrap_or_else(default_control_path);

  sync_target(SyncOptions {
    repo_root,
    project_name,
    target_name,
    target,
    sync,
    control_path,
    control_persist: command.control_persist,
    dry_run: command.dry_run,
    force: command.force,
    verbosity: command.verbose,
    quiet: command.quiet,
  })
}

fn run_setup(command: SetupCommand) -> Result<()> {
  let config_path = command
    .config
    .unwrap_or_else(|| PathBuf::from("expri.toml"));
  let config_path = if config_path.is_absolute() {
    config_path
  } else {
    std::env::current_dir()?.join(config_path)
  };
  let config = Config::load(&config_path)?;
  let project_name = config.project_name().map(str::to_string);
  let repo_root = match command.repo {
    Some(path) if path.is_absolute() => path,
    Some(path) => std::env::current_dir()?.join(path),
    None => config_path
      .parent()
      .ok_or_else(|| ExpriError::Message("config path has no parent".to_string()))?
      .to_path_buf(),
  };
  let target_name = config.resolve_target_name(command.target.as_deref())?;
  let target = config.target(&target_name)?;
  let control_path = command
    .control_path
    .or_else(|| config.ssh.as_ref().and_then(|ssh| ssh.control_path.clone()))
    .unwrap_or_else(default_control_path);

  setup_target(SetupOptions {
    repo_root,
    project_name,
    target_name,
    target,
    steps: config.setup_steps(),
    control_path,
    control_persist: command.control_persist,
    dry_run: command.dry_run,
    force: command.force,
    verbosity: command.verbose,
    quiet: command.quiet,
  })
}

fn default_control_path() -> String {
  let value = "~/.ssh/cm-%r@%h:%p";
  match std::env::var("HOME") {
    Ok(home) => value.replacen('~', &home, 1),
    Err(_) => value.to_string(),
  }
}
