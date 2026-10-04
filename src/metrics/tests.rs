use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde_json::json;

use super::*;

struct Fixture {
  _root: tempfile::TempDir,
  runs: PathBuf,
}

impl Fixture {
  fn new() -> Self {
    let root = tempfile::tempdir().unwrap();
    let runs = fs::canonicalize(root.path()).unwrap().join(".expri/runs");
    fs::create_dir_all(&runs).unwrap();
    Self { _root: root, runs }
  }

  fn run(&self, id: &str, metrics: &[u8]) -> PathBuf {
    let run = self.runs.join(id);
    fs::create_dir_all(run.join("outputs")).unwrap();
    fs::write(
      run.join("run-state.json"),
      serde_json::to_vec(&json!({
        "schema_version": 1, "run_id": id, "task": "train", "status": "running",
        "started_at": "2026-10-03T01:00:00Z"
      }))
      .unwrap(),
    )
    .unwrap();
    fs::write(run.join("outputs/metrics.jsonl"), metrics).unwrap();
    run
  }
}

#[test]
fn summaries_use_logged_order_and_keep_sparse_metrics_and_first_tied_extrema() {
  let fixture = Fixture::new();
  fixture.run(
    "run-a",
    br#"{"step":10,"metrics":{"loss":3,"accuracy":0.1}}
{"schema_version":1,"step":20,"timestamp":"2026-10-03T01:00:01Z","metrics":{"loss":1}}
{"step":30,"metrics":{"loss":1,"accuracy":0.7}}
{"step":5,"metrics":{"loss":2}}
"#,
  );
  let run = read(&fixture.runs, "run-a", &[]).unwrap();
  let loss = &run.metrics["loss"];
  assert_eq!(loss.summary.count, 4);
  assert_eq!(loss.summary.last.step, 5);
  assert_eq!(loss.summary.last.value, 2.0);
  assert_eq!(loss.summary.min.step, 20);
  assert_eq!(loss.summary.max.step, 10);
  assert_eq!(
    loss.points[1].timestamp.as_deref(),
    Some("2026-10-03T01:00:01Z")
  );
  assert_eq!(run.metrics["accuracy"].summary.count, 2);
  assert!(run.warnings.iter().any(|warning| {
    warning["message"]
      .as_str()
      .unwrap()
      .contains("steps decreased")
  }));
}

