use std::path::PathBuf;
use std::process::Command;

use crate::config::{TargetConfig, TaskConfig};
use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result};
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
}

pub fn run_local_task(options: LocalTaskOptions) -> Result<()> {
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
  let argv = task_argv(&options.task, &options.args)?;
  let remote = Remote::new(
    options.target,
    options.control_path,
    options.control_persist,
    options.dry_run,
    options.verbosity,
    options.quiet,
  );
  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    eprintln!("task: {}", options.name);
    eprintln!("target: {}", options.target_name);
    eprintln!("repo root: {}", options.repo_root.display());
  }
  let _opened_master = remote.open_master()?;
  remote.ssh(&format!(
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
