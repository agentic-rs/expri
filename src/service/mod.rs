pub use expri_client::FetchOptions;
use expri_client::{client, resolve_client_config};
pub(crate) use expri_client::{fetch_files, publishing, registrations, validate_component};
use expri_core::service as types;
use expri_server as server;

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
  /// Inspect machine inventories and system-profile history.
  System(crate::system_cli::ServiceSystemCommand),
  /// Edit hosted run annotations or manage archival and retention.
  Run {
    #[command(subcommand)]
    command: RunCommand,
  },
  /// Inspect project storage or delete a project using unrestricted server credentials.
  Project {
    #[command(subcommand)]
    command: ProjectCommand,
  },
  /// Run the optional self-hosted catalog and artifact service.
  Serve {
    /// Server daemon TOML; separate from the global client connection.
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
    /// Override the global [server] connection with a standalone client file.
    #[arg(long)]
    config: Option<PathBuf>,
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
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  config: Option<PathBuf>,
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
  /// Show a hosted run's nickname and tags.
  Annotations(RunOptions),
  /// Replace nickname and tags; omitted values are cleared. Requires credentials with unrestricted server access.
  Annotate(AnnotateOptions),
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("annotation_values").required(true).multiple(true)))]
struct AnnotateOptions {
  #[command(flatten)]
  options: RunOptions,
  /// Optional display name (up to 128 characters); run ID stays unchanged.
  #[arg(long, group = "annotation_values")]
  nickname: Option<String>,
  /// Repeat for each tag (up to 16 tags, 48 characters each).
  #[arg(long = "tag", group = "annotation_values")]
  tags: Vec<String>,
  /// Clear both nickname and tags.
  #[arg(long, group = "annotation_values", conflicts_with_all = ["nickname", "tags"])]
  clear: bool,
}

#[derive(Debug, Args)]
struct RunOptions {
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  config: Option<PathBuf>,
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
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  pub config: Option<PathBuf>,
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
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  pub config: Option<PathBuf>,
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
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  config: Option<PathBuf>,
  #[arg(long)]
  project_id: String,
  #[arg(long)]
  origin: String,
  #[arg(long)]
  run_id: String,
  /// Upload an acknowledged prefix of an unfinished run; requires credentials with unrestricted server access.
  #[arg(long)]
  partial: bool,
}

#[derive(Debug, Args)]
pub struct FileUploadOptions {
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  pub config: Option<PathBuf>,
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
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  pub config: Option<PathBuf>,
  /// Source FileTarget as JSON (run output or project input).
  #[arg(long)]
  pub source: String,
  /// Destination FileTarget as JSON. Requires credentials with unrestricted server access.
  #[arg(long)]
  pub target: String,
  #[arg(long)]
  pub size: u64,
  #[arg(long)]
  pub sha256: String,
}

