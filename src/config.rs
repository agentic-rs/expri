use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

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
  #[serde(default)]
  pub environment: Option<EnvironmentConfig>,
  #[serde(default)]
  pub service: Option<RunServiceConfig>,
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
  #[serde(default)]
  pub transport: TransportKind,
  pub port: Option<u16>,
  pub protocol: Option<String>,
  pub node_bin: Option<String>,
  pub ctl_bin: Option<String>,
  pub ctl_method: Option<String>,
  #[serde(default)]
  pub environment: Option<EnvironmentConfig>,
  #[serde(default)]
  pub service: Option<RunServiceConfig>,
}

/// Safe publishing intent. Credentials are resolved on the executing worker.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunServiceConfig {
  pub client_config: PathBuf,
  pub project_id: String,
  pub origin: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub dashboard_url: Option<String>,
}

impl RunServiceConfig {
  pub fn validate(&self) -> Result<()> {
    if !self.client_config.is_absolute()
      || self
        .client_config
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
      return Err(ExpriError::Message(
        "service.client_config must be an absolute path on the executing worker without '..'"
          .into(),
      ));
    }
    for (name, value) in [("project_id", &self.project_id), ("origin", &self.origin)] {
      crate::service::validate_component(value)
        .map_err(|_| ExpriError::Message(format!("invalid service.{name}")))?;
    }
    if let Some(value) = &self.dashboard_url {
      let url = reqwest::Url::parse(value)
        .map_err(|_| ExpriError::Message("invalid service.dashboard_url".into()))?;
      if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.path() != "/"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
      {
        return Err(ExpriError::Message(
          "service.dashboard_url must be an HTTP(S) root URL without credentials, query, or fragment"
            .into(),
        ));
      }
    }
    Ok(())
  }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
  pub base_python: Option<String>,
  #[serde(default)]
  pub reuse_packages: Vec<String>,
  #[serde(default)]
  pub require_cuda: bool,
  #[serde(default)]
  pub env: BTreeMap<String, String>,
}

impl EnvironmentConfig {
  pub fn validate(&self) -> Result<()> {
    if let Some(base_python) = &self.base_python
      && base_python.trim().is_empty()
    {
      return Err(ExpriError::Message(
        "environment.base_python must not be empty".to_string(),
      ));
    }
    for package in &self.reuse_packages {
      if !valid_package_name(package) {
        return Err(ExpriError::Message(format!(
          "invalid environment.reuse_packages name: {package:?}; use a package name without versions or extras"
        )));
      }
    }
    if !self.reuse_packages.is_empty() && self.base_python.is_none() {
      return Err(ExpriError::Message(
        "environment.reuse_packages requires environment.base_python".to_string(),
      ));
    }
    if self.require_cuda
      && !self
        .reuse_packages
        .iter()
        .any(|package| normalize_package_name(package) == "torch")
    {
      return Err(ExpriError::Message(
        "environment.require_cuda requires torch in environment.reuse_packages".to_string(),
      ));
    }
    for name in self.env.keys() {
      if !valid_environment_name(name) {
        return Err(ExpriError::Message(format!(
          "invalid environment.env variable name: {name:?}; use a POSIX variable name"
        )));
      }
      if name.starts_with("UV_") || matches!(name.as_str(), "PYTHONHOME" | "VIRTUAL_ENV") {
        return Err(ExpriError::Message(format!(
          "environment.env cannot override expri's Python/environment selection: {name}"
        )));
      }
    }
    Ok(())
  }
}

pub fn normalize_package_name(name: &str) -> String {
  let mut normalized = String::with_capacity(name.len());
  let mut previous_separator = false;
  for character in name.chars() {
    if matches!(character, '-' | '_' | '.') {
      if !previous_separator {
        normalized.push('-');
      }
      previous_separator = true;
    } else {
      normalized.push(character.to_ascii_lowercase());
      previous_separator = false;
    }
  }
  normalized
}

