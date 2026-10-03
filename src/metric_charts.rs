use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde_json::Value;

use crate::error::{ExpriError, Result};
use crate::metrics::{MetricPoint, RunMetrics};

const POINT_BUDGET: usize = 2000;
const DASHBOARD_POINT_BUDGET: usize = 600;
const DASHBOARD_TOTAL_POINT_BUDGET: usize = 4800;
const COLORS: [&str; 10] = [
  "#4056b4", "#087f8c", "#c05a25", "#894caa", "#bd416c", "#577c30", "#9a6717", "#306a94",
  "#78594e", "#59616e",
];
const LEFT: f64 = 86.0;
const TOP: f64 = 28.0;
const WIDTH: f64 = 864.0;
const HEIGHT: f64 = 246.0;

#[derive(Clone, Copy)]
struct ParameterPreview {
  keys: usize,
  characters: usize,
}

#[derive(Clone, Copy)]
struct ChartOptions {
  point_budget: usize,
  total_point_budget: Option<usize>,
  default_metric_limit: Option<usize>,
  selected_metric_limit: Option<usize>,
  parameter_preview: Option<ParameterPreview>,
  text_preview: Option<usize>,
}

/// Produce a portable, offline comparison without external assets or scripts.
pub fn write_chart(path: &Path, runs: &[RunMetrics], filters: &[String]) -> Result<()> {
  let html = render_chart(runs, filters)?;
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
    Ok(_) => {
      return Err(message(
        "chart output must be a regular file, not a symlink",
      ));
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(error) => return Err(error.into()),
  }
  let parent = path
    .parent()
    .filter(|path| !path.as_os_str().is_empty())
    .unwrap_or(Path::new("."));
  fs::create_dir_all(parent)?;
  let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
  temporary.write_all(html.as_bytes())?;
  temporary.persist(path).map_err(|error| error.error)?;
  Ok(())
}

pub(crate) fn render_chart(runs: &[RunMetrics], filters: &[String]) -> Result<String> {
  render_with_options(
    runs,
    filters,
    ChartOptions {
      point_budget: POINT_BUDGET,
      total_point_budget: None,
      default_metric_limit: None,
      selected_metric_limit: None,
      parameter_preview: None,
      text_preview: None,
    },
  )
}

/// Keep browser previews compact while the exported CLI chart retains all data.
pub(crate) fn render_dashboard_chart(runs: &[RunMetrics], filters: &[String]) -> Result<String> {
  render_with_options(
    runs,
    filters,
    ChartOptions {
      point_budget: DASHBOARD_POINT_BUDGET,
      total_point_budget: Some(DASHBOARD_TOTAL_POINT_BUDGET),
      default_metric_limit: Some(4),
      selected_metric_limit: Some(6),
      parameter_preview: Some(ParameterPreview {
        keys: 24,
        characters: 1000,
      }),
      text_preview: Some(512),
    },
  )
}