#[derive(Debug, Args)]
pub struct InputUploadOptions {
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  pub config: Option<PathBuf>,
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
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  pub config: Option<PathBuf>,
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
    ServiceSubcommand::System(command) => return crate::system_cli::run_service(command),
    ServiceSubcommand::Run { command } => match command {
      RunCommand::Archive(options) => client::archive_run(
        &resolve_client_config(options.config.as_deref())?,
        &options.scope(),
      )?,
      RunCommand::Restore(options) => client::restore_run(
        &resolve_client_config(options.config.as_deref())?,
        &options.scope(),
      )?,
      RunCommand::Status(options) => client::run_archival(
        &resolve_client_config(options.config.as_deref())?,
        &options.scope(),
      )?,
      RunCommand::Annotations(options) => client::run_annotations(
        &resolve_client_config(options.config.as_deref())?,
        &options.scope(),
      )?,
      RunCommand::Annotate(options) => client::set_run_annotations(
        &resolve_client_config(options.options.config.as_deref())?,
        &options.options.scope(),
        options.nickname,
        options.tags,
      )?,
    },
    ServiceSubcommand::Project { command } => match command {
      ProjectCommand::Stats(options) => client::project_stats(
        &resolve_client_config(options.config.as_deref())?,
        &options.project_id,
      )?,
      ProjectCommand::DeletePreview(options) => client::project_delete_preview(
        &resolve_client_config(options.config.as_deref())?,
        &options.project_id,
      )?,
      ProjectCommand::Delete {
        options,
        revision,
        confirm_project,
      } => client::project_delete(
        &resolve_client_config(options.config.as_deref())?,
        &options.project_id,
        &revision,
        &confirm_project,
      )?,
      ProjectCommand::Deletion(options) => client::project_deletion(
        &resolve_client_config(options.config.as_deref())?,
        &options.project_id,
      )?,
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
    ServiceSubcommand::Publish(options) => client::publish(options.try_into()?)?,
    ServiceSubcommand::Resume { run_dir } => publishing::resume(&run_dir)?,
    ServiceSubcommand::PublishWorker { run_dir } => return publishing::worker(&run_dir),
    ServiceSubcommand::Fetch(options) => client::fetch(options.try_into()?)?,
    ServiceSubcommand::List {
      config,
      project_id,
      origin,
    } => client::list(
      resolve_client_config(config.as_deref())?,
      project_id,
      origin,
    )?,
    ServiceSubcommand::Upload(options) => client::upload(
      &resolve_client_config(options.config.as_deref())?,
      &types::RunScope {
        project_id: options.project_id,
        origin: options.origin,
        run_id: options.run_id,
      },
      options.partial,
    )?,
    ServiceSubcommand::FileUpload(options) => client::file_upload(options.try_into()?)?,
    ServiceSubcommand::Reference(options) => client::reference(options.try_into()?)?,
    ServiceSubcommand::Input { command } => match command {
      InputCommand::Upload(options) => client::input_upload(options.try_into()?)?,
      InputCommand::Download(options) => client::input_download(options.try_into()?)?,
    },
  };
  println!("{}", serde_json::to_string(&report)?);
  Ok(())
}

impl TryFrom<PublishOptions> for expri_client::PublishOptions {
  type Error = ExpriError;

  fn try_from(options: PublishOptions) -> Result<Self> {
    Ok(Self {
      config: resolve_client_config(options.config.as_deref())?,
      run_dir: options.run_dir,
      project_id: options.project_id,
      origin: options.origin,
      artifacts: options.artifacts,
      watch: options.watch,
      queue_dir: options.queue_dir,
    })
  }
}

impl TryFrom<ServiceFetchOptions> for expri_client::ServiceFetchOptions {
  type Error = ExpriError;

  fn try_from(options: ServiceFetchOptions) -> Result<Self> {
    Ok(Self {
      config: resolve_client_config(options.config.as_deref())?,
      project_id: options.project_id,
      origin: options.origin,
      run_id: options.run_id,
      repo: options.repo,
      results_dir: options.results_dir,
      source: options.source,
      artifacts: options.artifacts,
    })
  }
}

impl TryFrom<FileUploadOptions> for expri_client::FileUploadOptions {
  type Error = ExpriError;

  fn try_from(options: FileUploadOptions) -> Result<Self> {
    Ok(Self {
      config: resolve_client_config(options.config.as_deref())?,
      target: options.target,
      file: options.file,
      queue_dir: options.queue_dir,
    })
  }
}

impl TryFrom<ReferenceOptions> for expri_client::ReferenceOptions {
  type Error = ExpriError;

  fn try_from(options: ReferenceOptions) -> Result<Self> {
    Ok(Self {
      config: resolve_client_config(options.config.as_deref())?,
      source: options.source,
      target: options.target,
      size: options.size,
      sha256: options.sha256,
    })
  }
}

impl TryFrom<InputUploadOptions> for expri_client::InputUploadOptions {
  type Error = ExpriError;

  fn try_from(options: InputUploadOptions) -> Result<Self> {
    Ok(Self {
      config: resolve_client_config(options.config.as_deref())?,
      project_id: options.project_id,
      input_id: options.input_id,
      file: options.file,
      queue_dir: options.queue_dir,
    })
  }
}

