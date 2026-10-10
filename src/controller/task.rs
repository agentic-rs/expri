use std::path::PathBuf;
use std::process::Command;

use crate::config::{EnvironmentConfig, RunServiceConfig, TargetConfig, TaskConfig};
use crate::controller::protocol::{ProtocolPreference, apply_run_with_preference};
use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result, command_exit_code};
use crate::protocol::{RunRequest, SyncIdentity};
use crate::shell;

pub struct LocalTaskOptions {
  pub repo_root: PathBuf,
  pub project_name: Option<String>,
  pub name: String,
  pub task: TaskConfig,
  pub args: Vec<String>,
  pub dry_run: bool,
  pub detach: bool,
  pub verbosity: u8,
  pub quiet: bool,
  pub environment: Option<EnvironmentConfig>,
  pub service: Option<RunServiceConfig>,
  pub remote_managed: Vec<String>,
  pub extras: Vec<String>,
  pub sync_args: Vec<String>,
}

pub struct RemoteTaskOptions {
  pub repo_root: PathBuf,
  pub project_name: Option<String>,
  pub target_name: String,
  pub target: TargetConfig,
  pub control_path: String,
  pub control_persist: String,
  pub name: String,
  pub task: TaskConfig,
  pub args: Vec<String>,
  pub dry_run: bool,
  pub detach: bool,
  pub verbosity: u8,
  pub quiet: bool,
  pub remote_managed: Vec<String>,
  pub extras: Vec<String>,
  pub sync_args: Vec<String>,
  pub expected_sync: Option<SyncIdentity>,
}

/// Check before syncing so an incompatible worker cannot receive a publishing run.
pub fn check_run_publishing_target(
  target: &TargetConfig,
  control_path: &str,
  control_persist: &str,
  dry_run: bool,
  verbosity: u8,
  quiet: bool,
) -> Result<()> {
  let preference = ProtocolPreference::parse(target.protocol.as_deref())?;
  let node_bin = target.node_bin.as_deref().unwrap_or("expri");
  let remote = Remote::new(
    target.clone(),
    control_path.to_string(),
    control_persist.to_string(),
    dry_run,
    verbosity,
    quiet,
  )?;
  remote.connect()?;
  require_service(&remote, preference, node_bin, target.service.as_ref())
}

fn require_service(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
  service: Option<&RunServiceConfig>,
) -> Result<()> {
  if let Some(service) = service {
    if service.publish {
      super::protocol::require_run_publishing(remote, preference, node_bin)?;
    }
    if !service.inputs.is_empty() {
      super::protocol::require_run_inputs(remote, preference, node_bin)?;
    }
  }
  Ok(())
}

pub fn run_local_task(options: LocalTaskOptions) -> Result<()> {
  if let Some(environment) = &options.environment {
    let mut command = options.task.command.clone();
    command.extend(options.args.clone());
    let request = RunRequest {
      name: options.name.clone(),
      command,
      environment: environment.clone(),
      service: options.service,
      remote_managed: options.remote_managed,
      extras: options.extras,
      sync_args: options.sync_args,
      expected_sync: None,
      detach: options.detach,
    };
    if options.dry_run {
      if !options.quiet {
        eprintln!("isolated local run: {}", serde_json::to_string(&request)?);
      }
      return Ok(());
    }
    return crate::node::run::apply_request_at(&request, &options.repo_root);
  }
  if options.service.is_some() {
    if options
      .service
      .as_ref()
      .is_some_and(|service| !service.inputs.is_empty())
    {
      return Err(ExpriError::Message("private input preparation requires a configured environment; add [environment], or download inputs manually and remove service.inputs".into()));
    }
    return Err(ExpriError::Message(
      "automatic publishing requires a configured environment; add [environment] or use --no-publish".into(),
    ));
  }
  if options.detach {
    return Err(ExpriError::Message(
      "--detach requires a configured environment".to_string(),
    ));
  }
  let argv = task_argv(&options.task, &options.args)?;
  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    eprintln!("task: {}", options.name);
    eprintln!("repo root: {}", options.repo_root.display());
  }
  if (options.dry_run || options.verbosity > 0) && !options.quiet {
    eprintln!(
      "+ cd {} && {}",
      shell::quote(options.repo_root.to_string_lossy()),
      shell::join(&argv)
    );
  }
  if options.dry_run {
    return Ok(());
  }
  let status = Command::new(&argv[0])
    .args(&argv[1..])
    .current_dir(&options.repo_root)
    .status()
    .map_err(|source| ExpriError::IoContext {
      action: "run task in",
      path: options.repo_root.display().to_string(),
      source,
    })?;
  if !status.success() {
    return Err(ExpriError::CommandFailed {
      program: argv[0].clone(),
      code: command_exit_code(&status),
    });
  }
  Ok(())
}

