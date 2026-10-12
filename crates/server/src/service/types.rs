use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use expri_core::service::*;

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
  pub owner_token_env: String,
  #[serde(default)]
  pub workers: Vec<WorkerAuth>,
  #[serde(default)]
  pub machines: Vec<MachineAuth>,
  #[serde(default)]
  pub dashboard: Option<DashboardConfig>,
  pub storage: super::storage::S3Config,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DashboardConfig {
  pub public_url: String,
  pub password_env: String,
  /// Allow password-confirmed project deletion on the primary dashboard.
  #[serde(default)]
  pub allow_project_deletion: bool,
  #[serde(default)]
  pub previews: Vec<DashboardPreviewConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DashboardPreviewConfig {
  pub public_url: String,
  pub assets_dir: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkerAuth {
  pub project_id: String,
  pub origin: String,
  pub token_env: String,
}

/// A telemetry token authorizes exactly one physical machine, independently of runs.
#[derive(Clone, Debug, Deserialize)]
pub struct MachineAuth {
  pub machine_id: String,
  pub token_env: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ArchiveSnapshot {
  pub scope: RunScope,
  pub files: Vec<FileRecord>,
  pub incomplete: bool,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn existing_dashboard_configuration_defaults_to_no_previews() {
    let config: ServerConfig = toml::from_str(
      r#"
owner_token_env = "EXPRI_OWNER_TOKEN"
[storage]
bucket = "experiments"
region = "us-east-1"
[dashboard]
public_url = "https://expri.example.test"
password_env = "EXPRI_DASHBOARD_PASSWORD"
"#,
    )
    .unwrap();
    assert!(config.machines.is_empty());
    assert!(config.dashboard.unwrap().previews.is_empty());
    let config: ServerConfig = toml::from_str(
      r#"
owner_token_env = "EXPRI_OWNER_TOKEN"
[storage]
bucket = "experiments"
region = "us-east-1"
[dashboard]
public_url = "https://expri.example.test"
password_env = "EXPRI_DASHBOARD_PASSWORD"
[[dashboard.previews]]
public_url = "https://preview.example.test"
assets_dir = "/opt/expri/preview"
"#,
    )
    .unwrap();
    let dashboard = config.dashboard.unwrap();
    assert_eq!(dashboard.previews.len(), 1);
    assert_eq!(
      dashboard.previews[0].assets_dir,
      PathBuf::from("/opt/expri/preview")
    );
  }

  #[test]
  fn machine_token_configuration_is_independent_of_projects() {
    let config: ServerConfig = toml::from_str(
      r#"
owner_token_env = "EXPRI_OWNER_TOKEN"
[[machines]]
machine_id = "machine-123"
token_env = "EXPRI_MACHINE_TOKEN"
[storage]
bucket = "experiments"
region = "us-east-1"
"#,
    )
    .unwrap();
    assert!(config.workers.is_empty());
    assert_eq!(config.machines.len(), 1);
    assert_eq!(config.machines[0].machine_id, "machine-123");
    assert_eq!(config.machines[0].token_env, "EXPRI_MACHINE_TOKEN");
  }
}
