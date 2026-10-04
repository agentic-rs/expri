use serde::{Deserialize, Serialize};

pub const STREAM_BATCH: usize = 64 * 1024;
pub const MAX_REQUEST: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct ClientConfig {
  pub url: String,
  pub token_env: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
  pub owner_token_env: String,
  #[serde(default)]
  pub workers: Vec<WorkerAuth>,
  #[serde(default)]
  pub dashboard: Option<DashboardConfig>,
  pub storage: super::storage::S3Config,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DashboardConfig {
  pub public_url: String,
  pub password_env: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkerAuth {
  pub project_id: String,
  pub origin: String,
  pub token_env: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunScope {
  pub project_id: String,
  pub origin: String,
  pub run_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileTarget {
  Run {
    scope: RunScope,
    path: String,
  },
  Input {
    project_id: String,
    input_id: String,
  },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompletedPart {
  pub part_number: u32,
  pub etag: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadState {
  pub upload_id: String,
  pub part_size: u64,
  pub parts: Vec<CompletedPart>,
  pub complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStorage {
  Object,
  Stream,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRecord {
  pub target: FileTarget,
  pub size: u64,
  pub sha256: Option<String>,
  pub storage: FileStorage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
  BeginUpload {
    upload_id: String,
    target: FileTarget,
    size: u64,
    sha256: String,
  },
  PartUrl {
    upload_id: String,
    part_number: u32,
  },
  RecordPart {
    upload_id: String,
    part: CompletedPart,
  },
  CompleteUpload {
    upload_id: String,
  },
  ListFiles {
    scope: RunScope,
  },
  ListRuns {
    project_id: String,
    origin: String,
  },
  GetFile {
    target: FileTarget,
  },
  DownloadUrl {
    target: FileTarget,
  },
  AppendStream {
    scope: RunScope,
    path: String,
    offset: u64,
    data_base64: String,
  },
  ReadStream {
    scope: RunScope,
    path: String,
    offset: u64,
    limit: usize,
  },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
  Upload {
    upload: UploadState,
  },
  Url {
    url: String,
  },
  Acknowledged {
    offset: u64,
  },
  File {
    file: FileRecord,
  },
  Files {
    files: Vec<FileRecord>,
  },
  Runs {
    runs: Vec<RunScope>,
  },
  Stream {
    offset: u64,
    total_size: u64,
    data_base64: String,
  },
}

pub fn validate_component(value: &str) -> crate::error::Result<()> {
  if value.is_empty()
    || value.len() > 96
    || value == "."
    || value == ".."
    || !value
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
  {
    return Err(crate::error::ExpriError::Message(
      "invalid service identifier".into(),
    ));
  }
  Ok(())
}

pub fn validate_scope(scope: &RunScope) -> crate::error::Result<()> {
  for value in [&scope.project_id, &scope.origin, &scope.run_id] {
    validate_component(value)?;
  }
  Ok(())
}

pub fn validate_run_path(path: &str) -> crate::error::Result<()> {
  let fixed = [
    "run-state.json",
    "snapshot.json",
    "environment/environment-state.json",
  ];
  let allowed = fixed.contains(&path)
    || path.starts_with("outputs/")
    || path == "logs/stdout.log"
    || path == "logs/stderr.log";
  if !allowed
    || path.len() > 1024
    || path.contains('\\')
    || path.contains('\0')
    || path
      .split('/')
      .any(|part| part.is_empty() || part == "." || part == "..")
  {
    return Err(crate::error::ExpriError::Message(
      "invalid run artifact path".into(),
    ));
  }
  Ok(())
}

pub fn validate_target(target: &FileTarget) -> crate::error::Result<()> {
  match target {
    FileTarget::Run { scope, path } => {
      validate_scope(scope)?;
      validate_run_path(path)
    }
    FileTarget::Input {
      project_id,
      input_id,
    } => {
      validate_component(project_id)?;
      validate_component(input_id)
    }
  }
}

pub fn stream_path(path: &str) -> bool {
  matches!(
    path,
    "outputs/metrics.jsonl" | "logs/stdout.log" | "logs/stderr.log"
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn scope_and_paths_keep_service_objects_out_of_environment_and_parent_directories() {
    for path in [
      "../secret",
      "outputs/../secret",
      "outputs//x",
      "outputs/x\\y",
      "environment/.venv/bin/python",
      "/outputs/x",
      "logs/other.log",
    ] {
      assert!(validate_run_path(path).is_err(), "{path}");
    }
    for path in [
      "run-state.json",
      "outputs/model one.pt",
      "outputs/nested/model.pt",
      "logs/stderr.log",
      "environment/environment-state.json",
    ] {
      assert!(validate_run_path(path).is_ok(), "{path}");
    }
    for value in ["", ".", "..", "gpu/rental", "gpu rental"] {
      assert!(validate_component(value).is_err());
    }
    assert!(validate_component("gpu-1_a").is_ok());
  }
}
