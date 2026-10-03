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

fn read_stream(api: &Api, record: &FileRecord, output: &mut File) -> Result<()> {
  let FileTarget::Run { scope, path } = &record.target else {
    return Err(message("private inputs cannot use stream storage"));
  };
  if !stream_path(path) {
    return Err(message("service returned an invalid stream path"));
  }
  let mut offset = 0u64;
  while offset < record.size {
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
) -> Result<()> {
  let url = api.signed_url(Request::DownloadUrl {
    target: record.target.clone(),
  })?;
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
  let copied = std::io::copy(&mut response.take(length + 1), output)
    .map_err(|_| message("object download failed while reading or saving its bytes"))?;
  if copied != length {
    return Err(message(
      "object download size does not match its service record",
    ));
  }
  Ok(())
}

fn download(api: &Api, record: &FileRecord, output: &mut File) -> Result<()> {
  validate_target(&record.target)?;
  match record.storage {
    FileStorage::Stream => read_stream(api, record, output)?,
    FileStorage::Object => {
      let digest = record
        .sha256
        .as_deref()
        .ok_or_else(|| message("object record has no SHA256 digest"))?;
      validate_digest(digest)?;
      let mut offset = 0u64;
      loop {
        let length = (record.size - offset).min(OBJECT_BATCH);
        let mut error = None;
        for attempt in 0..3 {
          output.set_len(offset)?;
          output.seek(SeekFrom::Start(offset))?;
          match object_range(api, record, offset, length, output) {
            Ok(()) => {
              error = None;
              break;
            }
            Err(value) => error = Some(value),
          }
          if attempt < 2 {
            thread::sleep(Duration::from_millis(250 * (attempt + 1)));
          }
        }
        if let Some(error) = error {
          return Err(error);
        }
        offset += length;
        if offset == record.size {
          break;
        }
      }
      if fs::digest(output, record.size)? != digest {
        return Err(message(
          "download SHA256 does not match its service record; previous files were retained",
        ));
      }
    }
  }
  if output.metadata()?.len() != record.size {
    return Err(message("download has an unexpected size"));
  }
  output.sync_all()?;
  Ok(())
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
  let mut selected = BTreeMap::new();
  for file in files {
    validate_target(&file.target)?;
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
    if (METADATA.contains(&path.as_str())
      || STREAMS.contains(&path.as_str())
      || artifacts.contains(path))
      && selected.insert(path.clone(), file).is_some()
    {
      return Err(message("service returned duplicate file records"));
    }
  }
  if !selected.contains_key("run-state.json") {
    return Err(message(
      "run state is not synchronized yet; retry after its metadata upload",
    ));
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
  let staging = tempfile::Builder::new()
    .prefix(".service-pull-")
    .tempdir_in(&runs_dir)?;
  for (path, record) in &selected {
    let staged = staging.path().join(path);
    fs::directories(staged.parent().unwrap())?;
    let mut file = std::fs::OpenOptions::new()
      .write(true)
      .read(true)
      .create_new(true)
      .open(staged)?;
    download(&api, record, &mut file)?;
  }
  let state: Value = serde_json::from_slice(&fs::read_bounded(
    &staging.path().join("run-state.json"),
    RECORD_LIMIT,
  )?)?;
  if state.get("run_id").and_then(Value::as_str) != Some(scope.run_id.as_str()) {
    return Err(message("downloaded run state belongs to another run"));
  }
  match std::fs::create_dir(&destination) {
    Ok(()) => {
      fs::atomic_json(&destination.join(".pull-owner.json"), &owner)?;
      fs::sync_directory(&runs_dir)?;
    }
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
      cache_owner(&destination, &owner)?
    }
    Err(error) => return Err(error.into()),
  }
  let LockAttempt::Acquired(_lease) =
    crate::lock::try_lock_file(&destination.join(".pull.lock"), true)?
  else {
    return Err(message(
      "another pull is publishing this run; retry after it finishes",
    ));
  };
  cache_owner(&destination, &owner)?;
  fs::optional_regular(&destination.join("pull-state.json"))?;
  for path in selected.keys() {
    let target = destination.join(path);
    fs::directories(target.parent().unwrap())?;
    fs::optional_regular(&target)?;
  }
  for path in selected
    .keys()
    .filter(|path| path.as_str() != "run-state.json")
  {
    std::fs::rename(staging.path().join(path), destination.join(path))?;
  }
  std::fs::rename(
    staging.path().join("run-state.json"),
    destination.join("run-state.json"),
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
  });
  fs::atomic_json(&destination.join("pull-state.json"), &receipt)?;
  Ok(
    json!({"scope": scope, "source": source, "destination": destination, "files": selected.keys().collect::<Vec<_>>()}),
  )
}

pub fn input_get(options: InputGetOptions) -> Result<Value> {
  let api = Api::new(&options.config)?;
  let target = FileTarget::Input {
    project_id: options.project_id,
    input_id: options.input_id,
  };
  validate_target(&target)?;
  let Response::File { file } = api.request(&Request::GetFile {
    target: target.clone(),
  })?
  else {
    return Err(message("service did not return an input record"));
  };
  if file.target != target || !matches!(file.storage, FileStorage::Object) {
    return Err(message("service returned an inconsistent input record"));
  }
  let destination = std::path::absolute(options.destination)?;
  let parent = destination
    .parent()
    .ok_or_else(|| message("input destination has no parent"))?;
  fs::directories(parent)?;
  fs::optional_regular(&destination)?;
  let mut staging = tempfile::NamedTempFile::new_in(parent)?;
  download(&api, &file, staging.as_file_mut())?;
  fs::optional_regular(&destination)?;
  staging.persist(&destination).map_err(|error| error.error)?;
  fs::sync_directory(parent)?;
  Ok(
    json!({"target": target, "destination": destination, "size": file.size, "sha256": file.sha256}),
  )
}
