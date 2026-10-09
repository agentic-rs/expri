use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{ExpriError, Result};

pub const METRICS_FILE_LIMIT: u64 = 128 * 1024 * 1024;
pub const LINE_LIMIT: usize = 1024 * 1024;
const POINT_LIMIT: usize = 1_000_000;
const WARNING_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug)]
pub struct MetricFiles {
  pub metrics: bool,
  pub params: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MetricPoint {
  pub step: u64,
  pub value: f64,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub timestamp: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MetricSummary {
  pub count: usize,
  /// Complete count, independent of retained chart samples.
  #[serde(default)]
  pub missing_timestamp_count: usize,
  pub last: MetricPoint,
  pub min: MetricPoint,
  pub max: MetricPoint,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MetricSeries {
  pub summary: MetricSummary,
  pub points: Vec<MetricPoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunMetrics {
  pub run_id: String,
  /// The existing run catalog summary: task, status, timestamps, and exit code.
  pub run: Value,
  /// The effective parameter object, without the storage envelope.
  pub params: Option<Value>,
  /// First timestamped, valid metric event in logging order, before filtering.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub first_metric_timestamp: Option<String>,
  pub metrics: BTreeMap<String, MetricSeries>,
  pub warnings: Vec<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reduction {
  Last,
  Min,
  Max,
}

#[derive(Debug, Serialize)]
pub struct ComparisonRow {
  pub run_id: String,
  pub run: Value,
  pub params: Option<Value>,
  pub values: BTreeMap<String, Option<MetricPoint>>,
}

#[derive(Debug, Serialize)]
pub struct Comparison {
  pub reduction: Reduction,
  pub metric_names: Vec<String>,
  pub runs: Vec<ComparisonRow>,
  pub warnings: Vec<Value>,
}

pub fn read(runs_dir: &Path, run_id: &str, filters: &[String]) -> Result<RunMetrics> {
  read_with_files(
    runs_dir,
    run_id,
    filters,
    MetricFiles {
      metrics: true,
      params: true,
    },
  )
}

/// Online callers pass the fresh selection so absent remote files cannot be
/// confused with artifacts retained from an earlier selective pull.
pub fn read_with_files(
  runs_dir: &Path,
  run_id: &str,
  filters: &[String],
  files: MetricFiles,
) -> Result<RunMetrics> {
  read_mode(runs_dir, run_id, filters, files, true)
}

/// Read complete scalar summaries without retaining metric point arrays.
/// Dashboard tables do not need curves; the file/row limits still bound reads.
pub fn read_summaries(runs_dir: &Path, run_id: &str, filters: &[String]) -> Result<RunMetrics> {
  read_mode(
    runs_dir,
    run_id,
    filters,
    MetricFiles {
      metrics: true,
      params: true,
    },
    false,
  )
}

fn read_mode(
  runs_dir: &Path,
  run_id: &str,
  filters: &[String],
  files: MetricFiles,
  retain_points: bool,
) -> Result<RunMetrics> {
  validate_filters(filters)?;
  let report = crate::runs::summary_directory(runs_dir, run_id)?;
  let runs_dir = fs::canonicalize(runs_dir)?;
  let run_dir = runs_dir.join(run_id);
  let mut result = RunMetrics {
    run_id: run_id.to_string(),
    run: report["run"].clone(),
    params: None,
    first_metric_timestamp: None,
    metrics: BTreeMap::new(),
    warnings: report["warnings"].as_array().cloned().unwrap_or_default(),
  };
  if files.params {
    match read_params(&run_dir) {
      Ok(params) => result.params = params,
      Err(error) => warning(&mut result, error.to_string()),
    }
  }
  if files.metrics {
    let file = open_output(&run_dir, "metrics.jsonl")?;
    if let Some(file) = file {
      read_events(file, &mut result, filters, retain_points)?;
    } else {
      warning(
        &mut result,
        "outputs/metrics.jsonl is missing; no metrics have been recorded",
      );
    }
  } else {
    warning(
      &mut result,
      "outputs/metrics.jsonl was absent from the current remote selection",
    );
  }
  for name in filters {
    if !result.metrics.contains_key(name) {
      warning(&mut result, format!("metric is not recorded: {name}"));
    }
  }
  Ok(result)
}

pub fn compare(
  runs: &[RunMetrics],
  filters: &[String],
  reduction: Reduction,
) -> Result<Comparison> {
  validate_filters(filters)?;
  let ids: BTreeSet<_> = runs.iter().map(|run| &run.run_id).collect();
  if runs.len() < 2 || ids.len() != runs.len() {
    return Err(message("comparison requires at least two distinct runs"));
  }
  let available: BTreeSet<_> = runs
    .iter()
    .flat_map(|run| run.metrics.keys().cloned())
    .collect();
  let metric_names = if filters.is_empty() {
    available.into_iter().collect()
  } else {
    let mut names = Vec::new();
    for name in filters {
      if !available.contains(name) {
        return Err(message(format!(
          "requested metric is not recorded in any selected run: {name}"
        )));
      }
      if !names.contains(name) {
        names.push(name.clone());
      }
    }
    names
  };
  let rows = runs
    .iter()
    .map(|run| ComparisonRow {
      run_id: run.run_id.clone(),
      run: run.run.clone(),
      params: run.params.clone(),
      values: metric_names
        .iter()
        .map(|name| {
          let point = run
            .metrics
            .get(name)
            .map(|series| match reduction {
              Reduction::Last => &series.summary.last,
              Reduction::Min => &series.summary.min,
              Reduction::Max => &series.summary.max,
            })
            .cloned();
          (name.clone(), point)
        })
        .collect(),
    })
    .collect();
  Ok(Comparison {
    reduction,
    metric_names,
    runs: rows,
    warnings: runs
      .iter()
      .flat_map(|run| run.warnings.iter().cloned())
      .collect(),
  })
}

/// Inspect parameters independently when a metric artifact cannot be read.
pub fn read_parameters(runs_dir: &Path, run_id: &str) -> Result<Option<Value>> {
  crate::runs::summary_directory(runs_dir, run_id)?;
  read_params(&fs::canonicalize(runs_dir)?.join(run_id))
}

fn read_params(run_dir: &Path) -> Result<Option<Value>> {
  let Some(file) = open_output(run_dir, "params.json")? else {
    return Ok(None);
  };
  if file.metadata()?.len() > LINE_LIMIT as u64 {
    return Err(message("outputs/params.json exceeds the 1 MiB size limit"));
  }
  let mut raw = Vec::new();
  file.take(LINE_LIMIT as u64 + 1).read_to_end(&mut raw)?;
  if raw.len() > LINE_LIMIT {
    return Err(message("outputs/params.json exceeds the 1 MiB size limit"));
  }
  parse_parameters(&raw).map(Some)
}

/// Parse the same parameter envelope for local files and bounded hosted reads.
pub(crate) fn parse_parameters(raw: &[u8]) -> Result<Value> {
  if raw.len() > LINE_LIMIT {
    return Err(message("outputs/params.json exceeds the 1 MiB size limit"));
  }
  let value: Value = serde_json::from_slice(raw).map_err(|error| {
    message(format!(
      "outputs/params.json contains invalid JSON: {error}"
    ))
  })?;
  if !value.is_object() {
    return Err(message("outputs/params.json must contain an object"));
  }
  if let Some(schema) = value.get("schema_version") {
    if schema.as_u64() != Some(1) {
      return Err(message(
        "outputs/params.json uses an unsupported schema_version",
      ));
    }
    let params = value
      .get("params")
      .filter(|params| params.is_object())
      .ok_or_else(|| message("outputs/params.json params must be an object"))?;
    Ok(params.clone())
  } else {
    // Plain parameter objects remain easy to write from an existing trainer.
    Ok(value)
  }
}

fn open_output(run_dir: &Path, name: &str) -> Result<Option<File>> {
  let outputs = run_dir.join("outputs");
  let Some(metadata) = optional_metadata(&outputs)? else {
    return Ok(None);
  };
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(message("outputs must be a real directory"));
  }
  let path = outputs.join(name);
  let Some(initial) = optional_metadata(&path)? else {
    return Ok(None);
  };
  if !initial.is_file() || initial.file_type().is_symlink() {
    return Err(message(format!("outputs/{name} must be a regular file")));
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options.open(&path)?;
  let opened = file.metadata()?;
  let current = fs::symlink_metadata(&path)?;
  if !opened.is_file() || !current.is_file() || current.file_type().is_symlink() {
    return Err(message(format!("outputs/{name} must be a regular file")));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if initial.dev() != opened.dev() || initial.ino() != opened.ino() {
      return Err(message(format!("outputs/{name} changed while opening")));
    }
  }
  let current_parent = fs::symlink_metadata(&outputs)?;
  if !current_parent.is_dir() || current_parent.file_type().is_symlink() {
    return Err(message("outputs must be a real directory"));
  }
  Ok(Some(file))
}

fn optional_metadata(path: &Path) -> std::io::Result<Option<fs::Metadata>> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error),
  }
}

fn read_events(
  file: File,
  result: &mut RunMetrics,
  filters: &[String],
  retain_points: bool,
) -> Result<()> {
  let size = file.metadata()?.len();
  if size > METRICS_FILE_LIMIT {
    return Err(message(
      "outputs/metrics.jsonl exceeds the 128 MiB read limit",
    ));
  }
  read_event_data(file, size, result, filters, retain_points, None)
}

/// Hosted readers keep complete scalar summaries while bounding retained curve
/// data independently of the input length. Local CLI reads retain their existing
/// point limit and exported chart behavior.
pub(crate) fn read_event_data(
  reader: impl Read,
  size: u64,
  result: &mut RunMetrics,
  filters: &[String],
  retain_points: bool,
  preview_point_limit: Option<usize>,
) -> Result<()> {
  validate_filters(filters)?;
  if size > METRICS_FILE_LIMIT {
    return Err(message(
      "outputs/metrics.jsonl exceeds the 128 MiB read limit",
    ));
  }
  if preview_point_limit.is_some_and(|limit| limit < 4) {
    return Err(message("metric preview point limit must be at least four"));
  }
  // Inspect the file extent at open time. A busy writer cannot extend this read
  // indefinitely, and a partly written final row is reported rather than guessed.
  let mut reader = BufReader::new(reader.take(size));
  let mut line_number = 0;
  let mut previous_step = None;
  let mut warned_steps = false;
  let mut points = 0;
  let mut sampled = false;
  let mut raw = Vec::new();
  while let Some((complete, oversized)) = read_line(&mut reader, &mut raw)? {
    line_number += 1;
    if oversized {
      warning(
        result,
        format!("metrics.jsonl line {line_number} exceeds the 1 MiB limit; skipped"),
      );
      continue;
    }
    if raw.iter().all(u8::is_ascii_whitespace) {
      continue;
    }
    let value: Value = match serde_json::from_slice(&raw) {
      Ok(value) => value,
      Err(error) => {
        let description = if !complete && error.is_eof() {
          "incomplete final row"
        } else {
          "invalid JSON"
        };
        warning(
          result,
          format!("metrics.jsonl line {line_number}: {description}; skipped"),
        );
        continue;
      }
    };
    match event(value) {
      Ok((step, timestamp, metrics)) => {
        if result.first_metric_timestamp.is_none() {
          result.first_metric_timestamp = timestamp.clone();
        }
        if previous_step.is_some_and(|previous| step < previous) && !warned_steps {
          warning(
            result,
            format!(
              "metrics.jsonl line {line_number}: steps decreased; last values use recorded order"
            ),
          );
          warned_steps = true;
        }
        previous_step = Some(step);
        for (name, value) in metrics {
          if !filters.is_empty() && !filters.contains(&name) {
            continue;
          }
          points += 1;
          if preview_point_limit.is_some()
            && !result.metrics.contains_key(&name)
            && result.metrics.len() >= 2000
          {
            return Err(message(
              "hosted metric preview exceeds the 2000-series limit",
            ));
          }
          if retain_points && preview_point_limit.is_none() && points > POINT_LIMIT {
            return Err(message(
              "metrics exceed the 1,000,000 selected point limit; select fewer metrics",
            ));
          }
          add_point(
            result,
            name.clone(),
            MetricPoint {
              step,
              value,
              timestamp: timestamp.clone(),
            },
            retain_points,
          );
          if retain_points && let Some(limit) = preview_point_limit {
            let series = result.metrics.get_mut(&name).expect("metric just inserted");
            if series.points.len() > limit {
              sampled = true;
              // Keep timestamped endpoints as well: legacy rows without times
              // must not erase the only samples usable by a time-axis chart.
              thin_points(&mut series.points, limit);
            }
          }
        }
      }
      Err(error) => warning(
        result,
        format!("metrics.jsonl line {line_number}: {error}; skipped"),
      ),
    }
  }
  if sampled {
    warning(
      result,
      "Hosted chart curves are sampled to bound memory; last/min/max summaries use every recorded point.",
    );
  }
  Ok(())
}

/// Drain oversized lines in bounded chunks without allocating their full length.
fn read_line(
  reader: &mut impl BufRead,
  output: &mut Vec<u8>,
) -> std::io::Result<Option<(bool, bool)>> {
  output.clear();
  let mut seen = false;
  let mut oversized = false;
  loop {
    let bytes = reader.fill_buf()?;
    if bytes.is_empty() {
      return Ok(seen.then_some((false, oversized)));
    }
    seen = true;
    let newline = bytes.iter().position(|byte| *byte == b'\n');
    let size = newline.map(|position| position + 1).unwrap_or(bytes.len());
    if !oversized {
      if output.len() + size > LINE_LIMIT {
        oversized = true;
      } else {
        output.extend_from_slice(&bytes[..size]);
      }
    }
    reader.consume(size);
    if newline.is_some() {
      return Ok(Some((true, oversized)));
    }
  }
}

pub(crate) type ParsedEvent = (u64, Option<String>, BTreeMap<String, f64>);

pub(crate) fn event(value: Value) -> std::result::Result<ParsedEvent, String> {
  let object = value.as_object().ok_or("event must be an object")?;
  if object
    .get("schema_version")
    .is_some_and(|schema| schema.as_u64() != Some(1))
  {
    return Err("unsupported schema_version".to_string());
  }
  let step = object
    .get("step")
    .and_then(Value::as_u64)
    .ok_or("step must be a nonnegative 64-bit integer")?;
  let timestamp = match object.get("timestamp") {
    None => None,
    Some(Value::String(value))
      if value.len() <= 128 && chrono::DateTime::parse_from_rfc3339(value).is_ok() =>
    {
      Some(value.clone())
    }
    Some(_) => return Err("timestamp must be an RFC3339 string of at most 128 bytes".to_string()),
  };
  let metrics = object
    .get("metrics")
    .and_then(Value::as_object)
    .filter(|metrics| !metrics.is_empty())
    .ok_or("metrics must be a nonempty object of scalar numbers")?;
  let mut values = BTreeMap::new();
  for (name, value) in metrics {
    validate_name(name).map_err(|error| error.to_string())?;
    let value = value
      .as_f64()
      .filter(|value| value.is_finite())
      .ok_or_else(|| format!("metric {name:?} must be a finite scalar number"))?;
    values.insert(name.clone(), value);
  }
  Ok((step, timestamp, values))
}

fn add_point(run: &mut RunMetrics, name: String, point: MetricPoint, retain_points: bool) {
  match run.metrics.entry(name) {
    std::collections::btree_map::Entry::Vacant(entry) => {
      entry.insert(MetricSeries {
        summary: MetricSummary {
          count: 1,
          missing_timestamp_count: usize::from(point.timestamp.is_none()),
          last: point.clone(),
          min: point.clone(),
          max: point.clone(),
        },
        points: if retain_points {
          vec![point]
        } else {
          Vec::new()
        },
      });
    }
    std::collections::btree_map::Entry::Occupied(mut entry) => {
      let series = entry.get_mut();
      series.summary.count += 1;
      series.summary.missing_timestamp_count += usize::from(point.timestamp.is_none());
      series.summary.last = point.clone();
      if point.value < series.summary.min.value {
        series.summary.min = point.clone();
      }
      if point.value > series.summary.max.value {
        series.summary.max = point.clone();
      }
      if retain_points {
        series.points.push(point);
      }
    }
  }
}

fn thin_points(points: &mut Vec<MetricPoint>, limit: usize) {
  let mut selected = BTreeSet::from([0, points.len() - 1]);
  if let Some(first) = points.iter().position(|point| point.timestamp.is_some()) {
    selected.insert(first);
  }
  if let Some(last) = points.iter().rposition(|point| point.timestamp.is_some()) {
    selected.insert(last);
  }
  let target = (limit / 2 + 2).min(limit);
  // Source-order spacing bounds the preview without letting additional endpoint
  // guarantees exceed its budget. Complete reductions are retained separately.
  for index in 0..target {
    if selected.len() >= target {
      break;
    }
    selected.insert(index * (points.len() - 1) / (target - 1));
  }
  *points = points
    .drain(..)
    .enumerate()
    .filter_map(|(index, point)| selected.contains(&index).then_some(point))
    .collect();
}

pub(crate) fn validate_filters(filters: &[String]) -> Result<()> {
  for name in filters {
    validate_name(name)?;
  }
  Ok(())
}

fn validate_name(name: &str) -> Result<()> {
  if name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
    return Err(message(
      "metric names must be nonempty, contain no control characters, and use at most 256 UTF-8 bytes",
    ));
  }
  Ok(())
}

fn warning(run: &mut RunMetrics, message: impl Into<String>) {
  if run.warnings.len() < WARNING_LIMIT {
    run
      .warnings
      .push(json!({"run_id": run.run_id, "message": message.into()}));
  } else if run.warnings.len() == WARNING_LIMIT {
    run
      .warnings
      .push(json!({"run_id": run.run_id, "message": "additional metric warnings omitted"}));
  }
}

fn message(message: impl Into<String>) -> ExpriError {
  ExpriError::Message(message.into())
}

#[cfg(test)]
#[path = "metrics/tests.rs"]
mod tests;