pub fn run_remote_task(options: RemoteTaskOptions) -> Result<()> {
  if let Some(environment) = &options.target.environment {
    let mut command = options.task.command.clone();
    command.extend(options.args.clone());
    let request = RunRequest {
      name: options.name.clone(),
      command,
      environment: environment.clone(),
      service: options.target.service.clone(),
      remote_managed: options.remote_managed,
      extras: options.extras,
      sync_args: options.sync_args,
      expected_sync: options.expected_sync,
      detach: options.detach,
    };
    let mut preference = ProtocolPreference::parse(options.target.protocol.as_deref())?;
    let node_bin = options
      .target
      .node_bin
      .clone()
      .unwrap_or_else(|| "expri".to_string());
    let remote = Remote::new(
      options.target,
      options.control_path,
      options.control_persist,
      options.dry_run,
      options.verbosity,
      options.quiet,
    )?
    .with_diagnostic_stdout(options.detach);
    remote.connect()?;
    if request.service.is_some() {
      require_service(&remote, preference, &node_bin, request.service.as_ref())?;
      // Never fall back to a protocol that silently ignores publishing intent.
      preference = ProtocolPreference::ExpriNode;
    }
    let request_dir = tempfile::Builder::new().prefix("expri-run-").tempdir()?;
    let request_path = request_dir.path().join("run-request.json");
    std::fs::write(&request_path, serde_json::to_vec(&request)?)?;
    let request_id = request_dir
      .path()
      .file_name()
      .expect("request directory name")
      .to_string_lossy();
    let inbox = format!("{}/inbox/{request_id}", remote.meta_dir());
    // Inbox creation needs no login profile; keep profile banners off the
    // detached start receipt's stdout before dispatching the run.
    remote.execute_stream(&format!("mkdir -p {inbox}"))?;
    remote.upload_file(&request_path, &format!("{inbox}/run-request.json"))?;
    let request_path = format!(".expri/inbox/{request_id}/run-request.json");
    if options.detach {
      return super::jobs::start_with_preference(&remote, &request_path, preference, &node_bin);
    }
    return apply_run_with_preference(&remote, &request_path, preference, &node_bin);
  }
  if options.target.service.is_some() {
    if options
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
  if options.detach {
    return Err(ExpriError::Message(
      "--detach requires a configured target environment".to_string(),
    ));
  }
  let argv = task_argv(&options.task, &options.args)?;
  let remote = Remote::new(
    options.target,
    options.control_path,
    options.control_persist,
    options.dry_run,
    options.verbosity,
    options.quiet,
  )?;
  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    eprintln!("task: {}", options.name);
    eprintln!("target: {}", options.target_name);
    eprintln!("repo root: {}", options.repo_root.display());
  }
  remote.connect()?;
  remote.execute(&format!(
    "cd {} && {}",
    remote.quoted_remote_dir(),
    shell::join(&argv)
  ))
}

fn task_argv(task: &TaskConfig, args: &[String]) -> Result<Vec<String>> {
  if task.command.is_empty() {
    return Err(ExpriError::Message(
      "task command must not be empty".to_string(),
    ));
  }
  let mut argv = Vec::new();
  if task.uv {
    argv.push("uv".to_string());
    argv.push("run".to_string());
  }
  argv.extend(task.command.iter().cloned());
  argv.extend(args.iter().cloned());
  Ok(argv)
}
