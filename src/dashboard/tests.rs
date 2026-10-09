use super::*;
use crate::config::Config;

#[test]
fn local_scalar_cache_refreshes_appended_metrics_and_replaced_parameter_files() {
  use std::io::Write;
  let fixture = Fixture::new();
  let run = fixture.run("run-a");
  let params = run.join("outputs/params.json");
  fs::write(&params, json!({"rate":1}).to_string()).unwrap();
  let options = table::TableOptions::parse(
    vec!["/rate".into()],
    vec!["loss".into()],
    "last",
    Some("metric:loss"),
    Some("asc"),
  )
  .unwrap();
  let query = table::ListQuery {
    search: None,
    task: None,
    status: None,
    limit: 100,
    offset: 0,
    table: Some(&options),
  };
  let first = fixture.dashboard.list_table("local", &query).unwrap();
  assert_eq!(first["runs"][0]["table_values"]["metrics"]["loss"], 1.0);
  let revision = table_cache::Cache::revision(&run, true, true).unwrap();
  assert!(
    fixture
      .dashboard
      .table_cache
      .get(&run, true, true, &revision)
      .is_some()
  );
  assert_eq!(
    fixture.dashboard.list_table("local", &query).unwrap(),
    first
  );
  fs::OpenOptions::new()
    .append(true)
    .open(run.join("outputs/metrics.jsonl"))
    .unwrap()
    .write_all(b"{\"step\":2,\"metrics\":{\"loss\":0.25}}\n")
    .unwrap();
  let next = fixture.dashboard.list_table("local", &query).unwrap();
  assert_eq!(next["runs"][0]["table_values"]["metrics"]["loss"], 0.25);
  let replacement = run.join("outputs/new-params.json");
  fs::write(&replacement, json!({"rate":2}).to_string()).unwrap();
  fs::rename(&replacement, &params).unwrap();
  let next = fixture.dashboard.list_table("local", &query).unwrap();
  assert_eq!(next["runs"][0]["table_values"]["params"]["/rate"], 2);
}

