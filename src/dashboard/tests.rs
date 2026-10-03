use super::*;
use crate::config::Config;

struct Fixture {
  _directory: tempfile::TempDir,
  root: PathBuf,
  dashboard: Dashboard,
}

impl Fixture {
  fn new() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    fs::write(root.join("expri.toml"), "[project]\nname = 'review-test'\n").unwrap();
    let context = CommandContext {
      config: Config::load(&root.join("expri.toml")).unwrap(),
      repo_root: root.clone(),
      project_name: Some("review-test".into()),
    };
    let dashboard = Dashboard::new(context, None).unwrap();
    Self {
      _directory: directory,
      root,
      dashboard,
    }
  }

  fn run(&self, id: &str) -> PathBuf {
    let run = self.root.join(".expri/runs").join(id);
    fs::create_dir_all(run.join("outputs")).unwrap();
    fs::create_dir_all(run.join("logs")).unwrap();
    fs::write(
      run.join("run-state.json"),
      json!({
        "schema_version":1,"run_id":id,"task":"train","status":"completed",
        "started_at":"2026-10-03T01:00:00Z","finished_at":"2026-10-03T01:01:00Z","exit_code":0,
        "command":["python","train.py"]
      })
      .to_string(),
    )
    .unwrap();
    fs::write(run.join("outputs/metrics.jsonl"), "{\"step\":0,\"metrics\":{\"loss\":3,\"accuracy\":0.2}}\n{\"step\":1,\"metrics\":{\"loss\":1,\"accuracy\":0.9}}\n").unwrap();
    run
  }
}

#[test]
fn empty_catalog_does_not_create_directories_and_finds_unconfigured_cache() {
  let fixture = Fixture::new();
  assert_eq!(
    fixture
      .dashboard
      .list("local", None, None, None, 100, 0)
      .unwrap()["total_count"],
    0
  );
  assert!(!fixture.root.join(".expri").exists());
  assert!(!fixture.root.join("results").exists());
  fs::create_dir_all(fixture.root.join("results/gpu rental/runs")).unwrap();
  let sources = fixture.dashboard.catalog().unwrap()["sources"]
    .as_array()
    .unwrap()
    .clone();
  assert_eq!(sources.len(), 2);
  assert_eq!(sources[1]["source_id"], "cached:gpu rental");
  assert_eq!(
    fixture
      .dashboard
      .list("cached:gpu rental", None, None, None, 100, 0)
      .unwrap()["runs"],
    json!([])
  );
}

#[test]
fn list_filters_before_paging_and_reports_exhausted_offsets() {
  let fixture = Fixture::new();
  fixture.run("run-a");
  fixture.run("run-b");
  fixture.run("run-c");
  let page = fixture
    .dashboard
    .list(
      "local",
      Some("TRAIN"),
      Some("train"),
      Some("completed"),
      1,
      1,
    )
    .unwrap();
  assert_eq!(page["runs"][0]["run_id"], "run-b");
  assert_eq!(page["total_count"], 3);
  assert_eq!(page["next_offset"], 2);
  assert_eq!(
    fixture
      .dashboard
      .list("local", None, None, None, 1, usize::MAX)
      .unwrap()["next_offset"],
    Value::Null
  );
  assert!(
    fixture
      .dashboard
      .list("local", None, None, None, 0, 0)
      .is_err()
  );
}

