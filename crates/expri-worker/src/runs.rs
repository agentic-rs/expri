//! Compose persisted run inspection with the executing publisher's status.

use std::path::Path;

use serde_json::{Value, json};

use crate::error::Result;
use crate::protocol::RunQueryRequest;

pub use expri_run_data::runs::{
  CATALOG_SCRIPT, summary_directory, summary_from_state, validate_id,
};

pub fn query(repo_root: &Path, request: &RunQueryRequest) -> Result<Value> {
  let report = expri_run_data::runs::query(repo_root, request)?;
  with_publishing(report, &repo_root.join(".expri/runs"), request)
}

pub fn query_directory(runs_dir: &Path, request: &RunQueryRequest) -> Result<Value> {
  let report = expri_run_data::runs::query_directory(runs_dir, request)?;
  with_publishing(report, runs_dir, request)
}

fn with_publishing(mut report: Value, runs_dir: &Path, request: &RunQueryRequest) -> Result<Value> {
  let RunQueryRequest::Show { run_id } = request else {
    return Ok(report);
  };
  let publishing = match crate::service::publishing::status(&runs_dir.join(run_id)) {
    Ok(publishing) => publishing,
    Err(_) => {
      let detail = "Service publishing status could not be read.";
      report["warnings"]
        .as_array_mut()
        .expect("run inspection returns a warnings array")
        .push(json!({"run_id": run_id, "message": detail}));
      Some(json!({"status": "error", "last_error": detail}))
    }
  };
  if let Some(publishing) = publishing {
    report["publishing"] = publishing;
  }
  Ok(report)
}
