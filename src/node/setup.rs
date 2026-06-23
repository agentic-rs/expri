use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

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
  let state_dir = Path::new(&request.state_dir);
  fs::create_dir_all(state_dir).map_err(|source| ExpriError::IoContext {
    action: "create directory",
    path: state_dir.display().to_string(),
    source,
  })?;
  for step in &request.steps {
    run_step(step)?;
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

fn run_step(step: &SetupStep) -> Result<()> {
  match step {
    SetupStep::Uv { extras, args } => {
      let mut command_args = vec!["sync".to_string()];
      for extra in extras {
        command_args.push("--extra".to_string());
        command_args.push(extra.clone());
      }
      command_args.extend(args.iter().cloned());
      run_command("uv", command_args)
    }
    SetupStep::Hf {
      repo,
      revision,
      args,
    } => {
      let mut command_args = vec![
        "run".to_string(),
        "hf".to_string(),
        "download".to_string(),
        repo.clone(),
      ];
      if let Some(revision) = revision {
        command_args.push("--revision".to_string());
        command_args.push(revision.clone());
      }
      command_args.extend(args.iter().cloned());
      run_command("uv", command_args)
    }
    SetupStep::Script { path, args } => {
      let path = PathBuf::from(path);
      validate_relative_path(&path)?;
      let mut command_args = vec![path.to_string_lossy().to_string()];
      command_args.extend(args.iter().cloned());
      run_command("bash", command_args)
    }
  }
}

fn run_command(program: &str, args: Vec<String>) -> Result<()> {
  let status = Command::new(program).args(args).status()?;
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
