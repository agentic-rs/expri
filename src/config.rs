use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::error::{ExpriError, Result};
use crate::filter::{DEFAULT_EXCLUDED_DIRS, DEFAULT_EXCLUDED_FILES, SyncRules};
use crate::protocol::SetupStep;

#[derive(Debug, Deserialize)]
pub struct Config {
  pub project: Option<ProjectConfig>,
  pub ssh: Option<SshConfig>,
  #[serde(default)]
  pub target: BTreeMap<String, TargetConfig>,
  pub sync: Option<SyncConfig>,
  pub setup: Option<SetupConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ProjectConfig {
  pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SshConfig {
  pub control_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TargetConfig {
  pub host: String,
  pub remote_dir: String,
  pub port: Option<u16>,
  pub protocol: Option<String>,
  pub node_bin: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SyncConfig {
  pub exclude_dirs: Option<Vec<String>>,
  pub exclude_files: Option<Vec<String>>,
  pub include_ignored: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct SetupConfig {
  #[serde(default)]
  pub steps: Vec<SetupStep>,
}

impl Config {
  pub fn load(path: &Path) -> Result<Self> {
    let raw = fs::read_to_string(path).map_err(|source| ExpriError::IoContext {
      action: "read",
      path: path.display().to_string(),
      source,
    })?;
    Ok(toml::from_str(&raw)?)
  }

  pub fn project_name(&self) -> Option<&str> {
    self
      .project
      .as_ref()
      .and_then(|project| project.name.as_deref())
  }

  pub fn resolve_target_name(&self, requested: Option<&str>) -> Result<String> {
    if let Some(name) = requested {
      if self.target.contains_key(name) {
        return Ok(name.to_string());
      }
      return Err(ExpriError::Message(format!("unknown target: {name}")));
    }
    if self.target.len() == 1 {
      return Ok(self.target.keys().next().expect("one target").clone());
    }
    Err(ExpriError::Message(
      "target is required when expri.toml defines zero or multiple targets".to_string(),
    ))
  }

  pub fn target(&self, name: &str) -> Result<TargetConfig> {
    self
      .target
      .get(name)
      .cloned()
      .ok_or_else(|| ExpriError::Message(format!("unknown target: {name}")))
  }

  pub fn sync_rules(&self) -> Result<SyncRules> {
    let sync = self.sync.as_ref();
    SyncRules::new(
      sync
        .and_then(|sync| sync.exclude_dirs.clone())
        .unwrap_or_else(|| {
          DEFAULT_EXCLUDED_DIRS
            .iter()
            .map(ToString::to_string)
            .collect()
        }),
      sync
        .and_then(|sync| sync.exclude_files.clone())
        .unwrap_or_else(|| {
          DEFAULT_EXCLUDED_FILES
            .iter()
            .map(ToString::to_string)
            .collect()
        }),
      sync
        .and_then(|sync| sync.include_ignored.clone())
        .unwrap_or_default(),
    )
  }

  pub fn setup_steps(&self) -> Vec<SetupStep> {
    self
      .setup
      .as_ref()
      .map(|setup| setup.steps.clone())
      .unwrap_or_default()
  }
}
