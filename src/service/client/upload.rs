use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::blocking::Body;

use super::super::types::*;
use super::fs::message;
use super::{Api, Queue, RECORD_LIMIT, SavedFile, fs, validate_digest};
use crate::error::Result;

pub(super) fn sync_file(
  api: &Api,
  queue: &mut Queue,
  key: &str,
  target: FileTarget,
  path: &Path,
  snapshot: bool,
) -> Result<()> {
  sync_file_checked(api, queue, key, target, path, snapshot, None)
}

pub(super) fn sync_registered(
  api: &Api,
  queue: &mut Queue,
  key: &str,
  target: FileTarget,
  path: &Path,
  registration: &crate::service::registrations::Registration,
) -> Result<()> {
  sync_file_checked(api, queue, key, target, path, false, Some(registration))
}

fn sync_file_checked(
  api: &Api,
  queue: &mut Queue,
  key: &str,
  target: FileTarget,
  path: &Path,
  snapshot: bool,
  registration: Option<&crate::service::registrations::Registration>,
) -> Result<()> {
  validate_target(&target)?;
  verify_registration(path, registration)?;
  if queue
    .state
    .files
    .get(key)
    .is_some_and(|saved| !saved.upload.complete)
  {
    if queue.state.files[key].target != target {
      return Err(message("queued upload belongs to a different file"));
    }
    finish_upload(api, queue, key, path, registration)?;
  }
  let mut source = fs::open(path)?;
  let initial = source.metadata()?;
  if snapshot && initial.len() > RECORD_LIMIT {
    return Err(message("run metadata exceeds the supported 16 MiB limit"));
  }
  let sha256 = fs::digest(&mut source, initial.len())?;
  verify_registration(path, registration)?;
  if queue.state.files.get(key).is_some_and(|saved| {
    saved.target == target
      && saved.size == initial.len()
      && saved.sha256 == sha256
      && saved.upload.complete
  }) {
    return Ok(());
  }
  let mut temporary = queue.allocate()?;
  let upload_id = temporary
    .path()
    .file_name()
    .unwrap()
    .to_str()
    .unwrap()
    .to_string();
  let saved_snapshot = if snapshot {
    source.seek(SeekFrom::Start(0))?;
    let count = std::io::copy(&mut source.take(initial.len()), &mut temporary)?;
    if count != initial.len() || fs::digest(temporary.as_file_mut(), count)? != sha256 {
      return Err(message(
        "run metadata changed while making its upload snapshot",
      ));
    }
    temporary.as_file().sync_all()?;
    let (_, persisted) = temporary.keep().map_err(|error| error.error)?;
    Some(persisted.file_name().unwrap().to_str().unwrap().to_string())
  } else {
    if !fs::unchanged(&initial, &source.metadata()?) {
      return Err(message("finalized artifact changed while reading"));
    }
    None
  };
  queue.state.files.insert(
    key.to_string(),
    SavedFile {
      target,
      size: initial.len(),
      sha256,
      snapshot: saved_snapshot,
      upload: UploadState {
        upload_id,
        part_size: 0,
        parts: Vec::new(),
        complete: false,
      },
    },
  );
  // The generated upload ID is durable before the first server/object-store mutation.
  queue.save()?;
  finish_upload(api, queue, key, path, registration)
}

fn verify_registration(
  path: &Path,
  registration: Option<&crate::service::registrations::Registration>,
) -> Result<()> {
  if let Some(registration) = registration {
    crate::service::registrations::verify_metadata(registration, &fs::open(path)?.metadata()?)?;
  }
  Ok(())
}

pub(super) fn closed_prefix(file: &mut File, size: u64) -> Result<u64> {
  let mut end = size;
  let mut buffer = [0u8; STREAM_BATCH];
  while end > 0 {
    let start = end.saturating_sub(buffer.len() as u64);
    file.seek(SeekFrom::Start(start))?;
    let length = (end - start) as usize;
    file.read_exact(&mut buffer[..length])?;
    if let Some(index) = buffer[..length].iter().rposition(|byte| *byte == b'\n') {
      return Ok(start + index as u64 + 1);
    }
    end = start;
  }
  Ok(0)
}

