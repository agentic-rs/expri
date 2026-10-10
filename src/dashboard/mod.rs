pub(crate) mod artifacts;
pub(crate) mod preview;
pub(crate) mod server;
pub(crate) mod table;
mod table_cache;
pub(crate) mod updates;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use clap::Args;
use serde::Serialize;
use serde_json::{Value, json};

use artifacts::{cache_record, open_fixed, optional_metadata, real_prefix};
use preview::{bounded_warnings, preview};

use crate::context::CommandContext;
use crate::controller::run_pull;
use crate::error::{ExpriError, Result};
use crate::metric_charts::ChartXAxis;
use crate::metrics::{self, MetricSummary, Reduction};
use crate::protocol::RunQueryRequest;
use crate::runs;

// Even control bytes expand to at most six bytes when JSON-escaped; this
// leaves ample room inside the transport's 512 KiB JSON response limit.
const LOG_LIMIT: u64 = 64 * 1024;
const METRIC_PREVIEW_LIMIT: usize = 50;

#[derive(Debug, Args)]
pub struct DashboardCommand {
  #[arg(long)]
  config: Option<PathBuf>,
  #[arg(long)]
  repo: Option<PathBuf>,
  /// Local listening port; zero selects an available port.
  #[arg(long, default_value_t = 8765)]
  port: u16,
}

pub fn run(command: DashboardCommand, target: Option<&str>) -> Result<()> {
  let context = CommandContext::load(command.config, command.repo)?;
  server::serve(Dashboard::new(context, target)?, command.port)
}

#[derive(Clone, Debug, Serialize)]
struct Source {
  source_id: String,
  label: String,
  kind: &'static str,
  target_name: Option<String>,
}

pub struct Dashboard {
  repo_root: PathBuf,
  project_name: String,
  results_dir: String,
  targets: BTreeSet<String>,
  initial_source: String,
  table_cache: table_cache::Cache,
}

/// Shared dashboard views keep local and hosted routes on the same API contract.
pub(crate) trait DashboardView {
  fn catalog(&self) -> Result<Value>;
  fn projects(&self) -> Result<Option<Value>> {
    Ok(None)
  }
  fn storage(
    &self,
    _project_id: &str,
    _kind: &str,
    _search: &str,
    _limit: usize,
    _offset: usize,
  ) -> Result<Option<Value>> {
    Ok(None)
  }
  fn updates(&self, source: &str, run_ids: &[String]) -> Result<Value>;
  fn list_table(&self, source: &str, query: &table::ListQuery<'_>) -> Result<Value>;
  fn columns(&self, source: &str) -> Result<Value>;
  fn detail(&self, source: &str, run_id: &str) -> Result<Value>;
  fn artifacts(&self, source: &str, run_id: &str) -> Result<Value>;
  fn artifact_download(
    &self,
    source: &str,
    run_id: &str,
    path: &str,
  ) -> Result<artifacts::Download>;
  fn log(&self, source: &str, run_id: &str, stream: &str, tail: usize) -> Result<Value>;
  fn compare(
    &self,
    source: &str,
    run_ids: &[String],
    filters: &[String],
    reduction: Reduction,
  ) -> Result<Value>;
  fn chart(
    &self,
    source: &str,
    run_ids: &[String],
    filters: &[String],
    x_axis: ChartXAxis,
  ) -> Result<String>;
}

impl DashboardView for Dashboard {
  fn catalog(&self) -> Result<Value> {
    Dashboard::catalog(self)
  }
  fn updates(&self, source: &str, run_ids: &[String]) -> Result<Value> {
    Dashboard::updates(self, source, run_ids)
  }
  fn list_table(&self, source: &str, query: &table::ListQuery<'_>) -> Result<Value> {
    Dashboard::list_table(self, source, query)
  }
  fn columns(&self, source: &str) -> Result<Value> {
    Dashboard::columns(self, source)
  }
  fn detail(&self, source: &str, run_id: &str) -> Result<Value> {
    Dashboard::detail(self, source, run_id)
  }
  fn artifacts(&self, source: &str, run_id: &str) -> Result<Value> {
    Dashboard::artifacts(self, source, run_id)
  }
  fn artifact_download(
    &self,
    source: &str,
    run_id: &str,
    path: &str,
  ) -> Result<artifacts::Download> {
    Dashboard::artifact_download(self, source, run_id, path)
  }
  fn log(&self, source: &str, run_id: &str, stream: &str, tail: usize) -> Result<Value> {
    Dashboard::log(self, source, run_id, stream, tail)
  }
  fn compare(
    &self,
    source: &str,
    run_ids: &[String],
    filters: &[String],
    reduction: Reduction,
  ) -> Result<Value> {
    Dashboard::compare(self, source, run_ids, filters, reduction)
  }
  fn chart(
    &self,
    source: &str,
    run_ids: &[String],
    filters: &[String],
    x_axis: ChartXAxis,
  ) -> Result<String> {
    Dashboard::chart(self, source, run_ids, filters, x_axis)
  }
}

impl Dashboard {
  pub fn new(context: CommandContext, target: Option<&str>) -> Result<Self> {
    let repo_root = fs::canonicalize(context.repo_root)?;
    let project_name = context.project_name.unwrap_or_else(|| {
      repo_root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
    });
    let results_dir = context.config.download_results_dir();
    run_pull::validate_results_dir(&results_dir)?;
    let mut targets: BTreeSet<_> = context.config.target.into_keys().collect();
    let initial_source = if let Some(target) = target {
      run_pull::validate_component(target, "target name")?;
      targets.insert(target.to_string());
      format!("cached:{target}")
    } else {
      "local".to_string()
    };
    let dashboard = Self {
      repo_root,
      project_name,
      results_dir,
      targets,
      initial_source,
      table_cache: table_cache::Cache::default(),
    };
    dashboard.local_runs_dir()?;
    dashboard.sources()?;
    Ok(dashboard)
  }

