//! Bounded projections of retained run-owned observations, never global machine history.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};

use chrono::DateTime;
use expri_core::error::{ExpriError, Result};
use expri_core::identifiers::validate_component;
use expri_core::run_state::{is_active_status, is_terminal_status};
use expri_core::run_system::RunSystemRecord;
use expri_core::system_profile::{InventoryRevision, MetricKind, SystemSample};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PAGE_BYTES: usize = 512 * 1024;
pub const MAX_ROWS: usize = 1024;
const LINE_BYTES: usize = 1024 * 1024;
const MAX_METRICS: usize = 256;
const MAX_INVENTORIES: usize = 4096;
const MAX_WARNINGS: usize = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemMetricSummary {
  pub kind: MetricKind,
  pub count: u64,
  pub min: f64,
  pub max: f64,
  pub last: f64,
  pub mean: Option<f64>,
  pub unit: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunSystemMetrics {
  pub run_id: String,
  pub capture_id: Option<String>,
  pub machine_id: Option<String>,
  pub start_ms: Option<i64>,
  pub end_ms: Option<i64>,
  pub status: String,
  pub inventories: Vec<InventoryRevision>,
  pub samples: Vec<SystemSample>,
  pub metric_summaries: BTreeMap<String, SystemMetricSummary>,
  /// Complete raw Sample record count, also the durable stream-ordinal watermark.
  /// Samples outside the canonical run window still consume an ordinal.
  pub sample_count: u64,
  pub next_cursor: Option<u64>,
  pub warnings: Vec<String>,
}

impl RunSystemMetrics {
  pub fn unavailable(run_id: &str, detail: &str) -> Self {
    Self {
      run_id: run_id.into(),
      capture_id: None,
      machine_id: None,
      start_ms: None,
      end_ms: None,
      status: "unavailable".into(),
      inventories: Vec::new(),
      samples: Vec::new(),
      metric_summaries: BTreeMap::new(),
      sample_count: 0,
      next_cursor: None,
      warnings: vec![detail.chars().take(512).collect()],
    }
  }

  pub fn unavailable_for_run(run_id: &str, run: &Value, detail: &str) -> Self {
    let mut result = Self::unavailable(run_id, detail);
    if let Some(error) = run["system_capture_error"].as_str()
      && !error.is_empty()
      && error.len() <= 1024
      && !error.chars().any(char::is_control)
    {
      warning(&mut result, &format!("System capture: {error}"));
    }
    result
  }
}

#[derive(Clone, Debug)]
pub struct RunSystemWindow {
  pub capture_id: String,
  pub machine_id: String,
  pub start_ms: i64,
  pub end_ms: Option<i64>,
  pub terminal: bool,
  pub segments: Vec<RunSystemSegment>,
  capture_error: Option<String>,
  recording: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunSystemSegment {
  pub capture_id: String,
  pub start_ms: i64,
  pub end_ms: Option<i64>,
}

impl RunSystemWindow {
  pub fn from_state(run_id: &str, state: &Value) -> Option<Self> {
    let status = state["status"].as_str()?;
    if state["run_id"].as_str()? != run_id
      || state["schema_version"].as_u64()? != 1
      || !(is_active_status(status) || is_terminal_status(status))
    {
      return None;
    }
    let machine_id = state["machine_id"].as_str()?;
    validate_component(machine_id).ok()?;
    let start_ms = DateTime::parse_from_rfc3339(state["started_at"].as_str()?)
      .ok()?
      .timestamp_millis();
    let terminal = is_terminal_status(status);
    let end_ms = if terminal {
      state["finished_at"]
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis())
    } else {
      None
    };
    if start_ms < 0 || end_ms.is_some_and(|end| end < start_ms) || terminal && end_ms.is_none() {
      return None;
    }
    let mut segments = Vec::new();
    if let Some(attempts) = state["task_attempts"]["attempts"].as_array() {
      for attempt in attempts {
        let Some(capture_id) = attempt["system_capture_id"].as_str() else {
          continue;
        };
        validate_component(capture_id).ok()?;
        if attempt["machine_id"]
          .as_str()
          .is_some_and(|id| id != machine_id)
        {
          return None;
        }
        let segment_start = timestamp(&attempt["system_capture_started_at"])
          .or_else(|| timestamp(&attempt["started_at"]))?;
        let segment_end = timestamp(&attempt["system_capture_finished_at"])
          .or_else(|| timestamp(&attempt["finished_at"]));
        if segment_start < start_ms
          || segment_end.is_some_and(|end| end < segment_start)
          || end_ms
            .zip(segment_end)
            .is_some_and(|(end, segment)| segment > end)
          || segments
            .iter()
            .any(|segment: &RunSystemSegment| segment.capture_id == capture_id)
        {
          return None;
        }
        segments.push(RunSystemSegment {
          capture_id: capture_id.into(),
          start_ms: segment_start,
          end_ms: segment_end,
        });
      }
    }
    if let Some(capture_id) = state["system_capture_id"].as_str() {
      validate_component(capture_id).ok()?;
      let segment_start = timestamp(&state["system_capture_started_at"]).unwrap_or(start_ms);
      let segment_end = timestamp(&state["system_capture_finished_at"]).or(end_ms);
      let current = RunSystemSegment {
        capture_id: capture_id.into(),
        start_ms: segment_start,
        end_ms: segment_end,
      };
      if let Some(existing) = segments
        .iter_mut()
        .find(|segment| segment.capture_id == capture_id)
      {
        if existing.start_ms != segment_start
          || existing.end_ms.is_some() && existing.end_ms != segment_end
        {
          return None;
        }
        *existing = current;
      } else {
        segments.push(current);
      }
    }
    if segments.is_empty() || segments.len() > expri_core::protocol::MAX_TASK_ATTEMPTS {
      return None;
    }
    if segments.iter().any(|segment| {
      segment.start_ms < start_ms
        || segment.end_ms.is_some_and(|end| end < segment.start_ms)
        || end_ms
          .zip(segment.end_ms)
          .is_some_and(|(end, segment)| segment > end)
    }) {
      return None;
    }
    for pair in segments.windows(2) {
      if pair[0].end_ms.is_none_or(|end| end > pair[1].start_ms) {
        return None;
      }
    }
    let capture_id = segments.last()?.capture_id.clone();
    let recording = !terminal
      && state["system_capture_id"].as_str() == Some(capture_id.as_str())
      && segments.last()?.end_ms.is_none();
    Some(Self {
      capture_id,
      machine_id: machine_id.into(),
      start_ms,
      end_ms,
      terminal,
      segments,
      recording,
      capture_error: state["system_capture_error"]
        .as_str()
        .filter(|error| {
          !error.is_empty() && error.len() <= 1024 && !error.chars().any(char::is_control)
        })
        .map(str::to_owned),
    })
  }
}

fn timestamp(value: &Value) -> Option<i64> {
  DateTime::parse_from_rfc3339(value.as_str()?)
    .ok()
    .map(|time| time.timestamp_millis())
}

fn rejected(status: u16, detail: &str) -> ExpriError {
  ExpriError::ServiceRejected {
    status,
    detail: detail.into(),
  }
}

fn warning(result: &mut RunSystemMetrics, detail: &str) {
  let detail: String = detail
    .chars()
    .filter(|character| !character.is_control())
    .take(512)
    .collect();
  if result.warnings.len() < MAX_WARNINGS && !result.warnings.contains(&detail) {
    result.warnings.push(detail);
  }
}

fn records(
  reader: impl Read,
  mut visit: impl FnMut(RunSystemRecord) -> Result<()>,
) -> Result<bool> {
  let mut reader = BufReader::new(reader);
  let mut line = Vec::new();
  loop {
    let chunk = reader.fill_buf().map_err(|error| {
      if error.kind() == std::io::ErrorKind::TimedOut {
        rejected(
          504,
          "run system read time budget exceeded; retry or review its downloaded raw stream",
        )
      } else {
        ExpriError::Io(error)
      }
    })?;
    if chunk.is_empty() {
      return Ok(line.is_empty());
    }
    let count = chunk
      .iter()
      .position(|byte| *byte == b'\n')
      .map_or(chunk.len(), |index| index + 1);
    if line.len() + count > LINE_BYTES {
      return Err(rejected(
        413,
        "run system record exceeds the 1 MiB line limit",
      ));
    }
    line.extend_from_slice(&chunk[..count]);
    reader.consume(count);
    if line.last() != Some(&b'\n') {
      continue;
    }
    if !line.iter().all(u8::is_ascii_whitespace) {
      let record: RunSystemRecord = serde_json::from_slice(&line)
        .map_err(|_| rejected(422, "run system stream contains an invalid record"))?;
      record
        .validate()
        .map_err(|_| rejected(422, "run system stream contains an invalid record"))?;
      visit(record)?;
    }
    line.clear();
  }
}

fn unit(name: &str, kind: MetricKind) -> Option<String> {
  let unit = if name.ends_with("_bytes") {
    "bytes"
  } else if name.ends_with("_percent") {
    "%"
  } else if name.ends_with("_mhz") {
    "MHz"
  } else if name.ends_with("_watts") {
    "W"
  } else if name.ends_with("_celsius") {
    "°C"
  } else {
    return None;
  };
  Some(if kind == MetricKind::CounterDelta {
    format!("{unit}/s")
  } else {
    unit.into()
  })
}

fn fit_page(result: &mut RunSystemMetrics, ordinals: &mut Vec<u64>) -> Result<()> {
  while serde_json::to_vec(result)?.len() > PAGE_BYTES {
    if result.samples.pop().is_none() || result.samples.is_empty() {
      return Err(rejected(
        413,
        "one run system sample and its inventory exceed the 512 KiB response limit",
      ));
    }
    ordinals.pop();
    result.next_cursor = ordinals.last().copied();
    let used: BTreeSet<_> = result
      .samples
      .iter()
      .map(|sample| sample.inventory_revision)
      .collect();
    result
      .inventories
      .retain(|inventory| used.contains(&inventory.revision));
  }
  Ok(())
}

/// Read the entire stream into bounded summaries, retaining only the requested
/// sample page. A second independent reader fetches its referenced inventories.
pub fn read<R: Read>(
  reader: R,
  reopen: impl FnOnce() -> Result<R>,
  run_id: &str,
  window: &RunSystemWindow,
  cursor: Option<u64>,
  limit: usize,
) -> Result<RunSystemMetrics> {
  if !(1..=MAX_ROWS).contains(&limit) {
    return Err(rejected(400, "run system limit must be between 1 and 1024"));
  }
  let mut result = RunSystemMetrics {
    run_id: run_id.into(),
    capture_id: Some(window.capture_id.clone()),
    machine_id: Some(window.machine_id.clone()),
    start_ms: Some(window.start_ms),
    end_ms: window.end_ms,
    status: if window.recording {
      "recording"
    } else {
      "incomplete"
    }
    .into(),
    inventories: Vec::new(),
    samples: Vec::new(),
    metric_summaries: BTreeMap::new(),
    sample_count: 0,
    next_cursor: None,
    warnings: Vec::new(),
  };
  let mut header = false;
  if let Some(error) = &window.capture_error {
    warning(&mut result, &format!("System capture: {error}"));
  }
  let mut ended = false;
  let mut segment_count = 0usize;
  let mut segment_start = window.start_ms;
  let mut segment_end = None;
  let mut previous_end = None;
  let mut all_complete = true;
  let mut inventory_revisions = BTreeSet::new();
  let mut ordinal = 0u64;
  let mut ordinals = Vec::new();
  let mut bytes = 0usize;
  let mut page_closed = false;
  let mut coverage: BTreeMap<String, u64> = BTreeMap::new();
  let mut previous_time = None;
  let complete_lines = records(reader, |record| {
    match record {
      RunSystemRecord::Capture {
        capture_id,
        machine_id,
        start_ms,
        ..
      } => {
        let expected = window.segments.get(segment_count);
        if header && !ended
          || expected
            .is_none_or(|segment| segment.capture_id != capture_id || segment.start_ms != start_ms)
          || machine_id != window.machine_id
          || previous_end.is_some_and(|end| start_ms < end)
        {
          return Err(rejected(
            409,
            "run system capture identity does not match canonical run metadata",
          ));
        }
        if segment_count > 0 {
          warning(
            &mut result,
            "System capture resumed for another task attempt; no observations are inferred for the interval between attempts.",
          );
        }
        segment_count += 1;
        segment_start = start_ms;
        segment_end = expected.and_then(|segment| segment.end_ms);
        inventory_revisions.clear();
        ended = false;
        header = true;
      }
      _ if !header || ended => {
        return Err(rejected(
          422,
          "run system records must follow one capture header and precede its end",
        ));
      }
      RunSystemRecord::Inventory { inventory } => {
        inventory_revisions.insert(inventory.revision);
        if inventory_revisions.len() > MAX_INVENTORIES {
          return Err(rejected(
            413,
            "run system capture exceeds the 4096 inventory revision limit",
          ));
        }
      }
      RunSystemRecord::Sample { sample } => {
        ordinal = ordinal
          .checked_add(1)
          .ok_or_else(|| rejected(413, "run system sample ordinal exceeds its limit"))?;
        if !inventory_revisions.contains(&sample.inventory_revision) {
          return Err(rejected(
            422,
            "run system sample references an unavailable inventory",
          ));
        }
        if sample.observed_at_ms < segment_start
          || segment_end.is_some_and(|end| sample.observed_at_ms >= end)
        {
          return Ok(());
        }
        if previous_time.is_some_and(|previous| sample.observed_at_ms <= previous) {
          warning(
            &mut result,
            "System observation time moved backward or repeated; original recording order is preserved.",
          );
        }
        previous_time = Some(sample.observed_at_ms);
        for note in &sample.warnings {
          warning(&mut result, note);
        }
        if sample
          .observed_at_ms
          .saturating_sub(sample.interval_ms as i64)
          < segment_start
        {
          warning(
            &mut result,
            "A retained system reading measures an interval beginning before this run; rates describe the original whole-machine interval.",
          );
        }
        let duration = sample.interval_ms;
        for (name, reading) in &sample.metrics {
          let value = match reading.kind {
            MetricKind::Gauge => reading.value,
            MetricKind::CounterDelta if sample.interval_ms > 0 => {
              reading.value / sample.interval_ms as f64 * 1000.0
            }
            MetricKind::CounterDelta => {
              warning(
                &mut result,
                "Counter rates are unavailable for samples with zero measured interval.",
              );
              continue;
            }
          };
          if !value.is_finite() {
            warning(&mut result, "Non-finite derived system rates are omitted.");
            continue;
          }
          if !result.metric_summaries.contains_key(name)
            && result.metric_summaries.len() == MAX_METRICS
          {
            warning(
              &mut result,
              "Showing the first 256 system metric summaries; original raw samples retain every recorded metric.",
            );
            continue;
          }
          let summary =
            result
              .metric_summaries
              .entry(name.clone())
              .or_insert(SystemMetricSummary {
                kind: reading.kind,
                count: 0,
                min: value,
                max: value,
                last: value,
                mean: None,
                unit: unit(name, reading.kind),
              });
          if summary.kind != reading.kind {
            return Err(rejected(422, "run system metric changes measurement kind"));
          }
          summary.count += 1;
          summary.min = summary.min.min(value);
          summary.max = summary.max.max(value);
          summary.last = value;
          if duration > 0 {
            let covered = coverage.entry(name.clone()).or_default();
            let next = covered
              .checked_add(duration)
              .ok_or_else(|| rejected(413, "run system coverage exceeds its limit"))?;
            let weight = duration as f64 / next as f64;
            summary.mean = Some(
              summary
                .mean
                .map_or(value, |mean| mean * (1.0 - weight) + value * weight),
            );
            *covered = next;
          }
        }
        if ordinal > cursor.unwrap_or(0) && !page_closed {
          let size = serde_json::to_vec(&sample)?.len() + 1;
          if result.samples.len() == limit || bytes + size > PAGE_BYTES {
            if result.samples.is_empty() {
              return Err(rejected(
                413,
                "one run system sample exceeds the 512 KiB response limit",
              ));
            }
            result.next_cursor = ordinals.last().copied();
            page_closed = true;
          } else {
            bytes += size;
            result.samples.push(sample);
            ordinals.push(ordinal);
          }
        }
      }
      RunSystemRecord::End {
        end_ms,
        complete,
        warnings,
      } => {
        if end_ms < segment_start || segment_end.is_some_and(|expected| expected != end_ms) {
          return Err(rejected(
            409,
            "run system completion does not match canonical run metadata",
          ));
        }
        ended = true;
        previous_end = Some(end_ms);
        all_complete &= complete;
        if window.terminal && segment_count == window.segments.len() {
          result.status = if all_complete && window.end_ms == Some(end_ms) {
            "complete"
          } else {
            "incomplete"
          }
          .into();
        } else if segment_end.is_none() {
          warning(
            &mut result,
            "Captured completion is waiting for canonical run finish metadata.",
          );
        }
        for note in warnings {
          warning(&mut result, &note);
        }
      }
    }
    Ok(())
  })?;
  if !header {
    return Ok(RunSystemMetrics::unavailable(
      run_id,
      "This run has no captured system observations.",
    ));
  }
  result.sample_count = ordinal;
  if !complete_lines {
    result.status = "incomplete".into();
    warning(
      &mut result,
      "The captured system stream has an incomplete final record.",
    );
  }
  if window.terminal && !ended {
    warning(
      &mut result,
      "The run ended before system capture completion was recorded.",
    );
  }
  if segment_count < window.segments.len() {
    result.status = "incomplete".into();
    warning(
      &mut result,
      "The latest task attempt has not recorded its system capture header.",
    );
  }
  fit_page(&mut result, &mut ordinals)?;
  if !result.samples.is_empty() {
    let mut inventory_headers = 0usize;
    records(reopen()?, |record| {
      if let RunSystemRecord::Capture {
        capture_id,
        machine_id,
        start_ms,
        ..
      } = &record
      {
        if window
          .segments
          .get(inventory_headers)
          .is_none_or(|segment| segment.capture_id != *capture_id || segment.start_ms != *start_ms)
          || *machine_id != window.machine_id
        {
          return Err(rejected(409, "run system capture changed during review"));
        }
        inventory_headers += 1;
      }
      if inventory_headers == 0 {
        return Err(rejected(409, "run system capture changed during review"));
      }
      if let RunSystemRecord::Inventory { inventory } = record
        && result
          .samples
          .iter()
          .any(|sample| sample.inventory_revision == inventory.revision)
      {
        if let Some(existing) = result
          .inventories
          .iter()
          .find(|existing| existing.revision == inventory.revision)
        {
          if *existing != inventory {
            return Err(rejected(
              409,
              "run system inventory revision contains conflicting data",
            ));
          }
        } else {
          result.inventories.push(inventory);
          fit_page(&mut result, &mut ordinals)?;
        }
      }
      Ok(())
    })?;
    if inventory_headers != segment_count {
      return Err(rejected(409, "run system capture changed during review"));
    }
    if result.samples.iter().any(|sample| {
      !result
        .inventories
        .iter()
        .any(|inventory| inventory.revision == sample.inventory_revision)
    }) {
      return Err(rejected(
        409,
        "referenced run system inventory changed during review",
      ));
    }
  }
  result
    .inventories
    .sort_by_key(|inventory| inventory.revision);
  fit_page(&mut result, &mut ordinals)?;
  Ok(result)
}

#[cfg(test)]
mod tests;
