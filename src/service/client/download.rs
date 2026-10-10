use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use super::super::types::*;
use super::super::{InputGetOptions, PullOptions};
use super::fs::message;
use super::{Api, METADATA, OBJECT_BATCH, RECORD_LIMIT, STREAMS, artifacts, fs, validate_digest};
use crate::error::Result;
use crate::lock::LockAttempt;

mod checkpoints;
mod initialization;
#[cfg(test)]
pub(in crate::service::client) mod mock;
mod staging;
#[cfg(test)]
mod tests;

pub(super) use checkpoints::{ObjectResult, pull_objects};

pub(super) fn validate_record(record: &FileRecord) -> Result<()> {
  validate_target(&record.target)?;
  match record.storage {
    FileStorage::Object => validate_digest(
      record
        .sha256
        .as_deref()
        .ok_or_else(|| message("object record has no SHA256 digest"))?,
    )?,
    FileStorage::Stream if record.sha256.is_some() => {
      return Err(message(
        "stream record unexpectedly contains a SHA256 digest",
      ));
    }
    FileStorage::Stream => {}
    FileStorage::Tracking { revision, .. } => {
      let FileTarget::Run { path, .. } = &record.target else {
        return Err(message("private inputs cannot use tracking storage"));
      };
      if revision == 0
        || revision > i64::MAX as u64
        || (!stream_path(path) && !document_path(path))
        || (document_path(path) && record.size > RECORD_LIMIT)
      {
        return Err(message("invalid tracking file record"));
      }
      if let Some(digest) = &record.sha256 {
        validate_digest(digest)?;
      }
    }
  }
  Ok(())
}

fn stream_batch(api: &Api, record: &FileRecord, offset: u64) -> Result<Vec<u8>> {
  let FileTarget::Run { scope, path } = &record.target else {
    return Err(message("private inputs cannot use stream storage"));
  };
  if !stream_path(path)
    && !(matches!(record.storage, FileStorage::Tracking { .. }) && document_path(path))
  {
    return Err(message("service returned an invalid stream path"));
  }
  let limit = (record.size - offset).min(STREAM_BATCH as u64) as usize;
  let Response::Stream {
    offset: returned,
    total_size,
    data_base64,
  } = api.request(&Request::ReadStream {
    scope: scope.clone(),
    path: path.clone(),
    offset,
    limit,
  })?
  else {
    return Err(message("service did not return its stream batch"));
  };
  let bytes = STANDARD
    .decode(data_base64)
    .map_err(|_| message("invalid service stream encoding"))?;
  if returned != offset || total_size < record.size || bytes.is_empty() || bytes.len() > limit {
    return Err(message(
      "service stream changed or returned an invalid byte range",
    ));
  }
  Ok(bytes)
}

fn read_stream(api: &Api, record: &FileRecord, output: &mut File) -> Result<()> {
  let mut offset = 0u64;
  while offset < record.size {
    let bytes = stream_batch(api, record, offset)?;
    output.write_all(&bytes)?;
    offset += bytes.len() as u64;
  }
  Ok(())
}

fn object_range(
  api: &Api,
  record: &FileRecord,
  offset: u64,
  length: u64,
  output: &mut File,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  fs::check_cancelled(cancelled)?;
  let url = api.signed_url(Request::DownloadUrl {
    target: record.target.clone(),
  })?;
  fs::check_cancelled(cancelled)?;
  let mut request = api.objects.get(&url);
  if record.size > 0 {
    request = request.header(
      reqwest::header::RANGE,
      format!("bytes={offset}-{}", offset + length - 1),
    );
  }
  let response = request
    .send()
    .map_err(|_| message("object download failed"))?;
  fs::check_cancelled(cancelled)?;
  let expected_status = if record.size == 0 {
    reqwest::StatusCode::OK
  } else {
    reqwest::StatusCode::PARTIAL_CONTENT
  };
  if response.status() != expected_status {
    return Err(message(format!(
      "object download returned {}",
      response.status()
    )));
  }
  if record.size > 0 {
    let range = format!("bytes {offset}-{}/{}", offset + length - 1, record.size);
    if response
      .headers()
      .get(reqwest::header::CONTENT_RANGE)
      .and_then(|value| value.to_str().ok())
      != Some(range.as_str())
    {
      return Err(message(
        "object download returned an inconsistent byte range",
      ));
    }
  }
  let mut response = response.take(length + 1);
  let mut copied = 0u64;
  let mut buffer = [0u8; 64 * 1024];
  loop {
    fs::check_cancelled(cancelled)?;
    let count = response
      .read(&mut buffer)
      .map_err(|_| message("object download failed while reading or saving its bytes"))?;
    if count == 0 {
      break;
    }
    output
      .write_all(&buffer[..count])
      .map_err(|_| message("object download failed while reading or saving its bytes"))?;
    copied += count as u64;
  }
  if copied != length {
    return Err(message(
      "object download size does not match its service record",
    ));
  }
  Ok(())
}

