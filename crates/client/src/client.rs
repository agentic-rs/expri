mod catalog;
mod checkpoints;
mod download;
pub(super) mod fs;
mod http;
mod inventory;
mod login;
mod queue;
mod system_profile;
#[cfg(test)]
pub(super) mod tests;
mod tracking;
mod upload;
mod watch;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::types::*;
use super::{InputUploadOptions, PublishOptions};
use crate::error::Result;
use expri_core::run_system::{RUN_SYSTEM_PATH, RunSystemRecord};
use fs::message;
use http::Api;
use queue::{Queue, SavedFile};
use upload::{sync_file, sync_stream};

pub use catalog::ServiceCatalogClient;
pub use download::{fetch, input_download, input_download_prepared};
pub use login::{login, logout, revoke_login, validate_login_url};
pub use system_profile::SystemProfileClient;
pub use watch::{fetch as fetch_files, fetch_with_reporter as fetch_files_with_reporter};

pub(super) fn validate_config(path: &Path) -> Result<()> {
  Api::new(path).map(|_| ())
}

const RECORD_LIMIT: u64 = 16 * 1024 * 1024;
const OBJECT_BATCH: u64 = 8 * 1024 * 1024;
const METADATA: [&str; 5] = [
  crate::run_artifacts::INVENTORY_PATH,
  "snapshot.json",
  "environment/environment-state.json",
  "outputs/params.json",
  "run-state.json",
];
const STREAMS: [&str; 4] = [
  "outputs/metrics.jsonl",
  RUN_SYSTEM_PATH,
  "logs/stdout.log",
  "logs/stderr.log",
];

fn terminal(state: &Value) -> bool {
  matches!(
    state.get("status").and_then(Value::as_str),
    Some("completed" | "failed" | "cancelled" | "lost")
  )
}

pub(super) struct Publisher {
  api: Api,
  config: PathBuf,
  checkpoints: checkpoints::CheckpointLane,
  run_dir: PathBuf,
  scope: RunScope,
  artifacts: BTreeSet<String>,
  queue: Queue,
  watch: bool,
}

impl Publisher {
  pub(super) fn permanent_rejection(error: &crate::error::ExpriError) -> Option<&'static str> {
    match error {
      crate::error::ExpriError::ServiceRejected {
        status: 401 | 403, ..
      } => Some("Service authentication rejected; repair credentials and resume publishing"),
      crate::error::ExpriError::ServiceRejected { status: 410, .. } => Some(
        "Service run or project has expired or been deleted; publishing stopped. Use a new run identifier, or a new project identifier if the project was deleted",
      ),
      _ => None,
    }
  }

  pub(super) fn new(options: &PublishOptions) -> Result<Self> {
    let api = Api::new(&options.config)?;
    let run_dir = std::path::absolute(&options.run_dir)?;
    fs::directory(&run_dir)?;
    let state: Value = serde_json::from_slice(&fs::read_bounded(
      &run_dir.join("run-state.json"),
      RECORD_LIMIT,
    )?)?;
    let scope = RunScope {
      project_id: options.project_id.clone(),
      origin: options.origin.clone(),
      run_id: state
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or_else(|| message("run state has no run_id"))?
        .to_string(),
    };
    validate_scope(&scope)?;
    let artifacts = artifacts(&options.artifacts)?;
    if artifacts.contains("result.zip") {
      return Err(message(
        "result.zip is created by the service; select it with service fetch",
      ));
    }
    let queue_dir = std::path::absolute(&options.queue_dir)?
      .join("runs")
      .join(&scope.project_id)
      .join(&scope.origin)
      .join(&scope.run_id);
    let owner = json!({"endpoint": api.endpoint, "scope": scope});
    let queue = Queue::new(queue_dir, owner)?;
    Ok(Self {
      api,
      config: options.config.clone(),
      checkpoints: checkpoints::CheckpointLane::default(),
      run_dir,
      scope,
      artifacts,
      queue,
      watch: options.watch,
    })
  }

  pub(super) fn cycle(&mut self, progress: &mut dyn FnMut(Value) -> Result<()>) -> Result<bool> {
    let checkpoints_done = self.checkpoints.poll(
      &self.config,
      &self.run_dir,
      &self.scope,
      &self.queue.directory,
      self.watch,
    )?;
    let metadata_done = publish_cycle(
      &self.api,
      &mut self.queue,
      &self.scope,
      &self.run_dir,
      &self.artifacts,
      self.watch,
      progress,
    )?;
    Ok(metadata_done && checkpoints_done)
  }

  pub(super) fn report(&self, done: bool) -> Value {
    let files = self
      .queue
      .state
      .files
      .keys()
      .chain(self.queue.state.documents.keys())
      .cloned()
      .collect::<BTreeSet<_>>();
    let mut files = files;
    if let Ok(checkpoints) = super::registrations::records(&self.run_dir) {
      files.extend(
        checkpoints
          .into_iter()
          .filter(|file| file.sync_status == "cloud")
          .map(|file| file.path),
      );
    }
    json!({
      "scope": self.scope, "terminal": done,
      "files": files,
      "stream_offsets": self.queue.state.streams, "queue_dir": self.queue.directory,
      "protocol": self.queue.state.protocol,
      "system_capture_upload": system_upload_report(&self.queue),
      "result_upload": upload_report(json!(self.queue.state.archive)),
    })
  }

  pub(super) fn progress(&self, done: bool) -> Value {
    let mut progress = queue_progress(&self.queue);
    progress["terminal"] = json!(done);
    progress["queue_dir"] = json!(self.queue.directory);
    if let Ok(checkpoints) = super::registrations::records(&self.run_dir) {
      progress["checkpoints"] = json!({
        "registered": checkpoints.iter().filter(|file| file.sync_status == "registered").count(),
        "uploading": checkpoints.iter().filter(|file| file.sync_status == "uploading").count(),
        "cloud": checkpoints.iter().filter(|file| file.sync_status == "cloud").count(),
        "needs_attention": checkpoints.iter().filter(|file| file.sync_status == "needs_attention").count(),
      });
    }
    progress
  }

  pub(super) fn error_text(&self, error: &crate::error::ExpriError) -> String {
    self.api.redact(&error.to_string())
  }
}

