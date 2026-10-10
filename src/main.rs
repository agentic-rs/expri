mod archive;
mod artifacts_cli;
mod config;
mod context;
mod controller;
mod dashboard;
mod environment;
mod error;
mod fetch_cli;
mod filter;
mod git;
mod jobs;
mod lock;
mod metric_charts;
mod metrics;
mod metrics_cli;
mod node;
mod protocol;
mod push_cli;
mod run_artifacts;
mod run_logs;
mod runs;
mod runs_cli;
mod service;
mod shell;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::context::CommandContext;
use crate::controller::download::{DownloadOptions, download_target};
use crate::controller::setup::{SetupOptions, setup_target};
use crate::controller::sync::{
  SyncOptions, sync_target_with_diagnostic_receipt, sync_target_with_receipt,
};
use crate::controller::task::{
  LocalTaskOptions, RemoteTaskOptions, run_local_task, run_remote_task,
};
use crate::error::{ExpriError, Result};
use crate::node::cli::NodeCommand;
use crate::runs_cli::RunsCommand;

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
  /// Push source and configuration changes to a worker checkout.
  Push(PushCommand),
  /// Fetch run data and selected outputs from the expri service and S3.
  Fetch(FetchCommand),
  /// Register finalized training outputs for background upload.
  Artifact(artifacts_cli::ArtifactCommand),
  Download(DownloadCommand),
  Setup(SetupCommand),
  Run(RunCommand),
  Env(EnvironmentCommand),
  Runs(RunsCommand),
  /// Browse local and cached experiment results in a local dashboard.
  Dashboard(dashboard::DashboardCommand),
  /// Publish experiment data and transfer files through a self-hosted service.
  Service(service::ServiceCommand),
  Node {
    #[command(subcommand)]
    command: NodeCommand,
  },
}

#[derive(Debug, Args)]
struct EnvironmentCommand {
  #[command(subcommand)]
  command: EnvironmentSubcommand,
}

#[derive(Debug, Subcommand)]
enum EnvironmentSubcommand {
  /// Check the base Python stack against the selected lock without installing project dependencies.
  Doctor(EnvironmentArgs),
  /// Preview or remove inactive run virtual environments, retaining experiment records.
  Prune(PruneCommand),
}

#[derive(Debug, Args)]
struct EnvironmentArgs {
  #[arg(long)]
  config: Option<PathBuf>,
  #[arg(long)]
  repo: Option<PathBuf>,
  #[arg(long)]
  control_path: Option<String>,
  #[arg(long, default_value = "30m")]
  control_persist: String,
  #[arg(long)]
  json: bool,
}

#[derive(Debug, Args)]
struct PruneCommand {
  #[command(flatten)]
  options: EnvironmentArgs,
  #[arg(long, conflicts_with = "dry_run")]
  apply: bool,
  #[arg(long)]
  dry_run: bool,
  #[arg(long, default_value_t = 1)]
  keep_last: usize,
}

#[derive(Debug, Args)]
struct PushCommand {
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

  /// Keep pushing source and configuration changes to the worker checkout.
  #[arg(long, conflicts_with = "paths")]
  watch: bool,

