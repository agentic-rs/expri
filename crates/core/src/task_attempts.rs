//! Retained task attempts within one run. Callers hold the run-state update lease.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::{Value, json};

use crate::error::{ExpriError, Result};
use crate::run_state::{MAX_STATE_BYTES, is_terminal_status};

const HISTORY_RESERVE: usize = 12 * 1024;
const METRIC_BYTES: u64 = 128 * 1024 * 1024;
const LINE_BYTES: usize = 1024 * 1024;

fn message(detail: &str) -> ExpriError {
  ExpriError::Message(detail.into())
}

pub fn is_attempt_status(status: &str) -> bool {
  matches!(status, "preparing" | "running" | "waiting" | "continued") || is_terminal_status(status)
}

fn timestamp(value: &str) -> Result<()> {
  chrono::DateTime::parse_from_rfc3339(value)
    .map(|_| ())
    .map_err(|_| message("task attempt timestamp must be RFC3339"))
}

fn attempts(state: &Value) -> Result<&Vec<Value>> {
  let history = &state["task_attempts"];
  let entries = history["attempts"]
    .as_array()
    .filter(|entries| !entries.is_empty())
    .ok_or_else(|| message("invalid task attempt history"))?;
  let current = history["current_attempt"]
    .as_u64()
    .filter(|value| *value > 0)
    .ok_or_else(|| message("invalid current task attempt"))?;
  let mut ids = BTreeSet::new();
  for entry in entries {
    let id = entry["attempt"]
      .as_u64()
      .filter(|value| *value > 0 && *value <= crate::protocol::MAX_TASK_ATTEMPTS as u64)
      .ok_or_else(|| message("invalid task attempt number"))?;
    if !ids.insert(id) {
      return Err(message("duplicate task attempt number"));
    }
  }
  if history["protocol_version"] != 1
    || history["truncated"] != false
    || history["total_count"].as_u64() != Some(entries.len() as u64)
    || !ids.contains(&current)
  {
    return Err(message("invalid task attempt history envelope"));
  }
  Ok(entries)
}

/// Snapshot a legacy run's first attempt without changing its lifecycle fields.
pub fn ensure_initial(state: &mut Value) -> Result<()> {
  if state.get("task_attempts").is_some() {
    attempts(state)?;
    return Ok(());
  }
  if !state.is_object() {
    return Err(message("run state must be an object"));
  }
  let status = state["status"]
    .as_str()
    .filter(|value| is_attempt_status(value))
    .ok_or_else(|| message("run has no supported task attempt status"))?;
  let started_at = state["started_at"]
    .as_str()
    .ok_or_else(|| message("run has no start time"))?;
  timestamp(started_at)?;
  let initial = json!({
    "attempt": 1, "request_id": null, "reason": null, "status": status,
    "task": state["task"], "command": state["command"],
    "requested_at": null, "started_at": started_at, "finished_at": state["finished_at"],
    "exit_code": state.get("task_exit_code").filter(|value| !value.is_null()).unwrap_or(&state["exit_code"]),
    "source": state["source"], "checkpoint": null, "metric_step_offset": 0,
    "worker_restarted": false, "previous_worker_stopped_at": null,
  });
  let mut candidate = state.clone();
  candidate["task_attempts"] = json!({
    "protocol_version": 1, "current_attempt": 1, "attempts": [initial],
    "total_count": 1, "truncated": false,
  });
  capacity(&candidate)?;
  *state = candidate;
  Ok(())
}

pub fn current_attempt(state: &Value) -> Result<u64> {
  attempts(state)?;
  Ok(state["task_attempts"]["current_attempt"].as_u64().unwrap())
}

/// The original parameter record remains immutable; continued attempts own a new one.
pub fn current_params_path(state: &Value) -> Result<String> {
  let current = if state.get("task_attempts").is_some() {
    current_attempt(state)?
  } else {
    1
  };
  Ok(if current == 1 {
    "outputs/params.json".into()
  } else {
    format!("outputs/attempts/{current}/params.json")
  })
}

/// Task-reported phases update the active attempt under the same run-state lease.
pub fn set_current_phase(state: &mut Value, status: &str) -> Result<bool> {
  if state.get("task_attempts").is_none() {
    return Ok(false);
  }
  if !matches!(status, "running" | "waiting") {
    return Err(message("task attempt phase must be running or waiting"));
  }
  let current = current_attempt(state)?;
  let entry = attempt_mut(state, current)?;
  if !matches!(entry["status"].as_str(), Some("running" | "waiting"))
    || !entry["finished_at"].is_null()
  {
    return Err(message("current task attempt is not active"));
  }
  let changed = entry["status"] != status;
  entry["status"] = json!(status);
  Ok(changed)
}

