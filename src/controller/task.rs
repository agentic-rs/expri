use std::path::PathBuf;
use std::process::Command;

use crate::config::{EnvironmentConfig, TargetConfig, TaskConfig};
use crate::controller::protocol::{ProtocolPreference, apply_run_with_preference};
use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result};
use crate::protocol::{RunRequest, SyncIdentity};
use crate::shell;

pub struct LocalTaskOptions {
  pub repo_root: PathBuf,
  pub project_name: Option<String>,
  pub name: String,
  pub task: TaskConfig,
  pub args: Vec<String>,
  pub dry_run: bool,
  pub verbosity: u8,
  pub quiet: bool,
  pub environment: Option<EnvironmentConfig>,
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
  pub verbosity: u8,
  pub quiet: bool,
  pub remote_managed: Vec<String>,
  pub extras: Vec<String>,
  pub sync_args: Vec<String>,
  pub expected_sync: Option<SyncIdentity>,
}

pub fn run_local_task(options: LocalTaskOptions) -> Result<()> {
  if let Some(environment) = &options.environment {
    let mut command = options.task.command.clone();
    command.extend(options.args.clone());
    let request = RunRequest {
      name: options.name.clone(),
      command,
      environment: environment.clone(),
      remote_managed: options.remote_managed,
      extras: options.extras,
      sync_args: options.sync_args,
      expected_sync: None,
    };
    if options.dry_run {
      if !options.quiet {
        eprintln!("isolated local run: {}", serde_json::to_string(&request)?);
      }
      return Ok(());
    }
    return crate::node::run::apply_request_at(&request, &options.repo_root);
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
      code: status.code(),
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
      remote_managed: options.remote_managed,
      extras: options.extras,
      sync_args: options.sync_args,
      expected_sync: options.expected_sync,
    };
    let preference = ProtocolPreference::parse(options.target.protocol.as_deref())?;
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
    )?;
    remote.connect()?;
    let request_dir = tempfile::Builder::new().prefix("expri-run-").tempdir()?;
    let request_path = request_dir.path().join("run-request.json");
    std::fs::write(&request_path, serde_json::to_vec(&request)?)?;
    let request_id = request_dir
      .path()
      .file_name()
      .expect("request directory name")
      .to_string_lossy();
    let inbox = format!("{}/inbox/{request_id}", remote.meta_dir());
    remote.execute(&format!("mkdir -p {inbox}"))?;
    remote.upload_file(&request_path, &format!("{inbox}/run-request.json"))?;
    return apply_run_with_preference(
      &remote,
      &format!(".expri/inbox/{request_id}/run-request.json"),
      preference,
      &node_bin,
    );
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