fn render_with_options(
  runs: &[RunMetrics],
  filters: &[String],
  options: ChartOptions,
) -> Result<String> {
  let available: BTreeSet<&str> = runs
    .iter()
    .flat_map(|run| run.metrics.keys().map(String::as_str))
    .collect();
  let mut names = if filters.is_empty() {
    available.iter().copied().collect::<Vec<_>>()
  } else {
    let mut seen = BTreeSet::new();
    let mut names = Vec::new();
    for name in filters {
      if !available.contains(name.as_str()) {
        return Err(message(format!(
          "metric is missing from all selected runs: {name}"
        )));
      }
      if seen.insert(name.as_str()) {
        names.push(name.as_str());
      }
    }
    names
  };
  let available_count = names.len();
  if filters.is_empty()
    && let Some(limit) = options.default_metric_limit
  {
    names.truncate(limit);
  } else if !filters.is_empty()
    && let Some(limit) = options.selected_metric_limit
    && names.len() > limit
  {
    return Err(message(format!(
      "dashboard charts support at most {limit} metrics; select fewer metrics"
    )));
  }
  let curves = names
    .iter()
    .map(|name| {
      runs
        .iter()
        .filter(|run| run.metrics.contains_key(*name))
        .count()
    })
    .sum::<usize>();
  let point_budget = options
    .total_point_budget
    .map_or(options.point_budget, |total| {
      options.point_budget.min((total / curves.max(1)).max(4))
    });
  let mut html = String::from(HTML_HEAD);
  html.push_str(&format!("<header><p class=eyebrow>expri / experiment review</p><h1>Run comparison</h1><p class=muted>{} selected run{}. Curves follow logging order; repeated steps and step resets stay visible.</p></header>", runs.len(), if runs.len() == 1 { "" } else { "s" }));
  if available_count > names.len() {
    html.push_str(&format!("<p class=muted>Showing the first {} of {available_count} metrics to keep the dashboard chart compact. Select up to {} metrics to change this preview.</p>", names.len(), options.selected_metric_limit.unwrap_or(names.len())));
  }
  render_runs(&mut html, runs, options.text_preview);
  render_params(&mut html, runs, options.parameter_preview);
  render_warnings(&mut html, runs, options.text_preview);
  if names.is_empty() {
    html.push_str("<section class=card><h2>No scalar metrics</h2><p class=muted>No metric series are available in these run records.</p></section>");
  }
  for (index, name) in names.iter().enumerate() {
    render_metric(
      &mut html,
      runs,
      name,
      index,
      ChartOptions {
        point_budget,
        ..options
      },
    );
  }
  html.push_str(
    "<footer>Generated by expri. This document works offline.</footer></main></body></html>",
  );
  Ok(html)
}

fn render_runs(html: &mut String, runs: &[RunMetrics], text_preview: Option<usize>) {
  html.push_str("<section class=card><h2>Selected runs</h2><div class=table-scroll><table>");
  if text_preview.is_some() {
    html.push_str("<caption class=muted>Dashboard text previews may shorten long metadata and notes. Review the original run records for complete metadata.</caption>");
  }
  html.push_str("<thead><tr><th>Run</th><th>Task</th><th>Recorded status</th><th>Started</th><th>Exit code</th></tr></thead><tbody>");
  for (index, run) in runs.iter().enumerate() {
    html.push_str(&format!("<tr><th scope=row><span class=swatch style=\"background:{}\"></span><code>{}</code></th><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", color(index), escape(&run.run_id), escape(&preview_text(&field(&run.run, "task"), text_preview)), escape(&preview_text(&field(&run.run, "status"), text_preview)), escape(&preview_text(&field(&run.run, "started_at"), text_preview)), escape(&preview_text(&field(&run.run, "exit_code"), text_preview))));
  }
  html.push_str("</tbody></table></div></section>");
}

fn render_params(html: &mut String, runs: &[RunMetrics], preview: Option<ParameterPreview>) {
  let keys: BTreeSet<&str> = runs
    .iter()
    .filter_map(|run| run.params.as_ref()?.as_object())
    .flat_map(|params| params.keys().map(String::as_str))
    .collect();
  html.push_str("<section class=card><h2>Effective parameters</h2>");
  if keys.is_empty() {
    html
      .push_str("<p class=muted>No effective parameters were saved for these runs.</p></section>");
    return;
  }
  if let Some(preview) = preview {
    html.push_str(&format!("<p class=muted>Dashboard parameter preview: the first {} keys are shown, with names and values limited to {} characters. Highlighted rows compare complete saved values.</p>", preview.keys, preview.characters));
  }
  html.push_str("<p class=muted>Highlighted rows differ between runs. A dash means the parameter was not recorded.</p><div class=table-scroll><table class=params><thead><tr><th>Parameter</th>");
  for run in runs {
    html.push_str(&format!("<th><code>{}</code></th>", escape(&run.run_id)));
  }
  html.push_str("</tr></thead><tbody>");
  for key in keys
    .into_iter()
    .take(preview.map_or(usize::MAX, |preview| preview.keys))
  {
    let values: Vec<_> = runs
      .iter()
      .map(|run| run.params.as_ref().and_then(|params| params.get(key)))
      .collect();
    let differs = values.iter().skip(1).any(|value| *value != values[0]);
    html.push_str(&format!(
      "<tr{}><th scope=row><code>{}</code>{}</th>",
      if differs { " class=diff" } else { "" },
      escape(&preview_text(
        key,
        preview.map(|preview| preview.characters)
      )),
      if differs {
        "<span class=diff-label>differs</span>"
      } else {
        ""
      }
    ));
    for value in values {
      match value {
        Some(value) => html.push_str(&format!(
          "<td><pre>{}</pre></td>",
          escape(&preview_text(
            &serde_json::to_string_pretty(value).expect("JSON value serialization"),
            preview.map(|preview| preview.characters),
          ))
        )),
        None => html.push_str("<td class=muted>—</td>"),
      }
    }
    html.push_str("</tr>");
  }
  html.push_str("</tbody></table></div></section>");
}

