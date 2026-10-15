use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
  #[serde(default, skip_serializing_if = "is_false")]
  pub smoke: bool,
  #[serde(default)]
  pub remote_managed: Vec<String>,
  #[serde(default)]
  pub extras: Vec<String>,
  #[serde(default)]
  pub sync_args: Vec<String>,
  #[serde(default)]
  pub expected_sync: Option<SyncIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SupervisorRestartRequest {
  pub request_id: String,
  pub reason: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub expected_generation: Option<u64>,
  /// Executable on the training machine, never a controller-side upload.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub binary: Option<PathBuf>,
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
  RestartSupervisor {
    run_id: String,
    restart: SupervisorRestartRequest,
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn supervisor_restart_request_round_trips_with_strict_options() {
    let request = serde_json::json!({
      "operation": "restart_supervisor", "run_id": "run-123",
      "restart": {"request_id": "restart-123", "reason": "upgrade supervisor"},
    });
    let JobRequest::RestartSupervisor { run_id, restart } =
      serde_json::from_value::<JobRequest>(request.clone()).unwrap()
    else {
      panic!("expected restart supervisor request");
    };
    assert_eq!(run_id, "run-123");
    assert_eq!(restart.expected_generation, None);
    assert_eq!(restart.binary, None);
    assert_eq!(
      serde_json::to_value(JobRequest::RestartSupervisor { run_id, restart }).unwrap(),
      request
    );
    let mut explicit = request["restart"].clone();
    explicit["expected_generation"] = serde_json::json!(2);
    explicit["binary"] = serde_json::json!("/opt/expri next/bin/expri");
    let restart = serde_json::from_value::<SupervisorRestartRequest>(explicit.clone()).unwrap();
    assert_eq!(restart.expected_generation, Some(2));
    assert_eq!(
      restart.binary,
      Some(PathBuf::from("/opt/expri next/bin/expri"))
    );
    assert_eq!(serde_json::to_value(restart).unwrap(), explicit);
    for (field, value) in [
      ("expected_generation", serde_json::json!(-1)),
      ("expected_generation", serde_json::json!("2")),
      ("binary", serde_json::json!(true)),
      ("worker_restart", serde_json::json!(true)),
    ] {
      let mut invalid = request.clone();
      invalid["restart"][field] = value;
      assert!(serde_json::from_value::<JobRequest>(invalid).is_err());
    }
  }

  #[test]
  fn smoke_request_is_opt_in_and_rejects_non_boolean_markers() {
    let legacy = serde_json::json!({
      "name": "train", "command": ["python", "train.py"], "environment": {},
    });
    let mut request: RunRequest = serde_json::from_value(legacy.clone()).unwrap();
    assert!(!request.smoke);
    assert!(
      serde_json::to_value(&request)
        .unwrap()
        .get("smoke")
        .is_none()
    );
    request.smoke = true;
    assert_eq!(serde_json::to_value(&request).unwrap()["smoke"], true);
    for value in [
      serde_json::json!("true"),
      serde_json::json!(1),
      serde_json::Value::Null,
    ] {
      let mut invalid = legacy.clone();
      invalid["smoke"] = value;
      assert!(serde_json::from_value::<RunRequest>(invalid).is_err());
    }
  }
}
