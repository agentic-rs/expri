mod reader;
mod source;
mod table_data;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use source::{Source, parse_source, source_record};

use super::storage::ObjectStorage;
use super::store::{ApiError, ApiResult, Store};
use super::types::{
  FileRecord, FileTarget, ProjectDeletePreview, ProjectDeletionStatus, ProjectStorageStats,
  RunScope, STREAM_BATCH, validate_component,
};
use crate::dashboard::artifacts::Download;
use crate::dashboard::preview::{bounded_warnings, preview, run_metadata};
use crate::dashboard::table::{self, ListQuery};
use crate::error::{ExpriError, Result};
use crate::metric_charts::ChartXAxis;
use crate::metrics::{self, Reduction, RunMetrics};

const SOURCE_LIMIT: usize = 1000;
const OVERVIEW_LIMIT: usize = 500;
const STATE_LIMIT: u64 = 256 * 1024;
const JSON_LIMIT: u64 = 1024 * 1024;
const METRICS_LIMIT: u64 = 16 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

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
  fn projects(&self) -> Result<Option<Value>> {
    HostedDashboard::projects(self).map(Some)
  }
  fn storage(
    &self,
    project_id: &str,
    kind: &str,
    search: &str,
    limit: usize,
    offset: usize,
  ) -> Result<Option<Value>> {
    HostedDashboard::storage(self, project_id, kind, search, limit, offset).map(Some)
  }
  fn updates(&self, source: &str, run_ids: &[String]) -> Result<Value> {
    HostedDashboard::updates(self, source, run_ids)
  }
  fn list_table(&self, source: &str, query: &ListQuery<'_>) -> Result<Value> {
    HostedDashboard::list_table(self, source, query)
  }
  fn columns(&self, source: &str) -> Result<Value> {
    HostedDashboard::columns(self, source)
  }
  fn detail(&self, source: &str, run_id: &str) -> Result<Value> {
    HostedDashboard::detail(self, source, run_id)
  }
  fn artifacts(&self, source: &str, run_id: &str) -> Result<Value> {
    HostedDashboard::artifacts(self, source, run_id)
  }
  fn artifact_download(
    &self,
    source: &str,
    run_id: &str,
    path: &str,
  ) -> Result<crate::dashboard::artifacts::Download> {
    HostedDashboard::artifact_download(self, source, run_id, path)
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
  fn chart(
    &self,
    source: &str,
    run_ids: &[String],
    filters: &[String],
    x_axis: ChartXAxis,
  ) -> Result<String> {
    HostedDashboard::chart(self, source, run_ids, filters, x_axis)
  }
}

impl<'a, S: ObjectStorage> HostedDashboard<'a, S> {
  pub fn project_storage(&self, project_id: &str) -> ApiResult<ProjectStorageStats> {
    self.store.project_storage(project_id)
  }

  pub fn project_delete_preview(&self, project_id: &str) -> ApiResult<ProjectDeletePreview> {
    self.store.preview_project_delete(project_id)
  }

  pub fn delete_project(
    &self,
    project_id: &str,
    revision: &str,
    confirmation: &str,
  ) -> ApiResult<ProjectDeletionStatus> {
    self
      .store
      .delete_project(project_id, revision, confirmation)
  }

  pub fn project_deletion(&self, project_id: &str) -> ApiResult<ProjectDeletionStatus> {
    self.store.project_deletion(project_id)
  }

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

  pub fn storage(
    &self,
    project_id: &str,
    kind: &str,
    search: &str,
    limit: usize,
    offset: usize,
  ) -> Result<Value> {
    let page = self
      .store
      .dashboard_storage_objects(project_id, kind, search, limit, offset)
      .map_err(api_error)?;
    let mut items = Vec::with_capacity(page.items.len());
    for record in page.items {
      match record.target {
        FileTarget::Input { input_id, .. } => {
          let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs([("project_id", project_id), ("input_id", input_id.as_str())])
            .finish();
          items.push(json!({
            "input_id": input_id,
            "size": record.size,
            "download_url": format!("/api/input?{query}"),
          }));
        }
        FileTarget::Run { scope, path } => {
          let source = format!("hosted-project:{project_id}");
          let run_key = format!("{}:{}", scope.origin, scope.run_id);
          let download_url = crate::dashboard::artifacts::download_url(&source, &run_key, &path);
          items.push(json!({
            "origin": scope.origin,
            "run_id": scope.run_id,
            "path": path,
            "size": record.size,
            "download_url": download_url,
          }));
        }
      }
    }
    let next_offset = (offset + items.len() < page.total_count).then_some(offset + items.len());
    Ok(json!({
      "project_id": project_id,
      "kind": kind,
      "items": items,
      "total_count": page.total_count,
      "next_offset": next_offset,
    }))
  }