pub fn publish(options: PublishOptions) -> Result<Value> {
  let mut publisher = Publisher::new(&options)?;
  let mut last_error = String::new();
  let mut failures = 0u64;
  loop {
    match publisher.cycle(&mut |_| Ok(())) {
      Ok(done) if !options.watch || done => {
        return Ok(publisher.report(done));
      }
      Ok(_) => {
        last_error.clear();
        failures = 0;
      }
      Err(error) if !options.watch => return Err(error),
      Err(error) => {
        if let Some(reason) = Publisher::permanent_rejection(&error) {
          return Err(message(reason));
        }
        let detail = publisher.error_text(&error);
        if last_error != detail || failures.is_multiple_of(15) {
          eprintln!("Service publishing pending; saved work will retry: {detail}");
          last_error = detail;
        }
        failures += 1;
      }
    }
    thread::sleep(Duration::from_secs(5));
  }
}

pub(crate) use crate::types::upload_report;

pub(super) fn project_result_upload(report: &mut Value) {
  if let Some(fields) = report.as_object_mut()
    && let Some(archive) = fields.remove("archive")
  {
    fields.insert("result_upload".into(), upload_report(archive));
  }
}

pub fn upload(config: &Path, scope: &RunScope, partial: bool) -> Result<Value> {
  validate_scope(scope)?;
  let api = Api::new(config)?;
  let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
    return Err(message("service did not return protocol capabilities"));
  };
  if !features.iter().any(|feature| feature == "tracking-v1") {
    return Err(message("service does not support server tracking uploads"));
  }
  let Response::Files { files } = api.request(&Request::ListFiles {
    scope: scope.clone(),
  })?
  else {
    return Err(message("service did not return its tracking catalog"));
  };
  if files.len() > 1000 {
    return Err(message("service file catalog exceeds its size limit"));
  }
  let mut documents = std::collections::BTreeMap::new();
  let mut streams = std::collections::BTreeMap::new();
  let mut seen = BTreeSet::new();
  for file in files {
    download::validate_record(&file)?;
    let FileTarget::Run {
      scope: returned,
      path,
    } = file.target
    else {
      return Err(message(
        "tracking catalog unexpectedly contains private inputs",
      ));
    };
    if returned != *scope || !seen.insert(path.clone()) {
      return Err(message(
        "tracking catalog contains another run or duplicate file records",
      ));
    }
    if let FileStorage::Tracking { revision, .. } = file.storage {
      if document_path(&path) {
        documents.insert(path, revision);
      } else if stream_path(&path) {
        streams.insert(path, file.size);
      }
    }
  }
  if documents.is_empty() && streams.is_empty() {
    return Err(message("run has no received tracking data to upload"));
  }
  let Response::Archive { archive } = api.request(&Request::SealRun {
    scope: scope.clone(),
    documents,
    streams,
    incomplete: partial,
  })?
  else {
    return Err(message("service did not acknowledge its result.zip upload"));
  };
  serde_json::to_value(tracking::archive_receipt(&api, scope, archive, partial)?)
    .map(upload_report)
    .map_err(Into::into)
}