fn valid_package_name(name: &str) -> bool {
  let bytes = name.as_bytes();
  !bytes.is_empty()
    && bytes[0].is_ascii_alphanumeric()
    && bytes[bytes.len() - 1].is_ascii_alphanumeric()
    && bytes
      .iter()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_environment_name(name: &str) -> bool {
  let mut bytes = name.bytes();
  bytes
    .next()
    .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
    && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
  #[default]
  Ssh,
  Ctl,
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
  pub remote_managed: Option<Vec<String>>,
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
  pub ignore: Vec<String>,
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
    config.local_environment()?;
    config.local_service()?;
    for name in config.target.keys() {
      config.target(name)?;
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
    let mut target = self
      .target
      .get(name)
      .cloned()
      .ok_or_else(|| ExpriError::Message(format!("unknown target: {name}")))?;
    if target.environment.is_none() {
      target.environment = self.environment.clone();
    }
    if target.service.is_none() {
      target.service = self.service.clone();
    }
    if let Some(environment) = &target.environment {
      environment
        .validate()
        .map_err(|error| ExpriError::Message(format!("target {name:?}: {error}")))?;
    }
    if let Some(service) = &target.service {
      service
        .validate()
        .map_err(|error| ExpriError::Message(format!("target {name:?}: {error}")))?;
    }
    Ok(target)
  }

  pub fn local_environment(&self) -> Result<Option<EnvironmentConfig>> {
    if let Some(environment) = &self.environment {
      environment.validate()?;
    }
    Ok(self.environment.clone())
  }

  pub fn local_service(&self) -> Result<Option<RunServiceConfig>> {
    if let Some(service) = &self.service {
      service.validate()?;
    }
    Ok(self.service.clone())
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
        sync.remote_managed.clone().unwrap_or_default(),
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

  pub fn download_ignore(&self) -> Vec<String> {
    self
      .download
      .as_ref()
      .map(|download| download.ignore.clone())
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
  fn publishing_config_inherits_or_overrides_whole_worker_scope() {
    let config: Config = toml::from_str(
      r#"
[service]
client_config = "/etc/expri/local.toml"
project_id = "vision"
origin = "local"
dashboard_url = "https://expri.example.net/"
[target.inherited]
host = "gpu.example"
remote_dir = "/srv/project"
[target.overridden]
host = "gpu-2.example"
remote_dir = "/srv/project"
[target.overridden.service]
client_config = "/etc/expri/worker.toml"
project_id = "vision"
origin = "gpu-2"
"#,
    )
    .unwrap();
    assert_eq!(config.local_service().unwrap(), config.service);
    assert_eq!(config.target("inherited").unwrap().service, config.service);
    let service = config.target("overridden").unwrap().service.unwrap();
    assert_eq!(service.origin, "gpu-2");
    assert_eq!(service.dashboard_url, None);
  }

  #[test]
  fn publishing_config_rejects_credentials_and_unsafe_references() {
    let base = RunServiceConfig {
      client_config: "/etc/expri/worker.toml".into(),
      project_id: "vision".into(),
      origin: "gpu-1".into(),
      dashboard_url: None,
    };
    assert!(base.validate().is_ok());
    for path in ["worker.toml", "/etc/../worker.toml"] {
      assert!(
        RunServiceConfig {
          client_config: path.into(),
          ..base.clone()
        }
        .validate()
        .is_err()
      );
    }
    for origin in ["", "..", "worker/1", "x\n"] {
      assert!(
        RunServiceConfig {
          origin: origin.into(),
          ..base.clone()
        }
        .validate()
        .is_err()
      );
    }
    for url in [
      "file:///tmp/dashboard",
      "https://token@example.net",
      "https://example.net/?token=x",
      "https://example.net/#x",
    ] {
      assert!(
        RunServiceConfig {
          dashboard_url: Some(url.into()),
          ..base.clone()
        }
        .validate()
        .is_err()
      );
    }
    let raw = "client_config='/etc/expri/worker.toml'\nproject_id='vision'\norigin='gpu-1'\ntoken='must-not-be-copied'";
    assert!(toml::from_str::<RunServiceConfig>(raw).is_err());
  }

  #[test]
  fn target_defaults_to_ssh_transport() {
    let target: TargetConfig = toml::from_str(
      r#"
host = "user@example.com"
remote_dir = "~/project"
"#,
    )
    .expect("parse target");

    assert_eq!(target.transport, TransportKind::Ssh);
    assert_eq!(target.ctl_bin, None);
    assert_eq!(target.ctl_method, None);
  }

  #[test]
  fn target_parses_ctl_transport_with_optional_settings() {
    let target: TargetConfig = toml::from_str(
      r#"
host = "work"
remote_dir = "~/project"
transport = "ctl"
ctl_bin = "/opt/ctl/bin/ctl"
ctl_method = "vpn"
port = 2222
protocol = "python"
node_bin = "./expri"
"#,
    )
    .expect("parse ctl target");

    assert_eq!(target.transport, TransportKind::Ctl);
    assert_eq!(target.host, "work");
    assert_eq!(target.ctl_bin.as_deref(), Some("/opt/ctl/bin/ctl"));
    assert_eq!(target.ctl_method.as_deref(), Some("vpn"));
    assert_eq!(target.port, Some(2222));
    assert_eq!(target.protocol.as_deref(), Some("python"));
    assert_eq!(target.node_bin.as_deref(), Some("./expri"));
  }

  #[test]
  fn target_allows_ctl_transport_without_optional_settings() {
    let target: TargetConfig = toml::from_str(
      r#"
host = "work"
remote_dir = "~/project"
transport = "ctl"
"#,
    )
    .expect("parse minimal ctl target");

    assert_eq!(target.transport, TransportKind::Ctl);
    assert_eq!(target.ctl_bin, None);
    assert_eq!(target.ctl_method, None);
  }

  #[test]
  fn target_rejects_unknown_transport() {
    let result = toml::from_str::<TargetConfig>(
      r#"
host = "work"
remote_dir = "~/project"
transport = "ct1"
"#,
    );

    let error = result.expect_err("unknown transport must fail");
    assert!(error.to_string().contains("unknown variant `ct1`"));
  }

  #[test]
  fn environment_defaults_to_uv_without_inherited_packages() {
    let environment: EnvironmentConfig = toml::from_str("").expect("parse empty environment");
    environment.validate().expect("validate empty environment");
    assert_eq!(environment.base_python, None);
    assert!(environment.reuse_packages.is_empty());
    assert!(!environment.require_cuda);
    assert!(environment.env.is_empty());
  }

  #[test]
  fn environment_validates_package_names_and_normalized_torch() {
    let environment: EnvironmentConfig = toml::from_str(
      r#"
base_python = "/opt/conda/bin/python"
reuse_packages = ["Torch", "nvidia-cuda-runtime-cu12", "some_package.name"]
require_cuda = true

[env]
LD_LIBRARY_PATH = "/opt/conda/lib"
CUDA_VISIBLE_DEVICES = "0"
"#,
    )
    .expect("parse inherited environment");
    environment
      .validate()
      .expect("validate inherited environment");
    assert_eq!(
      normalize_package_name("NVIDIA__Cuda.Runtime-CU12"),
      "nvidia-cuda-runtime-cu12"
    );
  }

  #[test]
  fn environment_rejects_unknown_settings() {
    let error = toml::from_str::<EnvironmentConfig>("reuse_package = [\"torch\"]")
      .expect_err("typo must fail");
    assert!(error.to_string().contains("unknown field `reuse_package`"));
  }

  #[test]
  fn environment_rejects_empty_base_python() {
    let environment: EnvironmentConfig =
      toml::from_str("base_python = '  '").expect("parse empty interpreter");
    let error = environment
      .validate()
      .expect_err("empty interpreter must fail");
    assert!(error.to_string().contains("base_python must not be empty"));
  }

  #[test]
  fn environment_requires_base_python_to_reuse_packages() {
    let environment: EnvironmentConfig =
      toml::from_str("reuse_packages = ['torch']").expect("parse reuse without interpreter");
    let error = environment
      .validate()
      .expect_err("reuse needs base interpreter");
    assert!(
      error
        .to_string()
        .contains("reuse_packages requires environment.base_python")
    );
  }

  #[test]
  fn environment_rejects_package_requirements_and_malformed_names() {
    for package in [
      "",
      "-torch",
      "torch-",
      "torch>=2",
      "torch[extra]",
      "torch; bad",
      "pýtorch",
    ] {
      let environment = EnvironmentConfig {
        base_python: Some("python".to_string()),
        reuse_packages: vec![package.to_string()],
        require_cuda: false,
        env: BTreeMap::new(),
      };
      let error = environment
        .validate()
        .expect_err("invalid package name must fail");
      assert!(
        error
          .to_string()
          .contains("invalid environment.reuse_packages name")
      );
    }
  }

  #[test]
  fn environment_requires_torch_when_cuda_is_required() {
    let environment: EnvironmentConfig =
      toml::from_str("base_python = 'python'\nreuse_packages = ['numpy']\nrequire_cuda = true")
        .expect("parse CUDA without torch");
    let error = environment
      .validate()
      .expect_err("CUDA requirement needs torch");
    assert!(error.to_string().contains("require_cuda requires torch"));
  }

  #[test]
  fn environment_rejects_non_posix_variable_names() {
    for name in ["", "1PATH", "LD-LIBRARY-PATH", "A=B", "日本語"] {
      let environment = EnvironmentConfig {
        base_python: None,
        reuse_packages: Vec::new(),
        require_cuda: false,
        env: BTreeMap::from([(name.to_string(), "value".to_string())]),
      };
      let error = environment
        .validate()
        .expect_err("invalid variable name must fail");
      assert!(
        error
          .to_string()
          .contains("invalid environment.env variable name")
      );
    }
  }

  #[test]
  fn environment_rejects_environment_selection_overrides() {
    for name in [
      "UV_PROJECT_ENVIRONMENT",
      "UV_PYTHON",
      "UV_NEW_OPTION",
      "PYTHONHOME",
      "VIRTUAL_ENV",
    ] {
      let environment = EnvironmentConfig {
        base_python: None,
        reuse_packages: Vec::new(),
        require_cuda: false,
        env: BTreeMap::from([(name.to_string(), "value".to_string())]),
      };
      let error = environment
        .validate()
        .expect_err("environment selection must be owned by expri");
      assert!(
        error
          .to_string()
          .contains("cannot override expri's Python/environment selection")
      );
    }
  }

  #[test]
  fn target_inherits_whole_environment_or_overrides_it() {
    let config: Config = toml::from_str(
      r#"
[environment]
base_python = "/opt/conda/bin/python"
reuse_packages = ["torch"]
require_cuda = true

[target.inherited]
host = "gpu.example"
remote_dir = "~/project"

[target.overridden]
host = "cpu.example"
remote_dir = "~/project"

[target.overridden.environment]
"#,
    )
    .expect("parse defaults and overrides");
    let inherited = config.target("inherited").expect("inherit environment");
    assert_eq!(inherited.environment, config.environment);
    let overridden = config.target("overridden").expect("override environment");
    let environment = overridden.environment.expect("explicit empty environment");
    assert!(environment.reuse_packages.is_empty());
    assert_eq!(environment.base_python, None);
    assert!(!environment.require_cuda);
  }

  #[test]
  fn direct_config_access_validates_environment() {
    let config: Config = toml::from_str(
      r#"
[environment]
base_python = ""

[target.gpu]
host = "gpu.example"
remote_dir = "~/project"
"#,
    )
    .expect("deserialize invalid environment for validation");
    assert!(config.local_environment().is_err());
    assert!(config.target("gpu").is_err());
  }

  #[test]
  fn load_validates_target_environment_from_sibling_file() {
    let temp_dir = tempfile::Builder::new()
      .prefix("expri-config-")
      .tempdir()
      .expect("create temp dir");
    let config_path = temp_dir.path().join("expri.toml");
    fs::write(&config_path, "").expect("write project config");
    fs::write(
      temp_dir.path().join("expri.target.toml"),
      r#"
[target.gpu]
host = "gpu.example"
remote_dir = "~/project"

[target.gpu.environment]
reuse_packages = ["torch"]
"#,
    )
    .expect("write invalid target config");
    let error = Config::load(&config_path).expect_err("invalid target must fail on load");
    assert!(error.to_string().contains("target \"gpu\""));
    assert!(
      error
        .to_string()
        .contains("reuse_packages requires environment.base_python")
    );
  }

  #[test]
  fn environment_can_round_trip_through_serialization() {
    let environment: EnvironmentConfig = toml::from_str(
      "base_python = '/opt/conda/bin/python'\nreuse_packages = ['torch']\nrequire_cuda = true",
    )
    .expect("parse environment");
    let encoded = serde_json::to_string(&environment).expect("serialize environment");
    let decoded: EnvironmentConfig =
      serde_json::from_str(&encoded).expect("deserialize environment");
    assert_eq!(decoded, environment);
  }

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
