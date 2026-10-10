mod browser;
mod browser_assets;
mod browser_auth;
mod client;
mod dashboard_data;
mod notifications;
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
  /// Inspect project storage or delete a project with an owner token.
  Project {
    #[command(subcommand)]
    command: ProjectCommand,
  },
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
  /// Ask the server to archive received tracking files without downloading them first.
  Archive {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    project_id: String,
    #[arg(long)]
    origin: String,
    #[arg(long)]
    run_id: String,
    /// Export an acknowledged prefix of an unfinished run; requires an owner token.
    #[arg(long)]
    partial: bool,
  },
  /// Reference a completed object as an output or reusable input without copying bytes.
  Reference(ReferenceOptions),
  /// Upload one explicit run output or metadata file without creating a run archive.
  FilePut(FilePutOptions),
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

#[derive(Debug, Subcommand)]
enum ProjectCommand {
  /// Show recorded project storage usage, including retained objects.
  Stats(ProjectOptions),
  /// Preview the project records and objects affected by deletion.
  DeletePreview(ProjectOptions),
  /// Delete a project using a current preview revision and its exact name.
  Delete {
    #[command(flatten)]
    options: ProjectOptions,
    #[arg(long)]
    revision: String,
    #[arg(long)]
    confirm_project: String,
  },
  /// Show the durable object cleanup status after project deletion.
  Deletion(ProjectOptions),
}

#[derive(Debug, Args)]
struct ProjectOptions {
  #[arg(long)]
  config: PathBuf,
  #[arg(long)]
  project_id: String,
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
pub struct FilePutOptions {
  #[arg(long)]
  pub config: PathBuf,
  /// Destination FileTarget as JSON.
  #[arg(long)]
  pub target: String,
  #[arg(long)]
  pub file: PathBuf,
  #[arg(long, default_value = ".expri/service-sync")]
  pub queue_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct ReferenceOptions {
  #[arg(long)]
  pub config: PathBuf,
  /// Source FileTarget as JSON (run output or project input).
  #[arg(long)]
  pub source: String,
  /// Destination FileTarget as JSON. Requires an owner token.
  #[arg(long)]
  pub target: String,
  #[arg(long)]
  pub size: u64,
  #[arg(long)]
  pub sha256: String,
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
    ServiceSubcommand::Project { command } => match command {
      ProjectCommand::Stats(options) => {
        client::project_stats(&options.config, &options.project_id)?
      }
      ProjectCommand::DeletePreview(options) => {
        client::project_delete_preview(&options.config, &options.project_id)?
      }
      ProjectCommand::Delete {
        options,
        revision,
        confirm_project,
      } => client::project_delete(
        &options.config,
        &options.project_id,
        &revision,
        &confirm_project,
      )?,
      ProjectCommand::Deletion(options) => {
        client::project_deletion(&options.config, &options.project_id)?
      }
    },
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
    ServiceSubcommand::Archive {
      config,
      project_id,
      origin,
      run_id,
      partial,
    } => client::archive(
      &config,
      &types::RunScope {
        project_id,
        origin,
        run_id,
      },
      partial,
    )?,
    ServiceSubcommand::FilePut(options) => client::file_put(options)?,
    ServiceSubcommand::Reference(options) => client::reference(options)?,
    ServiceSubcommand::Input { command } => match command {
      InputCommand::Put(options) => client::input_put(options)?,
      InputCommand::Get(options) => client::input_get(options)?,
    },
  };
  println!("{}", serde_json::to_string(&report)?);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use clap::Parser;

  #[derive(Parser)]
  struct Cli {
    #[command(flatten)]
    service: ServiceCommand,
  }

  #[test]
  fn project_cli_requires_explicit_delete_revision_and_project_confirmation() {
    for operation in ["stats", "delete-preview", "deletion"] {
      assert!(
        Cli::try_parse_from([
          "expri",
          "project",
          operation,
          "--config",
          "owner.toml",
          "--project-id",
          "vision"
        ])
        .is_ok()
      );
    }
    let command = [
      "expri",
      "project",
      "delete",
      "--config",
      "owner.toml",
      "--project-id",
      "vision",
      "--revision",
      "7",
      "--confirm-project",
      "vision",
    ];
    assert!(Cli::try_parse_from(command).is_ok());
    assert!(Cli::try_parse_from(&command[..9]).is_err());
    assert!(
      Cli::try_parse_from([
        "expri",
        "project",
        "delete",
        "--config",
        "owner.toml",
        "--project-id",
        "vision",
        "--confirm-project",
        "vision"
      ])
      .is_err()
    );
  }
}
