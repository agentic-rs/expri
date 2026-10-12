//! Shared service wire contracts and bounded artifact path validation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::system_profile::{
  MachineProfile, ProfileAcknowledgement, ProfileBatch, ProfileHistory, ProfileHistoryQuery,
};

pub use crate::identifiers::validate_component;

pub const STREAM_BATCH: usize = 64 * 1024;
pub const MAX_REQUEST: usize = 1024 * 1024;
pub const MAX_SYSTEM_MACHINES: usize = 100;

#[derive(Clone, Debug, Deserialize)]
pub struct ClientConfig {
  pub url: String,
  pub token_env: String,
  #[serde(default)]
  pub project_id: Option<String>,
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
  Tracking { revision: u64, sealed: bool },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRecord {
  pub target: FileTarget,
  pub size: u64,
  pub sha256: Option<String>,
  pub storage: FileStorage,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub provenance: Option<AssetProvenance>,
}

/// Portable producer identity; deployment and project configuration stay local.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputProducer {
  pub origin: String,
  pub run_id: String,
  pub path: String,
}

/// Captured metadata remains available after the producing run expires.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetProvenance {
  pub producer: OutputProducer,
  pub captured_at: String,
  pub state: serde_json::Value,
  pub snapshot: serde_json::Value,
  pub environment: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
  /// Publish machine inventory, recent details, and authoritative hourly snapshots.
  PutSystemProfile {
    batch: ProfileBatch,
  },
  /// Browse system profiles independently of project and run history. Owner-only.
  ListSystemMachines {
    limit: usize,
    cursor: Option<String>,
  },
  GetSystemMachine {
    machine_id: String,
    inventory_revision: Option<u64>,
  },
  SystemHistory {
    query: ProfileHistoryQuery,
  },
  ProjectStorage {
    project_id: String,
  },
  PreviewProjectDelete {
    project_id: String,
  },
  DeleteProject {
    project_id: String,
    revision: String,
    confirmation: String,
  },
  ProjectDeletion {
    project_id: String,
  },
  /// Hide a finished run and schedule hosted cleanup after 15 days. Owner-only.
  ArchiveRun {
    scope: RunScope,
  },
  /// Cancel retention before deletion begins. Owner-only.
  RestoreRun {
    scope: RunScope,
  },
  /// Inspect the archived run lifecycle and cleanup status. Owner-only.
  RunArchival {
    scope: RunScope,
  },
  /// Inspect user-managed run metadata. Owner-only.
  RunAnnotations {
    scope: RunScope,
  },
  /// Replace user-managed run metadata without changing worker records. Owner-only.
  SetRunAnnotations {
    scope: RunScope,
    nickname: Option<String>,
    tags: Vec<String>,
  },
  Capabilities,
  PutDocument {
    scope: RunScope,
    path: String,
    revision: u64,
    offset: u64,
    total_size: u64,
    data_base64: String,
  },
  AppendTracking {
    scope: RunScope,
    path: String,
    offset: u64,
    data_base64: String,
  },
  SealRun {
    scope: RunScope,
    documents: BTreeMap<String, u64>,
    streams: BTreeMap<String, u64>,
    incomplete: bool,
  },
  ArchiveStatus {
    scope: RunScope,
  },
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
  /// Reference a completed object without copying its bytes. Owner-only.
  ReferenceFile {
    source: FileTarget,
    target: FileTarget,
    size: u64,
    sha256: String,
  },
  /// Import a finalized run output as a reusable private input without copying bytes.
  /// The server assigns an opaque input ID when one is not supplied. Owner-only.
  ImportOutput {
    scope: RunScope,
    path: String,
    input_id: Option<String>,
    size: u64,
    sha256: String,
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
  SystemProfileAcknowledged {
    acknowledgement: ProfileAcknowledgement,
  },
  SystemMachines {
    machines: Vec<MachineProfile>,
    next_cursor: Option<String>,
  },
  SystemMachine {
    machine: MachineProfile,
  },
  SystemHistory {
    history: ProfileHistory,
  },
  ProjectStorage {
    stats: ProjectStorageStats,
  },
  ProjectDeletePreview {
    preview: ProjectDeletePreview,
  },
  ProjectDeletion {
    deletion: ProjectDeletionStatus,
  },
  RunArchival {
    archival: RunArchival,
  },
  RunAnnotations {
    annotations: RunAnnotations,
  },
  Capabilities {
    features: Vec<String>,
  },
  DocumentAcknowledged {
    offset: u64,
    revision: u64,
    complete: bool,
  },
  Archive {
    archive: ArchiveRecord,
  },
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectStorageStats {
  pub project_id: String,
  pub revision: String,
  pub file_count: u64,
  pub logical_bytes: u64,
  pub object_count: u64,
  pub object_bytes: u64,
  pub shared_reference_count: u64,
  pub retained_object_count: u64,
  pub retained_object_bytes: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub s3_object_count: Option<u64>,
  /// Recorded completed object payloads, including retained uploads and counting each key once.
  /// This excludes pending multipart parts and provider-managed historical object versions.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub s3_storage_bytes: Option<u64>,
  pub pending_upload_count: u64,
  pub pending_upload_bytes: u64,
  pub tracking_bytes: u64,
  /// Saved result.zip payloads owned by registered archive jobs, including retained archives.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub local_archive_bytes: Option<u64>,
  /// Acknowledged tracking payloads and saved archives on the server, excluding shared SQLite
  /// metadata, projections, filesystem allocation overhead, and unregistered temporary files.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub local_storage_bytes: Option<u64>,
  pub reclaimable_object_count: u64,
  pub reclaimable_object_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectDeletePreview {
  pub project_id: String,
  pub revision: String,
  pub run_count: u64,
  pub stats: ProjectStorageStats,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectDeletionStatus {
  pub project_id: String,
  pub status: String,
  pub pending_tasks: u64,
  pub deleted_objects: u64,
  pub aborted_uploads: u64,
  pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunArchival {
  pub scope: RunScope,
  pub status: String,
  pub archived_at: Option<String>,
  pub delete_after: Option<String>,
  pub pending_tasks: u64,
  pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunAnnotations {
  pub scope: RunScope,
  pub nickname: Option<String>,
  pub tags: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveRecord {
  pub status: String,
  pub incomplete: bool,
  pub file: Option<FileRecord>,
  pub last_error: Option<String>,
}

pub fn document_path(path: &str) -> bool {
  matches!(
    path,
    "run-state.json"
      | "snapshot.json"
      | "environment/environment-state.json"
      | "outputs/params.json"
      | "outputs/.expri-artifacts.json"
  )
}

pub fn validate_scope(scope: &RunScope) -> crate::error::Result<()> {
  for value in [&scope.project_id, &scope.origin, &scope.run_id] {
    validate_component(value)?;
  }
  Ok(())
}

pub fn validate_run_path(path: &str) -> crate::error::Result<()> {
  let fixed = [
    "result.zip",
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
    "outputs/metrics.jsonl"
      | "outputs/system-profile.jsonl"
      | "logs/stdout.log"
      | "logs/stderr.log"
  )
}

/// Translate presentation fields without changing the saved protocol or queue.
pub fn upload_report(mut report: serde_json::Value) -> serde_json::Value {
  if report["status"] == "archived" {
    report["status"] = serde_json::json!("uploaded");
  }
  report
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

  #[test]
  fn run_lifecycle_wire_actions_do_not_reuse_result_zip_archive_status() {
    let scope = RunScope {
      project_id: "vision".into(),
      origin: "gpu-1".into(),
      run_id: "run-1".into(),
    };
    for (request, action) in [
      (
        Request::ArchiveRun {
          scope: scope.clone(),
        },
        "archive_run",
      ),
      (
        Request::RestoreRun {
          scope: scope.clone(),
        },
        "restore_run",
      ),
      (
        Request::RunArchival {
          scope: scope.clone(),
        },
        "run_archival",
      ),
      (
        Request::ArchiveStatus {
          scope: scope.clone(),
        },
        "archive_status",
      ),
    ] {
      let value = serde_json::to_value(request).unwrap();
      assert_eq!(value["action"], action);
      assert_eq!(value["scope"], serde_json::to_value(&scope).unwrap());
      assert!(serde_json::from_value::<Request>(value).is_ok());
    }
  }
}