impl TryFrom<InputDownloadOptions> for expri_client::InputDownloadOptions {
  type Error = ExpriError;

  fn try_from(options: InputDownloadOptions) -> Result<Self> {
    Ok(Self {
      config: resolve_client_config(options.config.as_deref())?,
      project_id: options.project_id,
      input_id: options.input_id,
      destination: options.destination,
    })
  }
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
  fn all_client_commands_accept_global_defaults_and_explicit_overrides() {
    let scope = [
      "--project-id",
      "vision",
      "--origin",
      "gpu-1",
      "--run-id",
      "run-1",
    ];
    let mut cases = vec![
      vec!["system", "list"],
      vec!["system", "profile", "machine-1"],
      vec!["system", "history", "machine-1"],
      vec!["list", "--project-id", "vision", "--origin", "gpu-1"],
      vec![
        "publish",
        "--run-dir",
        ".expri/runs/run-1",
        "--project-id",
        "vision",
        "--origin",
        "gpu-1",
      ],
      vec![
        "file-upload",
        "--target",
        "{}",
        "--file",
        "outputs/model.pt",
      ],
      vec![
        "reference",
        "--source",
        "{}",
        "--target",
        "{}",
        "--size",
        "1",
        "--sha256",
        "abc",
      ],
      vec![
        "input",
        "upload",
        "--project-id",
        "vision",
        "--input-id",
        "dataset",
        "--file",
        "data/train.jsonl",
      ],
      vec![
        "input",
        "download",
        "--project-id",
        "vision",
        "--input-id",
        "dataset",
        "--destination",
        "data/train.jsonl",
      ],
      vec![
        "project",
        "delete",
        "--project-id",
        "vision",
        "--revision",
        "7",
        "--confirm-project",
        "vision",
      ],
    ];
    for operation in ["archive", "restore", "status", "annotations"] {
      let mut args = vec!["run", operation];
      args.extend(scope);
      cases.push(args);
    }
    let mut annotate = vec!["run", "annotate", "--clear"];
    annotate.extend(scope);
    cases.push(annotate);
    for operation in ["fetch", "upload"] {
      let mut args = vec![operation];
      args.extend(scope);
      cases.push(args);
    }
    for operation in ["stats", "delete-preview", "deletion"] {
      cases.push(vec!["project", operation, "--project-id", "vision"]);
    }
    for args in cases {
      let default = std::iter::once("expri").chain(args.iter().copied());
      assert!(Cli::try_parse_from(default).is_ok(), "{args:?}");
      let explicit = std::iter::once("expri")
        .chain(args.iter().copied())
        .chain(["--config", "client.toml"]);
      assert!(Cli::try_parse_from(explicit).is_ok(), "{args:?}");
    }
    assert!(Cli::try_parse_from(["expri", "serve"]).is_err());
    let cli = Cli::try_parse_from(["expri", "serve", "--config", "server.toml"]).unwrap();
    let ServiceSubcommand::Serve { config, .. } = cli.service.command else {
      unreachable!()
    };
    assert_eq!(config, PathBuf::from("server.toml"));
  }