fn render_warnings(html: &mut String, runs: &[RunMetrics], text_preview: Option<usize>) {
  if !runs.iter().any(|run| !run.warnings.is_empty()) {
    return;
  }
  html.push_str("<section class=\"card warning\"><h2>Data notes</h2><ul>");
  for run in runs {
    for warning in &run.warnings {
      let note = warning
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| warning.to_string());
      html.push_str(&format!(
        "<li><code>{}</code>: {}</li>",
        escape(&run.run_id),
        escape(&preview_text(&note, text_preview))
      ));
    }
  }
  html.push_str("</ul></section>");
}

fn render_metric(
  html: &mut String,
  runs: &[RunMetrics],
  name: &str,
  index: usize,
  options: ChartOptions,
) {
  html.push_str(&format!("<section class=card><h2>{}</h2>", escape(name)));
  let all_points = runs
    .iter()
    .filter_map(|run| run.metrics.get(name))
    .flat_map(|series| series.points.iter())
    .filter(|point| point.value.is_finite());
  if let Some(domain) = Domain::new(all_points) {
    render_plot(html, runs, name, index, &domain, options);
  } else {
    html.push_str("<p class=muted>No finite samples are available to plot.</p>");
  }
  html.push_str("<div class=table-scroll><table><thead><tr><th>Run</th><th>Samples</th><th>Last step</th><th>Last</th><th>Minimum</th><th>Maximum</th></tr></thead><tbody>");
  for (run_index, run) in runs.iter().enumerate() {
    html.push_str(&format!(
      "<tr><th scope=row><span class=swatch style=\"background:{}\"></span><code>{}</code></th>",
      color(run_index),
      escape(&run.run_id)
    ));
    if let Some(series) = run.metrics.get(name) {
      let summary = &series.summary;
      html.push_str(&format!(
        "<td>{}</td><td>{}</td><td title=\"{}\">{}</td><td title=\"{}\">{}</td><td title=\"{}\">{}</td>",
        summary.count,
        summary.last.step,
        precise_number(summary.last.value),
        number(summary.last.value),
        precise_number(summary.min.value),
        number(summary.min.value),
        precise_number(summary.max.value),
        number(summary.max.value)
      ));
    } else {
      html.push_str("<td colspan=5 class=muted>Metric not recorded</td>");
    }
    html.push_str("</tr>");
  }
  html.push_str("</tbody></table></div></section>");
}

