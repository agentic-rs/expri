//! Plain transfer options shared by the CLI and worker integration.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct FetchOptions {
  pub config: PathBuf,
  pub project_id: String,
  pub origins: Vec<String>,
  pub repo: PathBuf,
  pub results_dir: PathBuf,
  pub artifacts: Vec<String>,
  pub labels: Vec<String>,
  pub watch: bool,
  pub dry_run: bool,
  pub quiet: bool,
}

#[derive(Clone, Debug)]
pub struct PublishOptions {
  pub config: PathBuf,
  pub run_dir: PathBuf,
  pub project_id: String,
  pub origin: String,
  pub artifacts: Vec<String>,
  pub watch: bool,
  pub queue_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ServiceFetchOptions {
  pub config: PathBuf,
  pub project_id: String,
  pub origin: String,
  pub run_id: String,
  pub repo: PathBuf,
  pub results_dir: PathBuf,
  /// Cache source name; defaults to service-<project_id>-<origin>.
  pub source: Option<String>,
  pub artifacts: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct FileUploadOptions {
  pub config: PathBuf,
  /// Destination FileTarget as JSON.
  pub target: String,
  pub file: PathBuf,
  pub queue_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ReferenceOptions {
  pub config: PathBuf,
  /// Source FileTarget as JSON (run output or project input).
  pub source: String,
  /// Destination FileTarget as JSON. Requires an owner token.
  pub target: String,
  pub size: u64,
  pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct InputUploadOptions {
  pub config: PathBuf,
  pub project_id: String,
  pub input_id: String,
  pub file: PathBuf,
  pub queue_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct InputDownloadOptions {
  pub config: PathBuf,
  pub project_id: String,
  pub input_id: String,
  pub destination: PathBuf,
}
