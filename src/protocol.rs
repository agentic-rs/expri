use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct SyncApplyRequest {
  pub head: String,
  pub remote_url: Option<String>,
  pub source_bundle: Option<String>,
  pub source_bundle_sha256: Option<String>,
  pub patch: String,
  pub patch_sha256: String,
  pub state_dir: String,
  pub force: bool,
}