fn artifacts(values: &[String]) -> Result<BTreeSet<String>> {
  if values.len() > 64 {
    return Err(message("select at most 64 explicit artifacts"));
  }
  values
    .iter()
    .map(|path| {
      validate_run_path(path)?;
      if !path.starts_with("outputs/") && path != "result.zip" {
        return Err(message(
          "explicit service artifacts must be files under outputs/ or result.zip",
        ));
      }
      Ok(path.clone())
    })
    .collect()
}

fn publish_cycle(
  api: &Api,
  queue: &mut Queue,
  scope: &RunScope,
  run_dir: &Path,
  artifacts: &BTreeSet<String>,
  watch: bool,
  progress: &mut dyn FnMut(Value) -> Result<()>,
) -> Result<bool> {
  fs::directory(run_dir)?;
  let state: Value = serde_json::from_slice(&fs::read_bounded(
    &run_dir.join("run-state.json"),
    RECORD_LIMIT,
  )?)?;
  if state.get("run_id").and_then(Value::as_str) != Some(scope.run_id.as_str()) {
    return Err(message("run identity changed while publishing"));
  }
  let done = terminal(&state);
  if !done && !artifacts.is_empty() && !watch {
    return Err(message(
      "explicit output artifacts require a completed, failed, cancelled or lost run",
    ));
  }
  if fs::inspect(&run_dir.join("snapshot.json"))?.is_none() {
    return Err(message("run snapshot metadata is missing"));
  }
  let protocol = tracking::negotiate(api, queue)?;
  let mut capture_failure = None;
  let capture = match system_capture_owned(run_dir, &state) {
    Ok(capture) => capture,
    Err(error) => {
      capture_failure = Some(error);
      false
    }
  };
  let capture_ready = state["system_capture_status"] != "capturing";
  let capture_supported = if capture {
    match tracking::system_supported(api, queue) {
      Ok(supported) => supported,
      Err(error) => {
        capture_failure = Some(error);
        false
      }
    }
  } else {
    false
  };
  if state["system_capture_id"].is_string() && !queue.state.run_system_capture {
    queue.state.run_system_capture = true;
    queue.save()?;
  }
  for path in METADATA
    .iter()
    .filter(|path| **path != "run-state.json" && **path != crate::run_artifacts::INVENTORY_PATH)
  {
    let source = run_dir.join(path);
    if fs::inspect(&source)?.is_some() {
      sync_metadata(api, queue, scope, path, &source, protocol)?;
      progress(queue_progress(queue))?;
    }
  }
  for path in STREAMS {
    if path == RUN_SYSTEM_PATH {
      if capture && capture_supported {
        let result = (|| {
          let prefix = expri_system::store::run_capture::committed_prefix(run_dir, &state)?;
          upload::sync_stream_up_to(
            api,
            queue,
            scope,
            path,
            &run_dir.join(path),
            done && capture_ready,
            prefix,
          )
        })();
        if let Err(error) = result {
          capture_failure = Some(error);
        }
        progress(queue_progress(queue))?;
      }
      continue;
    }
    let source = run_dir.join(path);
    if fs::inspect(&source)?.is_some() {
      if !queue
        .state
        .files
        .get(path)
        .is_some_and(|saved| saved.upload.complete)
      {
        sync_stream(api, queue, scope, path, &source, done)?;
      }
      if done && protocol == queue::Protocol::Legacy {
        sync_file(api, queue, path, run_target(scope, path), &source, false)?;
      }
      progress(queue_progress(queue))?;
    }
  }
  let registrations = super::registrations::records(run_dir)?;
  for path in artifacts
    .iter()
    .filter(|path| done && !registrations.iter().any(|file| file.path == **path))
  {
    sync_file(
      api,
      queue,
      path,
      run_target(scope, path),
      &run_dir.join(path),
      false,
    )?;
  }
  sync_metadata(
    api,
    queue,
    scope,
    "run-state.json",
    &run_dir.join("run-state.json"),
    protocol,
  )?;
  progress(queue_progress(queue))?;
  // Artifact discovery is optional metadata. Unsafe or unwritable outputs must
  // not prevent the established run-state and log/metric publication above.
  if inventory::record(run_dir).is_ok() {
    let path = crate::run_artifacts::INVENTORY_PATH;
    sync_metadata(api, queue, scope, path, &run_dir.join(path), protocol)?;
    progress(queue_progress(queue))?;
  }
  // Capture retries keep their durable queue, while ordinary logs and task
  // metadata above continue to reach the service in the same cycle.
  if let Some(error) = capture_failure {
    return Err(error);
  }
  if done {
    // The task can finish before the collector's last observation commits.
    // Publish its terminal state now, but keep its capture open while draining.
    if capture && !capture_ready {
      return Ok(false);
    }
    if !super::registrations::close_if_ready(run_dir, || inventory::record(run_dir))? {
      return Ok(false);
    }
    if inventory::record(run_dir).is_ok() {
      let path = crate::run_artifacts::INVENTORY_PATH;
      sync_metadata(api, queue, scope, path, &run_dir.join(path), protocol)?;
    }
    if protocol == queue::Protocol::TrackingV1 {
      tracking::seal(api, queue, scope)?;
      progress(queue_progress(queue))?;
    }
  }
  Ok(done)
}

