use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use base64::{Engine, engine::general_purpose::STANDARD};

use super::queue::{Protocol, SavedDocument};
use super::*;

#[cfg(test)]
mod tests;

pub(super) fn negotiate(api: &Api, queue: &mut Queue) -> Result<Protocol> {
  if let Some(protocol) = queue.state.protocol {
    return Ok(protocol);
  }
  let protocol = if !queue.state.files.is_empty() || !queue.state.streams.is_empty() {
    Protocol::Legacy
  } else {
    match api.request(&Request::Capabilities) {
      Ok(Response::Capabilities { features }) => {
        if features.iter().any(|feature| feature == "tracking-v1") {
          Protocol::TrackingV1
        } else {
          Protocol::Legacy
        }
      }
      Err(crate::error::ExpriError::ServiceRejected { status: 400, .. }) => Protocol::Legacy,
      Err(error) => return Err(error),
      _ => return Err(message("service returned invalid protocol capabilities")),
    }
  };
  queue.state.protocol = Some(protocol);
  queue.save()?;
  Ok(protocol)
}

pub(super) fn sync_document(
  api: &Api,
  queue: &mut Queue,
  scope: &RunScope,
  path: &str,
  source: &Path,
) -> Result<()> {
  if !document_path(path) {
    return Err(message("invalid tracking document path"));
  }
  if queue
    .state
    .documents
    .get(path)
    .is_some_and(|saved| !saved.complete)
  {
    finish_document(api, queue, scope, path)?;
  }
  if queue
    .state
    .documents
    .get(path)
    .is_some_and(|saved| saved.complete)
  {
    discard_snapshot(queue, path)?;
  }
  let mut file = fs::open(source)?;
  let initial = file.metadata()?;
  if initial.len() > RECORD_LIMIT {
    return Err(message("run metadata exceeds the supported 16 MiB limit"));
  }
  let digest = fs::digest(&mut file, initial.len())?;
  if queue
    .state
    .documents
    .get(path)
    .is_some_and(|saved| saved.complete && saved.size == initial.len() && saved.sha256 == digest)
  {
    return Ok(());
  }
  let revision = queue
    .state
    .documents
    .get(path)
    .map_or(Some(1), |saved| saved.revision.checked_add(1))
    .filter(|revision| *revision <= i64::MAX as u64)
    .ok_or_else(|| message("tracking document revision exceeds its supported limit"))?;
  let mut snapshot = queue.allocate()?;
  file.seek(SeekFrom::Start(0))?;
  let size = std::io::copy(&mut file.take(initial.len()), &mut snapshot)?;
  if size != initial.len() || fs::digest(snapshot.as_file_mut(), size)? != digest {
    return Err(message(
      "run metadata changed while making its tracking snapshot",
    ));
  }
  snapshot.as_file().sync_all()?;
  let (_, snapshot) = snapshot.keep().map_err(|error| error.error)?;
  fs::sync_directory(&queue.directory)?;
  queue.state.documents.insert(
    path.into(),
    SavedDocument {
      revision,
      size,
      sha256: digest,
      snapshot: Some(snapshot.file_name().unwrap().to_str().unwrap().into()),
      offset: 0,
      complete: false,
    },
  );
  // The immutable snapshot and its revision exist durably before the first ACK.
  queue.save()?;
  finish_document(api, queue, scope, path)
}

fn finish_document(api: &Api, queue: &mut Queue, scope: &RunScope, path: &str) -> Result<()> {
  let saved = &queue.state.documents[path];
  if saved.revision == 0 || saved.revision > i64::MAX as u64 || saved.offset > saved.size {
    return Err(message("invalid queued tracking document progress"));
  }
  let snapshot = saved
    .snapshot
    .as_ref()
    .ok_or_else(|| message("queued document snapshot is missing"))?;
  validate_component(snapshot)?;
  validate_digest(&saved.sha256)?;
  let mut file = fs::open(&queue.directory.join(snapshot))?;
  if file.metadata()?.len() != saved.size || fs::digest(&mut file, saved.size)? != saved.sha256 {
    return Err(message("queued tracking document snapshot changed"));
  }
  let revision = saved.revision;
  let size = saved.size;
  let mut offset = saved.offset;
  let mut bytes = [0; STREAM_BATCH];
  loop {
    let length = (size - offset).min(STREAM_BATCH as u64) as usize;
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes[..length])?;
    let Response::DocumentAcknowledged {
      offset: acknowledged,
      revision: returned,
      complete,
    } = api.request(&Request::PutDocument {
      scope: scope.clone(),
      path: path.into(),
      revision,
      offset,
      total_size: size,
      data_base64: STANDARD.encode(&bytes[..length]),
    })?
    else {
      return Err(message("service did not acknowledge its tracking document"));
    };
    if returned != revision
      || acknowledged < offset + length as u64
      || acknowledged > size
      || complete != (acknowledged == size)
    {
      return Err(message(
        "service returned invalid tracking document progress",
      ));
    }
    offset = acknowledged;
    let saved = queue.state.documents.get_mut(path).unwrap();
    saved.offset = offset;
    saved.complete = complete;
    queue.save()?;
    if complete {
      break;
    }
  }
  discard_snapshot(queue, path)
}

fn discard_snapshot(queue: &mut Queue, path: &str) -> Result<()> {
  if let Some(snapshot) = queue.state.documents.get_mut(path).unwrap().snapshot.take() {
    validate_component(&snapshot)?;
    queue.save()?;
    fs::optional_regular(&queue.directory.join(&snapshot))?;
    std::fs::remove_file(queue.directory.join(snapshot))?;
    fs::sync_directory(&queue.directory)?;
  }
  Ok(())
}

pub(super) fn seal(api: &Api, queue: &mut Queue, scope: &RunScope) -> Result<()> {
  let documents: BTreeMap<_, _> = queue
    .state
    .documents
    .iter()
    .map(|(path, saved)| (path.clone(), saved.revision))
    .collect();
  if queue.state.documents.values().any(|saved| !saved.complete) {
    return Err(message("tracking documents are not fully acknowledged"));
  }
  let Response::Archive { archive } = api.request(&Request::SealRun {
    scope: scope.clone(),
    documents,
    streams: queue.state.streams.clone(),
    incomplete: false,
  })?
  else {
    return Err(message(
      "service did not acknowledge its sealed tracking data",
    ));
  };
  queue.state.archive = Some(archive_receipt(api, scope, archive, false)?);
  queue.save()
}

pub(super) fn archive_receipt(
  api: &Api,
  scope: &RunScope,
  mut archive: ArchiveRecord,
  incomplete: bool,
) -> Result<ArchiveRecord> {
  if !matches!(
    archive.status.as_str(),
    "pending" | "uploading" | "archived" | "failed"
  ) || archive.incomplete != incomplete
  {
    return Err(message(
      "service returned an invalid result.zip upload receipt",
    ));
  }
  if let Some(file) = &archive.file {
    if file.target != run_target(scope, "result.zip")
      || !matches!(file.storage, FileStorage::Object)
    {
      return Err(message(
        "service returned a result.zip upload from another run",
      ));
    }
    validate_digest(
      file
        .sha256
        .as_deref()
        .ok_or_else(|| message("result.zip upload receipt has no SHA256 digest"))?,
    )?;
  }
  archive.last_error = archive.last_error.map(|detail| api.redact(&detail));
  Ok(archive)
}