fn retry_range(
  api: &Api,
  record: &FileRecord,
  offset: u64,
  length: u64,
  output: &mut File,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  let mut last_error = None;
  for attempt in 0..3 {
    fs::check_cancelled(cancelled)?;
    output.set_len(offset)?;
    output.seek(SeekFrom::Start(offset))?;
    match object_range(api, record, offset, length, output, cancelled) {
      Ok(()) => return Ok(()),
      Err(error @ crate::error::ExpriError::DownloadCancelled) => return Err(error),
      Err(error) => last_error = Some(error),
    }
    if attempt < 2 {
      thread::sleep(Duration::from_millis(250 * (attempt + 1)));
    }
  }
  Err(last_error.expect("range attempts return an error"))
}

fn staged_download(
  api: &Api,
  staging: &mut staging::Staging,
  path: &str,
  record: &FileRecord,
) -> Result<(u64, u64)> {
  staged_download_with_cancel(api, staging, path, record, &mut || Ok(false))
}

fn staged_download_with_cancel(
  api: &Api,
  staging: &mut staging::Staging,
  path: &str,
  record: &FileRecord,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<(u64, u64)> {
  fs::check_cancelled(cancelled)?;
  validate_record(record)?;
  let resumable = matches!(record.storage, FileStorage::Tracking { .. })
    || (matches!(record.storage, FileStorage::Object)
      && !METADATA.contains(&path)
      && !STREAMS.contains(&path));
  let (mut output, initial_offset) = staging.prepare(path, record, resumable)?;
  match record.storage {
    FileStorage::Stream => read_stream(api, record, &mut output)?,
    FileStorage::Tracking { revision, .. } => {
      let mut offset = initial_offset;
      while offset < record.size {
        let bytes = stream_batch(api, record, offset)?;
        output.write_all(&bytes)?;
        offset += bytes.len() as u64;
        staging.acknowledge(path, offset, false, &output)?;
      }
      let Response::File { file: current } = api.request(&Request::GetFile {
        target: record.target.clone(),
      })?
      else {
        return Err(message("service did not confirm its tracking revision"));
      };
      validate_record(&current)?;
      let unchanged = current.target == record.target
        && matches!(current.storage, FileStorage::Tracking { revision: current_revision, .. } if current_revision == revision)
        && if stream_path(path) {
          current.size >= record.size
        } else {
          current.size == record.size
        };
      if !unchanged {
        drop(output);
        staging.reset(path)?;
        return Err(message(
          "tracking revision changed during download; retry with its current catalog",
        ));
      }
      if let Some(digest) = &record.sha256
        && fs::digest(&mut output, record.size)? != *digest
      {
        drop(output);
        staging.reset(path)?;
        return Err(message(
          "tracking download SHA256 does not match its service record",
        ));
      }
    }
    FileStorage::Object => {
      let mut offset = initial_offset;
      // Empty objects still need one successful GET before they can be verified.
      let mut empty_get = record.size == 0 && !staging.verified(path);
      while offset < record.size || empty_get {
        let length = (record.size - offset).min(OBJECT_BATCH);
        retry_range(api, record, offset, length, &mut output, cancelled)?;
        fs::check_cancelled(cancelled)?;
        offset += length;
        empty_get = false;
        staging.acknowledge(path, offset, false, &output)?;
        if offset == record.size {
          break;
        }
      }
      if fs::digest_with_cancel(&mut output, record.size, cancelled)?
        != record.sha256.as_deref().unwrap()
      {
        // A corrupt prefix must not make every subsequent retry fail forever.
        drop(output);
        staging.reset(path)?;
        return Err(message(
          "download SHA256 does not match its service record; saved progress was reset and previous files were retained",
        ));
      }
    }
  }
  if output.metadata()?.len() != record.size {
    return Err(message("download has an unexpected size"));
  }
  fs::check_cancelled(cancelled)?;
  staging.acknowledge(path, record.size, true, &output)?;
  Ok((initial_offset, record.size - initial_offset))
}

fn publish(staged: &Path, destination: &Path) -> Result<()> {
  fs::optional_regular(staged)?;
  fs::optional_regular(destination)?;
  let parent = destination.parent().unwrap();
  // Preserve the verified staging inode until the receipt is committed. This
  // avoids copying large checkpoints and makes interrupted publication retryable.
  let temporary = tempfile::Builder::new()
    .prefix(".service-publish-")
    .tempdir_in(parent)?;
  let link = temporary.path().join("file");
  std::fs::hard_link(staged, &link)?;
  std::fs::rename(link, destination)?;
  fs::sync_directory(parent)
}

fn cache_owner(destination: &Path, expected: &Value) -> Result<()> {
  fs::directory(destination)?;
  let path = destination.join(".pull-owner.json");
  let actual: Value = serde_json::from_slice(
    &fs::read_bounded(&path, 16 * 1024)
      .map_err(|_| message("refusing to update an unowned service run cache"))?,
  )?;
  if actual != *expected {
    return Err(message(
      "run cache belongs to a different service origin; choose another --source",
    ));
  }
  Ok(())
}

pub fn pull(options: PullOptions) -> Result<Value> {
  pull_impl(options, None)
}

pub(super) fn pull_pinned(
  options: PullOptions,
  expected: &BTreeMap<String, FileRecord>,
) -> Result<Value> {
  pull_impl(options, Some(expected))
}

fn verify_expected_objects(
  selected: &BTreeMap<String, FileRecord>,
  artifacts: &std::collections::BTreeSet<String>,
  expected: &BTreeMap<String, FileRecord>,
) -> Result<()> {
  if expected.len() != artifacts.len()
    || artifacts.iter().any(|path| {
      !expected.get(path).is_some_and(|expected| {
        selected.get(path).is_some_and(|current| {
          matches!(expected.storage, FileStorage::Object)
            && matches!(current.storage, FileStorage::Object)
            && current.target == expected.target
            && current.size == expected.size
            && current.sha256 == expected.sha256
        })
      })
    })
  {
    return Err(crate::error::ExpriError::DownloadChanged);
  }
  Ok(())
}

fn pull_impl(
  options: PullOptions,
  expected: Option<&BTreeMap<String, FileRecord>>,
) -> Result<Value> {
  let api = Api::new(&options.config)?;
  let scope = RunScope {
    project_id: options.project_id,
    origin: options.origin,
    run_id: options.run_id,
  };
  validate_scope(&scope)?;
  let artifacts = artifacts(&options.artifacts)?;
  let Response::Files { files } = api.request(&Request::ListFiles {
    scope: scope.clone(),
  })?
  else {
    return Err(message("service did not return a file catalog"));
  };
  if files.len() > 1000 {
    return Err(message("service file catalog exceeds its size limit"));
  }
  let mut selected = BTreeMap::new();
  let mut object_records = BTreeMap::new();
  let mut available_files = Vec::new();
  let mut available_files_truncated = false;
  let mut seen = std::collections::BTreeSet::new();
  for file in files {
    validate_record(&file)?;
    let FileTarget::Run {
      scope: returned,
      path,
    } = &file.target
    else {
      return Err(message(
        "run catalog unexpectedly contains private input files",
      ));
    };
    if *returned != scope {
      return Err(message("run catalog contains files from another origin"));
    }
    if !seen.insert(path.clone()) {
      return Err(message("service returned duplicate file records"));
    }
    if crate::run_artifacts::validate_path(path).is_ok() {
      if matches!(file.storage, FileStorage::Object) {
        object_records.insert(path.clone(), file.clone());
      }
      if available_files.len() < 200 {
        let mut available = json!({"path": path, "size": file.size});
        if let Some(digest) = &file.sha256 {
          available["sha256"] = json!(digest);
        }
        available_files.push(available);
        if serde_json::to_vec_pretty(&available_files)?.len() + 1 > 64 * 1024 {
          available_files.pop();
          available_files_truncated = true;
        }
      } else {
        available_files_truncated = true;
      }
    }
    if METADATA.contains(&path.as_str())
      || STREAMS.contains(&path.as_str())
      || artifacts.contains(path)
    {
      selected.insert(path.clone(), file);
    }
  }
  if !selected.contains_key("run-state.json") {
    return Err(message(
      "run state is not published yet; retry after its metadata upload",
    ));
  }
  if let Some(expected) = expected {
    verify_expected_objects(&selected, &artifacts, expected)?;
  }
  for artifact in &artifacts {
    if !selected.contains_key(artifact) {
      return Err(message(format!(
        "selected artifact is not available: {artifact}"
      )));
    }
  }
  let source = options
    .source
    .unwrap_or_else(|| format!("service-{}-{}", scope.project_id, scope.origin));
  let results_dir = options
    .results_dir
    .to_str()
    .ok_or_else(|| message("results directory must be UTF-8"))?;
  let runs_dir = crate::controller::run_pull::cached_runs_dir(&options.repo, results_dir, &source)?;
  let destination = runs_dir.join(&scope.run_id);
  let owner = json!({"schema_version": 1, "service_endpoint": api.endpoint, "source": source, "scope": scope});
  if fs::inspect(&destination)?.is_some() {
    cache_owner(&destination, &owner)?;
  }
  fs::optional_regular(&destination.join("pull-state.json"))?;
  for path in selected.keys() {
    fs::optional_regular(&destination.join(path))?;
  }
  fs::directories(&runs_dir)?;
  let mut staging = staging::Staging::open(
    runs_dir
      .parent()
      .unwrap()
      .join(".service-pull")
      .join(&scope.run_id),
    &owner,
  )?;
  staging.select(&selected)?;
  let mut cache_lease = if fs::inspect(&destination)?.is_some() {
    cache_owner(&destination, &owner)?;
    let LockAttempt::Acquired(lease) =
      crate::lock::try_lock_file(&destination.join(".pull.lock"), true)?
    else {
      return Err(message(
        "another download is using this run cache; retry after it finishes",
      ));
    };
    Some(lease)
  } else {
    None
  };
  fs::optional_regular(&destination.join("pull-state.json"))?;
  for path in selected.keys() {
    let target = destination.join(path);
    fs::optional_regular(&target)?;
  }
  let mut resumed_files = 0usize;
  let mut resumed_bytes = 0u64;
  let mut downloaded_bytes = 0u64;
  let mut reused_bytes = 0u64;
  for (path, record) in &selected {
    if crate::run_artifacts::validate_path(path).is_ok()
      && staging.reuse(path, record, &destination.join(path))?
    {
      reused_bytes = reused_bytes.saturating_add(record.size);
      continue;
    }
    let (resumed, downloaded) = staged_download(&api, &mut staging, path, record)?;
    resumed_files += usize::from(resumed > 0);
    resumed_bytes = resumed_bytes.saturating_add(resumed);
    downloaded_bytes = downloaded_bytes.saturating_add(downloaded);
  }
  let state: Value = serde_json::from_slice(&fs::read_bounded(
    &staging.path("run-state.json"),
    RECORD_LIMIT,
  )?)?;
  if state.get("run_id").and_then(Value::as_str) != Some(scope.run_id.as_str()) {
    return Err(message("downloaded run state belongs to another run"));
  }
  initialization::initialize(&destination, ".pull-owner.json", &owner, None)?;
  cache_owner(&destination, &owner)?;
  if cache_lease.is_none() {
    let LockAttempt::Acquired(lease) =
      crate::lock::try_lock_file(&destination.join(".pull.lock"), true)?
    else {
      return Err(message(
        "another download is using this run cache; retry after it finishes",
      ));
    };
    cache_lease = Some(lease);
  }
  cache_owner(&destination, &owner)?;
  for path in selected.keys() {
    let target = destination.join(path);
    fs::directories(target.parent().unwrap())?;
    fs::optional_regular(&target)?;
  }
  for path in selected
    .keys()
    .filter(|path| path.as_str() != "run-state.json")
  {
    publish(&staging.path(path), &destination.join(path))?;
  }
  publish(
    &staging.path("run-state.json"),
    &destination.join("run-state.json"),
  )?;
  let parents = selected
    .keys()
    .map(|path| destination.join(path).parent().unwrap().to_path_buf())
    .collect::<std::collections::BTreeSet<_>>();
  for parent in parents {
    fs::sync_directory(&parent)?;
  }
  let receipt = json!({
    "schema_version": 1, "target_name": source, "scope": scope, "service_endpoint": api.endpoint,
    "pulled_at": chrono::Utc::now().to_rfc3339(), "selected_files": selected.keys().collect::<Vec<_>>(),
    "available_files": available_files,
    "available_files_truncated": available_files_truncated,
    "downloaded_files": downloaded_files(&destination, &object_records, &selected)?,
  });
  fs::atomic_json(&destination.join("pull-state.json"), &receipt)?;
  staging.retain_tracking()?;
  drop(cache_lease);
  Ok(
    json!({"scope": scope, "source": source, "destination": destination, "files": selected.keys().collect::<Vec<_>>(),
      "resumed_files": resumed_files, "resumed_bytes": resumed_bytes,
      "reused_bytes": reused_bytes, "downloaded_bytes": downloaded_bytes}),
  )
}

pub fn input_get(options: InputGetOptions) -> Result<Value> {
  input_get_bound(&options, &mut || Ok(false), &mut |_, _, _| Ok(()))
}

type InputBinder<'a> =
  dyn FnMut(&Path, &Value, &mut dyn FnMut() -> Result<bool>) -> Result<()> + 'a;

pub fn input_get_prepared(
  options: InputGetOptions,
  cancelled: &mut dyn FnMut() -> Result<bool>,
  bind: &mut InputBinder<'_>,
) -> Result<Value> {
  loop {
    fs::check_cancelled(cancelled)?;
    match input_get_bound(&options, cancelled, bind) {
      Err(crate::error::ExpriError::DownloadBusy { .. }) => {
        thread::sleep(Duration::from_millis(200));
      }
      result => return result,
    }
  }
}

fn bound_input_report(
  destination: &Path,
  report: Value,
  cancelled: &mut dyn FnMut() -> Result<bool>,
  bind: &mut InputBinder<'_>,
) -> Result<Value> {
  fs::check_cancelled(cancelled)?;
  bind(destination, &report, cancelled)?;
  Ok(report)
}

fn input_get_bound(
  options: &InputGetOptions,
  cancelled: &mut dyn FnMut() -> Result<bool>,
  bind: &mut InputBinder<'_>,
) -> Result<Value> {
  fs::check_cancelled(cancelled)?;
  let api = Api::new(&options.config)?;
  let target = FileTarget::Input {
    project_id: options.project_id.clone(),
    input_id: options.input_id.clone(),
  };
  validate_target(&target)?;
  let destination = std::path::absolute(&options.destination)?;
  let parent = destination
    .parent()
    .ok_or_else(|| message("input destination has no parent"))?;
  fs::directories(parent)?;
  fs::optional_regular(&destination)?;
  let filename = destination
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| message("input destination filename must be UTF-8"))?;
  let owner = json!({"schema_version": 1, "service_endpoint": api.endpoint,
    "target": target, "destination": destination});
  let mut staging =
    staging::Staging::open(parent.join(".expri-input-downloads").join(filename), &owner)?;
  let cached_record = staging.directory.join("input-record.json");
  fs::check_cancelled(cancelled)?;
  let file = match api.request(&Request::GetFile {
    target: target.clone(),
  }) {
    Ok(Response::File { file }) => file,
    Ok(_) => return Err(message("service did not return an input record")),
    Err(error) if offline_input_error(&error) && fs::inspect(&cached_record)?.is_some() => {
      let file: FileRecord = serde_json::from_slice(&fs::read_bounded(&cached_record, 16 * 1024)?)?;
      validate_input_record(&file, &target)?;
      if staging::verified_source_with_cancel(&file, &destination, cancelled)?.is_none() {
        return Err(error);
      }
      return bound_input_report(
        &destination,
        json!({"target": target, "destination": destination, "size": file.size,
        "sha256": file.sha256, "reused": true, "offline": true,
        "resumed_bytes": 0, "downloaded_bytes": 0}),
        cancelled,
        bind,
      );
    }
    Err(error) => return Err(error),
  };
  fs::check_cancelled(cancelled)?;
  validate_input_record(&file, &target)?;
  if staging::verified_source_with_cancel(&file, &destination, cancelled)?.is_some() {
    fs::atomic_json(&cached_record, &file)?;
    staging.clear()?;
    return bound_input_report(
      &destination,
      json!({"target":target,"destination":destination,"size":file.size,"sha256":file.sha256,
      "reused":true,"offline":false,"resumed_bytes":0,"downloaded_bytes":0}),
      cancelled,
      bind,
    );
  }
  staging.select(&BTreeMap::from([("input".into(), file.clone())]))?;
  let (resumed, downloaded) =
    staged_download_with_cancel(&api, &mut staging, "input", &file, cancelled)?;
  fs::check_cancelled(cancelled)?;
  publish(&staging.path("input"), &destination)?;
  fs::atomic_json(&cached_record, &file)?;
  staging.clear()?;
  bound_input_report(
    &destination,
    json!({"target": target, "destination": destination, "size": file.size, "sha256": file.sha256,
      "reused": false, "offline": false, "resumed_bytes": resumed, "downloaded_bytes": downloaded}),
    cancelled,
    bind,
  )
}