fn render_plot(
  html: &mut String,
  runs: &[RunMetrics],
  name: &str,
  index: usize,
  domain: &Domain,
  options: ChartOptions,
) {
  let point_budget = options.point_budget;
  let text_preview = options.text_preview;
  html.push_str(&format!("<svg viewBox=\"0 0 1000 340\" role=img aria-labelledby=\"plot-title-{index} plot-desc-{index}\"><title id=\"plot-title-{index}\">{}</title><desc id=\"plot-desc-{index}\">Metric values by global step. Each color represents one run. Samples remain in logging order; the table below lists statistics from all samples.</desc>", escape(name)));
  for (value, fraction) in domain.value_ticks() {
    let y = TOP + (1.0 - fraction) * HEIGHT;
    html.push_str(&format!("<line class=grid x1=\"{LEFT}\" x2=\"{}\" y1=\"{y:.2}\" y2=\"{y:.2}\"/><text class=tick x=\"{}\" y=\"{:.2}\" text-anchor=end>{}</text>", LEFT + WIDTH, LEFT - 12.0, y + 4.0, number(value)));
  }
  for step in domain.step_ticks() {
    let x = domain.x(step);
    let anchor = if domain.step_min == domain.step_max {
      "middle"
    } else if step == domain.step_min {
      "start"
    } else if step == domain.step_max {
      "end"
    } else {
      "middle"
    };
    html.push_str(&format!("<line class=grid x1=\"{x:.2}\" x2=\"{x:.2}\" y1=\"{TOP}\" y2=\"{}\"/><text class=tick x=\"{x:.2}\" y=\"{}\" text-anchor={anchor}>{step}</text>", TOP + HEIGHT, TOP + HEIGHT + 23.0));
  }
  html.push_str(&format!(
    "<text class=axis-label x=\"{}\" y=\"{}\" text-anchor=middle>Global step</text>",
    LEFT + WIDTH / 2.0,
    TOP + HEIGHT + 52.0
  ));
  let mut sampled = false;
  for (run_index, run) in runs.iter().enumerate() {
    let Some(series) = run.metrics.get(name) else {
      continue;
    };
    let points: Vec<_> = series
      .points
      .iter()
      .filter(|point| point.value.is_finite())
      .collect();
    let chosen = plot_points(&points, point_budget);
    sampled |= chosen.len() < points.len();
    let coordinates = chosen
      .iter()
      .map(|point| format!("{:.2},{:.2}", domain.x(point.step), domain.y(point.value)))
      .collect::<Vec<_>>()
      .join(" ");
    let dash = if run_index < COLORS.len() {
      ""
    } else {
      " stroke-dasharray=\"7 4\""
    };
    if !chosen.is_empty() {
      html.push_str(&format!(
        "<polyline fill=none stroke=\"{}\" stroke-width=2{dash} points=\"{coordinates}\"/>",
        color(run_index)
      ));
    }
    for point in chosen {
      let label = if text_preview.is_some() {
        format!(
          "Run {} · step {} · value {}",
          run_index + 1,
          point.step,
          precise_number(point.value)
        )
      } else {
        format!(
          "{} · {} · step {} · value {}",
          run.run_id,
          name,
          point.step,
          precise_number(point.value)
        )
      };
      html.push_str(&format!("<circle class=point cx=\"{:.2}\" cy=\"{:.2}\" r=2 fill=\"{}\" aria-label=\"{}\"><title>{}</title></circle>", domain.x(point.step), domain.y(point.value), color(run_index), escape(&label), escape(&label)));
    }
  }
  html.push_str("</svg><ul class=legend>");
  for (run_index, run) in runs.iter().enumerate() {
    let label = if text_preview.is_some() {
      format!("<span>Run {}</span>", run_index + 1)
    } else {
      String::new()
    };
    html.push_str(&format!("<li><span class=swatch style=\"background:{}\"></span>{label}<code>{}</code><span class=muted>{} · {}</span>{}</li>", color(run_index), escape(&run.run_id), escape(&preview_text(&field(&run.run, "task"), text_preview)), escape(&preview_text(&field(&run.run, "status"), text_preview)), if run_index >= COLORS.len() { "<span class=muted>(dashed)</span>" } else { "" }));
  }
  html.push_str("</ul>");
  if sampled {
    html.push_str(&format!("<p class=muted>Curves are downsampled to at most {point_budget} points per run, preserving endpoints and bucket minima/maxima. Statistics below use every logged sample.</p>"));
  }
}