pub(super) fn sync_stream(
  api: &Api,
  queue: &mut Queue,
  scope: &RunScope,
  path: &str,
  source: &Path,
  terminal: bool,
) -> Result<()> {
  let mut file = fs::open(source)?;
  let size = file.metadata()?.len();
  let size = if path == "outputs/metrics.jsonl" && !terminal {
    closed_prefix(&mut file, size)?
  } else {
    size
  };
  let mut offset = queue.state.streams.get(path).copied().unwrap_or(0);
  if size < offset {
    return Err(message(
      "run stream was truncated after synchronization; use a new run identity",
    ));
  }
  file.seek(SeekFrom::Start(offset))?;
  let mut buffer = [0u8; STREAM_BATCH];
  while offset < size {
    let length = (size - offset).min(buffer.len() as u64) as usize;
    file.read_exact(&mut buffer[..length])?;
    let data_base64 = STANDARD.encode(&buffer[..length]);
    let request = if queue.state.protocol == Some(super::queue::Protocol::TrackingV1) {
      Request::AppendTracking {
        scope: scope.clone(),
        path: path.into(),
        offset,
        data_base64,
      }
    } else {
      Request::AppendStream {
        scope: scope.clone(),
        path: path.into(),
        offset,
        data_base64,
      }
    };
    let response = api.request(&request)?;
    let Response::Acknowledged {
      offset: acknowledged,
    } = response
    else {
      return Err(message("service did not acknowledge its stream batch"));
    };
    if acknowledged != offset + length as u64 {
      return Err(message(
        "service stream acknowledgement has an invalid offset",
      ));
    }
    offset = acknowledged;
    queue.state.streams.insert(path.to_string(), offset);
    queue.save()?;
  }
  if queue.state.protocol == Some(super::queue::Protocol::TrackingV1)
    && !queue.state.streams.contains_key(path)
  {
    // Empty streams still have an explicit identity for the final seal.
    let Response::Acknowledged { offset: 0 } = api.request(&Request::AppendTracking {
      scope: scope.clone(),
      path: path.into(),
      offset: 0,
      data_base64: String::new(),
    })?
    else {
      return Err(message(
        "service did not acknowledge its empty tracking stream",
      ));
    };
    queue.state.streams.insert(path.into(), 0);
    queue.save()?;
  }
  Ok(())
}

