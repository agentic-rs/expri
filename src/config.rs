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
  #[serde(default)]
  pub tasks: BTreeMap<String, TaskDefinition>,
  pub sync: Option<SyncConfig>,
  pub setup: Option<SetupConfig>,
  pub download: Option<DownloadConfig>,
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

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum TaskDefinition {
  Command(Vec<String>),
  Options(TaskOptionsConfig),
}

#[derive(Clone, Debug, Deserialize)]
pub struct TaskOptionsConfig {
  pub command: Vec<String>,
  #[serde(default)]
  pub uv: bool,
}

#[derive(Clone, Debug)]
pub struct TaskConfig {
  pub command: Vec<String>,
  pub uv: bool,
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

#[derive(Clone, Debug, Deserialize)]
pub struct DownloadConfig {
  #[serde(default)]
  pub results_dir: Option<String>,
  #[serde(default)]
  pub mappings: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct DownloadMapping {
  pub name: String,
  pub remote_path: String,
  pub local_path: String,
}

impl Config {
  pub fn load(path: &Path) -> Result<Self> {
    let mut config = Self::load_file(path)?;
    let target_path = target_config_path(path)?;
    if target_path.exists() {
      let target_config = Self::load_file(&target_path)?;
      config.target.extend(target_config.target);
    }
    Ok(config)
  }

  fn load_file(path: &Path) -> Result<Self> {
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

  pub fn task(&self, name: &str) -> Result<TaskConfig> {
    self
      .tasks
      .get(name)
      .map(TaskConfig::from)
      .ok_or_else(|| ExpriError::Message(format!("unknown task: {name}")))
  }

  pub fn sync_rules(&self) -> Result<SyncRules> {
    let sync = self.sync.as_ref();
    match sync {
      Some(sync) => SyncRules::new(
        sync.exclude_dirs.clone().unwrap_or_else(|| {
          DEFAULT_EXCLUDED_DIRS
            .iter()
            .map(ToString::to_string)
            .collect()
        }),
        sync.exclude_files.clone().unwrap_or_else(|| {
          DEFAULT_EXCLUDED_FILES
            .iter()
            .map(ToString::to_string)
            .collect()
        }),
        sync.include_ignored.clone().unwrap_or_default(),
      ),
      None => SyncRules::defaults(),
    }
  }

  pub fn setup_steps(&self) -> Vec<SetupStep> {
    self
      .setup
      .as_ref()
      .map(|setup| setup.steps.clone())
      .unwrap_or_default()
  }

  pub fn download_results_dir(&self) -> String {
    self
      .download
      .as_ref()
      .and_then(|download| download.results_dir.clone())
      .unwrap_or_else(|| "results".to_string())
  }

  pub fn download_mappings(&self) -> Vec<DownloadMapping> {
    self
      .download
      .as_ref()
      .map(|download| {
        download
          .mappings
          .iter()
          .map(|(local_path, remote_path)| DownloadMapping {
            name: local_path.clone(),
            remote_path: remote_path.clone(),
            local_path: local_path.clone(),
          })
          .collect()
      })
      .unwrap_or_default()
  }
}

impl From<&TaskDefinition> for TaskConfig {
  fn from(definition: &TaskDefinition) -> Self {
    match definition {
      TaskDefinition::Command(command) => Self {
        command: command.clone(),
        uv: false,
      },
      TaskDefinition::Options(options) => Self {
        command: options.command.clone(),
        uv: options.uv,
      },
    }
  }
}

fn target_config_path(path: &Path) -> Result<std::path::PathBuf> {
  let stem = path
    .file_stem()
    .and_then(|stem| stem.to_str())
    .ok_or_else(|| {
      ExpriError::Message(format!("config path has no file stem: {}", path.display()))
    })?;
  Ok(path.with_file_name(format!("{stem}.target.toml")))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn load_merges_sibling_target_file() {
    let temp_dir = tempfile::Builder::new()
      .prefix("expri-config-")
      .tempdir()
      .expect("create temp dir");
    let config_path = temp_dir.path().join("cs336.toml");
    let target_path = temp_dir.path().join("cs336.target.toml");
    fs::write(
      &config_path,
      r#"
[project]
name = "demo"

[target.shared]
host = "shared.example"
remote_dir = "~/shared"

[download.mappings]
wandb = "wandb"

[tasks]
dev = ["pnpm", "dev"]
train = { command = ["python", "scripts/train.py"], uv = true }
"#,
    )
    .expect("write config");
    fs::write(
      &target_path,
      r#"
[target.local]
host = "local.example"
remote_dir = "~/local"

[target.shared]
host = "override.example"
remote_dir = "~/override"

[download.mappings]
ignored = "ignored"

[tasks]
ignored = ["false"]
"#,
    )
    .expect("write target config");

    let config = Config::load(&config_path).expect("load config");

    assert_eq!(config.project_name(), Some("demo"));
    assert_eq!(
      config.target("local").expect("local target").host,
      "local.example"
    );
    assert_eq!(
      config.target("shared").expect("shared target").host,
      "override.example"
    );
    assert_eq!(config.download_mappings().len(), 1);
    assert_eq!(
      config.task("dev").expect("dev task").command,
      ["pnpm", "dev"]
    );
    let train = config.task("train").expect("train task");
    assert_eq!(train.command, ["python", "scripts/train.py"]);
    assert!(train.uv);
    assert!(config.task("ignored").is_err());
  }
}