struct Domain {
  step_min: u64,
  step_max: u64,
  value_min: f64,
  value_max: f64,
  scale: f64,
}

impl Domain {
  fn new<'a>(mut points: impl Iterator<Item = &'a MetricPoint>) -> Option<Self> {
    let first = points.next()?;
    let mut domain = Self {
      step_min: first.step,
      step_max: first.step,
      value_min: first.value,
      value_max: first.value,
      scale: 1.0,
    };
    for point in points {
      domain.step_min = domain.step_min.min(point.step);
      domain.step_max = domain.step_max.max(point.step);
      domain.value_min = domain.value_min.min(point.value);
      domain.value_max = domain.value_max.max(point.value);
    }
    domain.scale = domain.value_min.abs().max(domain.value_max.abs());
    if domain.scale == 0.0 {
      domain.scale = 1.0;
    }
    Some(domain)
  }

  fn x(&self, step: u64) -> f64 {
    let fraction = if self.step_min == self.step_max {
      0.5
    } else {
      (step - self.step_min) as f64 / (self.step_max - self.step_min) as f64
    };
    LEFT + fraction.clamp(0.0, 1.0) * WIDTH
  }

  fn y(&self, value: f64) -> f64 {
    let fraction = if self.value_min == self.value_max {
      0.5
    } else {
      (value / self.scale - self.value_min / self.scale)
        / (self.value_max / self.scale - self.value_min / self.scale)
    };
    TOP + (1.0 - fraction.clamp(0.0, 1.0)) * HEIGHT
  }

  fn step_ticks(&self) -> Vec<u64> {
    let mut ticks: Vec<_> = (0..=4)
      .map(|index| self.step_min + ((u128::from(self.step_max - self.step_min) * index) / 4) as u64)
      .collect();
    ticks.dedup();
    ticks
  }

  fn value_ticks(&self) -> Vec<(f64, f64)> {
    if self.value_min == self.value_max {
      return vec![(self.value_min, 0.5)];
    }
    (0..=4)
      .map(|index| {
        let fraction = f64::from(index) / 4.0;
        let normalized = (self.value_min / self.scale) * (1.0 - fraction)
          + (self.value_max / self.scale) * fraction;
        (
          (normalized * self.scale).clamp(self.value_min, self.value_max),
          fraction,
        )
      })
      .collect()
  }
}

/// Emit bucket extrema in source order, retaining both endpoints separately.
fn plot_points<'a>(points: &[&'a MetricPoint], point_budget: usize) -> Vec<&'a MetricPoint> {
  if points.len() <= point_budget {
    return points.to_vec();
  }
  let buckets = (point_budget - 2) / 2;
  let interior = points.len() - 2;
  let mut selected = Vec::with_capacity(point_budget);
  selected.push(points[0]);
  for bucket in 0..buckets {
    let start = 1 + (interior * bucket) / buckets;
    let end = 1 + (interior * (bucket + 1)) / buckets;
    let (mut min, mut max) = (start, start);
    for index in start + 1..end {
      if points[index].value < points[min].value {
        min = index;
      }
      if points[index].value > points[max].value {
        max = index;
      }
    }
    selected.push(points[min.min(max)]);
    if min != max {
      selected.push(points[min.max(max)]);
    }
  }
  selected.push(points[points.len() - 1]);
  selected
}

fn preview_text(text: &str, limit: Option<usize>) -> String {
  let Some(limit) = limit else {
    return text.to_string();
  };
  if limit == 0 {
    return String::new();
  }
  let Some((boundary, _)) = text.char_indices().nth(limit) else {
    return text.to_string();
  };
  let mut preview = text[..boundary].to_string();
  preview.pop();
  preview.push('…');
  preview
}

fn number(value: f64) -> String {
  if !value.is_finite() {
    return "—".to_string();
  }
  if value == 0.0 {
    return "0".to_string();
  }
  if value.abs() >= 100_000.0 || value.abs() < 0.0001 {
    format!("{value:.3e}")
  } else {
    let text = format!("{value:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
  }
}

fn color(index: usize) -> &'static str {
  COLORS[index % COLORS.len()]
}

