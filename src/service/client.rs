mod checkpoints;
mod download;
pub(super) mod fs;
mod http;
mod inventory;
mod queue;
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
use super::{InputPutOptions, PushOptions};
use crate::error::Result;
use fs::message;
use http::Api;
use queue::{Queue, SavedFile};
use upload::{sync_file, sync_stream};

pub use download::{input_get, input_get_prepared, pull};
pub use watch::sync as sync_files;

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
const STREAMS: [&str; 3] = [
  "outputs/metrics.jsonl",
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
        "Service project has been deleted; publishing stopped. Use a new project identifier to publish",
      ),
      _ => None,
    }
  }

  pub(super) fn new(options: &PushOptions) -> Result<Self> {
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
        "result.zip is created by the service; select it with service pull",
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
    let metadata_done = push_cycle(
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
      "protocol": self.queue.state.protocol, "archive": self.queue.state.archive,
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

pub fn push(options: PushOptions) -> Result<Value> {
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
          eprintln!("Service sync pending; saved work will retry: {detail}");
          last_error = detail;
        }
        failures += 1;
      }
    }
    thread::sleep(Duration::from_secs(5));
  }
}

pub fn archive(config: &Path, scope: &RunScope, partial: bool) -> Result<Value> {
  validate_scope(scope)?;
  let api = Api::new(config)?;
  let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
    return Err(message("service did not return protocol capabilities"));
  };
  if !features.iter().any(|feature| feature == "tracking-v1") {
    return Err(message("service does not support tracking archives"));
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
    return Err(message("run has no synchronized tracking data to archive"));
  }
  let Response::Archive { archive } = api.request(&Request::SealRun {
    scope: scope.clone(),
    documents,
    streams,
    incomplete: partial,
  })?
  else {
    return Err(message("service did not acknowledge its tracking archive"));
  };
  serde_json::to_value(tracking::archive_receipt(&api, scope, archive, partial)?)
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

fn push_cycle(
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
    return Err(message("run identity changed while synchronizing"));
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
  if done {
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
  })
}

pub fn input_put(options: InputPutOptions) -> Result<Value> {
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
pub fn file_put(options: super::FilePutOptions) -> Result<Value> {
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
