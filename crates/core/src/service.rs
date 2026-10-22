//! Shared service wire contracts and bounded artifact path validation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::service_catalog::{RunFilesPage, RunFilesQuery, RunListPage, RunListQuery};
use crate::system_profile::{
  MachineProfile, ProfileAcknowledgement, ProfileBatch, ProfileHistory, ProfileHistoryQuery,
};

pub use crate::identifiers::validate_component;

pub const STREAM_BATCH: usize = 64 * 1024;
pub const MAX_REQUEST: usize = 1024 * 1024;
pub const MAX_SYSTEM_MACHINES: usize = 100;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
  pub url: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub token: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub token_env: Option<String>,
  #[serde(default)]
  pub project_id: Option<String>,
}

impl std::fmt::Debug for ClientConfig {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("ClientConfig")
      .field("url", &"[configured]")
      .field("token", &self.token.as_ref().map(|_| "[redacted]"))
      .field("token_env", &self.token_env.as_ref().map(|_| "[redacted]"))
      .field(
        "project_id",
        &self.project_id.as_ref().map(|_| "[configured]"),
      )
      .finish()
  }
}

impl ClientConfig {
  /// Validate the connection URL independently of any saved credential.
  pub fn validate_url(&self) -> crate::error::Result<()> {
    use crate::error::ExpriError;

    let invalid = |message: &str| ExpriError::Message(message.into());
    let url = url::Url::parse(&self.url).map_err(|_| invalid("invalid service URL"))?;
    if !matches!(url.scheme(), "http" | "https")
      || url.host_str().is_none()
      || !url.username().is_empty()
      || url.password().is_some()
      || url.query().is_some()
      || url.fragment().is_some()
    {
      return Err(invalid(
        "service URL must be HTTP(S) without embedded credentials, query or fragment",
      ));
    }
    Ok(())
  }

  /// Validate syntax without resolving credentials or contacting the server.
  pub fn validate(&self) -> crate::error::Result<()> {
    use crate::error::ExpriError;

    self.validate_url()?;
    let invalid = |message: &str| ExpriError::Message(message.into());
    match (&self.token, &self.token_env) {
      (Some(token), None) => {
        validate_client_token(token)?;
      }
      (None, Some(name)) => {
        let mut bytes = name.bytes();
        if !bytes
          .next()
          .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
          || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
          return Err(invalid(
            "service token_env must be an environment variable name",
          ));
        }
      }
      _ => {
        return Err(invalid(
          "service client configuration requires exactly one of token or token_env",
        ));
      }
    }
    Ok(())
  }
}

/// Match the server's bearer-token character set without imposing its length policy.
pub fn validate_client_token(token: &str) -> crate::error::Result<()> {
  if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
    return Err(crate::error::ExpriError::Message(
      "service token must be nonempty ASCII graphic characters".into(),
    ));
  }
  Ok(())
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
  /// Owner-only preflight; does not create a group or require a published child.
  ValidateExperimentSelection {
    project_id: String,
    intent: crate::experiment_assignment::RunExperimentIntent,
  },
  /// Add a published run without replacing existing experiment metadata.
  AssignRunExperiment {
    scope: RunScope,
    intent: crate::experiment_assignment::RunExperimentIntent,
  },
  /// Fence one mutation to the publication generation that prepared its bytes.
  AtRunEpoch {
    epoch: u64,
    request: Box<Request>,
  },
  /// Explicitly reopen existing byte prefixes for a new task attempt.
  BeginRunAttempt {
    scope: RunScope,
    request_id: String,
    expected_epoch: u64,
    attempt: u64,
    documents: BTreeMap<String, u64>,
    streams: BTreeMap<String, u64>,
  },
  /// Preflight retention and publication support before stopping a task.
  CheckRunContinuation {
    scope: RunScope,
  },
  /// Capture the current epoch for a new explicit scope-based owner operation.
  RunPublication {
    scope: RunScope,
  },
  /// Resolve an uncertain CAS, durably refusing a late commit if it never ran.
  ReconcileRunAttempt {
    scope: RunScope,
    request_id: String,
    expected_epoch: u64,
  },
  /// Shared execution queue. All operations require an owner credential.
  Queue {
    request: crate::resource_queue::QueueRequest,
  },
  RunControl {
    request: crate::run_control::RunControlRequest,
  },
  RunControlBatch {
    requests: Vec<crate::run_control::RunControlRequest>,
  },
  /// Paginated run discovery across the scopes visible to the authenticated caller.
  BrowseRuns {
    query: RunListQuery,
  },
  /// Received files and bounded worker-reported outputs for one published run.
  BrowseRunFiles {
    query: RunFilesQuery,
  },
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
  ExperimentSelectionValidated,
  RunExperimentAssigned {
    assignment: crate::experiment_assignment::RunExperimentAssignment,
  },
  ReconciledRunAttempt {
    committed_epoch: Option<u64>,
  },
  RunAttempt {
    epoch: u64,
  },
  Queue {
    response: crate::resource_queue::QueueResponse,
  },
  RunControl {
    response: crate::run_control::RunControlResponse,
  },
  RunControlBatch {
    responses: Vec<crate::run_control::ControlPollResult>,
  },
  RunPage {
    page: RunListPage,
  },
  RunFilesPage {
    page: RunFilesPage,
  },
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

/// Accepted tracking documents, ordered so run state follows its dependencies.
pub const TRACKING_DOCUMENT_PATHS: [&str; 5] = [
  "outputs/.expri-artifacts.json",
  "snapshot.json",
  "environment/environment-state.json",
  "outputs/params.json",
  "run-state.json",
];

pub const TRACKING_DOCUMENT_LIMIT: usize =
  TRACKING_DOCUMENT_PATHS.len() + 4 * crate::protocol::MAX_TASK_ATTEMPTS;

