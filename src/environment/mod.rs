pub mod maintenance;
pub mod snapshot;

#[cfg(all(test, unix))]
mod integration_tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::config::EnvironmentConfig;
use crate::error::{ExpriError, Result, command_exit_code};

pub const RUNTIME_SCRIPT: &str = include_str!("runtime.py");

pub const ENV_REMOVE: &[&str] = &[
  "VIRTUAL_ENV",
  "PYTHONHOME",
  "PYTHONPATH",
  "UV_PROJECT",
  "UV_PROJECT_ENVIRONMENT",
  "UV_PYTHON",
  "UV_NO_SYNC",
  "UV_FROZEN",
  "UV_TARGET",
  "UV_PREFIX",
  "UV_SYSTEM_PYTHON",
  "UV_BREAK_SYSTEM_PACKAGES",
  "UV_ACTIVE",
  "UV_DIRECTORY",
  "UV_WORKING_DIRECTORY",
  "UV_LOCKED",
  "UV_NO_BUILD",
  "UV_NO_BUILD_ISOLATION",
  "UV_NO_BUILD_ISOLATION_PACKAGE",
  "UV_NO_INSTALL_PACKAGE",
  "UV_NO_INSTALL_PROJECT",
  "UV_NO_INSTALL_WORKSPACE",
  "UV_NO_INSTALL_LOCAL",
  "UV_NO_BINARY",
  "UV_NO_BINARY_PACKAGE",
  "UV_ONLY_BINARY",
  "UV_NO_EDITABLE",
  "UV_NO_CONFIG",
  "UV_CONFIG_FILE",
  "UV_ENV_FILE",
  "UV_NO_ENV_FILE",
  "UV_PYTHON_PLATFORM",
];

#[derive(Debug, Serialize)]
pub struct EnvironmentRequest {
  pub environment: EnvironmentConfig,
  pub repo_root: String,
  pub state_dir: String,
  pub operation: String,
  pub extras: Vec<String>,
  pub sync_args: Vec<String>,
  pub install_project: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub cache_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PreparedEnvironment {
  pub environment_path: String,
  pub python: String,
  pub manifest_path: String,
  #[serde(default)]
  pub run_env: BTreeMap<String, String>,
  #[serde(default)]
  pub env_remove: Vec<String>,
}

pub fn helper_argv(request: &EnvironmentRequest) -> Result<Vec<String>> {
  request.environment.validate()?;
  Ok(vec![
    "uv".to_string(),
    "run".to_string(),
    "--isolated".to_string(),
    "--no-project".to_string(),
    "--no-config".to_string(),
    "--no-env-file".to_string(),
    "--with".to_string(),
    "packaging==25.0".to_string(),
    "--with".to_string(),
    "tomli==2.2.1".to_string(),
    "python".to_string(),
    "-I".to_string(),
    "-c".to_string(),
    RUNTIME_SCRIPT.to_string(),
    serde_json::to_string(request)?,
  ])
}

fn invoke_helper(
  request: &EnvironmentRequest,
  logs: Option<&crate::run_logs::RunLogs>,
) -> Result<serde_json::Value> {
  let argv = helper_argv(request)?;
  let mut command = Command::new(&argv[0]);
  command
    .args(&argv[1..])
    .current_dir(&request.repo_root)
    .stderr(Stdio::inherit());
  for key in ENV_REMOVE {
    command.env_remove(key);
  }
  command.envs(&request.environment.env);
  if let Some(cache_dir) = &request.cache_dir {
    command.env("UV_CACHE_DIR", cache_dir);
  }
  if request
    .sync_args
    .iter()
    .any(|argument| argument == "--no-cache")
  {
    command.env("UV_NO_CACHE", "true");
  }
  let (status, stdout, log_error) = if let Some(logs) = logs {
    let output = logs.helper(&mut command)?;
    (output.status, output.stdout, output.log_error)
  } else {
    let output = command.output()?;
    (output.status, output.stdout, None)
  };
  if !status.success() {
    let program = if let Some(error) = log_error {
      format!("uv environment helper (log capture failed: {error})")
    } else {
      "uv environment helper".to_string()
    };
    return Err(ExpriError::CommandFailed {
      program,
      code: command_exit_code(&status),
    });
  }
  if let Some(error) = log_error {
    return Err(error.into());
  }
  Ok(serde_json::from_slice(&stdout)?)
}

pub fn prepare(request: &EnvironmentRequest) -> Result<PreparedEnvironment> {
  Ok(serde_json::from_value(invoke_helper(request, None)?)?)
}

pub fn prepare_logged(
  request: &EnvironmentRequest,
  logs: &crate::run_logs::RunLogs,
) -> Result<PreparedEnvironment> {
  Ok(serde_json::from_value(invoke_helper(request, Some(logs))?)?)
}

pub fn doctor(request: &EnvironmentRequest) -> Result<serde_json::Value> {
  invoke_helper(request, None)
}

/// Resolve cache paths before changing into a run's source snapshot.
pub fn cache_dir(repo_root: &Path, sync_args: &[String]) -> Result<PathBuf> {
  let repo_root = repo_root.canonicalize()?;
  let mut explicit = None;
  let mut arguments = sync_args.iter();
  while let Some(argument) = arguments.next() {
    if argument == "--cache-dir" {
      let value = arguments
        .next()
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| ExpriError::Message("--cache-dir requires a path".to_string()))?;
      explicit = Some(value.clone());
    } else if let Some(value) = argument.strip_prefix("--cache-dir=") {
      explicit = Some(value.to_string());
    }
  }
  let path = explicit
    .map(PathBuf::from)
    .or_else(|| std::env::var_os("UV_CACHE_DIR").map(PathBuf::from))
    .unwrap_or_else(|| repo_root.join(".expri/cache/uv"));
  if path.as_os_str().is_empty() {
    return Err(ExpriError::Message(
      "cache directory must not be empty".to_string(),
    ));
  }
  Ok(std::path::absolute(repo_root.join(path))?)
}