pub fn next_attempt(state: &Value) -> Result<u64> {
  attempts(state)?
    .iter()
    .filter_map(|entry| entry["attempt"].as_u64())
    .max()
    .unwrap()
    .checked_add(1)
    .filter(|number| *number <= crate::protocol::MAX_TASK_ATTEMPTS as u64)
    .ok_or_else(|| message("task attempt history limit reached; existing attempts were retained"))
}

/// Record a requested attempt before preparation. A failed preflight never changes current_attempt.
pub fn begin_attempt(state: &mut Value, mut spec: Value) -> Result<()> {
  let expected = next_attempt(state)?;
  let number = spec["attempt"]
    .as_u64()
    .filter(|number| *number == expected)
    .ok_or_else(|| message("new task attempt must use the next attempt number"))?;
  crate::identifiers::validate_component(
    spec["request_id"]
      .as_str()
      .ok_or_else(|| message("task attempt needs a request ID"))?,
  )?;
  let reason = spec["reason"]
    .as_str()
    .ok_or_else(|| message("task attempt needs a reason"))?;
  if reason.trim().is_empty() || reason.len() > 1024 || reason.chars().any(char::is_control) {
    return Err(message(
      "task attempt reason must contain 1–1024 bytes without controls",
    ));
  }
  timestamp(
    spec["requested_at"]
      .as_str()
      .ok_or_else(|| message("task attempt needs a request time"))?,
  )?;
  decimal_u64(&spec["metric_step_offset"])?;
  spec["status"] = json!("preparing");
  spec["started_at"] = Value::Null;
  spec["finished_at"] = Value::Null;
  spec["exit_code"] = Value::Null;
  spec["worker_restarted"] = json!(false);
  let mut candidate = state.clone();
  let history = &mut candidate["task_attempts"];
  history["attempts"].as_array_mut().unwrap().push(spec);
  history["total_count"] = json!(history["attempts"].as_array().unwrap().len());
  capacity(&candidate)?;
  *state = candidate;
  debug_assert!(
    attempts(state)?
      .iter()
      .any(|entry| entry["attempt"] == number)
  );
  Ok(())
}

pub fn activate_attempt(
  state: &mut Value,
  number: u64,
  started_at: &str,
  worker_pid: u32,
) -> Result<()> {
  timestamp(started_at)?;
  let entry = attempt_mut(state, number)?;
  if entry["status"] != "preparing" || !entry["started_at"].is_null() {
    return Err(message("task attempt is not awaiting its first start"));
  }
  entry["status"] = json!("running");
  entry["started_at"] = json!(started_at);
  entry["worker_pid"] = json!(worker_pid);
  entry["worker_restarted"] = json!(true);
  state["task_attempts"]["current_attempt"] = json!(number);
  Ok(())
}

pub fn finish_attempt(
  state: &mut Value,
  number: u64,
  status: &str,
  finished_at: &str,
  exit_code: Option<i32>,
  error: Option<&str>,
) -> Result<()> {
  if !is_terminal_status(status) && status != "continued" {
    return Err(message(
      "task attempt must finish with a terminal status or continued",
    ));
  }
  timestamp(finished_at)?;
  let entry = attempt_mut(state, number)?;
  if let Some(start) = entry["started_at"].as_str()
    && chrono::DateTime::parse_from_rfc3339(finished_at).unwrap()
      < chrono::DateTime::parse_from_rfc3339(start)
        .map_err(|_| message("invalid attempt start time"))?
  {
    return Err(message("task attempt finish precedes its start"));
  }
  entry["status"] = json!(status);
  entry["finished_at"] = json!(finished_at);
  entry["exit_code"] = json!(exit_code);
  entry["error"] = error
    .map(|detail| {
      json!(
        detail
          .chars()
          .filter(|value| !value.is_control())
          .take(2048)
          .collect::<String>()
      )
    })
    .unwrap_or(Value::Null);
  Ok(())
}

fn attempt_mut(state: &mut Value, number: u64) -> Result<&mut Value> {
  attempts(state)?;
  state["task_attempts"]["attempts"]
    .as_array_mut()
    .unwrap()
    .iter_mut()
    .find(|entry| entry["attempt"].as_u64() == Some(number))
    .ok_or_else(|| message("task attempt does not exist"))
}