  pub fn catalog(&self) -> Result<Value> {
    let (sources, warnings) = self.sources()?;
    Ok(
      json!({"project_name": self.project_name, "initial_source": self.initial_source,
      "sources": sources, "warnings": bounded_warnings(&warnings)}),
    )
  }

  pub fn updates(&self, source_id: &str, run_ids: &[String]) -> Result<Value> {
    updates::validate_selection(source_id, run_ids)?;
    let runs = if source_id.is_empty() {
      Vec::new()
    } else {
      let (_, runs_dir) = self.source(source_id)?;
      real_prefix(&self.repo_root, &runs_dir)?;
      run_ids
        .iter()
        .map(|run_id| updates::local_run(&runs_dir, run_id))
        .collect::<Result<Vec<_>>>()?
    };
    // Directory mtimes cannot reveal state rewrites below existing run
    // directories. Local catalog/list refreshes use a periodic recovery read.
    updates::response(None, None, runs)
  }

  fn sources(&self) -> Result<(Vec<Source>, Vec<Value>)> {
    let mut labels = self.targets.clone();
    let mut warnings = Vec::new();
    let prefix = self
      .repo_root
      .join(run_pull::validate_results_dir(&self.results_dir)?);
    real_prefix(&self.repo_root, &prefix)?;
    if optional_metadata(&prefix)?.is_some() {
      for entry in fs::read_dir(&prefix)? {
        let entry = entry?;
        let Some(label) = entry.file_name().to_str().map(str::to_string) else {
          continue;
        };
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
          warnings
            .push(json!({"message": format!("cached source {label:?} is a symlink; skipped")}));
        } else if metadata.is_dir() && run_pull::validate_component(&label, "target name").is_ok() {
          labels.insert(label);
        }
      }
    }
    let mut sources = vec![Source {
      source_id: "local".into(),
      label: "Local runs".into(),
      kind: "local",
      target_name: None,
    }];
    for label in labels {
      match run_pull::cached_runs_dir(&self.repo_root, &self.results_dir, &label) {
        Ok(_) => sources.push(Source {
          source_id: format!("cached:{label}"),
          label: format!("{label} · cached"),
          kind: "cached",
          target_name: Some(label),
        }),
        Err(error) => {
          warnings.push(json!({"message": format!("cached source {label:?}: {error}; skipped")}))
        }
      }
    }
    Ok((sources, warnings))
  }