#[test]
fn detail_preserves_provenance_without_package_or_source_file_inventories() {
  let fixture = Fixture::new();
  let run = fixture.run("run-a");
  fs::write(run.join("snapshot.json"), json!({"run_id":"run-a","created_at":"today","source":{"kind":"git","git_head":"abc123","paths":["train.py"]},"files":[{"path":"train.py","sha256":"full"}]}).to_string()).unwrap();
  fs::create_dir(run.join("environment")).unwrap();
  fs::write(run.join("environment/environment-state.json"), json!({
    "schema_version":1,"base_python":"/opt/conda/bin/python","lock_sha256":"locked",
    "base_manifest":{"python":"/opt/conda/bin/python","marker_env":{"python_version":"3.12"},"packages":{"torch":{"version":"2.10.0+cu128"}},"torch":{"version":"2.10.0+cu128","cuda":"12.8"}},
    "combined_manifest":{"packages":{"torch":{},"local-package":{}}}
  }).to_string()).unwrap();
  let detail = fixture.dashboard.detail("local", "run-a").unwrap();
  assert_eq!(detail["snapshot"]["source"]["git_head"], "abc123");
  assert_eq!(detail["snapshot"]["file_count"], 1);
  assert!(detail["snapshot"].get("files").is_none());
  assert_eq!(detail["environment"]["base_manifest"]["package_count"], 1);
  assert_eq!(
    detail["environment"]["base_manifest"]["torch"]["cuda"],
    "12.8"
  );
  assert!(
    detail["environment"]["base_manifest"]
      .get("packages")
      .is_none()
  );
  assert_eq!(detail["metrics"]["loss"]["count"], 2);
  assert!(detail["metrics"]["loss"].get("points").is_none());
}

#[test]
fn summaries_match_full_reader_without_retaining_points() {
  let fixture = Fixture::new();
  fixture.run("run-a");
  let runs = fixture.root.join(".expri/runs");
  let full = metrics::read(&runs, "run-a", &[]).unwrap();
  let summaries = metrics::read_summaries(&runs, "run-a", &[]).unwrap();
  for (name, series) in summaries.metrics {
    assert!(series.points.is_empty());
    assert_eq!(
      serde_json::to_value(series.summary).unwrap(),
      serde_json::to_value(&full.metrics[&name].summary).unwrap()
    );
  }
}

#[test]
fn parameters_and_large_metadata_have_explicit_preview_flags() {
  let fixture = Fixture::new();
  let run = fixture.run("run-a");
  fs::write(run.join("outputs/params.json"), json!({"message":"雪".repeat(100_000),"depth":{"a":{"b":{"c":{"d":{"e":{"f":true}}}}}},"array":vec![1;30]}).to_string()).unwrap();
  let mut state: Value =
    serde_json::from_slice(&fs::read(run.join("run-state.json")).unwrap()).unwrap();
  state["command"] = json!("x".repeat(100_000));
  fs::write(run.join("run-state.json"), state.to_string()).unwrap();
  let detail = fixture.dashboard.detail("local", "run-a").unwrap();
  assert_eq!(detail["params_truncated"], true);
  assert_eq!(detail["metadata_truncated"], true);
  assert!(serde_json::to_vec(&detail).unwrap().len() < 32 * 1024);
  assert!(
    detail["warnings"]
      .as_array()
      .unwrap()
      .iter()
      .any(|warning| warning["message"]
        .as_str()
        .unwrap()
        .contains("Metadata previews"))
  );
}

#[test]
fn selected_metrics_are_passed_to_reader_and_comparison_omits_params() {
  let fixture = Fixture::new();
  fixture.run("run-a");
  fixture.run("run-b");
  let ids = vec!["run-a".into(), "run-b".into()];
  let selected = fixture
    .dashboard
    .read_metrics("local", &ids, &["loss".into()], 2, true)
    .unwrap();
  assert!(
    selected
      .iter()
      .all(|run| run.metrics.len() == 1 && run.metrics.contains_key("loss"))
  );
  let comparison = fixture
    .dashboard
    .compare("local", &ids, &["loss".into()], Reduction::Last)
    .unwrap();
  assert_eq!(comparison["comparison"]["metric_names"], json!(["loss"]));
  assert_eq!(
    comparison["comparison"]["runs"][0]["values"]["loss"]["value"],
    1.0
  );
  assert_eq!(comparison["comparison"]["runs"][0]["params"], Value::Null);
  assert!(
    fixture
      .dashboard
      .chart("local", &ids, &vec!["loss".into(); 7])
      .is_err()
  );
}

