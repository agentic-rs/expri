mod reader;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::storage::ObjectStorage;
use super::store::{ApiError, ApiResult, Store};
use super::types::{FileRecord, FileTarget, RunScope, STREAM_BATCH, validate_component};
use crate::dashboard::preview::{bounded_warnings, preview, run_metadata};
use crate::error::{ExpriError, Result};
use crate::metrics::{self, Reduction, RunMetrics};

const SOURCE_LIMIT: usize = 1000;
const OVERVIEW_LIMIT: usize = 500;
const STATE_LIMIT: u64 = 256 * 1024;
const JSON_LIMIT: u64 = 1024 * 1024;
const METRICS_LIMIT: u64 = 16 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Serialize)]
struct Source {
  source_id: String,
  label: String,
  kind: &'static str,
  target_name: Option<String>,
  project_id: String,
  origin: String,
}

struct MetricReadMode {
  minimum: usize,
  retain_points: bool,
  include_params: bool,
}

/// Review reads only the requested metadata, scalar events, and log tails.
/// Checkpoints, inputs, and arbitrary output files are never opened here.
pub(super) struct HostedDashboard<'a, S> {
  store: &'a Store<S>,
  client: Client,
}

impl<S: ObjectStorage> crate::dashboard::DashboardView for HostedDashboard<'_, S> {
  fn catalog(&self) -> Result<Value> {
    HostedDashboard::catalog(self)
  }
  fn list(
    &self,
    source: &str,
    search: Option<&str>,
    task: Option<&str>,
    status: Option<&str>,
    limit: usize,
    offset: usize,
  ) -> Result<Value> {
    HostedDashboard::list(self, source, search, task, status, limit, offset)
  }
  fn detail(&self, source: &str, run_id: &str) -> Result<Value> {
    HostedDashboard::detail(self, source, run_id)
  }
  fn log(&self, source: &str, run_id: &str, stream: &str, tail: usize) -> Result<Value> {
    HostedDashboard::log(self, source, run_id, stream, tail)
  }
  fn compare(
    &self,
    source: &str,
    run_ids: &[String],
    filters: &[String],
    reduction: Reduction,
  ) -> Result<Value> {
    HostedDashboard::compare(self, source, run_ids, filters, reduction)
  }
  fn chart(&self, source: &str, run_ids: &[String], filters: &[String]) -> Result<String> {
    HostedDashboard::chart(self, source, run_ids, filters)
  }
}