fn system_capture_owned(run_dir: &Path, state: &Value) -> Result<bool> {
  use std::io::{BufRead, BufReader, Read};
  let Some(capture_id) = state["system_capture_id"].as_str() else {
    return Ok(false);
  };
  validate_component(capture_id)?;
  let file = fs::open(&run_dir.join(RUN_SYSTEM_PATH))?;
  let mut reader = BufReader::new(file.take(1024 * 1024));
  let mut header = String::new();
  reader.read_line(&mut header)?;
  let record: RunSystemRecord = serde_json::from_str(&header)?;
  record.validate()?;
  let RunSystemRecord::Capture {
    capture_id: returned,
    machine_id,
    start_ms,
    ..
  } = record
  else {
    return Err(message("run system capture has no identity header"));
  };
  let started_at = state["started_at"]
    .as_str()
    .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
    .map(|timestamp| timestamp.timestamp_millis());
  if returned != capture_id
    || state["machine_id"].as_str() != Some(machine_id.as_str())
    || started_at != Some(start_ms)
  {
    return Err(message(
      "run system capture belongs to another run or machine",
    ));
  }
  Ok(true)
}

fn sync_metadata(
  api: &Api,
  queue: &mut Queue,
  scope: &RunScope,
  path: &str,
  source: &Path,
  protocol: queue::Protocol,
) -> Result<()> {
  match protocol {
    queue::Protocol::TrackingV1 => tracking::sync_document(api, queue, scope, path, source),
    queue::Protocol::Legacy => sync_file(api, queue, path, run_target(scope, path), source, true),
  }
}

fn queue_progress(queue: &Queue) -> Value {
  let streams: std::collections::BTreeMap<_, _> = STREAMS
    .iter()
    .filter_map(|path| {
      queue
        .state
        .streams
        .get(*path)
        .map(|offset| (*path, *offset))
    })
    .collect();
  json!({
    "files_completed": queue.state.files.values().filter(|saved| saved.upload.complete).count()
      + queue.state.documents.values().filter(|saved| saved.complete).count(),
    "stream_offsets": streams,
    "archive": queue.state.archive,
    "system_capture_upload": system_upload_report(queue),
  })
}