#[test]
fn configurable_columns_sort_the_filtered_history_before_pagination() {
  let fixture = Fixture::new();
  for (id, rate, loss) in [
    ("run-a", Some(10), Some(4)),
    ("run-b", Some(2), Some(9)),
    ("run-c", None, None),
  ] {
    let run = fixture.run(id);
    fs::write(
      run.join("outputs/params.json"),
      json!({"optimizer":{"rate":rate},"literal/key":true}).to_string(),
    )
    .unwrap();
    fs::write(
      run.join("outputs/metrics.jsonl"),
      format!("{}\n", json!({"step":0,"metrics":{"loss":loss}})),
    )
    .unwrap();
  }
  let options = table::TableOptions::parse(
    vec!["/optimizer/rate".into(), "/literal~1key".into()],
    vec!["loss".into()],
    "last",
    Some("param:/optimizer/rate"),
    Some("asc"),
  )
  .unwrap();
  let query = table::ListQuery {
    search: Some("TRAIN"),
    task: Some("train"),
    status: Some("completed"),
    limit: 1,
    offset: 1,
    table: Some(&options),
  };
  let page = fixture.dashboard.list_table("local", &query).unwrap();
  assert_eq!(page["total_count"], 3);
  assert_eq!(page["runs"][0]["run_id"], "run-a");
  assert_eq!(
    page["runs"][0]["table_values"]["params"]["/optimizer/rate"],
    10
  );
  assert_eq!(
    page["runs"][0]["table_values"]["params"]["/literal~1key"],
    true
  );
  assert_eq!(page["runs"][0]["table_values"]["metrics"]["loss"], 4.0);
  assert_eq!(page["next_offset"], 2);
  let columns = fixture.dashboard.columns("local").unwrap();
  assert!(
    columns["available_columns"]["params"]
      .as_array()
      .unwrap()
      .iter()
      .any(|column| column["key"] == "/optimizer/rate")
  );
  let options = table::TableOptions::parse(
    vec![],
    vec!["loss".into()],
    "max",
    Some("metric:loss"),
    Some("desc"),
  )
  .unwrap();
  let query = table::ListQuery {
    search: None,
    task: None,
    status: None,
    limit: 3,
    offset: 0,
    table: Some(&options),
  };
  let page = fixture.dashboard.list_table("local", &query).unwrap();
  assert_eq!(
    page["runs"]
      .as_array()
      .unwrap()
      .iter()
      .map(|row| row["run_id"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["run-b", "run-a", "run-c"]
  );
}

#[test]
fn parameter_only_tables_do_not_open_metrics_and_long_values_have_explicit_previews() {
  let fixture = Fixture::new();
  let run = fixture.run("run-a");
  fs::write(
    run.join("outputs/params.json"),
    json!({"name":"雪".repeat(100_000)}).to_string(),
  )
  .unwrap();
  fs::remove_file(run.join("outputs/metrics.jsonl")).unwrap();
  fs::create_dir(run.join("outputs/metrics.jsonl")).unwrap();
  let options = table::TableOptions::parse(
    vec!["/name".into()],
    vec![],
    "last",
    Some("param:/name"),
    Some("asc"),
  )
  .unwrap();
  let page = fixture
    .dashboard
    .list_table(
      "local",
      &table::ListQuery {
        search: None,
        task: None,
        status: None,
        limit: 100,
        offset: 0,
        table: Some(&options),
      },
    )
    .unwrap();
  assert!(page["warnings"].as_array().unwrap().is_empty());
  assert_eq!(page["runs"][0]["table_values_truncated"], true);
  assert!(serde_json::to_vec(&page).unwrap().len() < 2048);
  assert!(
    fixture
      .dashboard
      .list_table(
        "local",
        &table::ListQuery {
          search: None,
          task: None,
          status: None,
          limit: 101,
          offset: 0,
          table: Some(&options)
        }
      )
      .is_err()
  );
  let legacy = fixture
    .dashboard
    .list("local", None, None, None, 1000, 0)
    .unwrap();
  assert!(legacy["runs"][0].get("table_values").is_none());
}

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
      .chart("local", &ids, &vec!["loss".into(); 7], ChartXAxis::Step)
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

#[test]
fn update_probes_read_only_bounded_output_metadata_and_never_parse_contents() {
  let fixture = Fixture::new();
  assert_eq!(
    fixture.dashboard.updates("", &[]).unwrap(),
    json!({"catalog_revision":null,"source_revision":null,"runs":[]})
  );
  let run = fixture.run("run-a");
  fs::write(run.join("run-state.json"), "{").unwrap();
  let metrics = fs::File::create(run.join("outputs/metrics.jsonl")).unwrap();
  metrics.set_len(metrics::METRICS_FILE_LIMIT + 1).unwrap();
  fs::create_dir(run.join("outputs/checkpoints")).unwrap();
  fs::write(run.join("logs/stdout.log"), "before\n").unwrap();
  let ids = vec!["run-a".into(), "deleted".into()];
  let first = fixture.dashboard.updates("local", &ids).unwrap();
  assert_eq!(first["runs"][0]["missing"], false);
  assert!(first["runs"][0]["metrics_revision"].is_string());
  assert_eq!(first["runs"][1]["missing"], true);
  assert_eq!(first, fixture.dashboard.updates("local", &ids).unwrap());

  metrics.set_len(metrics::METRICS_FILE_LIMIT + 2).unwrap();
  let next = fixture.dashboard.updates("local", &ids).unwrap();
  assert_ne!(
    next["runs"][0]["metrics_revision"],
    first["runs"][0]["metrics_revision"]
  );
  assert_ne!(
    next["runs"][0]["metadata_revision"],
    first["runs"][0]["metadata_revision"]
  );
  assert_eq!(
    next["runs"][0]["stdout_revision"],
    first["runs"][0]["stdout_revision"]
  );

  fs::write(run.join("logs/stdout.log"), "before\nafter\n").unwrap();
  let logs = fixture.dashboard.updates("local", &ids).unwrap();
  assert_ne!(
    logs["runs"][0]["stdout_revision"],
    next["runs"][0]["stdout_revision"]
  );
  assert_eq!(
    logs["runs"][0]["metrics_revision"],
    next["runs"][0]["metrics_revision"]
  );
  fs::write(run.join("pull-state.json"), "cached provenance").unwrap();
  let provenance = fixture.dashboard.updates("local", &ids).unwrap();
  assert_ne!(
    provenance["runs"][0]["metadata_revision"],
    logs["runs"][0]["metadata_revision"]
  );

  fs::remove_dir_all(run).unwrap();
  let removed = fixture.dashboard.updates("local", &ids).unwrap();
  assert_eq!(
    removed["runs"][0],
    json!({
      "run_id":"run-a", "metadata_revision":null,"metrics_revision":null,
      "stdout_revision":null,"stderr_revision":null,"missing":true,
    })
  );
}

#[cfg(unix)]
#[test]
fn update_probes_detect_same_size_replacement_and_refuse_linked_paths() {
  use std::os::unix::fs::symlink;

  let fixture = Fixture::new();
  let run = fixture.run("run-a");
  let ids = vec!["run-a".into()];
  fs::write(run.join("run-state.json"), "{").unwrap();
  let first = fixture.dashboard.updates("local", &ids).unwrap();
  let modified = fs::metadata(run.join("run-state.json"))
    .unwrap()
    .modified()
    .unwrap();
  let mut replacement = tempfile::NamedTempFile::new_in(&run).unwrap();
  std::io::Write::write_all(&mut replacement, b"[").unwrap();
  replacement
    .as_file()
    .set_times(fs::FileTimes::new().set_modified(modified))
    .unwrap();
  replacement.persist(run.join("run-state.json")).unwrap();
  let next = fixture.dashboard.updates("local", &ids).unwrap();
  assert_ne!(
    next["runs"][0]["metadata_revision"],
    first["runs"][0]["metadata_revision"]
  );

  // Checkpoint contents are never read, and linked checkpoints are ignored.
  symlink(
    fixture.root.join("expri.toml"),
    run.join("outputs/model.pt"),
  )
  .unwrap();
  fixture.dashboard.updates("local", &ids).unwrap();
  fs::remove_file(run.join("outputs/metrics.jsonl")).unwrap();
  symlink(
    fixture.root.join("expri.toml"),
    run.join("outputs/metrics.jsonl"),
  )
  .unwrap();
  assert!(fixture.dashboard.updates("local", &ids).is_err());
  fs::remove_file(run.join("outputs/metrics.jsonl")).unwrap();
  fs::remove_file(run.join("outputs/model.pt")).unwrap();
  fs::remove_dir(run.join("outputs")).unwrap();
  symlink(&fixture.root, run.join("outputs")).unwrap();
  assert!(fixture.dashboard.updates("local", &ids).is_err());
  symlink(&run, fixture.root.join(".expri/runs/linked")).unwrap();
  assert!(
    fixture
      .dashboard
      .updates("local", &["linked".into()])
      .is_err()
  );
}

#[test]
fn artifact_catalog_merges_reported_outputs_with_real_local_files() {
  let fixture = Fixture::new();
  let run = fixture.run("files");
  let checkpoint = fs::File::create(run.join("outputs/model one.pt")).unwrap();
  checkpoint.set_len(16 * 1024 * 1024 * 1024).unwrap();
  fs::write(run.join("outputs/.env"), "private").unwrap();
  fs::create_dir_all(run.join("code")).unwrap();
  fs::write(run.join("code/train.py"), "private code").unwrap();
  fs::write(run.join(crate::run_artifacts::INVENTORY_PATH), json!({"schema_version":1,"recorded_at":"2026-10-07T01:02:03Z",
    "files":[{"path":"outputs/model one.pt","size":2},{"path":"outputs/worker-only.pt","size":99}],"truncated":false}).to_string()).unwrap();
  let catalog = fixture.dashboard.artifacts("local", "files").unwrap();
  assert_eq!(catalog["files"].as_array().unwrap().len(), 3);
  let rows = catalog["files"].as_array().unwrap();
  let model = rows
    .iter()
    .find(|row| row["path"] == "outputs/model one.pt")
    .unwrap();
  assert_eq!(model["size"], 16 * 1024_u64 * 1024 * 1024);
  assert_eq!(model["local"], true);
  assert_eq!(model["worker"], true);
  assert!(model["cloud"].is_null());
  assert!(
    model["download_url"]
      .as_str()
      .unwrap()
      .starts_with("/api/artifact?source=local&run_id=files&path=")
  );
  let reported = rows
    .iter()
    .find(|row| row["path"] == "outputs/worker-only.pt")
    .unwrap();
  assert_eq!(reported["local"], false);
  assert!(reported["download_url"].is_null());
  assert_eq!(catalog["inventory_recorded_at"], "2026-10-07T01:02:03Z");
  assert!(catalog["pull_scope"].is_null());
  assert!(!catalog["truncated"].as_bool().unwrap());
}

#[test]
fn cached_artifact_catalog_reports_last_cloud_catalog_and_pull_scope() {
  let fixture = Fixture::new();
  let local = fixture.run("cached-files");
  let run = fixture
    .root
    .join("results/service-worker/runs/cached-files");
  fs::create_dir_all(run.join("outputs")).unwrap();
  fs::copy(local.join("run-state.json"), run.join("run-state.json")).unwrap();
  fs::write(run.join("outputs/local.pt"), b"cache").unwrap();
  fs::write(
    run.join("pull-state.json"),
    json!({"schema_version":1,
    "scope":{"project_id":"project","origin":"worker","run_id":"cached-files"},
    "available_files":[{"path":"outputs/local.pt","size":7,"sha256":"a".repeat(64)},
      {"path":"outputs/cloud-only.pt","size":12,"sha256":"b".repeat(64)},
      {"path":"outputs/metrics.jsonl","size":3}],"available_files_truncated":false})
    .to_string(),
  )
  .unwrap();
  let catalog = fixture
    .dashboard
    .artifacts("cached:service-worker", "cached-files")
    .unwrap();
  assert_eq!(catalog["pull_scope"]["project_id"], "project");
  let rows = catalog["files"].as_array().unwrap();
  let local = rows
    .iter()
    .find(|row| row["path"] == "outputs/local.pt")
    .unwrap();
  assert_eq!(local["size"], 5);
  assert_eq!(local["local"], true);
  assert_eq!(local["cloud"], true);
  let cloud = rows
    .iter()
    .find(|row| row["path"] == "outputs/cloud-only.pt")
    .unwrap();
  assert_eq!(cloud["local"], false);
  assert_eq!(cloud["cloud"], true);
  assert!(cloud["download_url"].is_null());
  assert_eq!(
    rows
      .iter()
      .find(|row| row["path"] == "outputs/metrics.jsonl")
      .unwrap()["cloud"],
    false
  );
  assert!(!catalog["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn malformed_worker_inventory_warns_without_hiding_local_files_and_output_changes_refresh() {
  let fixture = Fixture::new();
  let run = fixture.run("safe-files");
  fs::write(
    run.join(crate::run_artifacts::INVENTORY_PATH),
    json!({"schema_version":1,"files":[{"path":"inputs/private.pt","size":1}],"truncated":false})
      .to_string(),
  )
  .unwrap();
  let catalog = fixture.dashboard.artifacts("local", "safe-files").unwrap();
  assert_eq!(catalog["files"].as_array().unwrap().len(), 1);
  assert!(!catalog["warnings"].as_array().unwrap().is_empty());
  let ids = vec!["safe-files".into()];
  let before = fixture.dashboard.updates("local", &ids).unwrap();
  fs::create_dir(run.join("outputs/nested")).unwrap();
  fs::write(run.join("outputs/nested/new.pt"), "output").unwrap();
  let after = fixture.dashboard.updates("local", &ids).unwrap();
  assert_ne!(
    before["runs"][0]["metadata_revision"],
    after["runs"][0]["metadata_revision"]
  );
  assert!(
    fixture
      .dashboard
      .artifacts("local", "../safe-files")
      .is_err()
  );
  for path in [
    "run-state.json",
    "outputs/../expri.toml",
    "inputs/data.pt",
    "outputs/.env",
    "outputs/a\\b",
  ] {
    assert!(
      fixture
        .dashboard
        .artifact_download("local", "safe-files", path)
        .is_err(),
      "{path}"
    );
  }
}

#[cfg(unix)]
#[test]
fn artifact_download_rejects_symlink_files_and_parent_directories() {
  use std::os::unix::fs::symlink;
  let fixture = Fixture::new();
  let run = fixture.run("links");
  let outside = tempfile::tempdir().unwrap();
  fs::write(outside.path().join("secret.pt"), "private").unwrap();
  symlink(
    outside.path().join("secret.pt"),
    run.join("outputs/link.pt"),
  )
  .unwrap();
  symlink(outside.path(), run.join("outputs/linked")).unwrap();
  assert!(
    fixture
      .dashboard
      .artifact_download("local", "links", "outputs/link.pt")
      .is_err()
  );
  assert!(
    fixture
      .dashboard
      .artifact_download("local", "links", "outputs/linked/secret.pt")
      .is_err()
  );
  assert_eq!(
    fixture.dashboard.artifacts("local", "links").unwrap()["files"]
      .as_array()
      .unwrap()
      .len(),
    1
  );
  let file = fs::File::create(run.join("outputs/opened.pt")).unwrap();
  std::io::Write::write_all(&mut &file, b"safe").unwrap();
  let download = fixture
    .dashboard
    .artifact_download("local", "links", "outputs/opened.pt")
    .unwrap();
  fs::rename(run.join("outputs"), run.join("original-outputs")).unwrap();
  symlink(outside.path(), run.join("outputs")).unwrap();
  let artifacts::Download::Local { mut file, .. } = download else {
    panic!("local file expected")
  };
  let mut bytes = Vec::new();
  file.read_to_end(&mut bytes).unwrap();
  assert_eq!(bytes, b"safe");
}
