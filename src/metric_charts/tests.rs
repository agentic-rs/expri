use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::metrics::{MetricSeries, MetricSummary};

fn point(step: u64, value: f64) -> MetricPoint {
  MetricPoint {
    step,
    value,
    timestamp: None,
  }
}

fn series(points: Vec<MetricPoint>) -> MetricSeries {
  assert!(!points.is_empty());
  let min = points
    .iter()
    .min_by(|left, right| left.value.total_cmp(&right.value))
    .unwrap()
    .clone();
  let max = points
    .iter()
    .max_by(|left, right| left.value.total_cmp(&right.value))
    .unwrap()
    .clone();
  MetricSeries {
    summary: MetricSummary {
      count: points.len(),
      last: points.last().unwrap().clone(),
      min,
      max,
    },
    points,
  }
}

fn run(id: &str, points: Vec<MetricPoint>) -> RunMetrics {
  RunMetrics {
    run_id: id.to_string(),
    run: json!({"task":"train", "status":"completed", "started_at":"2026-10-03T00:00:00Z", "exit_code":0}),
    params: None,
    metrics: if points.is_empty() {
      BTreeMap::new()
    } else {
      BTreeMap::from([("loss".to_string(), series(points))])
    },
    warnings: Vec::new(),
  }
}

#[test]
fn user_text_is_escaped_in_markup_attributes_parameters_and_warnings() {
  let mut malicious = run("run\"'><script>alert(1)</script>", vec![point(0, 1.0)]);
  malicious.run["task"] = json!("<img src=x onerror=alert(2)>");
  malicious.run["status"] = json!("<svg onload=alert(3)>");
  malicious.params =
    Some(json!({"<parameter>": {"nested":"</pre><script>alert(4)</script>", "ampersand":"a & b"}}));
  malicious
    .warnings
    .push(json!({"message":"<script>alert(5)</script>"}));
  let name = "loss\" onmouseover=\"alert(6)<tag>";
  let series = malicious.metrics.remove("loss").unwrap();
  malicious.metrics.insert(name.to_string(), series);
  let html = render_chart(&[malicious], &[]).unwrap();
  assert!(!html.contains("<script"));
  assert!(!html.contains("<img"));
  assert!(!html.contains("<svg onload"));
  assert!(!html.contains("\" onmouseover=\""));
  assert!(html.contains("run&quot;&#39;&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
  assert!(html.contains("&lt;parameter&gt;"));
  assert!(html.contains("a &amp; b"));
  assert!(html.contains("aria-label=\"run&quot;"));
  assert!(!html.contains("https://"));
  assert!(!html.contains("http://"));
}

#[test]
fn parameter_differences_include_missing_values_and_nested_objects() {
  let mut first = run("run-a", vec![]);
  first.params = Some(json!({"same":3, "optimizer":{"lr":0.01}, "nullable":null}));
  let mut second = run("run-b", vec![]);
  second.params = Some(json!({"same":3, "optimizer":{"lr":0.02}}));
  let html = render_chart(&[first, second], &[]).unwrap();
  assert_eq!(html.matches("<tr class=diff>").count(), 2);
  assert!(html.contains("<tr><th scope=row><code>same</code>"));
  assert!(html.contains("<td><pre>null</pre></td><td class=muted>—</td>"));
  assert!(html.contains("&quot;lr&quot;: 0.01"));
  assert!(html.contains("&quot;lr&quot;: 0.02"));
}

#[test]
fn empty_and_single_point_series_make_valid_finite_plots() {
  let empty = render_chart(&[], &[]).unwrap();
  assert!(empty.contains("No scalar metrics"));
  assert!(empty.ends_with("</body></html>"));
  let single = render_chart(&[run("run-zero", vec![point(u64::MAX, 0.0)])], &[]).unwrap();
  assert!(single.contains("cx=\"518.00\" cy=\"151.00\""));
  assert!(!single.contains("NaN"));
  assert!(!single.contains("inf"));
  assert!(single.contains(&u64::MAX.to_string()));
  let points = [point(0, 0.0), point(1, 0.0), point(1, 0.0)];
  let domain = Domain::new(points.iter()).unwrap();
  assert_eq!(domain.y(0.0), 151.0);
  assert_eq!(domain.step_ticks(), vec![0, 1]);
  assert_eq!(domain.value_ticks(), vec![(0.0, 0.5)]);
}

#[test]
fn scales_handle_extreme_tiny_and_adjacent_values_without_overflow() {
  for values in [
    [-1.0e308, 1.0e308],
    [-f64::MAX, f64::MAX],
    [f64::MAX, f64::from_bits(f64::MAX.to_bits() - 1)],
    [-f64::MAX, -f64::from_bits(f64::MAX.to_bits() - 1)],
    [f64::from_bits(1), f64::from_bits(2)],
  ] {
    let points = vec![point(0, values[0]), point(u64::MAX, values[1])];
    let domain = Domain::new(points.iter()).unwrap();
    for point in &points {
      assert!(domain.x(point.step).is_finite());
      assert!(domain.y(point.value).is_finite());
      assert!((TOP..=TOP + HEIGHT).contains(&domain.y(point.value)));
    }
    for (value, _) in domain.value_ticks() {
      assert!(value.is_finite());
    }
    let html = render_chart(&[run("run-extreme", points)], &[]).unwrap();
    assert!(!html.contains("NaN"));
    assert!(!html.contains("inf"));
  }
  let points = [point(u64::MAX - 1, 0.0), point(u64::MAX, 1.0)];
  let domain = Domain::new(points.iter()).unwrap();
  assert_eq!(domain.x(u64::MAX - 1), LEFT);
  assert_eq!(domain.x(u64::MAX), LEFT + WIDTH);
}

#[test]
fn curves_keep_logging_order_and_distinct_missing_metrics() {
  let first = run(
    "run-reset",
    vec![point(42, 1.0), point(0, 2.0), point(42, 3.0)],
  );
  let mut second = run("run-other", vec![]);
  second
    .metrics
    .insert("accuracy".to_string(), series(vec![point(9, 0.5)]));
  let html = render_chart(
    &[first, second],
    &["loss".to_string(), "accuracy".to_string()],
  )
  .unwrap();
  assert!(html.contains("points=\"950.00,274.00 86.00,151.00 950.00,28.00\""));
  assert_eq!(html.matches("Metric not recorded").count(), 2);
  assert!(html.find("<h2>loss</h2>").unwrap() < html.find("<h2>accuracy</h2>").unwrap());
  assert!(render_chart(&[run("run-empty", vec![])], &["unknown".to_string()]).is_err());
}

#[test]
fn downsampling_bounds_points_preserves_extrema_and_full_series_summary() {
  let mut points: Vec<_> = (0..10_000)
    .map(|step| point(step, (step % 13) as f64))
    .collect();
  points[5000].value = -99_999.0;
  points[5001].value = 99_999.0;
  let references = points.iter().collect::<Vec<_>>();
  let chosen = plot_points(&references, POINT_BUDGET);
  assert!(chosen.len() <= POINT_BUDGET);
  assert!(std::ptr::eq(chosen[0], &points[0]));
  assert!(std::ptr::eq(
    chosen[chosen.len() - 1],
    points.last().unwrap()
  ));
  assert!(chosen.iter().any(|point| point.step == 5000));
  assert!(chosen.iter().any(|point| point.step == 5001));
  assert!(chosen.windows(2).all(|pair| pair[0].step < pair[1].step));
  let run = run("run-long", points);
  let before = serde_json::to_value(&run).unwrap();
  let html = render_chart(std::slice::from_ref(&run), &[]).unwrap();
  assert!(html.matches("<circle ").count() <= POINT_BUDGET);
  assert!(html.contains("Curves are downsampled"));
  assert!(html.contains("<td>10000</td><td>9999</td>"));
  assert!(html.contains(">-99999</td><td title=\"99999.0\">99999</td>"));
  assert_eq!(serde_json::to_value(&run).unwrap(), before);
}

#[test]
fn dashboard_sampling_keeps_extrema_and_complete_sample_counts_with_a_smaller_budget() {
  let mut points: Vec<_> = (0..10_000)
    .map(|step| point(step, (step % 13) as f64))
    .collect();
  points[5000].value = -99_999.0;
  points[5001].value = 99_999.0;
  let references = points.iter().collect::<Vec<_>>();
  let chosen = plot_points(&references, DASHBOARD_POINT_BUDGET);
  assert!(chosen.len() <= DASHBOARD_POINT_BUDGET);
  assert!(std::ptr::eq(chosen[0], &points[0]));
  assert!(std::ptr::eq(
    chosen[chosen.len() - 1],
    points.last().unwrap()
  ));
  assert!(chosen.iter().any(|point| point.step == 5000));
  assert!(chosen.iter().any(|point| point.step == 5001));
  assert!(chosen.windows(2).all(|pair| pair[0].step < pair[1].step));
  let html = render_dashboard_chart(&[run("run-long", points)], &[]).unwrap();
  assert!(html.matches("<circle ").count() <= DASHBOARD_POINT_BUDGET);
  assert!(html.contains("at most 600 points per run"));
  assert!(html.contains("10000 samples"));
  assert!(html.contains("value -99999.0") && html.contains("value 99999.0"));
  assert!(html.contains("Sample counts include every logged sample"));
  assert!(!html.contains("<table>"));
}

#[test]
fn dashboard_metric_selection_is_bounded_and_cli_export_keeps_every_metric() {
  let mut selected = run("run-many", vec![]);
  let names: Vec<_> = (0..7).map(|index| format!("metric_{index:02}")).collect();
  for name in &names {
    selected
      .metrics
      .insert(name.clone(), series(vec![point(0, 1.0)]));
  }
  let html = render_dashboard_chart(std::slice::from_ref(&selected), &[]).unwrap();
  assert_eq!(html.matches("<svg ").count(), 4);
  assert!(html.contains("Showing the first 4 of 7 metrics"));
  assert!(!html.contains("metric_04"));
  let explicit = render_dashboard_chart(std::slice::from_ref(&selected), &names[..6]).unwrap();
  assert_eq!(explicit.matches("<svg ").count(), 6);
  assert!(render_dashboard_chart(std::slice::from_ref(&selected), &names).is_err());
  let full = render_chart(&[selected], &[]).unwrap();
  assert_eq!(full.matches("<svg ").count(), 7);
  assert!(!full.contains("Showing the first"));
}

#[test]
fn dashboard_aggregate_sampling_keeps_supported_large_comparisons_below_the_html_cap() {
  let names: Vec<_> = (0..6).map(|index| format!("metric_{index:02}")).collect();
  let mut runs = Vec::new();
  for index in 0..8 {
    let mut selected = run(&format!("{}{index:02}", "r".repeat(30)), vec![]);
    for name in &names {
      selected.metrics.insert(
        name.clone(),
        series(
          (0..10_000)
            .map(|step| point(step, (step % 13) as f64))
            .collect(),
        ),
      );
    }
    runs.push(selected);
  }
  for (filters, plots) in [(&[][..], 4), (names.as_slice(), 6)] {
    let html = render_dashboard_chart(&runs, filters).unwrap();
    assert!(html.len() < 2 * 1024 * 1024);
    assert_eq!(html.matches("<svg ").count(), plots);
    assert!(html.matches("<circle ").count() <= DASHBOARD_TOTAL_POINT_BUDGET);
    assert_eq!(html.matches("10000 samples").count(), plots * 8);
  }
}

#[test]
fn dashboard_parameter_preview_is_bounded_but_differences_use_complete_values() {
  let mut first = run("run-a", vec![]);
  let mut params = serde_json::Map::new();
  for index in 0..30 {
    params.insert(format!("key-{index:02}"), json!("λ".repeat(10_000)));
  }
  params.insert(
    "key-00".to_string(),
    json!(format!("{}END_A", "λ".repeat(10_000))),
  );
  first.params = Some(Value::Object(params));
  let mut second = first.clone();
  second.run_id = "run-b".to_string();
  second.params.as_mut().unwrap()["key-00"] = json!(format!("{}END_B", "λ".repeat(10_000)));
  let runs = [first, second];
  let before = serde_json::to_value(&runs).unwrap();
  let preview = render_dashboard_chart(&runs, &[]).unwrap();
  assert!(preview.contains("Dashboard parameter preview"));
  assert!(preview.contains("first 24 keys"));
  assert!(preview.contains("1000 characters"));
  assert_eq!(preview.matches("<tr class=diff>").count(), 1);
  assert!(preview.contains("key-23"));
  assert!(!preview.contains("key-24"));
  assert!(!preview.contains("END_A") && !preview.contains("END_B"));
  assert!(preview.len() < 160 * 1024);
  assert!(
    preview.contains("<details class=parameter-comparison><summary>Compare parameters</summary>")
  );
  assert!(!preview.contains("<details open"));
  let text = preview_text(&"猫".repeat(1001), Some(1000));
  assert_eq!(text.chars().count(), 1000);
  assert!(text.ends_with('…'));
  assert_eq!(preview_text("unchanged", Some(1000)), "unchanged");
  let full = render_chart(&runs, &[]).unwrap();
  assert!(full.contains("key-29") && full.contains("END_A") && full.contains("END_B"));
  assert!(!full.contains("Dashboard parameter preview"));
  assert_eq!(serde_json::to_value(&runs).unwrap(), before);
}

#[test]
fn dashboard_bounds_metadata_notes_and_repeated_point_labels() {
  let mut selected = run(&"r".repeat(240), vec![]);
  selected.run["task"] = json!(format!("{}TASK_END", "猫".repeat(100_000)));
  selected
    .warnings
    .push(json!({"message":format!("{}WARNING_END", "<".repeat(100_000))}));
  let name = "<".repeat(256);
  selected.metrics.insert(
    name.clone(),
    series((0..10_000).map(|step| point(step, step as f64)).collect()),
  );
  let preview = render_dashboard_chart(std::slice::from_ref(&selected), &[]).unwrap();
  assert!(preview.len() < 512 * 1024);
  assert!(!preview.contains("TASK_END") && !preview.contains("WARNING_END"));
  assert!(preview.contains("Long data notes are shortened"));
  assert!(preview.contains("Run 1 · step"));
  assert!(preview.contains(&escape(&name)));
  let full = render_chart(&[selected], &[]).unwrap();
  assert!(full.contains("TASK_END") && full.contains("WARNING_END"));
  assert!(!full.contains("Long data notes are shortened"));
}

#[test]
fn dashboard_starts_with_curves_and_keeps_multi_run_parameter_differences_collapsed() {
  let mut first = run("run-a", vec![point(0, 3.0), point(1, 1.0)]);
  first.params = Some(json!({"learning_rate": 0.01}));
  let mut second = run("run-b", vec![point(0, 4.0), point(1, 2.0)]);
  second.params = Some(json!({"learning_rate": 0.02}));
  let runs = [first, second];
  let full = render_chart(&runs, &[]).unwrap();
  for retained in [
    "<h1>Run comparison</h1>",
    "<h2>Selected runs</h2>",
    "<h2>Effective parameters</h2>",
    "<th>Minimum</th><th>Maximum</th>",
    "Generated by expri. This document works offline.",
  ] {
    assert!(full.contains(retained));
  }
  let preview = render_dashboard_chart(&runs, &[]).unwrap();
  assert!(preview.contains("1 metric chart · 2 runs"));
  assert!(!preview.contains("<h1>") && !preview.contains("<footer>"));
  assert!(!preview.contains("<h2>Selected runs</h2>"));
  assert!(!preview.contains("<th>Minimum</th><th>Maximum</th>"));
  assert_eq!(preview.matches("2 samples").count(), 2);
  assert!(preview.contains("<tr class=diff>"));
  assert!(preview.contains("0.01") && preview.contains("0.02"));
  let parameters = "<details class=parameter-comparison><summary>Compare parameters</summary>";
  assert!(preview.find(parameters).unwrap() > preview.rfind("</svg>").unwrap());
  let single = render_dashboard_chart(&runs[..1], &[]).unwrap();
  assert!(!single.contains(parameters));
  assert!(!single.contains("Effective parameters") && !single.contains("learning_rate"));
}

#[test]
fn dashboard_plot_labels_and_warnings_escape_user_text_and_identify_missing_series() {
  let mut recorded = run("run\"<recorded>", vec![point(0, 1.0)]);
  let name = "loss\" onmouseover=\"alert(1)<metric>";
  let series = recorded.metrics.remove("loss").unwrap();
  recorded.metrics.insert(name.to_string(), series);
  recorded
    .warnings
    .push(json!({"message":"incomplete <final> row & skipped"}));
  let absent = run("run-missing", vec![]);
  let preview = render_dashboard_chart(&[recorded, absent], &[]).unwrap();
  assert!(!preview.contains("\" onmouseover=\""));
  assert!(
    preview.contains("aria-label=\"Plot of loss&quot; onmouseover=&quot;alert(1)&lt;metric&gt;\"")
  );
  assert!(preview.contains("class=plot-scroll tabindex=0 role=region"));
  assert!(preview.contains("run&quot;&lt;recorded&gt;"));
  assert!(preview.contains("<code>run-missing</code><span class=muted>not recorded</span>"));
  assert!(preview.contains("1 sample</span>"));
  assert!(preview.contains("incomplete &lt;final&gt; row &amp; skipped"));
  assert!(!preview.contains("table below lists statistics"));
  assert!(!preview.contains("Long data notes are shortened"));
}

#[test]
fn dashboard_tick_labels_have_left_padding_and_keep_exact_tooltip_values() {
  let exact = 1.844475;
  let selected = run("run-precise", vec![point(0, exact)]);
  let dashboard = render_dashboard_chart(std::slice::from_ref(&selected), &[]).unwrap();
  assert!(dashboard.contains("viewBox=\"-84 0 1084 340\""));
  assert!(dashboard.contains("text-anchor=end><title>1.844475</title>1.844475</text>"));
  assert!(dashboard.contains("step 0 · value 1.844475"));
  let report = render_chart(&[selected], &[]).unwrap();
  assert!(report.contains("viewBox=\"0 0 1000 340\""));
  assert!(report.contains("text-anchor=end>1.844475</text>"));
  for value in [
    -99_999.999999,
    9_999.999999,
    -0.00123456789,
    f64::MAX,
    -f64::MAX,
    f64::MIN_POSITIVE,
    -f64::from_bits(1),
    0.0,
  ] {
    assert!(number(value).len() <= 13);
  }
}

#[test]
fn dashboard_preserves_distinct_ticks_for_narrow_loss_ranges() {
  let selected = run("run-narrow", vec![point(0, 1.8441), point(1, 1.8444)]);
  let dashboard = render_dashboard_chart(std::slice::from_ref(&selected), &[]).unwrap();
  let report = render_chart(&[selected], &[]).unwrap();
  for label in ["1.8441", "1.844175", "1.84425", "1.844325", "1.8444"] {
    assert!(dashboard.contains(&format!("</title>{label}</text>")));
    assert!(report.contains(&format!("text-anchor=end>{label}</text>")));
  }
}

#[test]
fn chart_publication_replaces_regular_files_atomically_and_creates_parent() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("reports/compare.html");
  write_chart(&path, &[run("run-one", vec![point(0, 1.0)])], &[]).unwrap();
  assert!(
    fs::read_to_string(&path)
      .unwrap()
      .starts_with("<!doctype html>")
  );
  fs::write(&path, "old report").unwrap();
  write_chart(&path, &[], &[]).unwrap();
  assert!(
    fs::read_to_string(&path)
      .unwrap()
      .contains("No scalar metrics")
  );
  assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn chart_publication_refuses_symlink_and_special_output_without_touching_target() {
  use std::os::unix::fs::symlink;
  let directory = tempfile::tempdir().unwrap();
  let target = directory.path().join("outside.html");
  fs::write(&target, "user content").unwrap();
  let linked = directory.path().join("comparison.html");
  symlink(&target, &linked).unwrap();
  assert!(write_chart(&linked, &[], &[]).is_err());
  assert_eq!(fs::read_to_string(&target).unwrap(), "user content");
  assert!(write_chart(directory.path(), &[], &[]).is_err());
}