fn system_upload_report(queue: &Queue) -> Value {
  if !queue.state.run_system_capture {
    return Value::Null;
  }
  if queue.state.run_system_supported != Some(true)
    || queue.state.protocol != Some(queue::Protocol::TrackingV1)
  {
    return json!({
      "status": "unsupported", "retained_local": true,
      "message": if queue.state.protocol == Some(queue::Protocol::Legacy) {
        "This legacy publishing queue does not support run system captures; exact readings remain in the local run. Start new runs with the upgraded service."
      } else {
        "The service does not support run system captures; exact readings remain in the local run. Upgrade the service before starting new runs."
      }
    });
  }
  json!({
    "status": if queue.state.archive.is_some() { "published" } else { "publishing" },
    "retained_local": true,
    "acknowledged_bytes": queue.state.streams.get(RUN_SYSTEM_PATH).copied().unwrap_or(0),
  })
}

pub fn input_upload(options: InputUploadOptions) -> Result<Value> {
  let api = Api::new(&options.config)?;
  let target = FileTarget::Input {
    project_id: options.project_id,
    input_id: options.input_id,
  };
  validate_target(&target)?;
  let FileTarget::Input {
    project_id,
    input_id,
  } = &target
  else {
    unreachable!()
  };
  let directory = std::path::absolute(options.queue_dir)?
    .join("inputs")
    .join(project_id)
    .join(input_id);
  let mut queue = Queue::new(
    directory,
    json!({"endpoint": api.endpoint, "target": target}),
  )?;
  sync_file(
    &api,
    &mut queue,
    "input",
    target.clone(),
    &std::path::absolute(options.file)?,
    false,
  )?;
  let file = &queue.state.files["input"];
  Ok(
    json!({"target": target, "size": file.size, "sha256": file.sha256, "queue_dir": queue.directory}),
  )
}

pub fn list(config: PathBuf, project_id: String, origin: String) -> Result<Value> {
  validate_component(&project_id)?;
  validate_component(&origin)?;
  let api = Api::new(&config)?;
  let Response::Runs { runs } = api.request(&Request::ListRuns { project_id, origin })? else {
    return Err(message("service did not return a run catalog"));
  };
  Ok(json!({"runs": runs}))
}

pub fn project_stats(config: &Path, project_id: &str) -> Result<Value> {
  validate_component(project_id)?;
  let Response::ProjectStorage { stats } = Api::new(config)?.request(&Request::ProjectStorage {
    project_id: project_id.into(),
  })?
  else {
    return Err(message("service did not return project storage usage"));
  };
  if stats.project_id != project_id {
    return Err(message(
      "service returned storage usage for another project",
    ));
  }
  serde_json::to_value(stats).map_err(Into::into)
}

pub fn archive_run(config: &Path, scope: &RunScope) -> Result<Value> {
  run_management(
    config,
    scope,
    Request::ArchiveRun {
      scope: scope.clone(),
    },
  )
}

pub fn restore_run(config: &Path, scope: &RunScope) -> Result<Value> {
  run_management(
    config,
    scope,
    Request::RestoreRun {
      scope: scope.clone(),
    },
  )
}

pub fn run_archival(config: &Path, scope: &RunScope) -> Result<Value> {
  run_management(
    config,
    scope,
    Request::RunArchival {
      scope: scope.clone(),
    },
  )
}

pub fn run_annotations(config: &Path, scope: &RunScope) -> Result<Value> {
  annotations_request(
    config,
    scope,
    Request::RunAnnotations {
      scope: scope.clone(),
    },
  )
}

pub fn set_run_annotations(
  config: &Path,
  scope: &RunScope,
  nickname: Option<String>,
  tags: Vec<String>,
) -> Result<Value> {
  annotations_request(
    config,
    scope,
    Request::SetRunAnnotations {
      scope: scope.clone(),
      nickname,
      tags,
    },
  )
}

fn annotations_request(config: &Path, scope: &RunScope, request: Request) -> Result<Value> {
  validate_scope(scope)?;
  let Response::RunAnnotations { annotations } = Api::new(config)?.request(&request)? else {
    return Err(message("service did not return run annotations"));
  };
  if annotations.scope != *scope {
    return Err(message("service returned annotations for another run"));
  }
  serde_json::to_value(annotations).map_err(Into::into)
}