  #[test]
  fn transfer_options_keep_the_selected_standalone_client_file() {
    let directory = tempfile::tempdir().unwrap();
    let selected = directory.path().canonicalize().unwrap().join("client.toml");
    std::fs::write(
      &selected,
      "url='https://service.example.invalid'\ntoken_env='EXPRI_SERVER_TOKEN'\n",
    )
    .unwrap();
    for args in [
      vec![
        "publish",
        "--run-dir",
        ".expri/runs/run-1",
        "--project-id",
        "vision",
        "--origin",
        "gpu-1",
      ],
      vec![
        "fetch",
        "--project-id",
        "vision",
        "--origin",
        "gpu-1",
        "--run-id",
        "run-1",
      ],
      vec![
        "file-upload",
        "--target",
        "{}",
        "--file",
        "outputs/model.pt",
      ],
      vec![
        "reference",
        "--source",
        "{}",
        "--target",
        "{}",
        "--size",
        "1",
        "--sha256",
        "abc",
      ],
      vec![
        "input",
        "upload",
        "--project-id",
        "vision",
        "--input-id",
        "dataset",
        "--file",
        "data/train.jsonl",
      ],
      vec![
        "input",
        "download",
        "--project-id",
        "vision",
        "--input-id",
        "dataset",
        "--destination",
        "data/train.jsonl",
      ],
    ] {
      let mut values = vec![std::ffi::OsString::from("expri")];
      values.extend(args.iter().map(|arg| std::ffi::OsString::from(*arg)));
      values.push("--config".into());
      values.push(selected.as_os_str().into());
      let cli = Cli::try_parse_from(values).unwrap();
      let converted = match cli.service.command {
        ServiceSubcommand::Publish(options) => {
          expri_client::PublishOptions::try_from(options)
            .unwrap()
            .config
        }
        ServiceSubcommand::Fetch(options) => {
          expri_client::ServiceFetchOptions::try_from(options)
            .unwrap()
            .config
        }
        ServiceSubcommand::FileUpload(options) => {
          expri_client::FileUploadOptions::try_from(options)
            .unwrap()
            .config
        }
        ServiceSubcommand::Reference(options) => {
          expri_client::ReferenceOptions::try_from(options)
            .unwrap()
            .config
        }
        ServiceSubcommand::Input {
          command: InputCommand::Upload(options),
        } => {
          expri_client::InputUploadOptions::try_from(options)
            .unwrap()
            .config
        }
        ServiceSubcommand::Input {
          command: InputCommand::Download(options),
        } => {
          expri_client::InputDownloadOptions::try_from(options)
            .unwrap()
            .config
        }
        _ => unreachable!(),
      };
      assert_eq!(converted, selected, "{args:?}");
    }
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
        "client.toml",
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
        _ => panic!("expected archival command"),
      };
      assert_eq!(options.scope().run_id, "run-1");
      assert_eq!(options.scope().origin, "gpu-1");
      assert!(Cli::try_parse_from(&args[..args.len() - 2]).is_err());
      assert!(Cli::try_parse_from(["expri", operation, "--config", "client.toml"]).is_err());
    }
  }

  #[test]
  fn run_annotations_require_an_explicit_replacement_or_clear() {
    let base = [
      "expri",
      "run",
      "annotate",
      "--config",
      "client.toml",
      "--project-id",
      "vision",
      "--origin",
      "gpu-1",
      "--run-id",
      "run-1",
    ];
    assert!(Cli::try_parse_from(base).is_err());
    let args = base.into_iter().chain([
      "--nickname",
      "Baseline",
      "--tag",
      "model-a",
      "--tag",
      "baseline",
    ]);
    let parsed = Cli::try_parse_from(args).unwrap();
    let ServiceSubcommand::Run {
      command: RunCommand::Annotate(options),
    } = parsed.service.command
    else {
      panic!("expected annotations replacement");
    };
    assert_eq!(options.nickname.as_deref(), Some("Baseline"));
    assert_eq!(options.tags, ["model-a", "baseline"]);
    assert_eq!(options.options.scope().origin, "gpu-1");
    assert!(Cli::try_parse_from(base.into_iter().chain(["--clear"])).is_ok());
    assert!(Cli::try_parse_from(base.into_iter().chain(["--clear", "--tag", "baseline"])).is_err());
    assert!(
      Cli::try_parse_from(
        base
          .into_iter()
          .chain(["--clear", "--nickname", "Baseline"])
      )
      .is_err()
    );
    let mut show = base;
    show[2] = "annotations";
    assert!(Cli::try_parse_from(show).is_ok());
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
          "client.toml",
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
          "client.toml",
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
          "client.toml",
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
          "client.toml",
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
      "client.toml",
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
      "client.toml",
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
          "client.toml",
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
      "client.toml",
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
        "client.toml",
        "--project-id",
        "vision",
        "--confirm-project",
        "vision"
      ])
      .is_err()
    );
  }
}