fn validate_input_record(file: &FileRecord, target: &FileTarget) -> Result<()> {
  if file.target != *target || !matches!(file.storage, FileStorage::Object) {
    return Err(message("service returned an inconsistent input record"));
  }
  validate_record(file)
}

fn offline_input_error(error: &crate::error::ExpriError) -> bool {
  matches!(
    error,
    crate::error::ExpriError::ServiceRejected {
      status: 429 | 500..=599,
      ..
    } | crate::error::ExpriError::ServiceUnavailable { .. }
  )
}

fn modified(metadata: &std::fs::Metadata) -> Option<Value> {
  let time = metadata
    .modified()
    .ok()?
    .duration_since(std::time::UNIX_EPOCH)
    .ok()?;
  Some(json!([time.as_secs(), time.subsec_nanos()]))
}

fn downloaded_files(
  destination: &Path,
  catalog: &BTreeMap<String, FileRecord>,
  selected: &BTreeMap<String, FileRecord>,
) -> Result<Vec<Value>> {
  let mut downloaded = BTreeMap::new();
  let previous = destination.join("pull-state.json");
  if fs::inspect(&previous)?.is_some() {
    let receipt: Value = serde_json::from_slice(&fs::read_bounded(&previous, 256 * 1024)?)?;
    for saved in receipt["downloaded_files"]
      .as_array()
      .into_iter()
      .flatten()
      .take(200)
    {
      let Some(path) = saved["path"].as_str() else {
        continue;
      };
      let Some(record) = catalog.get(path) else {
        continue;
      };
      if saved["size"].as_u64() != Some(record.size)
        || saved["sha256"].as_str() != record.sha256.as_deref()
      {
        continue;
      }
      fs::optional_regular(&destination.join(path))?;
      if let Some(metadata) = fs::inspect(&destination.join(path))?
        && metadata.len() == record.size
        && modified(&metadata).as_ref() == Some(&saved["modified"])
      {
        downloaded.insert(path.to_string(), saved.clone());
      }
    }
  }
  for (path, record) in selected {
    if !catalog.contains_key(path) {
      continue;
    }
    let metadata = std::fs::symlink_metadata(destination.join(path))?;
    downloaded.insert(
      path.clone(),
      json!({"path": path, "size": record.size,
      "sha256": record.sha256, "modified": modified(&metadata)}),
    );
  }
  // Receipts are bounded even if the selection changes many times.
  let mut files = Vec::new();
  for file in downloaded.into_values().take(200) {
    files.push(file);
    if serde_json::to_vec(&files)?.len() > 64 * 1024 {
      files.pop();
      break;
    }
  }
  Ok(files)
}