impl<'a, S: ObjectStorage> HostedDashboard<'a, S> {
  pub fn new(store: &'a Store<S>) -> Result<Self> {
    super::storage::init_tls();
    let client = Client::builder()
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(5))
      .timeout(READ_TIMEOUT)
      .build()
      .map_err(|_| message("cannot initialize dashboard storage reader"))?;
    Ok(Self { store, client })
  }

  pub fn catalog(&self) -> Result<Value> {
    let page = self
      .store
      .dashboard_sources(SOURCE_LIMIT, 0)
      .map_err(api_error)?;
    let sources: Vec<_> = page
      .items
      .into_iter()
      .map(|source| source_record(&source.project_id, &source.origin))
      .collect();
    let mut warnings = Vec::new();
    if page.total_count > SOURCE_LIMIT {
      warnings.push(
        json!({"message": "The hosted catalog shows the first 1000 project/worker sources."}),
      );
    }
    Ok(
      json!({"project_name": "Hosted experiments", "access_mode": "hosted", "initial_source": sources.first().map_or("", |source| source.source_id.as_str()), "sources": sources, "warnings": warnings}),
    )
  }

  pub fn list(
    &self,
    source_id: &str,
    search: Option<&str>,
    task: Option<&str>,
    status: Option<&str>,
    limit: usize,
    offset: usize,
  ) -> Result<Value> {
    if !(1..=100).contains(&limit) || offset > OVERVIEW_LIMIT {
      return Err(message(
        "hosted list pages support 1 to 100 rows and offsets up to 500",
      ));
    }
    if task.is_some_and(|value| value.trim().is_empty())
      || search.is_some_and(|value| value.len() > 512)
    {
      return Err(message("invalid hosted run filter"));
    }
    if status.is_some_and(|value| {
      ![
        "preparing",
        "running",
        "completed",
        "failed",
        "cancelled",
        "lost",
        "unknown",
      ]
      .contains(&value)
    }) {
      return Err(message("invalid run status"));
    }
    let source = parse_source(source_id)?;
    let page = self
      .store
      .dashboard_runs(&source.project_id, &source.origin, OVERVIEW_LIMIT, 0)
      .map_err(api_error)?;
    if page.total_count == 0 {
      return Err(message(format!("unknown source: {source_id}")));
    }
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut reports = Vec::with_capacity(page.items.len());
    let mut warnings = Vec::new();
    if page.total_count > OVERVIEW_LIMIT {
      warnings.push(json!({"message": "Hosted browsing and filters cover the 500 runs most recently updated in this service; use expri service pull with a known run ID to inspect other runs."}));
    }
    if page.legacy_order {
      warnings.push(json!({"message": "Some legacy runs have reconstructed catalog order; their historical service update times are unknown. New uploads or stream bytes establish their current service update order."}));
    }
    for group in page.items.chunks(4) {
      let next = std::thread::scope(|threads| {
        let handles: Vec<_> = group
          .iter()
          .map(|scope| threads.spawn(move || self.overview(scope, deadline)))
          .collect();
        handles.into_iter().zip(group).map(|(handle, scope)| {
          match handle.join() {
            Ok(Ok(record)) => record,
            _ => unknown_overview(scope, "Run overview is temporarily unavailable or exceeded the request time budget; refresh to retry. Filters may omit runs whose overview is unavailable."),
          }
        }).collect::<Vec<_>>()
      });
      reports.extend(next);
    }
    let search = search.unwrap_or_default().to_lowercase();
    let overview_truncated = reports
      .iter()
      .any(|report| report["metadata_truncated"] == true);
    let mut rows = Vec::new();
    for report in reports {
      warnings.extend(report["warnings"].as_array().cloned().unwrap_or_default());
      let run = &report["run"];
      if task.is_some_and(|task| report["task_filter"].as_str() != Some(task))
        || status.is_some_and(|status| run["status"].as_str() != Some(status))
      {
        continue;
      }
      if !search.is_empty()
        && !["run_id", "status"].iter().any(|field| {
          run[field]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains(&search)
        })
        && !report["task_filter"]
          .as_str()
          .unwrap_or_default()
          .to_lowercase()
          .contains(&search)
      {
        continue;
      }
      rows.push(run.clone());
    }
    rows.sort_by(|left, right| {
      started_at(right)
        .cmp(&started_at(left))
        .then_with(|| right["run_id"].as_str().cmp(&left["run_id"].as_str()))
    });
    let total_count = rows.len();
    let mut metadata_truncated = overview_truncated;
    let rows: Vec<_> = rows
      .into_iter()
      .skip(offset)
      .take(limit)
      .map(|row| preview(&row, &mut metadata_truncated))
      .collect();
    let next_offset = offset.saturating_add(rows.len());
    Ok(
      json!({"source": source, "runs": rows, "warnings": bounded_warnings(&warnings), "total_count": total_count, "catalog_total_count": page.total_count, "limit": limit, "offset": offset, "next_offset": (next_offset < total_count).then_some(next_offset), "metadata_truncated": metadata_truncated}),
    )
  }

  pub fn detail(&self, source_id: &str, run_id: &str) -> Result<Value> {
    let (source, scope) = self.scope(source_id, run_id)?;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let state = self.json_artifact(&scope, "run-state.json", STATE_LIMIT, deadline);
    let mut warnings = Vec::new();
    let state = optional_record(state, &scope, "run-state.json", &mut warnings);
    let mut record = crate::runs::summary_from_state(run_id, state);
    warnings.extend(record["warnings"].as_array().cloned().unwrap_or_default());
    for (field, path) in [
      ("snapshot", "snapshot.json"),
      ("environment", "environment/environment-state.json"),
    ] {
      record[field] = optional_record(
        self.json_artifact(&scope, path, JSON_LIMIT, deadline),
        &scope,
        path,
        &mut warnings,
      )
      .unwrap_or(Value::Null);
    }
    let mut params_truncated = false;
    let params = match self.parameters(&scope, deadline) {
      Ok(Some(value)) => preview(&value, &mut params_truncated),
      Ok(None) => Value::Null,
      Err(error) => {
        warnings.push(json!({"run_id": run_id, "message": error.message}));
        Value::Null
      }
    };
    let mut result = metric_record(&scope, record["run"].clone(), None);
    let mut metrics_error = None;
    if let Err(error) = self.metric_data(&scope, &mut result, &[], false, deadline) {
      metrics_error = Some(error.to_string());
    }
    warnings.extend(result.warnings);
    let metric_count = result.metrics.len();
    let summaries: BTreeMap<_, _> = result
      .metrics
      .into_iter()
      .take(50)
      .map(|(name, series)| (name, series.summary))
      .collect();
    if metric_count > 50 {
      warnings.push(json!({"message": "Showing the first 50 metric summaries; enter an exact metric name to chart another series."}));
    }
    let mut metadata_truncated = false;
    let metadata = run_metadata(&record, &mut metadata_truncated);
    let run = preview(&record["run"], &mut metadata_truncated);
    if metadata_truncated || params_truncated {
      warnings.push(json!({"message": "Hosted previews are limited; download the original run records for complete metadata and parameters."}));
    }
    Ok(
      json!({"source": source, "run": run, "state": metadata["state"], "snapshot": metadata["snapshot"], "environment": metadata["environment"], "metadata_truncated": metadata_truncated, "params": params, "params_truncated": params_truncated, "metrics": summaries, "metric_count": metric_count, "metrics_truncated": metric_count > 50, "metrics_error": metrics_error, "warnings": bounded_warnings(&warnings), "cache": null}),
    )
  }

  pub fn log(&self, source_id: &str, run_id: &str, stream: &str, tail: usize) -> Result<Value> {
    if !matches!(stream, "stdout" | "stderr") || tail > 1000 {
      return Err(message(
        "stream must be stdout or stderr and tail must be at most 1000",
      ));
    }
    let (_, scope) = self.scope(source_id, run_id)?;
    let path = format!("logs/{stream}.log");
    let Some(record) = self
      .store
      .dashboard_artifact(&scope, &path)
      .map_err(api_error)?
    else {
      return Ok(json!({"content": "", "stream": stream, "missing": true, "truncated": false}));
    };
    let start = record.size.saturating_sub(STREAM_BATCH as u64);
    let mut bytes = self
      .range(
        &record,
        start,
        (record.size - start) as usize,
        Instant::now() + REQUEST_TIMEOUT,
      )
      .map_err(api_error)?;
    if start > 0
      && let Some(newline) = bytes.iter().position(|byte| *byte == b'\n')
      && newline + 1 < bytes.len()
    {
      bytes.drain(..=newline);
    }
    let lines: Vec<_> = bytes.split_inclusive(|byte| *byte == b'\n').collect();
    let content: Vec<u8> = lines
      .iter()
      .skip(lines.len().saturating_sub(tail))
      .flat_map(|line| line.iter().copied())
      .collect();
    Ok(
      json!({"content": String::from_utf8_lossy(&content), "stream": stream, "missing": false, "truncated": start > 0}),
    )
  }

  pub fn compare(
    &self,
    source_id: &str,
    run_ids: &[String],
    filters: &[String],
    reduction: Reduction,
  ) -> Result<Value> {
    let source = parse_source(source_id)?;
    let runs = self.read_metrics(
      source_id,
      run_ids,
      filters,
      MetricReadMode {
        minimum: 2,
        retain_points: false,
        include_params: false,
      },
      Instant::now() + REQUEST_TIMEOUT,
    )?;
    let (filters, omitted) = selected_metrics(&runs, filters);
    let mut comparison = metrics::compare(&runs, &filters, reduction)?;
    for run in &mut comparison.runs {
      run.params = None;
      run.run = preview(&run.run, &mut false);
    }
    comparison.warnings = bounded_warnings(&comparison.warnings);
    if omitted {
      comparison.warnings.push(json!({"message": "Showing the first four metrics; select exact metric names to compare other series."}));
    }
    Ok(json!({"source": source, "comparison": comparison}))
  }

  pub fn chart(&self, source_id: &str, run_ids: &[String], filters: &[String]) -> Result<String> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let selected = if filters.is_empty() {
      let summaries = self.read_metrics(
        source_id,
        run_ids,
        &[],
        MetricReadMode {
          minimum: 1,
          retain_points: false,
          include_params: false,
        },
        deadline,
      )?;
      selected_metrics(&summaries, &[]).0
    } else {
      filters.to_vec()
    };
    let runs = self.read_metrics(
      source_id,
      run_ids,
      &selected,
      MetricReadMode {
        minimum: 1,
        retain_points: !selected.is_empty(),
        include_params: true,
      },
      deadline,
    )?;
    crate::metric_charts::render_dashboard_chart(&runs, &selected)
  }

  fn scope(&self, source_id: &str, run_id: &str) -> Result<(Source, RunScope)> {
    let source = parse_source(source_id)?;
    validate_component(run_id)?;
    let scope = RunScope {
      project_id: source.project_id.clone(),
      origin: source.origin.clone(),
      run_id: run_id.into(),
    };
    if !self.store.dashboard_run_exists(&scope).map_err(api_error)? {
      return Err(message(format!("run is missing: {run_id}")));
    }
    Ok((source, scope))
  }

  fn overview(&self, scope: &RunScope, deadline: Instant) -> ApiResult<Value> {
    let record = self.store.dashboard_artifact(scope, "run-state.json")?;
    let version = record
      .as_ref()
      .and_then(|record| record.sha256.as_deref())
      .unwrap_or("missing");
    if let Some(value) = self.store.dashboard_cached_overview(scope, version)?
      && value["overview_schema"] == 1
    {
      return Ok(value);
    }
    let mut warnings = Vec::new();
    let state = optional_record(
      record
        .as_ref()
        .map(|record| self.json_record(record, "run-state.json", STATE_LIMIT, deadline))
        .transpose()
        .map(|value| value.flatten()),
      scope,
      "run-state.json",
      &mut warnings,
    );
    let normalized = crate::runs::summary_from_state(&scope.run_id, state);
    warnings.extend(
      normalized["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default(),
    );
    if record.is_some() && normalized["state"].is_null() {
      warnings.push(json!({"run_id": scope.run_id, "message": "Filters may omit runs whose overview is unavailable; refresh to retry."}));
    }
    let task_filter = normalized["run"]["task"]
      .as_str()
      .filter(|task| task.len() <= 4096);
    if normalized["run"]["task"].is_string() && task_filter.is_none() {
      warnings.push(json!({"run_id": scope.run_id, "message": "This task name exceeds the 4 KiB hosted filter limit; task filters and searches omit it. Download the original state for its full name."}));
    }
    let mut metadata_truncated = false;
    let run = preview(&normalized["run"], &mut metadata_truncated);
    let result = json!({"overview_schema": 1, "run": run, "task_filter": task_filter, "metadata_truncated": metadata_truncated, "warnings": bounded_warnings(&warnings)});
    // A failed network read must remain retryable; a missing or malformed
    // immutable state can safely share its versioned normalized preview.
    if record.is_none() || normalized["state"].is_object() {
      self
        .store
        .dashboard_cache_overview(scope, version, &result)?;
    }
    Ok(result)
  }

  fn json_artifact(
    &self,
    scope: &RunScope,
    path: &str,
    limit: u64,
    deadline: Instant,
  ) -> ApiResult<Option<Value>> {
    let Some(record) = self.store.dashboard_artifact(scope, path)? else {
      return Ok(None);
    };
    self.json_record(&record, path, limit, deadline)
  }

  fn json_record(
    &self,
    record: &FileRecord,
    path: &str,
    limit: u64,
    deadline: Instant,
  ) -> ApiResult<Option<Value>> {
    if record.size > limit {
      return Err(ApiError::new(
        413,
        format!("{path} exceeds the hosted metadata read limit"),
      ));
    }
    let bytes = self.range(record, 0, record.size as usize, deadline)?;
    verify_bytes(record, &bytes)?;
    serde_json::from_slice(&bytes)
      .map(Some)
      .map_err(|_| ApiError::new(422, format!("{path} contains invalid JSON")))
  }

  fn parameters(&self, scope: &RunScope, deadline: Instant) -> ApiResult<Option<Value>> {
    let Some(record) = self
      .store
      .dashboard_artifact(scope, "outputs/params.json")?
    else {
      return Ok(None);
    };
    if record.size > JSON_LIMIT {
      return Err(ApiError::new(
        413,
        "outputs/params.json exceeds the 1 MiB size limit",
      ));
    }
    let bytes = self.range(&record, 0, record.size as usize, deadline)?;
    verify_bytes(&record, &bytes)?;
    metrics::parse_parameters(&bytes)
      .map(Some)
      .map_err(|error| ApiError::new(422, error.to_string()))
  }

  fn read_metrics(
    &self,
    source_id: &str,
    run_ids: &[String],
    filters: &[String],
    mode: MetricReadMode,
    deadline: Instant,
  ) -> Result<Vec<RunMetrics>> {
    if !(mode.minimum..=8).contains(&run_ids.len())
      || run_ids.iter().collect::<BTreeSet<_>>().len() != run_ids.len()
    {
      return Err(message(format!(
        "select {} to 8 distinct runs",
        mode.minimum
      )));
    }
    if filters.len() > 6 {
      return Err(message(
        "select at most six metrics for a dashboard comparison",
      ));
    }
    run_ids
      .iter()
      .map(|run_id| {
        let (_, scope) = self.scope(source_id, run_id)?;
        let overview = self.overview(&scope, deadline).map_err(api_error)?;
        let mut result = metric_record(&scope, overview["run"].clone(), None);
        result.warnings = overview["warnings"].as_array().cloned().unwrap_or_default();
        if mode.include_params {
          match self.parameters(&scope, deadline) {
            Ok(params) => result.params = params,
            Err(error) => result
              .warnings
              .push(json!({"run_id": run_id, "message": error.message})),
          }
        }
        self.metric_data(&scope, &mut result, filters, mode.retain_points, deadline)?;
        Ok(result)
      })
      .collect()
  }
}