/// Accepted append-only tracking streams, including the retained system capture.
pub const TRACKING_STREAM_PATHS: [&str; 4] = [
  "outputs/metrics.jsonl",
  crate::run_system::RUN_SYSTEM_PATH,
  "logs/stdout.log",
  "logs/stderr.log",
];

pub fn document_path(path: &str) -> bool {
  TRACKING_DOCUMENT_PATHS.contains(&path) || attempt_document(path).is_some()
}

/// Attempt metadata is published without permitting arbitrary source files.
pub fn attempt_document(path: &str) -> Option<u64> {
  let rest = path.strip_prefix("outputs/attempts/")?;
  let (attempt, name) = rest.split_once('/')?;
  if !matches!(
    name,
    "snapshot.json" | "environment-state.json" | "params.json" | "attempt-state.json"
  ) || attempt.starts_with('0')
    || attempt.len() > 19
    || !attempt.bytes().all(|byte| byte.is_ascii_digit())
  {
    return None;
  }
  attempt
    .parse::<u64>()
    .ok()
    .filter(|attempt| *attempt > 0 && *attempt <= crate::protocol::MAX_TASK_ATTEMPTS as u64)
}

pub fn run_mutation(request: &Request) -> bool {
  matches!(
    request,
    Request::PutDocument { .. }
      | Request::AppendTracking { .. }
      | Request::SealRun { .. }
      | Request::AppendStream { .. }
      | Request::BeginUpload { .. }
      | Request::PartUrl { .. }
      | Request::RecordPart { .. }
      | Request::CompleteUpload { .. }
      | Request::ReferenceFile { .. }
      | Request::ImportOutput { .. }
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
  TRACKING_STREAM_PATHS.contains(&path)
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
  fn attempt_documents_have_one_bounded_canonical_path_per_metadata_kind() {
    for path in [
      "outputs/attempts/1/params.json",
      "outputs/attempts/100/snapshot.json",
    ] {
      assert!(document_path(path));
    }
    for path in [
      "outputs/attempts/+1/params.json",
      "outputs/attempts/01/params.json",
      "outputs/attempts/101/params.json",
      "outputs/attempts/1/code/secrets.json",
      "outputs/attempts/1/../params.json",
    ] {
      assert!(!document_path(path), "unsafe attempt document: {path}");
    }
  }

  fn client_config(token: Option<&str>, token_env: Option<&str>) -> ClientConfig {
    ClientConfig {
      url: "https://example.net".into(),
      token: token.map(str::to_owned),
      token_env: token_env.map(str::to_owned),
      project_id: Some("vision".into()),
    }
  }

  #[test]
  fn client_authentication_requires_exactly_one_valid_source() {
    assert!(
      client_config(Some("stored-secret"), None)
        .validate()
        .is_ok()
    );
    assert!(
      client_config(None, Some("EXPRI_UNSET_TEST_TOKEN"))
        .validate()
        .is_ok()
    );
    for (token, env) in [
      (None, None),
      (Some("stored-secret"), Some("EXPRI_TOKEN")),
      (Some(""), None),
      (Some("stored\nsecret"), None),
      (Some("stored secret"), None),
      (Some("secret\u{007f}"), None),
      (Some("stored-secret-🔒"), None),
      (None, Some("")),
      (None, Some("9EXPRI_TOKEN")),
      (None, Some("EXPRI=TOKEN")),
      (None, Some("EXPRI\nTOKEN")),
    ] {
      assert!(client_config(token, env).validate().is_err());
    }
    // Client configuration keeps legacy short tokens compatible.
    assert!(client_config(Some("x"), None).validate().is_ok());
    assert!(
      client_config(Some("!#$%&()*+,-./:;<=>?@[\\]^_`{|}~"), None)
        .validate()
        .is_ok()
    );
  }

  #[test]
  fn connection_url_validation_does_not_imply_authenticated_client_configuration() {
    let mut config = client_config(None, None);
    config.validate_url().unwrap();
    assert!(config.validate().is_err());
    config.url = "https://must-not-leak@host".into();
    assert!(config.validate_url().is_err());
    assert!(
      !config
        .validate_url()
        .unwrap_err()
        .to_string()
        .contains("must-not-leak")
    );
  }

  #[test]
  fn client_urls_are_validated_without_echoing_credentials() {
    for url in [
      "https://must-not-leak@host",
      "https://user:must-not-leak@host",
      "https://host/?token=must-not-leak",
      "https://host/#must-not-leak",
      "file:///must-not-leak",
      "must-not-leak",
    ] {
      let mut config = client_config(Some("stored-secret"), None);
      config.url = url.into();
      let error = config.validate().unwrap_err();
      assert!(!format!("{error:?} {error} {config:?}").contains("must-not-leak"));
    }
    let mut config = client_config(Some("stored-secret"), None);
    config.url = "http://localhost:4444/service/".into();
    config.validate().unwrap();
  }

  #[test]
  fn client_configuration_serializes_credentials_but_debug_redacts_them() {
    for config in [
      client_config(Some("must-not-leak"), None),
      client_config(None, Some("MUST_NOT_LEAK_ENV")),
    ] {
      let debug = format!("{config:?}");
      assert!(debug.contains("[redacted]"));
      assert!(!debug.contains("must-not-leak"));
      assert!(!debug.contains("MUST_NOT_LEAK_ENV"));
      let encoded = toml::to_string(&config).unwrap();
      let restored: ClientConfig = toml::from_str(&encoded).unwrap();
      restored.validate().unwrap();
      assert_eq!(restored.token, config.token);
      assert_eq!(restored.token_env, config.token_env);
      assert_eq!(restored.project_id, config.project_id);
    }
  }

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