  fn local_runs_dir(&self) -> Result<PathBuf> {
    let directory = self.repo_root.join(".expri/runs");
    real_prefix(&self.repo_root, &directory)?;
    Ok(directory)
  }

  fn source(&self, source_id: &str) -> Result<(Source, PathBuf)> {
    let source = self
      .sources()?
      .0
      .into_iter()
      .find(|source| source.source_id == source_id)
      .ok_or_else(|| message(format!("unknown source: {source_id}")))?;
    let runs_dir = match &source.target_name {
      None => self.local_runs_dir()?,
      Some(target) => run_pull::cached_runs_dir(&self.repo_root, &self.results_dir, target)?,
    };
    Ok((source, runs_dir))
  }

  pub fn artifacts(&self, source_id: &str, run_id: &str) -> Result<Value> {
    let (source, runs_dir) = self.source(source_id)?;
    runs::query_directory(
      &runs_dir,
      &RunQueryRequest::Show {
        run_id: run_id.into(),
      },
    )?;
    artifacts::local_catalog(&runs_dir.join(run_id), &source, run_id)
  }

  fn artifact_download(
    &self,
    source_id: &str,
    run_id: &str,
    path: &str,
  ) -> Result<artifacts::Download> {
    artifacts::validate_output(path)?;
    let (_, runs_dir) = self.source(source_id)?;
    runs::query_directory(
      &runs_dir,
      &RunQueryRequest::Show {
        run_id: run_id.into(),
      },
    )?;
    let relative = runs_dir.join(run_id).join(path);
    let relative = relative
      .strip_prefix(&self.repo_root)
      .map_err(|_| message("artifact is outside the repository"))?;
    let file = artifacts::open_beneath(&self.repo_root, relative)?
      .ok_or_else(|| message(format!("artifact is missing: {path}")))?;
    let size = file.metadata()?.len();
    Ok(artifacts::Download::Local {
      file,
      size,
      filename: artifacts::filename(path).into(),
    })
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
      &table::ListQuery {
        archived: false,
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

  pub(crate) fn list_table(&self, source_id: &str, query: &table::ListQuery<'_>) -> Result<Value> {
    let table::ListQuery {
      archived,
      origin,
      search,
      task,
      status,
      limit,
      offset,
      table,
    } = *query;
    if archived {
      return Err(message("run archival is available on the hosted dashboard"));
    }
    if origin.is_some() {
      return Err(message("machine filters are available for hosted projects"));
    }
    if !(1..=1000).contains(&limit) {
      return Err(message("limit must be between 1 and 1000"));
    }
    if table.is_some_and(|table| table.needs_params() || table.needs_metrics()) && limit > 100 {
      return Err(message("table column pages support at most 100 rows"));
    }
    let (source, runs_dir) = self.source(source_id)?;
    let report = runs::query_directory(
      &runs_dir,
      &RunQueryRequest::List {
        task: task.map(str::to_string),
        status: status.map(str::to_string),
        limit: None,
      },
    )?;
    let search = search.unwrap_or_default().to_lowercase();
    let records: Vec<Value> = report["runs"]
      .as_array()
      .unwrap()
      .iter()
      .filter(|run| {
        search.is_empty()
          || ["run_id", "task", "status"].iter().any(|field| {
            run[field]
              .as_str()
              .unwrap_or_default()
              .to_lowercase()
              .contains(&search)
          })
      })
      .cloned()
      .collect();
    let mut warnings = report["warnings"].as_array().cloned().unwrap_or_default();
    let records = if let Some(options) = table {
      let mut rows = records
        .into_iter()
        .map(|run| {
          let data = self.table_data(
            &runs_dir,
            run["run_id"].as_str().unwrap(),
            options.needs_params(),
            options.needs_metrics(),
            &mut warnings,
          );
          table::TableRow::new(run, data)
        })
        .collect::<Vec<_>>();
      options.sort_rows(&mut rows);
      rows.into_iter().map(|row| options.project(row)).collect()
    } else {
      records
    };
    let total_count = records.len();
    let mut metadata_truncated = false;
    let records: Vec<_> = records
      .into_iter()
      .skip(offset)
      .take(limit)
      .map(|record| preview(&record, &mut metadata_truncated))
      .collect();
    let next_offset = offset.saturating_add(records.len());
    let next_offset = (next_offset < total_count).then_some(next_offset);
    Ok(
      json!({"source": source, "runs": records, "warnings": bounded_warnings(&warnings), "total_count": total_count, "limit": limit, "offset": offset, "next_offset": next_offset, "metadata_truncated": metadata_truncated}),
    )
  }

  pub(crate) fn columns(&self, source_id: &str) -> Result<Value> {
    let (source, runs_dir) = self.source(source_id)?;
    let report = runs::query_directory(
      &runs_dir,
      &RunQueryRequest::List {
        task: None,
        status: None,
        limit: None,
      },
    )?;
    let runs = report["runs"].as_array().unwrap();
    let mut columns = table::Columns::default();
    let mut warnings = report["warnings"].as_array().cloned().unwrap_or_default();
    // Discovery is an explicit picker read, bounded independently of run sorting.
    for run in runs.iter().take(500) {
      columns.add(&self.table_data(
        &runs_dir,
        run["run_id"].as_str().unwrap(),
        true,
        true,
        &mut warnings,
      ));
    }
    columns.truncated |= runs.len() > 500;
    if columns.truncated {
      warnings.push(json!({"message":"Column discovery covers up to 500 recent runs and 100 parameter and metric keys; some columns are omitted."}));
    }
    Ok(
      json!({"source":source,"available_columns":columns.response(),"warnings":bounded_warnings(&warnings)}),
    )
  }

  fn table_data(
    &self,
    runs_dir: &std::path::Path,
    run_id: &str,
    params: bool,
    metric_values: bool,
    warnings: &mut Vec<Value>,
  ) -> table::ScalarData {
    if !params && !metric_values {
      return table::ScalarData::default();
    }
    let run = runs_dir.join(run_id);
    let revision = match table_cache::Cache::revision(&run, params, metric_values) {
      Ok(revision) => revision,
      Err(error) => {
        warnings.push(
          json!({"run_id":run_id,"message":format!("Table columns are unavailable: {error}")}),
        );
        return table::ScalarData::default();
      }
    };
    if let Some((data, next_warnings)) =
      self.table_cache.get(&run, params, metric_values, &revision)
    {
      warnings.extend(next_warnings);
      return data;
    }
    let mut next_warnings = Vec::new();
    let data =
      self.table_data_uncached(runs_dir, run_id, params, metric_values, &mut next_warnings);
    if table_cache::Cache::revision(&run, params, metric_values)
      .is_ok_and(|after| after == revision)
    {
      self.table_cache.insert(
        (run, params, metric_values),
        revision,
        data.clone(),
        bounded_warnings(&next_warnings),
      );
    }
    warnings.extend(next_warnings);
    data
  }

  fn table_data_uncached(
    &self,
    runs_dir: &std::path::Path,
    run_id: &str,
    params: bool,
    metric_values: bool,
    warnings: &mut Vec<Value>,
  ) -> table::ScalarData {
    let mut data = table::ScalarData::default();
    if metric_values {
      match metrics::read_summary_files(
        runs_dir,
        run_id,
        &[],
        metrics::MetricFiles {
          metrics: true,
          params,
        },
      ) {
        Ok(result) => {
          if params {
            data.params = result.params;
          }
          warnings.extend(result.warnings);
          for (name, series) in result.metrics {
            data.add_metric(name, series.summary);
          }
          return data;
        }
        Err(error) => warnings.push(
          json!({"run_id":run_id,"message":format!("Metric columns are unavailable: {error}")}),
        ),
      }
    }
    if params {
      match metrics::read_parameters(runs_dir, run_id) {
        Ok(value) => data.params = value,
        Err(error) => warnings.push(
          json!({"run_id":run_id,"message":format!("Parameter columns are unavailable: {error}")}),
        ),
      }
    }
    data
  }

  pub fn detail(&self, source_id: &str, run_id: &str) -> Result<Value> {
    let (source, runs_dir) = self.source(source_id)?;
    let record = runs::query_directory(
      &runs_dir,
      &RunQueryRequest::Show {
        run_id: run_id.into(),
      },
    )?;
    let mut warnings = record["warnings"].as_array().cloned().unwrap_or_default();
    let mut params = Value::Null;
    let mut params_truncated = false;
    let mut summaries = BTreeMap::<String, MetricSummary>::new();
    let mut metric_count = 0;
    let mut metrics_error = None;
    match metrics::read_summaries(&runs_dir, run_id, &[]) {
      Ok(metrics) => {
        metric_count = metrics.metrics.len();
        summaries = metrics
          .metrics
          .into_iter()
          .take(METRIC_PREVIEW_LIMIT)
          .map(|(name, series)| (name, series.summary))
          .collect();
        if let Some(value) = metrics.params {
          params = preview(&value, &mut params_truncated);
        }
        for warning in metrics.warnings {
          if !warnings.contains(&warning) {
            warnings.push(warning);
          }
        }
      }
      Err(error) => {
        metrics_error = Some(error.to_string());
        // A broken metric artifact must not hide otherwise valid parameters.
        match metrics::read_parameters(&runs_dir, run_id) {
          Ok(Some(value)) => params = preview(&value, &mut params_truncated),
          Ok(None) => {}
          Err(error) => warnings.push(json!({"run_id": run_id, "message": error.to_string()})),
        }
      }
    }
    if params_truncated {
      warnings.push(json!({"run_id": run_id, "message": "Parameter preview is limited; original parameters remain in outputs/params.json."}));
    }
    if metric_count > METRIC_PREVIEW_LIMIT {
      warnings.push(json!({"run_id": run_id, "message": "Showing the first 50 metric summaries; enter an exact metric name to chart another series."}));
    }
    let mut metadata_truncated = false;
    let metadata = preview::run_metadata(&record, &mut metadata_truncated);
    let state = metadata["state"].clone();
    let snapshot = metadata["snapshot"].clone();
    let environment = metadata["environment"].clone();
    let run = preview(&record["run"], &mut metadata_truncated);
    let run_dir = runs_dir.join(run_id);
    let cache = if source.kind == "cached" {
      match cache_record(&run_dir, &mut metadata_truncated) {
        Ok(value) => value,
        Err(error) => {
          warnings.push(json!({"run_id": run_id, "message": error.to_string()}));
          Value::Null
        }
      }
    } else {
      Value::Null
    };
    if metadata_truncated {
      warnings.push(json!({"run_id": run_id, "message": "Metadata previews are limited; original records remain in the run directory."}));
    }
    Ok(
      json!({"source": source, "run": run, "state": state, "snapshot": snapshot,
      "environment": environment, "metadata_truncated": metadata_truncated,
      "params": params, "params_truncated": params_truncated, "metrics": summaries,
      "metric_count": metric_count, "metrics_truncated": metric_count > METRIC_PREVIEW_LIMIT,
      "metrics_error": metrics_error, "warnings": bounded_warnings(&warnings), "cache": cache}),
    )
  }

  pub fn log(&self, source_id: &str, run_id: &str, stream: &str, tail: usize) -> Result<Value> {
    if !matches!(stream, "stdout" | "stderr") || tail > 1000 {
      return Err(message(
        "stream must be stdout or stderr and tail must be at most 1000",
      ));
    }
    let (_, runs_dir) = self.source(source_id)?;
    runs::summary_directory(&runs_dir, run_id)?;
    let Some(mut file) = open_fixed(&runs_dir.join(run_id), &format!("logs/{stream}.log"))? else {
      return Ok(json!({"content": "", "stream": stream, "missing": true, "truncated": false}));
    };
    let size = file.metadata()?.len();
    let start = size.saturating_sub(LOG_LIMIT);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(size - start).read_to_end(&mut bytes)?;
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
      json!({"content": String::from_utf8_lossy(&content), "stream": stream,
      "missing": false, "truncated": start > 0}),
    )
  }

  pub fn compare(
    &self,
    source_id: &str,
    run_ids: &[String],
    filters: &[String],
    reduction: Reduction,
  ) -> Result<Value> {
    let (source, _) = self.source(source_id)?;
    let runs = self.read_metrics(source_id, run_ids, filters, 2, false)?;
    let (filters, omitted) = selected_metrics(&runs, filters)?;
    let mut comparison = metrics::compare(&runs, &filters, reduction)?;
    // Parameters appear in the bounded chart preview; the value table needs no
    // duplicate parameter payload or full metric arrays.
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
    let runs = if filters.is_empty() {
      let summaries = self.read_metrics(source_id, run_ids, &[], 1, false)?;
      let (selected, _) = selected_metrics(&summaries, &[])?;
      if selected.is_empty() {
        summaries
      } else {
        let mut curves = self.read_metrics(source_id, run_ids, &selected, 1, true)?;
        let available_count = summaries
          .iter()
          .flat_map(|run| run.metrics.keys())
          .collect::<BTreeSet<_>>()
          .len();
        if available_count > selected.len() {
          curves[0].warnings.push(json!({"message": format!("Showing the first {} of {available_count} metrics; select up to six exact metric names to chart other series.", selected.len())}));
        }
        curves
      }
    } else {
      self.read_metrics(source_id, run_ids, filters, 1, true)?
    };
    crate::metric_charts::render_dashboard_chart(&runs, filters, x_axis)
  }

  fn read_metrics(
    &self,
    source_id: &str,
    run_ids: &[String],
    filters: &[String],
    minimum: usize,
    retain_points: bool,
  ) -> Result<Vec<metrics::RunMetrics>> {
    if !(minimum..=8).contains(&run_ids.len())
      || run_ids.iter().collect::<BTreeSet<_>>().len() != run_ids.len()
    {
      return Err(message(format!("select {minimum} to 8 distinct runs")));
    }
    if filters.len() > 6 {
      return Err(message(
        "select at most six metrics for a dashboard comparison",
      ));
    }
    let (_, runs_dir) = self.source(source_id)?;
    run_ids
      .iter()
      .map(|id| {
        if retain_points {
          metrics::read(&runs_dir, id, filters)
        } else {
          metrics::read_summaries(&runs_dir, id, filters)
        }
      })
      .collect()
  }
}

fn selected_metrics(
  runs: &[metrics::RunMetrics],
  filters: &[String],
) -> Result<(Vec<String>, bool)> {
  let available: BTreeSet<_> = runs
    .iter()
    .flat_map(|run| run.metrics.keys().cloned())
    .collect();
  if filters.is_empty() {
    let omitted = available.len() > 4;
    Ok((available.into_iter().take(4).collect(), omitted))
  } else {
    let filters: Vec<_> = filters
      .iter()
      .collect::<BTreeSet<_>>()
      .into_iter()
      .cloned()
      .collect();
    if filters.len() > 6 {
      return Err(message(
        "select at most six metrics for a dashboard comparison",
      ));
    }
    Ok((filters, false))
  }
}

fn message(value: impl Into<String>) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests;