fn source_record(project_id: &str, origin: &str) -> Source {
  Source {
    source_id: format!("hosted:{project_id}:{origin}"),
    label: format!("{project_id} / {origin}"),
    kind: "hosted",
    target_name: None,
    project_id: project_id.into(),
    origin: origin.into(),
  }
}

fn started_at(run: &Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
  run["started_at"]
    .as_str()
    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
}

fn parse_source(source_id: &str) -> Result<Source> {
  let mut parts = source_id.split(':');
  if parts.next() != Some("hosted") {
    return Err(message(format!("unknown source: {source_id}")));
  }
  let project_id = parts.next().unwrap_or_default();
  let origin = parts.next().unwrap_or_default();
  if parts.next().is_some()
    || validate_component(project_id).is_err()
    || validate_component(origin).is_err()
  {
    return Err(message(format!("unknown source: {source_id}")));
  }
  Ok(source_record(project_id, origin))
}

fn metric_record(scope: &RunScope, run: Value, params: Option<Value>) -> RunMetrics {
  RunMetrics {
    run_id: scope.run_id.clone(),
    run,
    params,
    metrics: BTreeMap::new(),
    warnings: Vec::new(),
  }
}

fn optional_record(
  result: ApiResult<Option<Value>>,
  scope: &RunScope,
  path: &str,
  warnings: &mut Vec<Value>,
) -> Option<Value> {
  match result {
    Ok(Some(value)) if value.is_object() => Some(value),
    result => {
      let text = match result {
        Err(error) => error.message,
        Ok(None) => format!("{path} is missing"),
        _ => format!("{path} must contain a JSON object"),
      };
      warnings.push(json!({"run_id": scope.run_id, "message": text}));
      None
    }
  }
}

fn unknown_overview(scope: &RunScope, warning: &str) -> Value {
  let mut report = crate::runs::summary_from_state(&scope.run_id, None);
  report["warnings"] = json!([{"run_id": scope.run_id, "message": warning}]);
  report
}

fn selected_metrics(runs: &[RunMetrics], filters: &[String]) -> (Vec<String>, bool) {
  if !filters.is_empty() {
    return (
      filters
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect(),
      false,
    );
  }
  let available: BTreeSet<_> = runs
    .iter()
    .flat_map(|run| run.metrics.keys().cloned())
    .collect();
  let omitted = available.len() > 4;
  (available.into_iter().take(4).collect(), omitted)
}

fn verify_bytes(record: &FileRecord, bytes: &[u8]) -> ApiResult<()> {
  if let Some(digest) = &record.sha256
    && hex_digest(&Sha256::digest(bytes)) != *digest
  {
    return Err(ApiError::new(
      409,
      "dashboard artifact changed or failed integrity verification; refresh the run",
    ));
  }
  Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn api_error(error: ApiError) -> ExpriError {
  message(error.message)
}
fn message(text: impl Into<String>) -> ExpriError {
  ExpriError::Message(text.into())
}

#[cfg(test)]
mod tests;