fn run_management(config: &Path, scope: &RunScope, request: Request) -> Result<Value> {
  validate_scope(scope)?;
  let Response::RunArchival { archival } = Api::new(config)?.request(&request)? else {
    return Err(message("service did not return run archival status"));
  };
  if archival.scope != *scope {
    return Err(message("service returned archival status for another run"));
  }
  serde_json::to_value(archival).map_err(Into::into)
}

pub fn project_delete_preview(config: &Path, project_id: &str) -> Result<Value> {
  validate_component(project_id)?;
  let Response::ProjectDeletePreview { preview } =
    Api::new(config)?.request(&Request::PreviewProjectDelete {
      project_id: project_id.into(),
    })?
  else {
    return Err(message("service did not return a project deletion preview"));
  };
  if preview.project_id != project_id || preview.stats.project_id != project_id {
    return Err(message("service returned a preview for another project"));
  }
  serde_json::to_value(preview).map_err(Into::into)
}

pub fn project_delete(
  config: &Path,
  project_id: &str,
  revision: &str,
  confirmation: &str,
) -> Result<Value> {
  validate_component(project_id)?;
  if confirmation != project_id {
    return Err(message("--confirm-project must match --project-id exactly"));
  }
  if revision.is_empty()
    || revision.len() > 128
    || !revision.bytes().all(|byte| byte.is_ascii_graphic())
  {
    return Err(message(
      "--revision must be the current deletion preview revision",
    ));
  }
  let Response::ProjectDeletion { deletion } =
    Api::new(config)?.request(&Request::DeleteProject {
      project_id: project_id.into(),
      revision: revision.into(),
      confirmation: confirmation.into(),
    })?
  else {
    return Err(message("service did not acknowledge project deletion"));
  };
  project_deletion_value(project_id, deletion)
}

pub fn project_deletion(config: &Path, project_id: &str) -> Result<Value> {
  validate_component(project_id)?;
  let Response::ProjectDeletion { deletion } =
    Api::new(config)?.request(&Request::ProjectDeletion {
      project_id: project_id.into(),
    })?
  else {
    return Err(message("service did not return project deletion status"));
  };
  project_deletion_value(project_id, deletion)
}

fn project_deletion_value(project_id: &str, deletion: ProjectDeletionStatus) -> Result<Value> {
  if deletion.project_id != project_id {
    return Err(message(
      "service returned deletion status for another project",
    ));
  }
  serde_json::to_value(deletion).map_err(Into::into)
}

fn validate_digest(value: &str) -> Result<()> {
  if value.len() != 64
    || !value
      .bytes()
      .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
  {
    return Err(message("service file has an invalid SHA256 digest"));
  }
  Ok(())
}

fn run_target(scope: &RunScope, path: &str) -> FileTarget {
  FileTarget::Run {
    scope: scope.clone(),
    path: path.to_string(),
  }
}

/// Inspect a file's pinned identity without requesting a download URL or payload.
pub fn file_metadata(config: &Path, target: &FileTarget) -> Result<FileRecord> {
  validate_target(target)?;
  let Response::File { file } = Api::new(config)?.request(&Request::GetFile {
    target: target.clone(),
  })?
  else {
    return Err(message("service did not return file metadata"));
  };
  if file.target != *target {
    return Err(message("service returned metadata for another file"));
  }
  if let Some(sha256) = &file.sha256 {
    validate_digest(sha256)?;
  }
  Ok(file)
}