#[test]
fn malformed_middle_and_partial_final_rows_preserve_valid_events_and_report_lines() {
  let fixture = Fixture::new();
  fixture.run(
    "run-a",
    br#"{"step":0,"metrics":{"loss":4}}
not json
{"step":1,"metrics":{"loss":2}}
{"step":2,"metrics":{"loss":"#,
  );
  let run = read(&fixture.runs, "run-a", &[]).unwrap();
  assert_eq!(run.metrics["loss"].summary.last.value, 2.0);
  assert_eq!(run.metrics["loss"].summary.count, 2);
  assert!(
    run
      .warnings
      .iter()
      .any(|warning| warning["message"] == "metrics.jsonl line 2: invalid JSON; skipped")
  );
  assert!(
    run
      .warnings
      .iter()
      .any(|warning| warning["message"] == "metrics.jsonl line 4: incomplete final row; skipped")
  );
  fs::write(
    fixture.runs.join("run-a/outputs/metrics.jsonl"),
    b"{\"step\":3,\"metrics\":{\"loss\":1}}",
  )
  .unwrap();
  let run = read(&fixture.runs, "run-a", &[]).unwrap();
  assert_eq!(
    run.metrics["loss"].summary.count, 1,
    "a valid last row needs no final newline"
  );
}

#[test]
fn invalid_scalar_shapes_versions_steps_and_names_are_visible_and_not_coerced() {
  let fixture = Fixture::new();
  fixture.run(
    "run-a",
    br#"{"step":true,"metrics":{"loss":3}}
{"step":-1,"metrics":{"loss":3}}
{"step":1.5,"metrics":{"loss":3}}
{"schema_version":2,"step":1,"metrics":{"loss":3}}
{"step":1,"metrics":{"loss":true}}
{"step":1,"metrics":{"loss":"3"}}
{"step":1,"metrics":{"loss":[3]}}
{"step":1,"metrics":{}}
{"step":1,"timestamp":"yesterday","metrics":{"loss":3}}
{"step":1,"metrics":{"\nloss":3}}
{"step":1,"metrics":{" ":3}}
{"step":1,"metrics":{"loss":1e400}}
{"step":18446744073709551615,"metrics":{"loss":-1e308," loss ":2}}
"#,
  );
  let run = read(&fixture.runs, "run-a", &[]).unwrap();
  assert_eq!(run.warnings.len(), 12);
  assert_eq!(run.metrics["loss"].summary.last.step, u64::MAX);
  assert_eq!(run.metrics["loss"].summary.last.value, -1e308);
  assert_eq!(run.metrics[" loss "].summary.last.value, 2.0);
}

#[test]
fn comparisons_select_reduction_and_leave_missing_values_explicit() {
  let fixture = Fixture::new();
  let first = fixture.run("run-a", b"{\"step\":0,\"metrics\":{\"loss\":3}}\n{\"step\":1,\"metrics\":{\"loss\":1,\"accuracy\":0.7}}\n");
  fixture.run("run-b", b"{\"step\":1,\"metrics\":{\"loss\":2}}\n");
  fs::write(
    first.join("outputs/params.json"),
    br#"{"schema_version":1,"params":{"lr":0.01,"model":{"width":64}}}"#,
  )
  .unwrap();
  let runs = [
    read(&fixture.runs, "run-a", &[]).unwrap(),
    read(&fixture.runs, "run-b", &[]).unwrap(),
  ];
  let min = compare(&runs, &[], Reduction::Min).unwrap();
  assert_eq!(min.metric_names, ["accuracy", "loss"]);
  assert_eq!(min.runs[0].values["loss"].as_ref().unwrap().value, 1.0);
  assert!(min.runs[1].values["accuracy"].is_none());
  assert_eq!(min.runs[0].params.as_ref().unwrap()["model"]["width"], 64);
  let max = compare(&runs, &["loss".to_string()], Reduction::Max).unwrap();
  assert_eq!(max.runs[0].values["loss"].as_ref().unwrap().value, 3.0);
  assert!(compare(&runs, &["typo".to_string()], Reduction::Last).is_err());
  assert!(compare(&runs[..1], &[], Reduction::Last).is_err());
  assert!(compare(&[runs[0].clone(), runs[0].clone()], &[], Reduction::Last).is_err());
}

#[test]
fn absent_remote_files_do_not_reuse_retained_cached_metrics_or_parameters() {
  let fixture = Fixture::new();
  let root = fixture.run("run-a", b"{\"step\":0,\"metrics\":{\"loss\":9}}\n");
  fs::write(root.join("outputs/params.json"), br#"{"lr":0.9}"#).unwrap();
  let fresh = read_with_files(
    &fixture.runs,
    "run-a",
    &[],
    MetricFiles {
      metrics: false,
      params: false,
    },
  )
  .unwrap();
  assert!(fresh.metrics.is_empty());
  assert!(fresh.params.is_none());
  let cached = read(&fixture.runs, "run-a", &[]).unwrap();
  assert_eq!(cached.metrics["loss"].summary.last.value, 9.0);
  assert_eq!(cached.params.as_ref().unwrap()["lr"], 0.9);
}

#[test]
fn oversized_rows_are_drained_and_warning_count_is_bounded() {
  let fixture = Fixture::new();
  let mut data = vec![b'x'; LINE_LIMIT + 32];
  data.extend_from_slice(b"\n{\"step\":1,\"metrics\":{\"loss\":1}}\n");
  data.extend_from_slice(&b"invalid\n".repeat(200));
  fixture.run("run-a", &data);
  let result = read(&fixture.runs, "run-a", &[]).unwrap();
  assert_eq!(result.metrics["loss"].summary.count, 1);
  assert_eq!(result.warnings.len(), WARNING_LIMIT + 1);
  assert!(
    result.warnings[0]["message"]
      .as_str()
      .unwrap()
      .contains("1 MiB")
  );
  assert_eq!(
    result.warnings.last().unwrap()["message"],
    "additional metric warnings omitted"
  );
}

#[test]
fn file_limits_params_errors_and_missing_metric_files_are_reported() {
  let fixture = Fixture::new();
  let run = fixture.run("run-a", b"");
  fs::write(
    run.join("outputs/params.json"),
    br#"{"schema_version":2,"params":{}}"#,
  )
  .unwrap();
  let result = read(&fixture.runs, "run-a", &[]).unwrap();
  assert!(result.params.is_none());
  assert!(result.warnings.iter().any(|warning| {
    warning["message"]
      .as_str()
      .unwrap()
      .contains("unsupported schema_version")
  }));
  File::options()
    .write(true)
    .open(run.join("outputs/metrics.jsonl"))
    .unwrap()
    .set_len(METRICS_FILE_LIMIT + 1)
    .unwrap();
  assert!(
    read(&fixture.runs, "run-a", &[])
      .unwrap_err()
      .to_string()
      .contains("128 MiB")
  );
  fs::remove_file(run.join("outputs/metrics.jsonl")).unwrap();
  let result = read(&fixture.runs, "run-a", &[]).unwrap();
  assert!(result.metrics.is_empty());
  assert!(result.warnings.iter().any(|warning| {
    warning["message"]
      .as_str()
      .unwrap()
      .contains("no metrics have been recorded")
  }));
}

#[cfg(unix)]
#[test]
fn symlink_run_output_and_metric_paths_are_not_followed() {
  use std::os::unix::fs::symlink;
  let fixture = Fixture::new();
  let run = fixture.run("run-a", b"{\"step\":1,\"metrics\":{\"loss\":2}}\n");
  let outside = tempfile::tempdir().unwrap();
  fs::write(outside.path().join("metrics.jsonl"), b"private").unwrap();
  fs::remove_file(run.join("outputs/metrics.jsonl")).unwrap();
  symlink(
    outside.path().join("metrics.jsonl"),
    run.join("outputs/metrics.jsonl"),
  )
  .unwrap();
  assert!(read(&fixture.runs, "run-a", &[]).is_err());
  fs::remove_file(run.join("outputs/metrics.jsonl")).unwrap();
  fs::remove_dir(run.join("outputs")).unwrap();
  symlink(outside.path(), run.join("outputs")).unwrap();
  assert!(read(&fixture.runs, "run-a", &[]).is_err());
  symlink(&run, fixture.runs.join("run-linked")).unwrap();
  assert!(read(&fixture.runs, "run-linked", &[]).is_err());
  assert!(read(&fixture.runs, "../outside", &[]).is_err());
}

#[cfg(unix)]
#[test]
fn vendored_python_producer_is_readable_without_site_packages_or_pythonpath() {
  let fixture = Fixture::new();
  let run = fixture.run("run-python", b"");
  let code = run.join("code");
  fs::create_dir(&code).unwrap();
  fs::copy(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("python/expri_metrics.py"),
    code.join("expri_metrics.py"),
  )
  .unwrap();
  fs::write(
    code.join("train.py"),
    r#"from expri_metrics import MetricsLogger
with MetricsLogger() as logger:
  logger.params({"learning_rate": 0.01, "model": {"width": 64}, "seed": 42})
  logger.log(0, {"train/loss": 4, "train/accuracy": 0.25})
  logger.log(18446744073709551615, {"train/loss": 0.5})
"#,
  )
  .unwrap();
  let output = std::process::Command::new("python3")
    .args(["-B", "-S", "train.py"])
    .current_dir(&code)
    .env_remove("PYTHONPATH")
    .env("EXPRI_OUTPUT_DIR", run.join("outputs"))
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  let read = read(&fixture.runs, "run-python", &[]).unwrap();
  assert!(read.warnings.is_empty(), "{:?}", read.warnings);
  assert_eq!(read.params.as_ref().unwrap()["model"]["width"], 64);
  assert_eq!(read.metrics["train/loss"].summary.count, 2);
  assert_eq!(read.metrics["train/loss"].summary.last.step, u64::MAX);
  assert_eq!(read.metrics["train/loss"].summary.last.value, 0.5);
  assert!(read.metrics["train/loss"].points[0].timestamp.is_some());
  assert_eq!(read.metrics["train/accuracy"].summary.count, 1);
}

#[test]
fn long_fractional_timestamps_cannot_amplify_memory_across_metric_points() {
  let fixture = Fixture::new();
  let timestamp = format!("2026-10-03T01:00:00.{}Z", "1".repeat(100_000));
  let many_metrics: BTreeMap<_, _> = (0..1000)
    .map(|index| (format!("metric-{index}"), 1))
    .collect();
  let mut data =
    serde_json::to_vec(&json!({"step":0,"timestamp":timestamp,"metrics":many_metrics})).unwrap();
  data.extend_from_slice(b"\n{\"step\":1,\"metrics\":{\"loss\":0.5}}\n");
  fixture.run("run-a", &data);
  let run = read(&fixture.runs, "run-a", &[]).unwrap();
  assert_eq!(run.metrics.len(), 1);
  assert_eq!(run.metrics["loss"].summary.last.value, 0.5);
  assert!(
    run.warnings[0]["message"]
      .as_str()
      .unwrap()
      .contains("128 bytes")
  );
}

#[test]
fn point_limit_counts_selected_metrics_and_filtering_can_read_large_runs() {
  let fixture = Fixture::new();
  let names: BTreeMap<_, _> = (0..1000)
    .map(|index| (format!("metric-{index}"), 1))
    .collect();
  let mut row = serde_json::to_vec(&json!({"step":0,"metrics":names})).unwrap();
  row.push(b'\n');
  let rows = POINT_LIMIT / 1000 + 1;
  fixture.run("run-a", &row.repeat(rows));
  assert!(
    read(&fixture.runs, "run-a", &[])
      .unwrap_err()
      .to_string()
      .contains("selected point limit")
  );
  let filtered = read(&fixture.runs, "run-a", &["metric-0".to_string()]).unwrap();
  assert_eq!(filtered.metrics.len(), 1);
  assert_eq!(filtered.metrics["metric-0"].summary.count, rows);
}

#[test]
fn hosted_reader_bounds_curves_without_changing_complete_reductions() {
  let mut bytes = Vec::new();
  for step in 0..10000 {
    writeln!(
      &mut bytes,
      "{}",
      json!({"step": step, "metrics": {"loss": step}})
    )
    .unwrap();
  }
  let mut run = RunMetrics {
    run_id: "preview".into(),
    run: json!({"run_id": "preview"}),
    params: None,
    metrics: BTreeMap::new(),
    warnings: Vec::new(),
  };
  read_event_data(
    bytes.as_slice(),
    bytes.len() as u64,
    &mut run,
    &["loss".into()],
    true,
    Some(16),
  )
  .unwrap();
  let series = &run.metrics["loss"];
  assert!(series.points.len() <= 16);
  assert_eq!(series.points.first().unwrap().step, 0);
  assert_eq!(series.points.last().unwrap().step, 9999);
  assert_eq!(series.summary.count, 10000);
  assert_eq!(series.summary.min.value, 0.0);
  assert_eq!(series.summary.max.value, 9999.0);
  assert_eq!(series.summary.last.value, 9999.0);
  assert!(run.warnings.iter().any(|warning| {
    warning["message"]
      .as_str()
      .unwrap_or_default()
      .contains("sampled")
  }));
}

#[test]
fn hosted_reader_bounds_distinct_series_and_selection_can_read_another_metric() {
  let names: BTreeMap<_, _> = (0..2001)
    .map(|index| (format!("metric-{index}"), index))
    .collect();
  let bytes = serde_json::to_vec(&json!({"step": 0, "metrics": names})).unwrap();
  let mut run = RunMetrics {
    run_id: "preview".into(),
    run: json!({}),
    params: None,
    metrics: BTreeMap::new(),
    warnings: Vec::new(),
  };
  assert!(
    read_event_data(
      bytes.as_slice(),
      bytes.len() as u64,
      &mut run,
      &[],
      false,
      Some(2000)
    )
    .unwrap_err()
    .to_string()
    .contains("2000-series")
  );
  run.metrics.clear();
  read_event_data(
    bytes.as_slice(),
    bytes.len() as u64,
    &mut run,
    &["metric-2000".into()],
    true,
    Some(2000),
  )
  .unwrap();
  assert_eq!(run.metrics.len(), 1);
  assert_eq!(run.metrics["metric-2000"].summary.last.value, 2000.0);
}
