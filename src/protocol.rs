use serde::{Deserialize, Serialize};

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