fn capacity(state: &Value) -> Result<()> {
  if serde_json::to_vec_pretty(state)?.len() > MAX_STATE_BYTES - HISTORY_RESERVE {
    return Err(message(
      "run metadata has insufficient space to retain another task attempt",
    ));
  }
  Ok(())
}

/// Raw metadata accepts an exact JSON u64 or its canonical decimal spelling.
pub fn decimal_u64(value: &Value) -> Result<u64> {
  value
    .as_u64()
    .or_else(|| {
      value
        .as_str()
        .filter(|text| {
          !text.is_empty()
            && text.bytes().all(|value| value.is_ascii_digit())
            && (text == &"0" || !text.starts_with('0'))
        })
        .and_then(|text| text.parse().ok())
    })
    .ok_or_else(|| message("step offset must be an exact unsigned 64-bit integer"))
}

/// Determine a continuation's first global step after the preceding writer stopped.
/// Any malformed or partial row is refused; canonical history is never repaired here.
pub fn next_metric_step(path: &Path) -> Result<u64> {
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file: File = match options.open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
    Err(error) => return Err(error.into()),
  };
  let metadata = file.metadata()?;
  if !metadata.is_file() || metadata.len() > METRIC_BYTES {
    return Err(message(
      "canonical metrics must be a regular file within 128 MiB",
    ));
  }
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
  loop {
    match file.try_lock_shared() {
      Ok(()) => break,
      Err(std::fs::TryLockError::WouldBlock) => {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
          return Err(message("canonical metrics are busy; retry continuation"));
        }
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
      }
      Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }
  }
  let mut reader = BufReader::new(file);
  let mut maximum = None;
  let mut line = Vec::new();
  let mut total = 0u64;
  loop {
    line.clear();
    let mut bounded = std::io::Read::take(&mut reader, LINE_BYTES as u64 + 1);
    if bounded.read_until(b'\n', &mut line)? == 0 {
      break;
    }
    total += line.len() as u64;
    if total > METRIC_BYTES {
      return Err(message("canonical metrics exceed the 128 MiB limit"));
    }
    if line.len() > LINE_BYTES || line.last() != Some(&b'\n') {
      return Err(message(
        "canonical metrics contain an oversized or incomplete row",
      ));
    }
    if line.iter().all(u8::is_ascii_whitespace) {
      continue;
    }
    let event: Value = serde_json::from_slice(&line)
      .map_err(|_| message("canonical metrics contain invalid JSON"))?;
    let step = event["step"]
      .as_u64()
      .ok_or_else(|| message("canonical metrics contain an invalid step"))?;
    if event.get("schema_version").is_some_and(|value| value != 1)
      || event["metrics"].as_object().is_none_or(|values| {
        values.is_empty()
          || values.keys().any(|name| {
            name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control)
          })
          || values
            .values()
            .any(|value| value.as_f64().is_none_or(|value| !value.is_finite()))
      })
    {
      return Err(message("canonical metrics contain an invalid event"));
    }
    if event.get("timestamp").is_some_and(|value| {
      value
        .as_str()
        .is_none_or(|value| chrono::DateTime::parse_from_rfc3339(value).is_err())
    }) {
      return Err(message("canonical metrics contain an invalid timestamp"));
    }
    maximum = Some(maximum.map_or(step, |value: u64| value.max(step)));
  }
  maximum.map_or(Ok(0), |value| {
    value
      .checked_add(1)
      .ok_or_else(|| message("canonical global metric steps are exhausted"))
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn state() -> Value {
    json!({"status":"completed","started_at":"2026-10-15T01:00:00Z","finished_at":"2026-10-15T02:00:00Z","exit_code":0,"task":"train","command":["python","train.py"]})
  }
  fn spec(attempt: u64) -> Value {
    json!({"attempt":attempt,"request_id":format!("request-{attempt}"),"reason":"Extend training","requested_at":"2026-10-15T03:00:00Z","metric_step_offset":"101","code_dir":"/private/code","source":{"git_head":"abc"}})
  }
  #[test]
  fn retained_attempts_distinguish_failed_preflight_and_actual_worker_restart() {
    let mut state = state();
    let original_start = state["started_at"].clone();
    ensure_initial(&mut state).unwrap();
    ensure_initial(&mut state).unwrap();
    assert_eq!(current_attempt(&state).unwrap(), 1);
    begin_attempt(&mut state, spec(2)).unwrap();
    finish_attempt(
      &mut state,
      2,
      "failed",
      "2026-10-15T03:00:01Z",
      None,
      Some("Invalid checkpoint"),
    )
    .unwrap();
    assert_eq!(current_attempt(&state).unwrap(), 1);
    assert_eq!(next_attempt(&state).unwrap(), 3);
    begin_attempt(&mut state, spec(3)).unwrap();
    activate_attempt(&mut state, 3, "2026-10-15T03:00:03Z", 123).unwrap();
    finish_attempt(
      &mut state,
      3,
      "continued",
      "2026-10-15T03:01:00Z",
      Some(0),
      None,
    )
    .unwrap();
    assert_eq!(current_attempt(&state).unwrap(), 3);
    assert_eq!(state["task_attempts"]["attempts"][0]["status"], "completed");
    assert_eq!(
      state["task_attempts"]["attempts"][1]["worker_restarted"],
      false
    );
    assert_eq!(
      state["task_attempts"]["attempts"][2]["worker_restarted"],
      true
    );
    assert_eq!(state["task_attempts"]["total_count"], 3);
    assert_eq!(state["started_at"], original_start);
    assert_eq!(
      state["status"], "completed",
      "caller owns the overall run lifecycle"
    );
  }
  #[test]
  fn invalid_attempt_and_full_metadata_do_not_drop_or_partially_append_history() {
    let mut state = state();
    ensure_initial(&mut state).unwrap();
    let before = state.clone();
    assert!(begin_attempt(&mut state, spec(3)).is_err());
    assert_eq!(state, before);
    let mut huge = spec(2);
    huge["private"] = json!("x".repeat(MAX_STATE_BYTES));
    assert!(begin_attempt(&mut state, huge).is_err());
    assert_eq!(state, before);
    assert!(activate_attempt(&mut state, 2, "invalid", 123).is_err());
  }
  #[test]
  fn metric_base_uses_complete_global_max_and_preserves_history_bytes() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("metrics.jsonl");
    assert_eq!(next_metric_step(&path).unwrap(), 0);
    let raw = b"{\"step\":100,\"metrics\":{\"loss\":1}}\n{\"step\":4,\"metrics\":{\"loss\":2}}\n";
    std::fs::write(&path, raw).unwrap();
    assert_eq!(next_metric_step(&path).unwrap(), 101);
    assert_eq!(std::fs::read(&path).unwrap(), raw);
    for raw in [
      b"{\"step\":1}".as_slice(),
      b"{\"step\":-1,\"metrics\":{\"loss\":1}}\n",
      b"{\"step\":18446744073709551615,\"metrics\":{\"loss\":1}}\n",
    ] {
      std::fs::write(&path, raw).unwrap();
      assert!(next_metric_step(&path).is_err());
      assert_eq!(std::fs::read(&path).unwrap(), raw);
    }
    assert_eq!(decimal_u64(&json!(u64::MAX.to_string())).unwrap(), u64::MAX);
    for value in [
      json!("00"),
      json!("+1"),
      json!("18446744073709551616"),
      json!(-1),
      json!(1.5),
    ] {
      assert!(decimal_u64(&value).is_err());
    }
  }

  #[test]
  fn metric_scan_waits_for_a_cooperating_writer_to_finish_its_row() {
    use std::io::Write;
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("metrics.jsonl");
    let mut writer = OpenOptions::new()
      .read(true)
      .write(true)
      .create_new(true)
      .open(&path)
      .unwrap();
    writer.lock().unwrap();
    writer
      .write_all(b"{\"step\":10,\"metrics\":{\"loss\":1}}")
      .unwrap();
    let reader = std::thread::spawn(move || next_metric_step(&path));
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(!reader.is_finished());
    writer.write_all(b"\n").unwrap();
    writer.unlock().unwrap();
    assert_eq!(reader.join().unwrap().unwrap(), 11);
  }

  #[test]
  fn protocol_attempt_limit_refuses_new_requests_without_dropping_failed_attempts() {
    let mut state = state();
    ensure_initial(&mut state).unwrap();
    for number in 2..=crate::protocol::MAX_TASK_ATTEMPTS as u64 {
      begin_attempt(&mut state, spec(number)).unwrap();
      finish_attempt(
        &mut state,
        number,
        "failed",
        "2026-10-15T03:00:01Z",
        None,
        Some("preflight failed"),
      )
      .unwrap();
    }
    let before = state.clone();
    assert!(next_attempt(&state).is_err());
    assert!(
      begin_attempt(
        &mut state,
        spec(crate::protocol::MAX_TASK_ATTEMPTS as u64 + 1)
      )
      .is_err()
    );
    assert_eq!(state, before);
    assert_eq!(current_attempt(&state).unwrap(), 1);
  }
}