  pub fn input_download(
    &self,
    project_id: &str,
    input_id: &str,
  ) -> ApiResult<crate::dashboard::artifacts::Download> {
    use crate::dashboard::artifacts::{Download, disposition};
    validate_component(project_id).map_err(|_| ApiError::new(400, "invalid input project"))?;
    validate_component(input_id).map_err(|_| ApiError::new(400, "invalid input ID"))?;
    let (url, size) = self
      .store
      .dashboard_attachment_url(
        &FileTarget::Input {
          project_id: project_id.into(),
          input_id: input_id.into(),
        },
        &disposition(input_id),
      )
      .map_err(|error| {
        if error.status == 404 {
          ApiError::new(404, "input is missing")
        } else {
          error
        }
      })?;
    if !valid_storage_download_url(&url) {
      return Err(ApiError::new(502, "invalid object storage download URL"));
    }
    Ok(Download::Cloud {
      url,
      size,
      filename: input_id.into(),
    })
  }

  pub fn updates(&self, source_id: &str, run_ids: &[String]) -> Result<Value> {
    crate::dashboard::updates::validate_selection(source_id, run_ids)?;
    let source = if source_id.is_empty() {
      None
    } else {
      let parsed = parse_source(source_id)?;
      if parsed.origin.is_none() {
        let selections = run_ids
          .iter()
          .map(|key| parsed.resolve(key).map(|scope| (key.clone(), scope)))
          .collect::<Result<Vec<_>>>()?;
        return self
          .store
          .dashboard_project_updates(&parsed.project_id, &selections)
          .map_err(api_error);
      }
      Some(super::store::DashboardSource {
        project_id: parsed.project_id,
        origin: parsed.origin.unwrap(),
      })
    };
    self
      .store
      .dashboard_updates(source.as_ref(), run_ids)
      .map_err(api_error)
  }

  #[cfg(test)]
  pub fn list(
    &self,
    source_id: &str,
    search: Option<&str>,
    task: Option<&str>,
    status: Option<&str>,
    limit: usize,
    offset: usize,
  ) -> Result<Value> {
    self.list_table(
      source_id,
      &ListQuery {
        origin: None,
        search,
        task,
        status,
        limit,
        offset,
        table: None,
      },
    )
  }

