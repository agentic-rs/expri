use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct SyncApplyRequest {
  pub head: String,
  pub source_bundle: String,
  pub source_bundle_sha256: String,
  pub patch: String,
  pub patch_sha256: String,
  pub state_dir: String,
  pub force: bool,
}
