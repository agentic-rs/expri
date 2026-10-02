use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::environment::{self, PreparedEnvironment};
use crate::error::{ExpriError, Result};
use crate::protocol::{SetupRequest, SetupStep};

pub fn apply_request_file(path: &Path) -> Result<()> {
  let raw = fs::read_to_string(path).map_err(|source| ExpriError::IoContext {
    action: "read",
    path: path.display().to_string(),
    source,
  })?;
  let request = serde_json::from_str(&raw)?;
  apply_request(&request)
}

pub fn apply_request(request: &SetupRequest) -> Result<()> {
  apply_request_at(request, &std::env::current_dir()?)
}

pub fn apply_request_at(request: &SetupRequest, repo_root: &Path) -> Result<()> {
  if let Some(config) = &request.environment {
    config.validate()?;
  }
  let repo_root = repo_root.canonicalize()?;
  let state_dir = repo_root.join(&request.state_dir);
  fs::create_dir_all(&state_dir).map_err(|source| ExpriError::IoContext {
    action: "create directory",
    path: state_dir.display().to_string(),
    source,
  })?;
  let mut prepared = None;
  for step in &request.steps {
    if let (Some(config), SetupStep::Uv { extras, args }) = (&request.environment, step) {
      prepared = Some(environment::prepare(&environment::setup_request(
        config, &repo_root, &state_dir, extras, args,
      )?)?);
      continue;
    }
    if prepared.is_none()
      && matches!(step, SetupStep::Hf { .. })
      && let Some(config) = &request.environment
    {
      prepared = Some(environment::prepare(&environment::setup_request(
        config,
        &repo_root,
        &state_dir,
        &[],
        &[],
      )?)?);
    }
    run_step(step, &repo_root, request, prepared.as_ref())?;
  }
  if prepared.is_none()
    && let Some(config) = &request.environment
  {
    environment::prepare(&environment::setup_request(
      config,
      &repo_root,
      &state_dir,
      &[],
      &[],
    )?)?;
  }
  fs::write(
    state_dir.join("setup-state.json"),
    serde_json::to_string_pretty(request)?,
  )
  .map_err(|source| ExpriError::IoContext {
    action: "write",
    path: state_dir.join("setup-state.json").display().to_string(),
    source,
  })?;
  Ok(())
}

fn run_step(
  step: &SetupStep,
  repo_root: &Path,
  request: &SetupRequest,
  prepared: Option<&PreparedEnvironment>,
) -> Result<()> {
  match step {
    SetupStep::Uv { extras, args } => {
      let mut command_args = vec!["sync".to_string()];
      for extra in extras {
        command_args.push("--extra".to_string());
        command_args.push(extra.clone());
      }
      command_args.extend(args.iter().cloned());
      run_command("uv", command_args, repo_root, request, prepared)
    }
    SetupStep::Hf {
      repo,
      revision,
      args,
    } => {
      let mut command_args = vec!["run".to_string()];
      if prepared.is_some() {
        command_args.push("--no-sync".to_string());
        command_args.push("--no-env-file".to_string());
      }
      command_args.extend(["hf".to_string(), "download".to_string(), repo.clone()]);
      if let Some(revision) = revision {
        command_args.push("--revision".to_string());
        command_args.push(revision.clone());
      }
      command_args.extend(args.iter().cloned());
      run_command("uv", command_args, repo_root, request, prepared)
    }
    SetupStep::Script { path, args } => {
      let path = PathBuf::from(path);
      validate_relative_path(&path)?;
      let mut command_args = vec![path.to_string_lossy().to_string()];
      command_args.extend(args.iter().cloned());
      if prepared.is_some() {
        let mut command = vec!["bash".to_string()];
        command.extend(command_args);
        let argv = environment::task_argv(&command)?;
        run_command(&argv[0], argv[1..].to_vec(), repo_root, request, prepared)
      } else {
        run_command("bash", command_args, repo_root, request, prepared)
      }
    }
  }
}

fn run_command(
  program: &str,
  args: Vec<String>,
  repo_root: &Path,
  request: &SetupRequest,
  prepared: Option<&PreparedEnvironment>,
) -> Result<()> {
  let mut command = Command::new(program);
  command.args(args).current_dir(repo_root);
  if let Some(config) = &request.environment {
    command.envs(&config.env);
    if let Some(prepared) = prepared {
      environment::configure_command(&mut command, config, prepared);
    }
  }
  let status = command.status()?;
  if !status.success() {
    return Err(ExpriError::CommandFailed {
      program: program.to_string(),
      code: status.code(),
    });
  }
  Ok(())
}

fn validate_relative_path(path: &Path) -> Result<()> {
  if !path.is_relative()
    || path
      .components()
      .any(|component| !matches!(component, Component::Normal(_)))
  {
    return Err(ExpriError::Message(format!(
      "unsafe setup script path: {}",
      path.display()
    )));
  }
  Ok(())
}