#[test]
fn logs_are_bounded_and_support_empty_missing_and_partial_lines() {
  let fixture = Fixture::new();
  let run = fixture.run("run-a");
  fs::write(run.join("logs/stdout.log"), "first\n雪\nlast").unwrap();
  assert_eq!(
    fixture
      .dashboard
      .log("local", "run-a", "stdout", 2)
      .unwrap()["content"],
    "雪\nlast"
  );
  assert_eq!(
    fixture
      .dashboard
      .log("local", "run-a", "stdout", 0)
      .unwrap()["content"],
    ""
  );
  assert_eq!(
    fixture
      .dashboard
      .log("local", "run-a", "stderr", 100)
      .unwrap()["missing"],
    true
  );
  fs::write(
    run.join("logs/stdout.log"),
    format!("{}\nending\n", "x".repeat(2 * 1024 * 1024)),
  )
  .unwrap();
  let log = fixture
    .dashboard
    .log("local", "run-a", "stdout", 1000)
    .unwrap();
  assert_eq!(log["content"], "ending\n");
  assert_eq!(log["truncated"], true);
  assert!(
    fixture
      .dashboard
      .log("local", "run-a", "stdout", 1001)
      .is_err()
  );
}

#[cfg(unix)]
#[test]
fn linked_logs_and_cached_sources_are_refused_and_metric_errors_keep_detail() {
  use std::os::unix::fs::symlink;
  let fixture = Fixture::new();
  let run = fixture.run("run-a");
  symlink(run.join("run-state.json"), run.join("logs/stderr.log")).unwrap();
  assert!(
    fixture
      .dashboard
      .log("local", "run-a", "stderr", 100)
      .is_err()
  );
  fs::create_dir_all(fixture.root.join("results")).unwrap();
  symlink(
    fixture.root.join(".expri"),
    fixture.root.join("results/linked"),
  )
  .unwrap();
  assert_eq!(
    fixture.dashboard.catalog().unwrap()["sources"]
      .as_array()
      .unwrap()
      .len(),
    1
  );
  assert!(fixture.dashboard.detail("cached:linked", "run-a").is_err());
  fs::write(run.join("outputs/params.json"), "{\"learning_rate\":0.001}").unwrap();
  fs::remove_file(run.join("outputs/metrics.jsonl")).unwrap();
  symlink(
    run.join("run-state.json"),
    run.join("outputs/metrics.jsonl"),
  )
  .unwrap();
  let detail = fixture.dashboard.detail("local", "run-a").unwrap();
  assert_eq!(detail["run"]["run_id"], "run-a");
  assert_eq!(detail["params"]["learning_rate"], 0.001);
  assert!(
    detail["metrics_error"]
      .as_str()
      .unwrap()
      .contains("regular file")
  );
}

#[test]
fn nested_empty_containers_still_obey_the_preview_byte_budget() {
  let mut value = json!([]);
  for _ in 0..4 {
    value = Value::Array(vec![value; 16]);
  }
  let mut truncated = false;
  let output = preview(&value, &mut truncated);
  assert!(truncated);
  assert!(serde_json::to_vec(&output).unwrap().len() <= 16 * 1024);
}

#[test]
fn control_byte_log_tails_fit_the_json_response_limit() {
  let fixture = Fixture::new();
  let run = fixture.run("run-a");
  fs::write(run.join("logs/stdout.log"), vec![0; 2 * 1024 * 1024]).unwrap();
  let log = fixture
    .dashboard
    .log("local", "run-a", "stdout", 1)
    .unwrap();
  assert_eq!(log["truncated"], true);
  assert!(serde_json::to_vec(&log).unwrap().len() < 512 * 1024);
}

#[test]
fn a_single_long_log_line_keeps_its_tail_with_or_without_a_final_newline() {
  let fixture = Fixture::new();
  let run = fixture.run("run-a");
  for newline in ["", "\n"] {
    fs::write(
      run.join("logs/stdout.log"),
      format!("{}{newline}", "x".repeat(2 * 1024 * 1024)),
    )
    .unwrap();
    let log = fixture
      .dashboard
      .log("local", "run-a", "stdout", 1)
      .unwrap();
    assert_eq!(log["truncated"], true);
    assert_eq!(log["content"].as_str().unwrap().len(), LOG_LIMIT as usize);
    assert!(log["content"].as_str().unwrap().ends_with(newline));
  }
}
