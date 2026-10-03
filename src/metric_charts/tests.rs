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
  let chosen = plot_points(&references);
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