/// Publish a durable reusable input reference to completed output bytes.
pub fn import_output(
  config: &Path,
  scope: &RunScope,
  path: &str,
  input_id: Option<&str>,
  size: u64,
  sha256: &str,
) -> Result<FileRecord> {
  validate_scope(scope)?;
  crate::run_artifacts::validate_path(path)?;
  if let Some(input_id) = input_id {
    validate_component(input_id)?;
  }
  validate_digest(sha256)?;
  let Response::File { file } = Api::new(config)?.request(&Request::ImportOutput {
    scope: scope.clone(),
    path: path.into(),
    input_id: input_id.map(str::to_string),
    size,
    sha256: sha256.into(),
  })?
  else {
    return Err(message("service did not return an imported output"));
  };
  let target_matches = matches!(&file.target, FileTarget::Input {
    project_id, input_id: returned,
  } if project_id == &scope.project_id && input_id.is_none_or(|expected| returned == expected));
  let producer = OutputProducer {
    origin: scope.origin.clone(),
    run_id: scope.run_id.clone(),
    path: path.into(),
  };
  if !target_matches
    || file.size != size
    || file.sha256.as_deref() != Some(sha256)
    || !matches!(file.storage, FileStorage::Object)
    || file
      .provenance
      .as_ref()
      .is_none_or(|value| value.producer != producer)
  {
    return Err(message("service returned an inconsistent imported output"));
  }
  validate_target(&file.target)?;
  Ok(file)
}

/// Create an identity-pinned reference; the service performs no object transfer.
pub fn reference(options: super::ReferenceOptions) -> Result<Value> {
  let source: FileTarget = serde_json::from_str(&options.source)?;
  let target: FileTarget = serde_json::from_str(&options.target)?;
  validate_target(&source)?;
  validate_target(&target)?;
  validate_digest(&options.sha256)?;
  let api = Api::new(&options.config)?;
  let Response::File { file } = api.request(&Request::ReferenceFile {
    source: source.clone(),
    target: target.clone(),
    size: options.size,
    sha256: options.sha256.clone(),
  })?
  else {
    return Err(message("service did not return a file reference"));
  };
  if file.target != target
    || file.size != options.size
    || file.sha256.as_deref() != Some(&options.sha256)
    || !matches!(file.storage, FileStorage::Object)
  {
    return Err(message("service returned an inconsistent file reference"));
  }
  Ok(json!({"source":source,"target":file.target,"size":file.size,"sha256":file.sha256}))
}

/// Explicit single-file publication, without sealing or archiving a historical run.
pub fn file_upload(options: super::FileUploadOptions) -> Result<Value> {
  let target: FileTarget = serde_json::from_str(&options.target)?;
  validate_target(&target)?;
  if matches!(&target, FileTarget::Run { path, .. } if path == "result.zip") {
    return Err(message("result.zip is managed by the server"));
  }
  let api = Api::new(&options.config)?;
  let path = std::path::absolute(&options.file)?;
  let mut source = fs::open(&path)?;
  let initial = source.metadata()?;
  let size = initial.len();
  let digest = fs::digest(&mut source, size)?;
  if !fs::unchanged(&initial, &source.metadata()?) {
    return Err(message("finalized artifact changed while reading"));
  }
  // An existing output may be a reference adopted from an older input upload.
  // Keep that object key instead of uploading identical bytes under a new key.
  match api.request(&Request::GetFile {
    target: target.clone(),
  }) {
    Ok(Response::File { file }) if file.target == target => {
      if matches!(file.storage, FileStorage::Object)
        && file.size == size
        && file.sha256.as_deref() == Some(&digest)
      {
        if !fs::unchanged(&initial, &fs::open(&path)?.metadata()?) {
          return Err(message("finalized artifact changed while reading"));
        }
        return Ok(json!({"target":target,"size":size,"sha256":digest,"reused":true}));
      }
    }
    Err(crate::error::ExpriError::ServiceRejected { status: 404, .. }) => {}
    Err(error) => return Err(error),
    _ => return Err(message("service returned an inconsistent file record")),
  }
  let target_bytes = serde_json::to_vec(&target)?;
  use sha2::{Digest, Sha256};
  let identity = Sha256::digest(&target_bytes)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect::<String>();
  let mut queue = Queue::new(
    std::path::absolute(options.queue_dir)?
      .join("files")
      .join(identity),
    json!({"endpoint":api.endpoint,"target":target}),
  )?;
  sync_file(
    &api,
    &mut queue,
    "file",
    target.clone(),
    &std::path::absolute(options.file)?,
    false,
  )?;
  let file = &queue.state.files["file"];
  Ok(json!({"target":target,"size":file.size,"sha256":file.sha256,"queue_dir":queue.directory}))
}