fn precise_number(value: f64) -> String {
  serde_json::Number::from_f64(value)
    .map(|value| value.to_string())
    .unwrap_or_else(|| "—".to_string())
}

fn field(value: &Value, key: &str) -> String {
  match &value[key] {
    Value::Null => "—".to_string(),
    Value::String(text) => text.clone(),
    other => other.to_string(),
  }
}

fn escape(text: &str) -> String {
  let mut escaped = String::with_capacity(text.len());
  for character in text.chars() {
    escaped.push_str(match character {
      '&' => "&amp;",
      '<' => "&lt;",
      '>' => "&gt;",
      '"' => "&quot;",
      '\'' => "&#39;",
      _ => {
        escaped.push(character);
        continue;
      }
    });
  }
  escaped
}

fn message(value: impl Into<String>) -> ExpriError {
  ExpriError::Message(value.into())
}

const HTML_HEAD: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Run comparison · expri</title>
<style>
:root{color-scheme:light;--ink:#20283a;--muted:#647086;--line:#e2e6ed;--paper:#fff;--bg:#f4f5f8}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.55 system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}main{max-width:1160px;margin:0 auto;padding:48px 28px}header{margin-bottom:30px}h1{font-size:36px;letter-spacing:-1px;margin:4px 0 10px;line-height:1.2}h2{font-size:19px;margin:0 0 16px;overflow-wrap:anywhere}.eyebrow{color:#4056b4;font-size:12px;font-weight:700;letter-spacing:1.4px;text-transform:uppercase;margin:0}.muted{color:var(--muted)}.card{background:var(--paper);border:1px solid var(--line);border-radius:14px;padding:24px;margin:18px 0;box-shadow:0 2px 4px #18284404}.card>p{margin:0 0 18px}.table-scroll{overflow-x:auto}table{border-collapse:collapse;width:100%;font-size:13px}th,td{text-align:left;padding:11px 12px;border-bottom:1px solid var(--line);vertical-align:top}thead th{font-weight:600;color:var(--muted);font-size:11px;letter-spacing:.5px;text-transform:uppercase}tbody tr:last-child>*{border-bottom:0}tbody th{font-weight:500}code,pre{font-family:ui-monospace,SFMono-Regular,Consolas,monospace;font-size:12px}code{overflow-wrap:anywhere}pre{margin:0;white-space:pre-wrap;overflow-wrap:anywhere;min-width:130px;max-width:380px}.params .diff>*{background:#fff8e8}.diff-label{display:block;color:#8b681e;font:10px/1.4 system-ui;letter-spacing:.3px;margin-top:4px}.swatch{display:inline-block;width:10px;height:10px;border-radius:3px;margin-right:8px;flex:none}svg{display:block;width:100%;height:auto;min-width:520px;margin:8px 0 10px;overflow:visible}.grid{stroke:#e6eaf0;stroke-width:1}.tick{fill:#647086;font:11px ui-monospace,SFMono-Regular,Consolas,monospace}.axis-label{fill:#647086;font:12px system-ui}.point{transition:r .1s}.point:hover{r:4}.legend{padding:0;margin:8px 0 18px;display:flex;flex-wrap:wrap;gap:12px 24px;list-style:none;font-size:12px}.legend li{display:flex;align-items:center;gap:5px}.legend .swatch{margin-right:2px}.warning{border-color:#e9d79e;background:#fffdf7}.warning ul{padding-left:20px}.warning li{margin:6px 0}footer{color:var(--muted);font-size:12px;margin-top:28px}@media(max-width:650px){main{padding:24px 14px}h1{font-size:28px}.card{padding:16px;overflow-x:auto}.legend li{flex-wrap:wrap}}
</style></head><body><main>
"#;

#[cfg(test)]
mod tests;