pub fn execution_env(
  environment: &EnvironmentConfig,
  prepared: &PreparedEnvironment,
) -> BTreeMap<String, String> {
  let mut env = environment.env.clone();
  env.extend(prepared.run_env.clone());
  env.insert("PYTHONNOUSERSITE".to_string(), "1".to_string());
  env.insert(
    "UV_PROJECT_ENVIRONMENT".to_string(),
    prepared.environment_path.clone(),
  );
  env
}

pub fn configure_command(
  command: &mut Command,
  environment: &EnvironmentConfig,
  prepared: &PreparedEnvironment,
) {
  for key in ENV_REMOVE {
    command.env_remove(key);
  }
  for key in &prepared.env_remove {
    command.env_remove(key);
  }
  command.envs(execution_env(environment, prepared));
}

pub fn task_argv(command: &[String]) -> Result<Vec<String>> {
  if command.is_empty() {
    return Err(ExpriError::Message(
      "task command must not be empty".to_string(),
    ));
  }
  let mut argv = vec![
    "uv".to_string(),
    "run".to_string(),
    "--no-sync".to_string(),
    "--no-env-file".to_string(),
    "--".to_string(),
  ];
  argv.extend_from_slice(command);
  Ok(argv)
}

pub fn setup_request(
  environment: &EnvironmentConfig,
  repo_root: &Path,
  state_dir: &Path,
  extras: &[String],
  sync_args: &[String],
) -> Result<EnvironmentRequest> {
  Ok(EnvironmentRequest {
    environment: environment.clone(),
    repo_root: repo_root.canonicalize()?.to_string_lossy().into_owned(),
    state_dir: state_dir.to_string_lossy().into_owned(),
    operation: "setup".to_string(),
    extras: extras.to_vec(),
    sync_args: sync_args.to_vec(),
    install_project: false,
    cache_dir: Some(
      cache_dir(repo_root, sync_args)?
        .to_string_lossy()
        .into_owned(),
    ),
  })
}
