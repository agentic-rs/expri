use serde::{Deserialize, Serialize};

use crate::config::{EnvironmentConfig, RunServiceConfig};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SetupStep {
  Uv {
    #[serde(default)]
    extras: Vec<String>,
    #[serde(default)]
    args: Vec<String>,
  },
  Hf {
    repo: String,
    revision: Option<String>,
    #[serde(default)]
    args: Vec<String>,
  },
  Script {
    path: String,
    #[serde(default)]
    args: Vec<String>,
  },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SetupRequest {
  pub state_dir: String,
  pub force: bool,
  pub steps: Vec<SetupStep>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub environment: Option<EnvironmentConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RunRequest {
  pub name: String,
  pub command: Vec<String>,
  pub environment: EnvironmentConfig,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub service: Option<RunServiceConfig>,
  #[serde(default)]
  pub detach: bool,
  #[serde(default)]
  pub remote_managed: Vec<String>,
  #[serde(default)]
  pub extras: Vec<String>,
  #[serde(default)]
  pub sync_args: Vec<String>,
  #[serde(default)]
  pub expected_sync: Option<SyncIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum JobRequest {
  Status {
    run_id: String,
  },
  Logs {
    run_id: String,
    #[serde(default = "default_log_stream")]
    stream: String,
    #[serde(default)]
    follow: bool,
    #[serde(default = "default_log_tail")]
    tail: usize,
  },
  Cancel {
    run_id: String,
  },
}

fn default_log_stream() -> String {
  "stdout".to_string()
}

fn default_log_tail() -> usize {
  100
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum RunQueryRequest {
  List {
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
  },
  Show {
    run_id: String,
  },
  Files {
    run_id: String,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    metrics: bool,
  },
}

fn is_false(value: &bool) -> bool {
  !*value
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EnvironmentCommandRequest {
  #[serde(default)]
  pub json: bool,
  #[serde(flatten)]
  pub action: EnvironmentAction,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum EnvironmentAction {
  Doctor(DoctorRequest),
  Prune(PruneRequest),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DoctorRequest {
  pub environment: EnvironmentConfig,
  #[serde(default)]
  pub extras: Vec<String>,
  #[serde(default)]
  pub sync_args: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PruneRequest {
  pub apply: bool,
  pub keep_last: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SyncIdentity {
  pub head: String,
  pub patch_sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SyncApplyRequest {
  pub head: String,
  pub remote_url: Option<String>,
  pub source_bundle: Option<String>,
  pub source_bundle_sha256: Option<String>,
  pub patch: String,
  pub patch_sha256: String,
  pub state_dir: String,
  #[serde(default)]
  pub remote_managed: Vec<String>,
  pub force: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PullArtifacts {
  pub head: String,
  pub source_bundle: String,
  pub source_bundle_sha256: String,
  pub patch: String,
  pub patch_sha256: String,
  pub state_dir: String,
}