  pub(super) fn list_table(&self, source_id: &str, query: &ListQuery<'_>) -> Result<Value> {
    let ListQuery {
      origin,
      search,
      task,
      status,
      limit,
      offset,
      table,
    } = *query;
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
    if let Some(origin) = origin {
      validate_component(origin)?;
      if source
        .origin
        .as_deref()
        .is_some_and(|selected| selected != origin)
      {
        return Err(message("machine filter is outside the dashboard source"));
      }
    }
    let page = self
      .store
      .dashboard_project_runs(
        &source.project_id,
        origin.or(source.origin.as_deref()),
        OVERVIEW_LIMIT,
        0,
      )
      .map_err(api_error)?;
    if page.total_count == 0
      && (source.origin.is_some()
        || !self
          .store
          .dashboard_input_project_exists(&source.project_id)
          .map_err(api_error)?)
    {
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
      for (mut report, scope) in next.into_iter().zip(group) {
        source.decorate_run(&mut report["run"], scope);
        if let Some(warnings) = report["warnings"].as_array_mut() {
          source.decorate_warnings(warnings, scope);
        }
        reports.push(report);
      }
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
        && !["run_id", "status", "origin"].iter().any(|field| {
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
    if let Some(options) = table {
      let mut table_rows = Vec::with_capacity(rows.len());
      for group in rows.chunks(4) {
        let next = std::thread::scope(|threads| {
          let handles: Vec<_> = group
            .iter()
            .map(|run| {
              let scope = source
                .resolve(&source.reference(run))
                .expect("validated run scope");
              threads.spawn(move || {
                let (data, warnings) = self.table_data(
                  &scope,
                  options.needs_params(),
                  options.needs_metrics(),
                  deadline,
                );
                (table::TableRow::new(run.clone(), data), warnings)
              })
            })
            .collect();
          handles
            .into_iter()
            .zip(group)
            .map(|(handle,run)| handle.join().unwrap_or_else(|_| (
              table::TableRow::new(run.clone(),table::ScalarData::default()),
              vec![json!({"run_id":run["run_id"],"message":"Table columns are unavailable; refresh to retry."})],
            )))
            .collect::<Vec<_>>()
        });
        for (row, mut next_warnings) in next {
          let scope = source.resolve(&source.reference(&row.run))?;
          source.decorate_warnings(&mut next_warnings, &scope);
          table_rows.push(row);
          warnings.extend(next_warnings);
        }
      }
      options.sort_rows(&mut table_rows);
      rows = table_rows
        .into_iter()
        .map(|row| options.project(row))
        .collect();
    } else {
      rows.sort_by(|left, right| {
        started_at(right)
          .cmp(&started_at(left))
          .then_with(|| right["run_id"].as_str().cmp(&left["run_id"].as_str()))
          .then_with(|| left["origin"].as_str().cmp(&right["origin"].as_str()))
      });
    }
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

  pub(super) fn columns(&self, source_id: &str) -> Result<Value> {
    let source = parse_source(source_id)?;
    let page = self
      .store
      .dashboard_project_runs(
        &source.project_id,
        source.origin.as_deref(),
        OVERVIEW_LIMIT,
        0,
      )
      .map_err(api_error)?;
    if page.total_count == 0
      && (source.origin.is_some()
        || !self
          .store
          .dashboard_input_project_exists(&source.project_id)
          .map_err(api_error)?)
    {
      return Err(message(format!("unknown source: {source_id}")));
    }
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut columns = table::Columns::default();
    let mut warnings = Vec::new();
    for group in page.items.chunks(4) {
      let results = std::thread::scope(|threads| {
        let handles: Vec<_> = group
          .iter()
          .map(|scope| threads.spawn(move || self.table_data(scope, true, true, deadline)))
          .collect();
        handles
          .into_iter()
          .zip(group)
          .map(|(handle,scope)| handle.join().unwrap_or_else(|_| (
            table::ScalarData::default(),
            vec![json!({"run_id":scope.run_id,"message":"Table columns are unavailable; refresh to retry."})],
          )))
          .collect::<Vec<_>>()
      });
      for ((data, mut next_warnings), scope) in results.into_iter().zip(group) {
        source.decorate_warnings(&mut next_warnings, scope);
        columns.add(&data);
        columns.truncated |= next_warnings.iter().any(|warning| {
          warning["message"]
            .as_str()
            .is_some_and(|message| message.contains("columns are unavailable"))
        });
        warnings.extend(next_warnings);
      }
    }
    columns.truncated |= page.total_count > OVERVIEW_LIMIT;
    if columns.truncated {
      warnings.push(json!({"message":"Column discovery covers the 500 most recently updated hosted runs and up to 100 parameter and metric keys; some columns are omitted."}));
    }
    Ok(
      json!({"source":source,"available_columns":columns.response(),"warnings":bounded_warnings(&warnings)}),
    )
  }

  pub fn detail(&self, source_id: &str, run_id: &str) -> Result<Value> {
    let (source, scope) = self.scope(source_id, run_id)?;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let state = self.json_artifact(&scope, "run-state.json", STATE_LIMIT, deadline);
    let mut warnings = Vec::new();
    let state = optional_record(state, &scope, "run-state.json", &mut warnings);
    let mut record = crate::runs::summary_from_state(&scope.run_id, state);
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
    let mut run = preview(&record["run"], &mut metadata_truncated);
    source.decorate_run(&mut run, &scope);
    source.decorate_warnings(&mut warnings, &scope);
    if metadata_truncated || params_truncated {
      warnings.push(json!({"message": "Hosted previews are limited; download the original run records for complete metadata and parameters."}));
    }
    let archive = match self
      .store
      .execute(super::types::Request::ArchiveStatus {
        scope: scope.clone(),
      })
      .map_err(api_error)?
    {
      super::types::Response::Archive { archive } => {
        let mut value = serde_json::to_value(&archive)?;
        value["download_url"] = if archive.status == "archived" && archive.file.is_some() {
          let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs([("source", source_id), ("run_id", run_id)])
            .finish();
          json!(format!("/api/archive?{query}"))
        } else {
          Value::Null
        };
        value
      }
      _ => return Err(message("invalid result archive response")),
    };
    Ok(
      json!({"source": source, "archive": archive, "run": run, "state": metadata["state"], "snapshot": metadata["snapshot"], "environment": metadata["environment"], "metadata_truncated": metadata_truncated, "params": params, "params_truncated": params_truncated, "metrics": summaries, "metric_count": metric_count, "metrics_truncated": metric_count > 50, "metrics_error": metrics_error, "warnings": bounded_warnings(&warnings), "cache": null}),
    )
  }

  pub(super) fn archive_download(&self, source_id: &str, run_id: &str) -> Result<Download> {
    let (_, scope) = self.scope(source_id, run_id)?;
    let (url, size) = self
      .store
      .dashboard_archive_attachment(&scope)
      .map_err(api_error)?;
    let parsed = reqwest::Url::parse(&url).map_err(|_| message("invalid archive download URL"))?;
    if !matches!(parsed.scheme(), "http" | "https")
      || parsed.host_str().is_none()
      || !parsed.username().is_empty()
      || parsed.password().is_some()
      || parsed.fragment().is_some()
    {
      return Err(message("invalid archive download URL"));
    }
    Ok(Download::Cloud {
      url,
      size,
      filename: "result.zip".into(),
    })
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

  pub fn chart(
    &self,
    source_id: &str,
    run_ids: &[String],
    filters: &[String],
    x_axis: ChartXAxis,
  ) -> Result<String> {
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
    crate::metric_charts::render_dashboard_chart(&runs, &selected, x_axis)
  }

  fn scope(&self, source_id: &str, run_id: &str) -> Result<(Source, RunScope)> {
    let source = parse_source(source_id)?;
    let scope = source.resolve(run_id)?;
    if !self.store.dashboard_run_exists(&scope).map_err(api_error)? {
      return Err(message(format!("run is missing: {run_id}")));
    }
    Ok((source, scope))
  }

  pub fn artifacts(&self, source_id: &str, run_id: &str) -> Result<Value> {
    use crate::dashboard::artifacts::{ArtifactRow, bound_rows, download_url, parse_inventory};
    let (source, scope) = self.scope(source_id, run_id)?;
    let mut rows = BTreeMap::new();
    let mut warnings = Vec::new();
    let mut truncated = false;
    let mut recorded_at = None;
    // Only this small published manifest is fetched. Checkpoint objects are
    // described by SQLite records and never read to construct the catalog.
    match self.json_artifact(&scope, crate::run_artifacts::INVENTORY_PATH, crate::run_artifacts::INVENTORY_LIMIT as u64, Instant::now() + REQUEST_TIMEOUT)
      .map_err(api_error).and_then(|value| value.as_ref().map(parse_inventory).transpose()) {
      Ok(Some(inventory)) => {
        truncated |= inventory.truncated;
        recorded_at = inventory.recorded_at;
        for file in inventory.files {
          rows.insert(file.path.clone(), ArtifactRow { path: file.path, size: file.size,
            local: None, cloud: None, worker: Some(true), download_url: None });
        }
      }
      Ok(None) => {}
      Err(_) => warnings.push(json!({"message": "Worker artifact inventory is unavailable or invalid; reported availability is unknown."})),
    }
    let (objects, objects_truncated) = self
      .store
      .dashboard_output_objects(&scope)
      .map_err(api_error)?;
    truncated |= objects_truncated;
    for object in objects {
      let FileTarget::Run { path, .. } = object.target else {
        unreachable!("validated output target")
      };
      let row = rows.entry(path.clone()).or_insert_with(|| ArtifactRow {
        path: path.clone(),
        size: object.size,
        local: None,
        cloud: None,
        worker: None,
        download_url: None,
      });
      row.size = object.size;
      row.cloud = Some(true);
      row.download_url = Some(download_url(source_id, run_id, &path));
    }
    if !objects_truncated {
      for row in rows.values_mut() {
        if row.cloud.is_none() {
          row.cloud = Some(false);
        }
      }
    }
    let files = bound_rows(rows, &mut truncated)?;
    if truncated {
      warnings.push(json!({"message": "Artifact listing is limited to 200 files and bounded metadata; some files are omitted."}));
    }
    source.decorate_warnings(&mut warnings, &scope);
    let mut result = json!({"source": source, "run_id": scope.run_id, "files": files, "truncated": truncated,
      "warnings": bounded_warnings(&warnings), "pull_scope": scope, "inventory_recorded_at": recorded_at});
    if source.origin.is_none() {
      result["run_key"] = json!(run_id);
    }
    Ok(result)
  }

  fn artifact_download(
    &self,
    source_id: &str,
    run_id: &str,
    path: &str,
  ) -> Result<crate::dashboard::artifacts::Download> {
    use crate::dashboard::artifacts::{Download, disposition, filename, validate_output};
    validate_output(path)?;
    let (_, scope) = self.scope(source_id, run_id)?;
    let filename = filename(path).to_string();
    let (url, size) = self
      .store
      .dashboard_attachment_url(
        &FileTarget::Run {
          scope,
          path: path.into(),
        },
        &disposition(&filename),
      )
      .map_err(|error| {
        if error.status == 404 {
          message(format!("artifact is missing: {path}"))
        } else {
          api_error(error)
        }
      })?;
    if !valid_storage_download_url(&url) {
      return Err(message("invalid object storage download URL"));
    }
    Ok(Download::Cloud {
      url,
      size,
      filename,
    })
  }

  fn overview(&self, scope: &RunScope, deadline: Instant) -> ApiResult<Value> {
    let record = self.store.dashboard_artifact(scope, "run-state.json")?;
    let version = record.as_ref().map_or_else(
      || "missing".to_string(),
      |record| match record.storage {
        super::types::FileStorage::Tracking { revision, .. } => format!("tracking:{revision}"),
        _ => record.sha256.clone().unwrap_or_else(|| "missing".into()),
      },
    );
    if let Some(value) = self.store.dashboard_cached_overview(scope, &version)?
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
        .dashboard_cache_overview(scope, &version, &result)?;
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
        let (source, scope) = self.scope(source_id, run_id)?;
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
        if source.origin.is_none() {
          result.run_id = run_id.clone();
          source.decorate_run(&mut result.run, &scope);
          source.decorate_warnings(&mut result.warnings, &scope);
        }
        Ok(result)
      })
      .collect()
  }
}

fn started_at(run: &Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
  run["started_at"]
    .as_str()
    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
}

fn metric_record(scope: &RunScope, run: Value, params: Option<Value>) -> RunMetrics {
  RunMetrics {
    run_id: scope.run_id.clone(),
    run,
    params,
    first_metric_timestamp: None,
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

fn valid_storage_download_url(url: &str) -> bool {
  let Ok(parsed) = reqwest::Url::parse(url) else {
    return false;
  };
  matches!(parsed.scheme(), "https" | "http")
    && parsed.host_str().is_some()
    && parsed.username().is_empty()
    && parsed.password().is_none()
    && parsed.fragment().is_none()
}

fn api_error(error: ApiError) -> ExpriError {
  message(error.message)
}
fn message(text: impl Into<String>) -> ExpriError {
  ExpriError::Message(text.into())
}

#[cfg(test)]
mod tests;