fn finish_upload(
  api: &Api,
  queue: &mut Queue,
  key: &str,
  source: &Path,
  registration: Option<&crate::service::registrations::Registration>,
) -> Result<()> {
  verify_registration(source, registration)?;
  let saved = queue
    .state
    .files
    .get(key)
    .ok_or_else(|| message("upload queue entry is missing"))?;
  validate_target(&saved.target)?;
  validate_component(&saved.upload.upload_id)?;
  validate_digest(&saved.sha256)?;
  let path = match &saved.snapshot {
    Some(snapshot) => {
      validate_component(snapshot)?;
      queue.directory.join(snapshot)
    }
    None => source.to_path_buf(),
  };
  let mut file = fs::open(&path)?;
  let initial = file.metadata()?;
  if initial.len() != saved.size || fs::digest(&mut file, saved.size)? != saved.sha256 {
    return Err(message(
      "queued upload source changed; preserve the original finalized file",
    ));
  }
  let begin = Request::BeginUpload {
    upload_id: saved.upload.upload_id.clone(),
    target: saved.target.clone(),
    size: saved.size,
    sha256: saved.sha256.clone(),
  };
  let Response::Upload { mut upload } = api.request(&begin)? else {
    return Err(message("service did not return an upload state"));
  };
  if upload.upload_id != saved.upload.upload_id
    || upload.part_size < 5 * 1024 * 1024
    || upload.part_size > 5 * 1024 * 1024 * 1024
  {
    return Err(message(
      "service returned invalid multipart upload parameters",
    ));
  }
  let size = saved.size;
  let count = size.div_ceil(upload.part_size).max(1);
  if count > 1_000 {
    return Err(message("service multipart upload exceeds its part limit"));
  }
  let upload_id = upload.upload_id.clone();
  if upload.complete {
    upload.parts.clear();
  }
  queue.state.files.get_mut(key).unwrap().upload = upload;
  queue.save()?;
  if !queue.state.files[key].upload.complete {
    for number in 1..=count as u32 {
      if queue.state.files[key]
        .upload
        .parts
        .iter()
        .any(|part| part.part_number == number)
      {
        continue;
      }
      verify_registration(source, registration)?;
      let part_size = queue.state.files[key].upload.part_size;
      let offset = (number as u64 - 1) * part_size;
      let length = size.saturating_sub(offset).min(part_size);
      let url = api.signed_url(Request::PartUrl {
        upload_id: upload_id.clone(),
        part_number: number,
      })?;
      file.seek(SeekFrom::Start(offset))?;
      let body = Body::sized(file.try_clone()?.take(length), length);
      let response = api.objects.put(&url).body(body).send().map_err(|_| {
        message("object part upload failed; acknowledged parts are saved for retry")
      })?;
      if !response.status().is_success() {
        return Err(message(format!(
          "object part upload returned {}",
          response.status()
        )));
      }
      let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| message("object part upload returned no valid ETag"))?
        .to_string();
      let part = CompletedPart {
        part_number: number,
        etag,
      };
      let Response::Upload { upload } = api.request(&Request::RecordPart {
        upload_id: upload_id.clone(),
        part,
      })?
      else {
        return Err(message("service did not acknowledge the object part"));
      };
      if upload.upload_id != upload_id
        || upload.part_size != part_size
        || !upload.parts.iter().any(|part| part.part_number == number)
      {
        return Err(message("service returned inconsistent multipart progress"));
      }
      verify_registration(source, registration)?;
      queue.state.files.get_mut(key).unwrap().upload = upload;
      queue.save()?;
    }
    let verified = (|| -> Result<bool> {
      Ok(
        fs::unchanged(&initial, &file.metadata()?)
          && fs::digest(&mut file, size)? == queue.state.files[key].sha256
          && fs::unchanged(&initial, &file.metadata()?),
      )
    })();
    if !matches!(verified, Ok(true)) {
      // Acknowledged parts may contain different bytes from the queued identity.
      // Even restoring the source later cannot make those receipts safe to reuse.
      discard_upload(queue, key)?;
      return Err(message(
        "finalized artifact changed or could not be verified while uploading; upload was not published and its receipts were discarded",
      ));
    }
    verify_registration(source, registration)?;
    let Response::File { file: record } = api.request(&Request::CompleteUpload { upload_id })?
    else {
      return Err(message("service did not publish the completed upload"));
    };
    let expected = &queue.state.files[key];
    if record.target != expected.target
      || record.size != expected.size
      || record.sha256.as_deref() != Some(expected.sha256.as_str())
    {
      return Err(message(
        "service completed upload does not match its queued identity",
      ));
    }
    queue.state.files.get_mut(key).unwrap().upload.complete = true;
    queue.state.files.get_mut(key).unwrap().upload.parts.clear();
    queue.save()?;
  }
  if let Some(snapshot) = queue.state.files.get_mut(key).unwrap().snapshot.take() {
    queue.save()?;
    fs::optional_regular(&queue.directory.join(&snapshot))?;
    std::fs::remove_file(queue.directory.join(snapshot))?;
  }
  Ok(())
}

fn discard_upload(queue: &mut Queue, key: &str) -> Result<()> {
  let saved = queue
    .state
    .files
    .remove(key)
    .ok_or_else(|| message("upload queue entry is missing"))?;
  // Retire the upload identity durably before removing its owned snapshot.
  queue.save()?;
  if let Some(snapshot) = saved.snapshot {
    validate_component(&snapshot)?;
    let path = queue.directory.join(snapshot);
    fs::optional_regular(&path)?;
    if fs::inspect(&path)?.is_some() {
      std::fs::remove_file(path)?;
      fs::sync_directory(&queue.directory)?;
    }
  }
  Ok(())
}