  #[arg(value_name = "PATH", last = true)]
  paths: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct FetchCommand {
  #[arg(long)]
  config: Option<PathBuf>,

  #[arg(long)]
  repo: Option<PathBuf>,

  #[arg(long)]
  dry_run: bool,

  /// Keep fetching new run data and selected outputs every five seconds.
  #[arg(long)]
  watch: bool,
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

  /// Skip pushing source changes before starting the run.
  #[arg(long)]
  no_push: bool,

  /// Disable automatic publishing for this run.
  #[arg(long)]
  no_publish: bool,

  /// Start an isolated run in the background and return its run identifier.
  #[arg(long)]
  detach: bool,

  #[arg(
    value_name = "TASK",
    required = true,
    num_args = 1..,
    trailing_var_arg = true
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
    Command::Push(command) => push_cli::run(command, cli.target.as_deref(), cli.verbose, cli.quiet),
    Command::Fetch(command) => fetch_cli::run(command, cli.target.as_deref(), cli.quiet),
    Command::Artifact(command) => artifacts_cli::run(command, cli.target.as_deref()),
    Command::Download(command) => {
      run_download(command, cli.target.as_deref(), cli.verbose, cli.quiet)
    }
    Command::Setup(command) => run_setup(command, cli.target.as_deref(), cli.verbose, cli.quiet),
    Command::Run(command) => run_task(command, cli.target.as_deref(), cli.verbose, cli.quiet),
    Command::Env(command) => {
      run_environment(command, cli.target.as_deref(), cli.verbose, cli.quiet)
    }
    Command::Runs(command) => runs_cli::run(command, cli.target.as_deref(), cli.verbose, cli.quiet),
    Command::Dashboard(command) => dashboard::run(command, cli.target.as_deref()),
    Command::Service(command) => service::run(command, cli.target.as_deref()),
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

fn run_environment(
  command: EnvironmentCommand,
  target: Option<&str>,
  verbosity: u8,
  quiet: bool,
) -> Result<()> {
  let (options, prune) = match command.command {
    EnvironmentSubcommand::Doctor(options) => (options, None),
    EnvironmentSubcommand::Prune(command) => (
      command.options,
      Some(protocol::PruneRequest {
        apply: command.apply && !command.dry_run,
        keep_last: command.keep_last,
      }),
    ),
  };
  let context = CommandContext::load(options.config, options.repo)?;
  let (extras, sync_args) = environment_selection(&context.config);
  if target.is_some() {
    let context = context.into_target(target, options.control_path)?;
    let action = if let Some(prune) = prune {
      protocol::EnvironmentAction::Prune(prune)
    } else {
      let environment = context.target.environment.clone().ok_or_else(|| {
        ExpriError::Message("env doctor requires a configured target environment".to_string())
      })?;
      protocol::EnvironmentAction::Doctor(protocol::DoctorRequest {
        environment,
        extras,
        sync_args,
      })
    };
    return controller::environment::execute(controller::environment::EnvironmentOptions {
      target: context.target,
      control_path: context.control_path,
      control_persist: options.control_persist,
      verbosity,
      quiet,
      request: protocol::EnvironmentCommandRequest {
        json: options.json,
        action,
      },
    });
  }
  let action = if let Some(prune) = prune {
    protocol::EnvironmentAction::Prune(prune)
  } else {
    let environment = context.config.local_environment()?.ok_or_else(|| {
      ExpriError::Message("env doctor requires a configured [environment] table".to_string())
    })?;
    protocol::EnvironmentAction::Doctor(protocol::DoctorRequest {
      environment,
      extras,
      sync_args,
    })
  };
  node::environment::apply_request_at(
    &protocol::EnvironmentCommandRequest {
      json: options.json,
      action,
    },
    &context.repo_root,
  )
}

fn environment_selection(config: &config::Config) -> (Vec<String>, Vec<String>) {
  let mut extras = Vec::new();
  let mut sync_args = Vec::new();
  for step in config.setup_steps() {
    if let protocol::SetupStep::Uv {
      extras: step_extras,
      args,
    } = step
    {
      extras.extend(step_extras);
      sync_args.extend(args);
    }
  }
  (extras, sync_args)
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
  let ignore = context.config.download_ignore();

  download_target(DownloadOptions {
    repo_root: context.repo_root,
    project_name: context.project_name,
    target_name: context.target_name,
    target: context.target,
    results_dir,
    mappings,
    ignore,
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
  let context = CommandContext::load(command.config, command.repo)?;
  let steps = context.config.setup_steps();
  if target.is_none() && context.config.environment.is_some() {
    let request = protocol::SetupRequest {
      state_dir: ".expri".to_string(),
      force: command.force,
      steps,
      environment: context.config.local_environment()?,
    };
    if command.dry_run {
      if !quiet {
        eprintln!("local setup: {}", serde_json::to_string(&request)?);
      }
      return Ok(());
    }
    return node::setup::apply_request_at(&request, &context.repo_root);
  }
  let context = context.into_target(target, command.control_path)?;

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
  let remote_managed = context
    .config
    .push
    .as_ref()
    .and_then(|push| push.remote_managed.clone())
    .unwrap_or_default();
  let (extras, sync_args) = environment_selection(&context.config);
  if target.is_some() {
    let mut context = context.into_target(target, command.control_path)?;
    if command.no_publish {
      disable_publishing(&mut context.target.service);
    }
    if context.target.service.is_some() && context.target.environment.is_none() {
      if context
        .target
        .service
        .as_ref()
        .is_some_and(|service| !service.inputs.is_empty())
      {
        return Err(ExpriError::Message("private input preparation requires a configured target environment; add [environment], or download inputs manually and remove service.inputs".into()));
      }
      return Err(ExpriError::Message(
        "automatic publishing requires a configured target environment; add [environment] or use --no-publish".into(),
      ));
    }
    if command.detach && context.target.environment.is_none() {
      return Err(ExpriError::Message(
        "--detach requires a configured target environment".to_string(),
      ));
    }
    if context.target.service.is_some() {
      controller::task::check_run_publishing_target(
        &context.target,
        &context.control_path,
        &command.control_persist,
        command.dry_run,
        verbosity,
        quiet,
      )?;
    }
    let expected_sync = if !command.no_push {
      let sync = context.config.push_rules()?;
      let sync_with_receipt = if command.detach {
        sync_target_with_diagnostic_receipt
      } else {
        sync_target_with_receipt
      };
      sync_with_receipt(SyncOptions {
        repo_root: context.repo_root.clone(),
        project_name: context.project_name.clone(),
        target_name: context.target_name.clone(),
        target: context.target.clone(),
        sync,
        control_path: context.control_path.clone(),
        control_persist: command.control_persist.clone(),
        dry_run: command.dry_run,
        force: false,
        paths: Vec::new(),
        verbosity,
        quiet,
      })?
    } else {
      None
    };
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
      remote_managed,
      extras,
      sync_args,
      expected_sync,
      detach: command.detach,
    });
  }

  let environment = context.config.local_environment()?;
  let mut service = context.config.local_service()?;
  if command.no_publish {
    disable_publishing(&mut service);
  }
  let mut local_sources = remote_managed;
  if let Some(paths) = context
    .config
    .push
    .as_ref()
    .and_then(|push| push.include_ignored.as_ref())
  {
    local_sources.extend(paths.iter().cloned());
  }
  run_local_task(LocalTaskOptions {
    repo_root: context.repo_root,
    project_name: context.project_name,
    name,
    task,
    args,
    dry_run: command.dry_run,
    detach: command.detach,
    verbosity,
    quiet,
    environment,
    service,
    remote_managed: local_sources,
    extras,
    sync_args,
  })
}

fn disable_publishing(service: &mut Option<config::RunServiceConfig>) {
  if let Some(config) = service {
    if config.inputs.is_empty() {
      *service = None;
    } else {
      config.publish = false;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn push_and_fetch_select_separate_directions() {
    let cli =
      Cli::try_parse_from(["expri", "-T", "gpu-1", "push", "--watch", "--dry-run"]).unwrap();
    assert!(matches!(
      cli.command,
      Command::Push(PushCommand {
        watch: true,
        dry_run: true,
        ..
      })
    ));
    assert!(Cli::try_parse_from(["expri", "push", "--watch", "--", "code.py"]).is_err());
    let cli = Cli::try_parse_from(["expri", "push", "--", "code.py"]).unwrap();
    assert!(matches!(
      cli.command,
      Command::Push(PushCommand { paths, .. }) if paths == [PathBuf::from("code.py")]
    ));
    assert!(Cli::try_parse_from(["expri", "-T", "gpu-1", "sync", "--watch", "--dry-run"]).is_err());
    assert!(Cli::try_parse_from(["expri", "push", "--pull", "--", "code.py"]).is_err());
    assert!(Cli::try_parse_from(["expri", "run", "--no-sync", "train"]).is_err());
    let cli = Cli::try_parse_from(["expri", "fetch", "--watch", "--dry-run"]).unwrap();
    assert!(matches!(
      cli.command,
      Command::Fetch(FetchCommand {
        watch: true,
        dry_run: true,
        ..
      })
    ));
    for flag in ["--pull", "--force", "--control-path"] {
      assert!(Cli::try_parse_from(["expri", "fetch", flag]).is_err());
    }
    assert!(Cli::try_parse_from(["expri", "fetch", "--", "code.py"]).is_err());
  }

  #[test]
  fn node_apply_uses_push_without_a_sync_alias() {
    let cli =
      Cli::try_parse_from(["expri", "node", "push-apply", "--request", "request.json"]).unwrap();
    assert!(matches!(
      cli.command,
      Command::Node {
        command: NodeCommand::PushApply(_)
      }
    ));
    assert!(
      Cli::try_parse_from(["expri", "node", "sync-apply", "--request", "request.json",]).is_err()
    );
  }

  #[test]
  fn canonical_help_exposes_push_and_fetch() {
    use clap::CommandFactory;
    let mut command = Cli::command();
    let help = command.render_help().to_string();
    assert!(help.contains("push"));
    assert!(help.contains("fetch"));
    assert!(!help.contains("\n  sync "));
    let push = command
      .find_subcommand_mut("push")
      .unwrap()
      .render_help()
      .to_string();
    assert!(push.contains("--watch"));
    assert!(!push.contains("--pull"));
    assert!(push.contains("[PATH]"));
    assert!(!push.contains("cloud"));
    let run = command
      .find_subcommand_mut("run")
      .unwrap()
      .render_help()
      .to_string();
    assert!(run.contains("--no-push"));
    assert!(!run.contains("--no-sync"));
    let cli = Cli::try_parse_from(["expri", "run", "--no-push", "train"]).unwrap();
    assert!(matches!(
      cli.command,
      Command::Run(RunCommand { no_push: true, .. })
    ));
  }

  #[test]
  fn finalized_file_registration_choices_and_publishing_inputs_are_preserved() {
    assert!(
      Cli::try_parse_from([
        "expri",
        "artifact",
        "register",
        "outputs/1000.pt",
        "--label",
        "best",
        "--label",
        "latest"
      ])
      .is_ok()
    );
    assert!(
      Cli::try_parse_from([
        "expri",
        "artifact",
        "register",
        "outputs/1000.pt",
        "--label",
        "newest"
      ])
      .is_err()
    );
    let mut service = Some(config::RunServiceConfig {
      client_config: "/etc/expri/worker.toml".into(),
      project_id: "demo".into(),
      origin: "worker".into(),
      dashboard_url: None,
      inputs: vec![config::RunInputConfig {
        input_id: "dataset".into(),
        destination: "data.bin".into(),
      }],
      publish: true,
    });
    disable_publishing(&mut service);
    assert!(!service.as_ref().unwrap().publish);
    assert_eq!(service.as_ref().unwrap().inputs.len(), 1);
    service.as_mut().unwrap().inputs.clear();
    disable_publishing(&mut service);
    assert!(service.is_none());
  }

  #[test]
  fn run_options_before_name_belong_to_expri() {
    let cli = Cli::try_parse_from([
      "expri",
      "run",
      "--dry-run",
      "--no-push",
      "--no-publish",
      "--detach",
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
    assert!(command.no_push);
    assert!(command.no_publish);
    assert!(command.detach);
    assert_eq!(command.task[1..], ["--model", "tiny"]);
  }

  #[test]
  fn run_options_after_name_are_task_args() {
    let cli = Cli::try_parse_from([
      "expri",
      "run",
      "train",
      "--dry-run",
      "--detach",
      "--config",
      "task-config.toml",
    ])
    .unwrap();

    let Command::Run(command) = cli.command else {
      panic!("expected run command");
    };

    assert_eq!(command.task[0], "train");
    assert!(!command.dry_run);
    assert!(!command.detach);
    assert!(command.config.is_none());
    assert_eq!(
      command.task[1..],
      ["--dry-run", "--detach", "--config", "task-config.toml"]
    );
  }

  #[test]
  fn detached_legacy_remote_tasks_fail_before_push_or_transport() {
    let fixture = tempfile::tempdir().unwrap();
    let config = fixture.path().join("expri.toml");
    std::fs::write(
      &config,
      r#"[tasks]
train = ["python", "train.py"]
[target.gpu]
host = "gpu"
remote_dir = "/tmp/unused-expri"
transport = "ctl"
ctl_bin = "/missing-must-not-launch-ctl"
"#,
    )
    .unwrap();
    let cli = Cli::try_parse_from([
      "expri",
      "run",
      "--config",
      config.to_str().unwrap(),
      "--detach",
      "train",
    ])
    .unwrap();
    let Command::Run(command) = cli.command else {
      panic!("expected run command");
    };
    let error = run_task(command, Some("gpu"), 0, true).unwrap_err();
    assert_eq!(
      error.to_string(),
      "--detach requires a configured target environment"
    );
    assert!(!fixture.path().join(".expri").exists());
  }

  #[test]
  fn publishing_legacy_task_requires_recorded_environment_and_can_be_disabled() {
    let fixture = tempfile::tempdir().unwrap();
    let config = fixture.path().join("expri.toml");
    std::fs::write(
      &config,
      r#"
[tasks]
train = ["python", "train.py"]
[service]
client_config = "/etc/expri/worker.toml"
project_id = "vision"
origin = "local"
"#,
    )
    .unwrap();
    for disabled in [false, true] {
      let mut args = vec![
        "expri",
        "run",
        "--config",
        config.to_str().unwrap(),
        "--dry-run",
      ];
      if disabled {
        args.push("--no-publish");
      }
      args.push("train");
      let Command::Run(command) = Cli::try_parse_from(args).unwrap().command else {
        panic!("expected run");
      };
      let result = run_task(command, None, 0, true);
      if disabled {
        assert!(result.is_ok());
      } else {
        assert!(
          result
            .unwrap_err()
            .to_string()
            .contains("automatic publishing requires")
        );
      }
    }
    assert!(!fixture.path().join(".expri").exists());
  }
}
