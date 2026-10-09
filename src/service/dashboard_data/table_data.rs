use super::*;
use crate::dashboard::table::ScalarData;
use crate::service::types::FileStorage;

type MetricColumns = (BTreeMap<String, [f64; 3]>, Vec<Value>);

impl<S: ObjectStorage> HostedDashboard<'_, S> {
  pub(super) fn table_data(
    &self,
    scope: &RunScope,
    include_params: bool,
    include_metrics: bool,
    deadline: Instant,
  ) -> (ScalarData, Vec<Value>) {
    let mut data = ScalarData::default();
    let mut warnings = Vec::new();
    if include_params {
      match self.table_parameters(scope, deadline) {
        Ok(value) => data.params = value,
        Err(error) => warnings.push(json!({"run_id":scope.run_id,"message":format!("Parameter columns are unavailable: {}",error.message)})),
      }
    }
    if include_metrics {
      match self.table_metrics(scope, deadline) {
        Ok((values, next_warnings)) => {
          data.metrics = values;
          warnings.extend(next_warnings);
        }
        Err(error) => warnings.push(json!({"run_id":scope.run_id,"message":format!("Metric columns are unavailable: {error}")})),
      }
    }
    (data, warnings)
  }

  fn table_parameters(&self, scope: &RunScope, deadline: Instant) -> ApiResult<Option<Value>> {
    let path = "outputs/params.json";
    let Some(record) = self.store.dashboard_artifact(scope, path)? else {
      return Ok(None);
    };
    let version = version(&record);
    if let Some(cached) = self.store.dashboard_cached_scalars(scope, path, &version)?
      && cached["table_schema"] == 1
      && cached["params"].is_object()
    {
      return Ok(Some(cached["params"].clone()));
    }
    if record.size > JSON_LIMIT {
      return Err(ApiError::new(
        413,
        "outputs/params.json exceeds the 1 MiB size limit",
      ));
    }
    let bytes = self.range(&record, 0, record.size as usize, deadline)?;
    verify_bytes(&record, &bytes)?;
    let value =
      metrics::parse_parameters(&bytes).map_err(|error| ApiError::new(422, error.to_string()))?;
    self.store.dashboard_cache_scalars(
      scope,
      path,
      &version,
      &json!({"table_schema":1,"params":value}),
    )?;
    Ok(Some(value))
  }

  fn table_metrics(&self, scope: &RunScope, deadline: Instant) -> Result<MetricColumns> {
    let path = "outputs/metrics.jsonl";
    let Some(record) = self
      .store
      .dashboard_artifact(scope, path)
      .map_err(api_error)?
    else {
      return Ok((BTreeMap::new(), Vec::new()));
    };
    let version = version(&record);
    // Tracking already has a durable incremental SQLite projection. Its
    // summaries stay fresh without reading JSONL or keeping a second cache.
    let tracking = matches!(record.storage, FileStorage::Tracking { .. });
    if !tracking
      && let Some(cached) = self
        .store
        .dashboard_cached_scalars(scope, path, &version)
        .map_err(api_error)?
      && cached["table_schema"] == 1
      && let Ok(values) = serde_json::from_value(cached["metrics"].clone())
    {
      return Ok((
        values,
        cached["warnings"].as_array().cloned().unwrap_or_default(),
      ));
    }
    let mut result = metric_record(scope, Value::Null, None);
    self.metric_data(scope, &mut result, &[], false, deadline)?;
    let mut data = ScalarData::default();
    for (name, series) in result.metrics {
      data.add_metric(name, series.summary);
    }
    let warnings = bounded_warnings(&result.warnings);
    if !tracking {
      self
        .store
        .dashboard_cache_scalars(
          scope,
          path,
          &version,
          &json!({"table_schema":1,"metrics":data.metrics,"warnings":warnings}),
        )
        .map_err(api_error)?;
    }
    Ok((data.metrics, warnings))
  }
}

fn version(record: &FileRecord) -> String {
  match record.storage {
    FileStorage::Tracking { revision, .. } => format!("table1:t:{revision}:{}", record.size),
    FileStorage::Stream => format!("table1:s:{}", record.size),
    FileStorage::Object => format!(
      "table1:o:{}:{}",
      record.sha256.as_deref().unwrap_or("missing"),
      record.size
    ),
  }
}
