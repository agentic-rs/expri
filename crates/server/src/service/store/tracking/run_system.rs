//! A full seal must include the capture's acknowledged completion record.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use expri_core::run_system::{RUN_SYSTEM_PATH, RunSystemRecord};
use serde_json::Value;

use super::{ApiError, ApiResult, FileRecord, FileStorage, FileTarget, open_raw};

fn invalid() -> ApiError {
  ApiError::new(
    409,
    "run system capture is incomplete or does not match run metadata",
  )
}
fn time(value: &Value) -> Option<i64> {
  chrono::DateTime::parse_from_rfc3339(value.as_str()?)
    .ok()
    .map(|value| value.timestamp_millis())
}

pub(super) fn validate_seal(
  directory: &Path,
  files: &[FileRecord],
  state: &Value,
) -> ApiResult<()> {
  let Some(capture_id) = state["system_capture_id"].as_str() else {
    return Ok(());
  };
  let record = files
    .iter()
    .find(
      |record| matches!(&record.target, FileTarget::Run { path, .. } if path == RUN_SYSTEM_PATH),
    )
    .ok_or_else(invalid)?;
  let FileStorage::Tracking { revision, .. } = record.storage else {
    return Err(invalid());
  };
  let mut file = open_raw(directory, &record.target, revision, false)?;
  let mut first = Vec::new();
  BufReader::new((&mut file).take(record.size.min(1024)))
    .read_until(b'\n', &mut first)
    .map_err(|_| invalid())?;
  let header: RunSystemRecord = serde_json::from_slice(&first).map_err(|_| invalid())?;
  header.validate().map_err(|_| invalid())?;
  match header {
    RunSystemRecord::Capture {
      capture_id: actual,
      machine_id,
      start_ms,
      ..
    } if actual == capture_id
      && state["machine_id"].as_str() == Some(machine_id.as_str())
      && time(&state["started_at"]) == Some(start_ms) => {}
    _ => return Err(invalid()),
  }
  let offset = record.size.saturating_sub(1024 * 1024);
  file.seek(SeekFrom::Start(offset)).map_err(|_| invalid())?;
  let mut tail = Vec::new();
  file
    .take(record.size - offset)
    .read_to_end(&mut tail)
    .map_err(|_| invalid())?;
  if tail.last() != Some(&b'\n') {
    return Err(invalid());
  }
  tail.pop();
  let start = tail
    .iter()
    .rposition(|byte| *byte == b'\n')
    .map_or(0, |index| index + 1);
  if offset > 0 && start == 0 {
    return Err(invalid());
  }
  let end: RunSystemRecord = serde_json::from_slice(&tail[start..]).map_err(|_| invalid())?;
  end.validate().map_err(|_| invalid())?;
  match end {
    RunSystemRecord::End {
      end_ms, complete, ..
    } if time(&state["finished_at"]) == Some(end_ms)
      && state["system_capture_status"].as_str()
        == Some(if complete { "complete" } else { "incomplete" }) =>
    {
      Ok(())
    }
    _ => Err(invalid()),
  }
}
