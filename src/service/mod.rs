mod browser;
mod browser_assets;
mod browser_auth;
mod client;
mod dashboard_data;
pub(crate) mod publishing;
mod server;
mod storage;
mod store;
pub(crate) mod types;

pub(crate) use types::validate_component;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::error::{ExpriError, Result};

#[derive(Debug, Args)]
pub struct ServiceCommand {
  #[command(subcommand)]
  command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
enum ServiceSubcommand {
  /// Run the optional self-hosted catalog and artifact service.
  Serve {
    #[arg(long)]
    config: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    #[arg(long, default_value = ".expri/service")]
    data_dir: PathBuf,
    /// Explicitly provision a private bucket before serving requests.
    #[arg(long)]
    create_bucket: bool,
  },
  /// Upload saved run files; watch forwards live metrics without blocking training.
  Push(PushOptions),
  /// Resume the saved automatic publisher on this worker.
  Resume {
    #[arg(long)]
    run_dir: PathBuf,
  },
  #[command(hide = true)]
  PublishWorker {
    #[arg(long)]
    run_dir: PathBuf,
  },
  /// Download selected run files into the existing offline review cache.
  Pull(PullOptions),
  /// List runs recorded by the service.
  List {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    project_id: String,
    #[arg(long)]
    origin: String,
  },
  /// Publish or retrieve an immutable private input file.
  Input {
    #[command(subcommand)]
    command: InputCommand,
  },
}

#[derive(Debug, Subcommand)]
enum InputCommand {
  Put(InputPutOptions),
  Get(InputGetOptions),
}

#[derive(Debug, Args)]
pub struct PushOptions {
  #[arg(long)]
  pub config: PathBuf,
  #[arg(long)]
  pub run_dir: PathBuf,
  #[arg(long)]
  pub project_id: String,
  #[arg(long)]
  pub origin: String,
  #[arg(long = "artifact")]
  pub artifacts: Vec<String>,
  #[arg(long)]
  pub watch: bool,
  #[arg(long, default_value = ".expri/service-sync")]
  pub queue_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct PullOptions {
  #[arg(long)]
  pub config: PathBuf,
  #[arg(long)]
  pub project_id: String,
  #[arg(long)]
  pub origin: String,
  #[arg(long)]
  pub run_id: String,
  #[arg(long, default_value = ".")]
  pub repo: PathBuf,
  #[arg(long, default_value = "results")]
  pub results_dir: PathBuf,
  /// Cache source name; defaults to service-<project_id>-<origin>.
  #[arg(long)]
  pub source: Option<String>,
  #[arg(long = "artifact")]
  pub artifacts: Vec<String>,
}

#[derive(Debug, Args)]
pub struct InputPutOptions {
  #[arg(long)]
  pub config: PathBuf,
  #[arg(long)]
  pub project_id: String,
  #[arg(long)]
  pub input_id: String,
  #[arg(long)]
  pub file: PathBuf,
  #[arg(long, default_value = ".expri/service-sync")]
  pub queue_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct InputGetOptions {
  #[arg(long)]
  pub config: PathBuf,
  #[arg(long)]
  pub project_id: String,
  #[arg(long)]
  pub input_id: String,
  #[arg(long)]
  pub destination: PathBuf,
}

pub fn run(command: ServiceCommand, target: Option<&str>) -> Result<()> {
  if target.is_some() {
    return Err(ExpriError::Message(
      "service commands run on the current machine; omit --target".into(),
    ));
  }
  let report = match command.command {
    ServiceSubcommand::Serve {
      config,
      listen,
      data_dir,
      create_bucket,
    } => {
      let config = toml::from_str(&std::fs::read_to_string(config)?)?;
      return server::serve(config, listen, data_dir, create_bucket);
    }
    ServiceSubcommand::Push(options) => client::push(options)?,
    ServiceSubcommand::Resume { run_dir } => publishing::resume(&run_dir)?,
    ServiceSubcommand::PublishWorker { run_dir } => return publishing::worker(&run_dir),
    ServiceSubcommand::Pull(options) => client::pull(options)?,
    ServiceSubcommand::List {
      config,
      project_id,
      origin,
    } => client::list(config, project_id, origin)?,
    ServiceSubcommand::Input { command } => match command {
      InputCommand::Put(options) => client::input_put(options)?,
      InputCommand::Get(options) => client::input_get(options)?,
    },
  };
  println!("{}", serde_json::to_string(&report)?);
  Ok(())
}
