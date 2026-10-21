//! A full seal must include the capture's acknowledged completion record.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use expri_core::run_system::{RUN_SYSTEM_PATH, RunSystemRecord};
use serde_json::Value;

use super::{ApiError, ApiResult, FileRecord, FileStorage, FileTarget};

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

fn capture_time<'a>(value: &'a Value, field: &str, fallback: &str) -> Option<&'a Value> {
  value
    .get(field)
    .filter(|value| !value.is_null())
    .or_else(|| value.get(fallback).filter(|value| !value.is_null()))
}

#[derive(Eq, PartialEq)]
struct Capture {
  machine_id: String,
  start_ms: i64,
  end_ms: i64,
  complete: bool,
}

fn captures(state: &Value) -> ApiResult<BTreeMap<String, Capture>> {
  let mut captures = BTreeMap::<String, Capture>::new();
  let attempts = state["task_attempts"]["attempts"].as_array();
  if attempts.is_some_and(|attempts| attempts.len() > expri_core::protocol::MAX_TASK_ATTEMPTS) {
    return Err(invalid());
  }
  for value in std::iter::once(state).chain(attempts.into_iter().flatten()) {
    let Some(id) = value["system_capture_id"].as_str() else {
      continue;
    };
    let machine_id = value["machine_id"].as_str().ok_or_else(invalid)?;
    let start_ms = capture_time(value, "system_capture_started_at", "started_at")
      .and_then(time)
      .ok_or_else(invalid)?;
    let end = capture_time(value, "system_capture_finished_at", "finished_at");
    if value["system_capture_status"] == "capturing"
      && state["system_capture_id"].as_str() == Some(id)
      && let Some(current) = captures.get(id)
    {
      // Collector completion updates the current capture after the attempt's
      // terminal snapshot. Supersede that pending status only when its identity
      // and any recorded endpoint agree with the canonical completion.
      if machine_id != current.machine_id
        || start_ms != current.start_ms
        || end.is_some_and(|end| time(end) != Some(current.end_ms))
      {
        return Err(invalid());
      }
      continue;
    }
    let capture = Capture {
      machine_id: machine_id.into(),
      start_ms,
      end_ms: end.and_then(time).ok_or_else(invalid)?,
      complete: match value["system_capture_status"].as_str() {
        Some("complete") => true,
        Some("incomplete") => false,
        _ => return Err(invalid()),
      },
    };
    if capture.end_ms < capture.start_ms {
      return Err(invalid());
    }
    if let Some(previous) = captures.insert(id.to_owned(), capture)
      && captures[id] != previous
    {
      return Err(invalid());
    }
  }
  Ok(captures)
}

pub(super) fn validate_seal(
  directory: &Path,
  connection: &rusqlite::Connection,
  files: &[FileRecord],
  state: &Value,
) -> ApiResult<()> {
  if !state["system_capture_id"].is_string() {
    return Ok(());
  }
  let expected = captures(state)?;
  let record = files
    .iter()
    .find(
      |record| matches!(&record.target, FileTarget::Run { path, .. } if path == RUN_SYSTEM_PATH),
    )
    .ok_or_else(invalid)?;
  let FileStorage::Tracking { revision, .. } = record.storage else {
    return Err(invalid());
  };
  let file = super::system_compression::reader(directory, connection, &record.target, revision)?;
  let mut reader = BufReader::new(file.take(record.size));
  let mut seen = BTreeSet::new();
  let mut inventories = BTreeSet::new();
  let mut active: Option<&Capture> = None;
  let mut previous_end = None;
  let mut latest_id = None;
  loop {
    let mut line = Vec::new();
    let size = (&mut reader)
      .take(1024 * 1024 + 1)
      .read_until(b'\n', &mut line)
      .map_err(|_| invalid())?;
    if size == 0 {
      break;
    }
    if size > 1024 * 1024 || line.last() != Some(&b'\n') {
      return Err(invalid());
    }
    let value: RunSystemRecord = serde_json::from_slice(&line).map_err(|_| invalid())?;
    value.validate().map_err(|_| invalid())?;
    match value {
      RunSystemRecord::Capture {
        capture_id,
        machine_id,
        start_ms,
        ..
      } => {
        let capture = expected.get(&capture_id).ok_or_else(invalid)?;
        if active.is_some()
          || !seen.insert(capture_id.clone())
          || machine_id != capture.machine_id
          || start_ms != capture.start_ms
          || previous_end.is_some_and(|end| start_ms < end)
        {
          return Err(invalid());
        }
        latest_id = Some(capture_id);
        inventories.clear();
        active = Some(capture);
      }
      RunSystemRecord::Inventory { inventory } => {
        if active.is_none() {
          return Err(invalid());
        }
        inventories.insert(inventory.revision);
      }
      RunSystemRecord::Sample { sample } => {
        let capture = active.ok_or_else(invalid)?;
        if !inventories.contains(&sample.inventory_revision)
          || sample.observed_at_ms < capture.start_ms
          || sample.observed_at_ms > capture.end_ms
        {
          return Err(invalid());
        }
      }
      RunSystemRecord::End {
        end_ms, complete, ..
      } => {
        let capture = active.take().ok_or_else(invalid)?;
        if end_ms != capture.end_ms || complete != capture.complete {
          return Err(invalid());
        }
        previous_end = Some(end_ms);
      }
    }
  }
  if active.is_some()
    || seen.len() != expected.len()
    || latest_id.as_deref() != state["system_capture_id"].as_str()
  {
    return Err(invalid());
  }
  Ok(())
}
