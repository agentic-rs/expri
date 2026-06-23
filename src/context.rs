use std::path::PathBuf;

use crate::config::{Config, TargetConfig};
use crate::error::{ExpriError, Result};

pub struct CommandContext {
  pub config: Config,
  pub repo_root: PathBuf,
  pub project_name: Option<String>,
}

pub struct TargetCommandContext {
  pub config: Config,
  pub repo_root: PathBuf,
  pub project_name: Option<String>,
  pub target_name: String,
  pub target: TargetConfig,
  pub control_path: String,
}

impl CommandContext {
  pub fn load(config_path: Option<PathBuf>, repo_root: Option<PathBuf>) -> Result<Self> {
    let config_path = resolve_config_path(config_path)?;
    let config = Config::load(&config_path)?;
    let project_name = config.project_name().map(str::to_string);
    let repo_root = resolve_repo_root(repo_root, &config_path)?;
    Ok(Self {
      config,
      repo_root,
      project_name,
    })
  }

  pub fn into_target(
    self,
    requested_target: Option<&str>,
    control_path: Option<String>,
  ) -> Result<TargetCommandContext> {
    let target_name = self.config.resolve_target_name(requested_target)?;
    let target = self.config.target(&target_name)?;
    let control_path = control_path
      .or_else(|| {
        self
          .config
          .ssh
          .as_ref()
          .and_then(|ssh| ssh.control_path.clone())
      })
      .unwrap_or_else(default_control_path);

    Ok(TargetCommandContext {
      config: self.config,
      repo_root: self.repo_root,
      project_name: self.project_name,
      target_name,
      target,
      control_path,
    })
  }
}

fn resolve_config_path(config_path: Option<PathBuf>) -> Result<PathBuf> {
  let config_path = config_path.unwrap_or_else(|| PathBuf::from("expri.toml"));
  if config_path.is_absolute() {
    Ok(config_path)
  } else {
    Ok(std::env::current_dir()?.join(config_path))
  }
}

fn resolve_repo_root(repo_root: Option<PathBuf>, config_path: &std::path::Path) -> Result<PathBuf> {
  match repo_root {
    Some(path) if path.is_absolute() => Ok(path),
    Some(path) => Ok(std::env::current_dir()?.join(path)),
    None => config_path
      .parent()
      .ok_or_else(|| ExpriError::Message("config path has no parent".to_string()))
      .map(|path| path.to_path_buf()),
  }
}

fn default_control_path() -> String {
  let value = "~/.ssh/cm-%r@%h:%p";
  match std::env::var("HOME") {
    Ok(home) => value.replacen('~', &home, 1),
    Err(_) => value.to_string(),
  }
}
