use super::*;

const WARNING_LIMIT: usize = 100;
const PARAMETER_LIMIT: usize = 24;
const WARNING_TEXT_LIMIT: usize = 512;
const PARAMETER_TEXT_LIMIT: usize = 1000;

#[derive(Clone, Debug, Serialize)]
pub struct ChartData {
  pub schema_version: u8,
  pub x_axis: ChartXAxis,
  pub runs: Vec<String>,
  pub plots: Vec<ChartPlot>,
  pub warnings: Vec<ChartWarning>,
  pub warnings_truncated: bool,
  pub parameter_rows: Vec<ChartParameterRow>,
  pub parameters_truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChartPlot {
  pub metric_name: String,
  pub domain: Option<ChartDomain>,
  pub series: Vec<ChartSeries>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChartDomain {
  pub x_min: String,
  pub x_max: String,
  pub value_min: f64,
  pub value_max: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChartSeries {
  pub run_index: usize,
  pub summary: ChartSeriesSummary,
  pub points: Vec<ChartDataPoint>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChartSeriesSummary {
  pub count: usize,
  pub missing_timestamp_count: usize,
  pub last: Option<ChartSummaryPoint>,
  pub min: Option<ChartSummaryPoint>,
  pub max: Option<ChartSummaryPoint>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChartSummaryPoint {
  pub step: String,
  pub value: f64,
  pub timestamp: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChartDataPoint {
  #[serde(flatten)]
  pub point: ChartSummaryPoint,
  pub x_value: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChartWarning {
  pub run_index: usize,
  pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChartParameterRow {
  pub name: String,
  pub values: Vec<Option<String>>,
  pub differs: bool,
}

/// A compact browser preview shares the offline chart's selection and sampling.
/// Coordinates remain decimal strings so JavaScript cannot round steps or times.
pub fn dashboard_chart_data(
  runs: &[RunMetrics],
  filters: &[String],
  x_axis: ChartXAxis,
) -> Result<ChartData> {
  if !(1..=8).contains(&runs.len())
    || runs
      .iter()
      .map(|run| &run.run_id)
      .collect::<BTreeSet<_>>()
      .len()
      != runs.len()
  {
    return Err(message(
      "select one to eight distinct runs for dashboard charts",
    ));
  }
  if runs.iter().any(|run| {
    run.run_id.trim().is_empty()
      || run.run_id.len() > 256
      || run.run_id.chars().any(char::is_control)
  }) {
    return Err(message(
      "chart run references must be nonempty, contain no control characters, and use at most 256 UTF-8 bytes",
    ));
  }
  crate::metrics::validate_filters(filters)?;
  let options = dashboard_options(x_axis);
  let (names, available_count) = selected_metric_names(runs, filters, options)?;
  crate::metrics::validate_filters(
    &names
      .iter()
      .map(|name| (*name).to_owned())
      .collect::<Vec<_>>(),
  )?;
  let point_budget = chart_point_budget(runs, &names, options);
  let mut data = ChartData {
    schema_version: 1,
    x_axis,
    runs: runs.iter().map(|run| run.run_id.clone()).collect(),
    plots: Vec::with_capacity(names.len()),
    warnings: Vec::new(),
    warnings_truncated: false,
    parameter_rows: Vec::new(),
    parameters_truncated: false,
  };
  for name in &names {
    let domain = metric_domain(runs, name, x_axis).map(|domain| ChartDomain {
      x_min: domain.x_min.to_string(),
      x_max: domain.x_max.to_string(),
      value_min: domain.value_min,
      value_max: domain.value_max,
    });
    let mut series = Vec::new();
    for (index, run) in runs.iter().enumerate() {
      let Some(source) = run.metrics.get(*name) else {
        continue;
      };
      let origin = run
        .first_metric_timestamp
        .as_deref()
        .and_then(timestamp_nanos);
      let points: Vec<_> = source
        .points
        .iter()
        .filter(|point| point.value.is_finite() && coordinate(point, x_axis, origin).is_some())
        .collect();
      let points = plot_points(&points, point_budget)
        .into_iter()
        .map(|point| {
          Ok(ChartDataPoint {
            point: summary_point(point)?,
            x_value: coordinate(point, x_axis, origin)
              .expect("filtered coordinate")
              .to_string(),
          })
        })
        .collect::<Result<Vec<_>>>()?;
      let summary = &source.summary;
      if summary.missing_timestamp_count > summary.count {
        return Err(message(
          "metric timestamp counts exceed the complete sample count",
        ));
      }
      series.push(ChartSeries {
        run_index: index + 1,
        summary: ChartSeriesSummary {
          count: summary.count,
          missing_timestamp_count: summary.missing_timestamp_count,
          last: (summary.count > 0)
            .then(|| summary_point(&summary.last))
            .transpose()?,
          min: (summary.count > 0)
            .then(|| summary_point(&summary.min))
            .transpose()?,
          max: (summary.count > 0)
            .then(|| summary_point(&summary.max))
            .transpose()?,
        },
        points,
      });
    }
    data.plots.push(ChartPlot {
      metric_name: (*name).into(),
      domain,
      series,
    });
  }
  if available_count > names.len() && !runs.is_empty() {
    data.warnings.push(ChartWarning {
      run_index: 1,
      message: format!("Showing the first {} of {available_count} metrics; select up to six exact metric names to change this preview.", names.len()),
    });
  }
  for (index, run) in runs.iter().enumerate() {
    for warning in &run.warnings {
      if data.warnings.len() == WARNING_LIMIT {
        data.warnings_truncated = true;
        break;
      }
      let note = warning
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| warning.to_string());
      let preview = preview_text(&note, Some(WARNING_TEXT_LIMIT));
      data.warnings_truncated |= preview != note;
      data.warnings.push(ChartWarning {
        run_index: index + 1,
        message: preview,
      });
    }
  }
  let keys: BTreeSet<_> = runs
    .iter()
    .filter_map(|run| run.params.as_ref()?.as_object())
    .flat_map(|params| params.keys().map(String::as_str))
    .collect();
  data.parameters_truncated = keys.len() > PARAMETER_LIMIT;
  for key in keys.into_iter().take(PARAMETER_LIMIT) {
    let values: Vec<_> = runs
      .iter()
      .map(|run| run.params.as_ref().and_then(|params| params.get(key)))
      .collect();
    let differs = values.iter().skip(1).any(|value| *value != values[0]);
    let name = preview_text(key, Some(PARAMETER_TEXT_LIMIT));
    data.parameters_truncated |= name != key;
    let values = values
      .into_iter()
      .map(|value| {
        value.map(|value| {
          let raw = serde_json::to_string_pretty(value).expect("JSON value serialization");
          let preview = preview_text(&raw, Some(PARAMETER_TEXT_LIMIT));
          data.parameters_truncated |= preview != raw;
          preview
        })
      })
      .collect();
    data.parameter_rows.push(ChartParameterRow {
      name,
      values,
      differs,
    });
  }
  // Preserve the curve sample budget. Only exceptionally large metadata needs
  // shorter previews to fit the same response cap as standalone chart HTML.
  let mut text_limit = PARAMETER_TEXT_LIMIT;
  while !fits_response(&data) {
    if text_limit == 0 {
      return Err(message(
        "dashboard chart data exceeds the 2 MiB preview limit",
      ));
    }
    text_limit /= 2;
    for warning in &mut data.warnings {
      let preview = preview_text(&warning.message, Some(text_limit));
      data.warnings_truncated |= preview != warning.message;
      warning.message = preview;
    }
    for row in &mut data.parameter_rows {
      let preview = preview_text(&row.name, Some(text_limit));
      data.parameters_truncated |= preview != row.name;
      row.name = preview;
      for value in row.values.iter_mut().flatten() {
        let preview = preview_text(value, Some(text_limit));
        data.parameters_truncated |= preview != *value;
        *value = preview;
      }
    }
  }
  Ok(data)
}

fn summary_point(point: &MetricPoint) -> Result<ChartSummaryPoint> {
  if !point.value.is_finite()
    || point
      .timestamp
      .as_ref()
      .is_some_and(|timestamp| timestamp.len() > 128 || timestamp_nanos(timestamp).is_none())
  {
    return Err(message(
      "metric points require finite values and supported RFC3339 timestamps",
    ));
  }
  Ok(ChartSummaryPoint {
    step: point.step.to_string(),
    value: point.value,
    timestamp: point.timestamp.clone(),
  })
}

fn fits_response(data: &ChartData) -> bool {
  struct Counter(usize);
  impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
      self.0 = self.0.saturating_add(bytes.len());
      if self.0 > DASHBOARD_HTML_LIMIT {
        return Err(std::io::Error::other("chart data response limit exceeded"));
      }
      Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }
  serde_json::to_writer(Counter(0), data).is_ok()
}
