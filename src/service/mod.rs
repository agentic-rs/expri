mod browser;
mod browser_assets;
mod browser_auth;
mod client;
mod dashboard_data;
pub(crate) mod inputs;
mod notifications;
pub(crate) mod publishing;
pub(crate) mod registrations;
mod server;
mod storage;
mod store;
pub(crate) mod types;

pub(crate) use client::fetch_files;
pub(crate) use types::validate_component;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::error::{ExpriError, Result};

#[derive(Clone, Debug)]
pub struct FetchOptions {
  pub config: PathBuf,
  pub project_id: String,
  pub origins: Vec<String>,
  pub repo: PathBuf,
  pub results_dir: PathBuf,
  pub artifacts: Vec<String>,
  pub labels: Vec<String>,
  pub watch: bool,
  pub dry_run: bool,
  pub quiet: bool,
}

#[derive(Debug, Args)]
pub struct ServiceCommand {
  #[command(subcommand)]
  command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
enum ServiceSubcommand {
  /// Archive finished hosted runs, restore them, or inspect their retention status.
  Run {
    #[command(subcommand)]
    command: RunCommand,
  },
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
  /// Publish saved run data; watch forwards live metrics without blocking training.
  Publish(PublishOptions),
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
  Fetch(ServiceFetchOptions),
  /// List runs recorded by the service.
  List {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    project_id: String,
    #[arg(long)]
    origin: String,
  },
  /// Ask the server to bundle received tracking files and upload result.zip to S3.
  Upload(UploadOptions),
  /// Reference a completed object as an output or reusable input without copying bytes.
  Reference(ReferenceOptions),
  /// Upload one explicit run output or metadata file.
  FileUpload(FileUploadOptions),
  /// Upload or download an immutable private input file.
  Input {
    #[command(subcommand)]
    command: InputCommand,
  },
}

#[derive(Debug, Subcommand)]
enum InputCommand {
  /// Upload an immutable private input to S3.
  Upload(InputUploadOptions),
  /// Download a private input using verified local cache reuse.
  Download(InputDownloadOptions),
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

#[derive(Debug, Subcommand)]
enum RunCommand {
  /// Hide a finished run; its hosted data is deleted after 15 days.
  Archive(RunOptions),
  /// Restore an archived run before its deletion deadline.
  Restore(RunOptions),
  /// Show the run's archival deadline and durable cleanup status.
  Status(RunOptions),
}

#[derive(Debug, Args)]
struct RunOptions {
  #[arg(long)]
  config: PathBuf,
  #[arg(long)]
  project_id: String,
  #[arg(long)]
  origin: String,
  #[arg(long)]
  run_id: String,
}

impl RunOptions {
  fn scope(&self) -> types::RunScope {
    types::RunScope {
      project_id: self.project_id.clone(),
      origin: self.origin.clone(),
      run_id: self.run_id.clone(),
    }
  }
}

#[derive(Debug, Args)]
pub struct PublishOptions {
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
  #[arg(long, default_value = ".expri/publish")]
  pub queue_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct ServiceFetchOptions {
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
struct UploadOptions {
  #[arg(long)]
  config: PathBuf,
  #[arg(long)]
  project_id: String,
  #[arg(long)]
  origin: String,
  #[arg(long)]
  run_id: String,
  /// Upload an acknowledged prefix of an unfinished run; requires an owner token.
  #[arg(long)]
  partial: bool,
}

#[derive(Debug, Args)]
pub struct FileUploadOptions {
  #[arg(long)]
  pub config: PathBuf,
  /// Destination FileTarget as JSON.
  #[arg(long)]
  pub target: String,
  #[arg(long)]
  pub file: PathBuf,
  #[arg(long, default_value = ".expri/publish")]
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
pub struct InputUploadOptions {
  #[arg(long)]
  pub config: PathBuf,
  #[arg(long)]
  pub project_id: String,
  #[arg(long)]
  pub input_id: String,
  #[arg(long)]
  pub file: PathBuf,
  #[arg(long, default_value = ".expri/publish")]
  pub queue_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct InputDownloadOptions {
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
    ServiceSubcommand::Run { command } => match command {
      RunCommand::Archive(options) => client::archive_run(&options.config, &options.scope())?,
      RunCommand::Restore(options) => client::restore_run(&options.config, &options.scope())?,
      RunCommand::Status(options) => client::run_archival(&options.config, &options.scope())?,
    },
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
    ServiceSubcommand::Publish(options) => client::publish(options)?,
    ServiceSubcommand::Resume { run_dir } => publishing::resume(&run_dir)?,
    ServiceSubcommand::PublishWorker { run_dir } => return publishing::worker(&run_dir),
    ServiceSubcommand::Fetch(options) => client::fetch(options)?,
    ServiceSubcommand::List {
      config,
      project_id,
      origin,
    } => client::list(config, project_id, origin)?,
    ServiceSubcommand::Upload(options) => client::upload(
      &options.config,
      &types::RunScope {
        project_id: options.project_id,
        origin: options.origin,
        run_id: options.run_id,
      },
      options.partial,
    )?,
    ServiceSubcommand::FileUpload(options) => client::file_upload(options)?,
    ServiceSubcommand::Reference(options) => client::reference(options)?,
    ServiceSubcommand::Input { command } => match command {
      InputCommand::Upload(options) => client::input_upload(options)?,
      InputCommand::Download(options) => client::input_download(options)?,
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
  fn service_exposes_only_directional_verbs() {
    use clap::CommandFactory;
    let mut cli = Cli::command();
    let help = cli.render_long_help().to_string();
    for name in ["publish", "fetch", "upload", "file-upload"] {
      assert!(
        help
          .lines()
          .any(|line| line.starts_with(&format!("  {name} ")))
      );
    }
    for name in ["push", "pull", "archive", "file-put"] {
      assert!(
        !help
          .lines()
          .any(|line| line.starts_with(&format!("  {name} ")))
      );
      assert!(cli.find_subcommand(name).is_none());
      assert!(Cli::try_parse_from(["expri", name]).is_err());
    }
    let input = cli.find_subcommand_mut("input").unwrap();
    let help = input.render_long_help().to_string();
    for name in ["upload", "download"] {
      assert!(
        help
          .lines()
          .any(|line| line.starts_with(&format!("  {name} ")))
      );
    }
    for name in ["put", "get"] {
      assert!(
        !help
          .lines()
          .any(|line| line.starts_with(&format!("  {name} ")))
      );
      assert!(input.find_subcommand(name).is_none());
      assert!(Cli::try_parse_from(["expri", "input", name]).is_err());
    }
    assert!(!help.contains("archive"));
  }

  #[test]
  fn run_archival_requires_a_complete_scope_and_keeps_zip_upload_separate() {
    for operation in ["archive", "restore", "status"] {
      let args = [
        "expri",
        "run",
        operation,
        "--config",
        "owner.toml",
        "--project-id",
        "vision",
        "--origin",
        "gpu-1",
        "--run-id",
        "run-1",
      ];
      let parsed = Cli::try_parse_from(args).unwrap();
      let ServiceSubcommand::Run { command } = parsed.service.command else {
        panic!("expected scoped run command");
      };
      let options = match command {
        RunCommand::Archive(options)
        | RunCommand::Restore(options)
        | RunCommand::Status(options) => options,
      };
      assert_eq!(options.scope().run_id, "run-1");
      assert_eq!(options.scope().origin, "gpu-1");
      assert!(Cli::try_parse_from(&args[..args.len() - 2]).is_err());
      assert!(Cli::try_parse_from(["expri", operation, "--config", "owner.toml"]).is_err());
    }
  }

  #[test]
  fn removed_service_verbs_reject_complete_requests() {
    let cases: [(&str, &str, &[&str]); 4] = [
      (
        "publish",
        "push",
        &[
          "--config",
          "worker.toml",
          "--run-dir",
          ".expri/runs/run-1",
          "--project-id",
          "vision",
          "--origin",
          "gpu-1",
        ],
      ),
      (
        "fetch",
        "pull",
        &[
          "--config",
          "owner.toml",
          "--project-id",
          "vision",
          "--origin",
          "gpu-1",
          "--run-id",
          "run-1",
        ],
      ),
      (
        "upload",
        "archive",
        &[
          "--config",
          "owner.toml",
          "--project-id",
          "vision",
          "--origin",
          "gpu-1",
          "--run-id",
          "run-1",
          "--partial",
        ],
      ),
      (
        "file-upload",
        "file-put",
        &[
          "--config",
          "worker.toml",
          "--target",
          "{}",
          "--file",
          "outputs/checkpoint.pt",
        ],
      ),
    ];
    for (canonical, removed, options) in cases {
      let request = |operation| {
        let mut args = vec!["expri", operation];
        args.extend_from_slice(options);
        args
      };
      assert!(Cli::try_parse_from(request(canonical)).is_ok());
      let error = Cli::try_parse_from(request(removed)).err().unwrap();
      assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
    let cases: [(&str, &str, &[&str]); 2] = [
      (
        "upload",
        "put",
        &[
          "--config",
          "owner.toml",
          "--project-id",
          "vision",
          "--input-id",
          "dataset",
          "--file",
          "data/train.jsonl",
        ],
      ),
      (
        "download",
        "get",
        &[
          "--config",
          "owner.toml",
          "--project-id",
          "vision",
          "--input-id",
          "dataset",
          "--destination",
          "data/train.jsonl",
        ],
      ),
    ];
    for (canonical, removed, options) in cases {
      let request = |operation| {
        let mut args = vec!["expri", "input", operation];
        args.extend_from_slice(options);
        args
      };
      assert!(Cli::try_parse_from(request(canonical)).is_ok());
      let error = Cli::try_parse_from(request(removed)).err().unwrap();
      assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
  }

  #[test]
  fn server_upload_accepts_explicit_scope_and_partial_option() {
    let cli = Cli::try_parse_from([
      "expri",
      "upload",
      "--config",
      "owner.toml",
      "--project-id",
      "vision",
      "--origin",
      "gpu-1",
      "--run-id",
      "training-1",
      "--partial",
    ])
    .unwrap();
    let ServiceSubcommand::Upload(options) = cli.service.command else {
      panic!("unexpected operation");
    };
    assert_eq!(options.project_id, "vision");
    assert_eq!(options.origin, "gpu-1");
    assert_eq!(options.run_id, "training-1");
    assert!(options.partial);
  }

  #[test]
  fn uploads_use_the_publishing_queue_by_default() {
    let command = Cli::try_parse_from([
      "expri",
      "publish",
      "--config",
      "worker.toml",
      "--run-dir",
      ".expri/runs/training-1",
      "--project-id",
      "vision",
      "--origin",
      "gpu-1",
    ])
    .unwrap();
    let ServiceSubcommand::Publish(options) = command.service.command else {
      panic!("unexpected operation");
    };
    assert_eq!(options.queue_dir, PathBuf::from(".expri/publish"));
    let command = Cli::try_parse_from([
      "expri",
      "input",
      "upload",
      "--config",
      "owner.toml",
      "--project-id",
      "vision",
      "--input-id",
      "dataset",
      "--file",
      "data/train.jsonl",
    ])
    .unwrap();
    let ServiceSubcommand::Input {
      command: InputCommand::Upload(options),
    } = command.service.command
    else {
      panic!("unexpected operation");
    };
    assert_eq!(options.queue_dir, PathBuf::from(".expri/publish"));
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
