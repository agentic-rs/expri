use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Parser;
use serde_json::{Value, json};

use super::*;

fn fixture() -> (tempfile::TempDir, PathBuf) {
  let directory = tempfile::Builder::new()
    .prefix("expri catalog test ")
    .tempdir()
    .expect("catalog fixture");
  let root = fs::canonicalize(directory.path()).expect("canonical catalog root");
  (directory, root)
}

fn state(run_id: &str, status: &str, started_at: &str) -> Value {
  let mut state = json!({
    "schema_version": 1, "run_id": run_id, "task": "train", "status": status,
    "started_at": started_at, "code_dir": "/remote/project/.expri/runs/remote/code",
    "environment_manifest": "/remote/project/.expri/runs/remote/environment/environment-state.json",
  });
  if matches!(status, "completed" | "failed") {
    state["finished_at"] = json!("2026-10-02T14:00:00Z");
    state["exit_code"] = json!(if status == "completed" { 0 } else { 7 });
  }
  state
}

fn run(runs_dir: &Path, run_id: &str, state: Option<&Value>) -> PathBuf {
  let directory = runs_dir.join(run_id);
  fs::create_dir_all(&directory).expect("run directory");
  if let Some(state) = state {
    write_json(&directory.join("run-state.json"), state);
  }
  directory
}

fn write_json(path: &Path, value: &Value) {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).expect("metadata parent");
  }
  fs::write(path, serde_json::to_vec(value).unwrap()).expect("metadata JSON");
}

fn request_list() -> RunQueryRequest {
  RunQueryRequest::List {
    task: None,
    status: None,
    limit: None,
  }
}

fn python_query(
  root: &Path,
  request: &RunQueryRequest,
  directory: bool,
) -> std::result::Result<Value, String> {
  let script = format!(
    "{CATALOG_SCRIPT}\nimport sys\ntry:\n  value = {}(sys.argv[1], _catalog_json.loads(sys.argv[2]))\n  print(_catalog_json.dumps({{'result': value}}))\nexcept Exception as error:\n  print(_catalog_json.dumps({{'error': str(error)}}))\n",
    if directory {
      "query_runs_directory"
    } else {
      "query_runs"
    },
  );
  let output = Command::new("python3")
    .args(["-I", "-c", &script])
    .arg(root)
    .arg(serde_json::to_string(request).unwrap())
    .output()
    .expect("Python catalog");
  assert!(
    output.status.success(),
    "Python catalog: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  let response: Value = serde_json::from_slice(&output.stdout).expect("Python catalog JSON");
  if let Some(error) = response.get("error").and_then(Value::as_str) {
    Err(error.to_string())
  } else {
    Ok(response["result"].clone())
  }
}

fn parity(root: &Path, request: &RunQueryRequest, directory: bool) -> Value {
  let native = if directory {
    query_directory(root, request)
  } else {
    query(root, request)
  }
  .expect("native catalog query");
  let python = python_query(root, request, directory).expect("Python catalog query");
  assert_eq!(
    native,
    python,
    "request: {}",
    serde_json::to_string(request).unwrap()
  );
  native
}

fn parity_error(root: &Path, request: &RunQueryRequest, directory: bool) -> String {
  let native = if directory {
    query_directory(root, request)
  } else {
    query(root, request)
  }
  .expect_err("native catalog must reject request")
  .to_string();
  let python = python_query(root, request, directory).expect_err("Python must reject request");
  assert_eq!(native, python);
  native
}

fn selected_files(report: &Value) -> BTreeSet<&str> {
  report["files"]
    .as_array()
    .unwrap()
    .iter()
    .map(|value| value.as_str().unwrap())
    .collect()
}

#[test]
fn metric_selection_reads_fixed_files_without_logs_checkpoints_or_environment_contents() {
  let (_directory, root) = fixture();
  let directory = run(
    &root.join(".expri/runs"),
    "run-metrics",
    Some(&state("run-metrics", "completed", "2026-10-02T12:00:00Z")),
  );
  fs::create_dir_all(directory.join("outputs/checkpoints")).unwrap();
  fs::create_dir_all(directory.join("logs")).unwrap();
  fs::write(
    directory.join("outputs/metrics.jsonl"),
    b"{\"step\":0,\"metrics\":{\"loss\":1}}\n",
  )
  .unwrap();
  fs::write(
    directory.join("outputs/params.json"),
    b"{\"learning_rate\":0.01}",
  )
  .unwrap();
  fs::write(
    directory.join("outputs/checkpoints/model.pt"),
    b"heavy checkpoint",
  )
  .unwrap();
  fs::write(directory.join("logs/stdout.log"), b"bulk output").unwrap();
  let report = parity(
    &root,
    &RunQueryRequest::Files {
      run_id: "run-metrics".to_string(),
      artifacts: Vec::new(),
      metrics: true,
    },
    false,
  );
  assert_eq!(
    selected_files(&report),
    BTreeSet::from([
      "run-state.json",
      "outputs/metrics.jsonl",
      "outputs/params.json"
    ])
  );
  assert!(report["warnings"].as_array().unwrap().is_empty());
  let explicit = parity(
    &root,
    &RunQueryRequest::Files {
      run_id: "run-metrics".to_string(),
      artifacts: vec!["outputs/checkpoints/model.pt".to_string()],
      metrics: true,
    },
    false,
  );
  assert!(selected_files(&explicit).contains("outputs/checkpoints/model.pt"));
  assert!(!selected_files(&explicit).contains("logs/stdout.log"));
  fs::remove_file(directory.join("outputs/metrics.jsonl")).unwrap();
  fs::remove_file(directory.join("outputs/params.json")).unwrap();
  let missing = parity(
    &root,
    &RunQueryRequest::Files {
      run_id: "run-metrics".to_string(),
      artifacts: Vec::new(),
      metrics: true,
    },
    false,
  );
  assert_eq!(selected_files(&missing), BTreeSet::from(["run-state.json"]));
  assert_eq!(missing["warnings"].as_array().unwrap().len(), 2);
}

#[test]
fn legacy_file_requests_default_to_logs_and_reject_nonboolean_metric_selection() {
  let request: RunQueryRequest =
    serde_json::from_value(json!({"operation":"files", "run_id":"run-old"})).unwrap();
  assert!(matches!(
    request,
    RunQueryRequest::Files { metrics: false, .. }
  ));
  assert!(
    serde_json::from_value::<RunQueryRequest>(
      json!({"operation":"files", "run_id":"run-old", "metrics":"yes"})
    )
    .is_err()
  );
}

#[test]
fn catalog_lists_other_runs_despite_corrupted_missing_and_future_records() {
  let (_directory, root) = fixture();
  let runs = root.join(".expri/runs");
  run(
    &runs,
    "run-active",
    Some(&state(
      "run-active",
      "running",
      "2026-10-02T12:00:00.000000001Z",
    )),
  );
  let mut legacy = state("run-legacy", "completed", "2026-10-02T12:00:00Z");
  legacy.as_object_mut().unwrap().remove("schema_version");
  run(&runs, "run-legacy", Some(&legacy));
  let corrupt = run(&runs, "run-corrupt", None);
  fs::write(corrupt.join("run-state.json"), "{unfinished").unwrap();
  let deep = run(&runs, "run-deep", None);
  fs::write(
    deep.join("run-state.json"),
    format!("{}unfinished{}", "[".repeat(1500), "]".repeat(1500)),
  )
  .unwrap();
  run(&runs, "run-missing", None);
  let mut future = state("run-future", "completed", "2026-10-02T15:00:00Z");
  future["schema_version"] = json!(5);
  run(&runs, "run-future", Some(&future));
  let mismatch = state("some-other-id", "completed", "2026-10-02T16:00:00Z");
  run(&runs, "run-mismatch", Some(&mismatch));
  run(&runs, "run-array", Some(&json!(["untrusted"])));
  run(
    &runs,
    "run-incomplete",
    Some(&json!({"run_id":"run-incomplete", "schema_version":1})),
  );
  // Inventory data is irrelevant to catalog rows and is never parsed by list.
  fs::create_dir_all(runs.join("run-active/environment")).unwrap();
  fs::write(
    runs.join("run-active/environment/environment-state.json"),
    "not JSON",
  )
  .unwrap();
  let report = parity(&root, &request_list(), false);
  let rows = report["runs"].as_array().unwrap();
  assert_eq!(rows.len(), 9);
  assert_eq!(rows[0]["run_id"], "run-active");
  assert_eq!(rows[1]["run_id"], "run-legacy");
  assert_eq!(rows[1]["schema_version"], 0);
  for row in &rows[2..] {
    assert_eq!(row["status"], "unknown");
    assert!(row["task"].is_null());
  }
  let future = rows
    .iter()
    .find(|row| row["run_id"] == "run-future")
    .unwrap();
  assert_eq!(future["schema_version"], 5);
  assert!(
    report["warnings"]
      .as_array()
      .unwrap()
      .iter()
      .all(|warning| warning["run_id"] != "run-active")
  );
  let filtered = parity(
    &root,
    &RunQueryRequest::List {
      task: Some("train".to_string()),
      status: Some("running".to_string()),
      limit: Some(1),
    },
    false,
  );
  assert_eq!(filtered["runs"].as_array().unwrap().len(), 1);
  assert_eq!(filtered["runs"][0]["run_id"], "run-active");
  assert!(
    parity(
      &root,
      &RunQueryRequest::List {
        task: None,
        status: None,
        limit: Some(0)
      },
      false
    )["runs"]
      .as_array()
      .unwrap()
      .is_empty()
  );
}

#[test]
fn catalog_sorts_offsets_and_nanoseconds_and_warns_on_invalid_timestamps() {
  let (_directory, root) = fixture();
  let runs = root.join(".expri/runs");
  for (id, time) in [
    ("run-offset", "2026-10-02T20:00:00+08:00"),
    ("run-nano1", "2026-10-02T12:00:00.000000001Z"),
    ("run-nano2", "2026-10-02T12:00:00.000000002Z"),
    ("run-invalid", "2026-02-30T12:00:00Z"),
    ("run-zero", "0000-01-01T00:00:00Z"),
    ("run-leap", "2026-10-02T12:00:60Z"),
    ("run-badoffset", "2026-10-02T12:00:00+01:99"),
  ] {
    run(&runs, id, Some(&state(id, "running", time)));
  }
  let report = parity(&root, &request_list(), false);
  let rows = report["runs"].as_array().unwrap();
  assert_eq!(
    rows
      .iter()
      .take(3)
      .map(|row| row["run_id"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["run-nano2", "run-nano1", "run-offset"]
  );
  assert_eq!(report["warnings"].as_array().unwrap().len(), 4);
}

#[test]
fn show_uses_fixed_cached_paths_and_tolerates_pruned_or_missing_detail_records() {
  let (_directory, root) = fixture();
  let runs = root.join("results/gpu/runs");
  let saved = state("run-cached", "failed", "2026-10-02T12:00:00Z");
  let directory = run(&runs, "run-cached", Some(&saved));
  let snapshot = json!({"run_id":"run-cached", "source":{"kind":"git"}, "files":[]});
  let environment = json!({"python":"/remote/conda/python", "base":{"packages":{"torch":"2.8.0"}}});
  write_json(&directory.join("snapshot.json"), &snapshot);
  write_json(
    &directory.join("environment/environment-state.json"),
    &environment,
  );
  let request = RunQueryRequest::Show {
    run_id: "run-cached".to_string(),
  };
  let report = parity(&runs, &request, true);
  assert_eq!(report["state"], saved);
  assert_eq!(report["snapshot"], snapshot);
  assert_eq!(report["environment"], environment);
  assert_eq!(
    report["state"]["code_dir"],
    "/remote/project/.expri/runs/remote/code"
  );
  assert!(!directory.join("environment/.venv").exists());
  fs::remove_file(directory.join("snapshot.json")).unwrap();
  fs::remove_dir_all(directory.join("environment")).unwrap();
  let missing = parity(&runs, &request, true);
  assert_eq!(missing["run"]["status"], "failed");
  assert!(missing["snapshot"].is_null());
  assert!(missing["environment"].is_null());
  assert_eq!(missing["warnings"].as_array().unwrap().len(), 2);
}

#[test]
fn catalog_bounds_list_state_reads_but_allows_larger_explicit_manifests() {
  let (_directory, root) = fixture();
  let runs = root.join(".expri/runs");
  let good = run(
    &runs,
    "run-good",
    Some(&state("run-good", "running", "2026-10-02T12:00:00Z")),
  );
  write_json(
    &good.join("environment/environment-state.json"),
    &json!({"inventory":"x".repeat(METADATA_LIMIT as usize + 20)}),
  );
  let huge = run(&runs, "run-huge", None);
  fs::File::create(huge.join("run-state.json"))
    .unwrap()
    .set_len(METADATA_LIMIT + 1)
    .unwrap();
  let report = parity(&root, &request_list(), false);
  assert_eq!(report["runs"][0]["run_id"], "run-good");
  assert_eq!(report["runs"][1]["status"], "unknown");
  assert_eq!(
    report["warnings"][0]["message"],
    "run-state.json exceeds the metadata size limit"
  );
  let detail = parity(
    &root,
    &RunQueryRequest::Show {
      run_id: "run-good".to_string(),
    },
    false,
  );
  assert_eq!(
    detail["environment"]["inventory"].as_str().unwrap().len(),
    METADATA_LIMIT as usize + 20
  );
  fs::File::create(good.join("environment/environment-state.json"))
    .unwrap()
    .set_len(DETAIL_LIMIT + 1)
    .unwrap();
  let too_large = parity(
    &root,
    &RunQueryRequest::Show {
      run_id: "run-good".to_string(),
    },
    false,
  );
  assert!(too_large["environment"].is_null());
}

#[test]
fn files_select_metadata_logs_and_explicit_artifacts_without_environments_or_caches() {
  let (_directory, root) = fixture();
  let runs = root.join(".expri/runs");
  let directory = run(
    &runs,
    "run-files",
    Some(&state("run-files", "completed", "2026-10-02T12:00:00Z")),
  );
  write_json(&directory.join("snapshot.json"), &json!({"files":[]}));
  write_json(
    &directory.join("environment/environment-state.json"),
    &json!({"base":{}}),
  );
  for relative in [
    "logs/stdout.log",
    "logs/stderr.log",
    "logs/steps/build.log",
    "outputs/checkpoint.bin",
    "code/out/result.csv",
    "code/train.py",
    "code/__pycache__/train.pyc",
    "code/.venv/large.bin",
    "outputs/cache/duplicate.bin",
    "outputs/.git/config",
    "code/cache",
  ] {
    let path = directory.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "not read by selection").unwrap();
  }
  let default = parity(
    &root,
    &RunQueryRequest::Files {
      run_id: "run-files".to_string(),
      artifacts: Vec::new(),
      metrics: false,
    },
    false,
  );
  assert_eq!(
    selected_files(&default),
    BTreeSet::from([
      "run-state.json",
      "snapshot.json",
      "environment/environment-state.json",
      "logs/stdout.log",
      "logs/stderr.log",
      "logs/steps/build.log"
    ])
  );
  let selected = parity(
    &root,
    &RunQueryRequest::Files {
      run_id: "run-files".to_string(),
      artifacts: vec!["outputs/checkpoint.bin".to_string(), "code/out".to_string()],
      metrics: false,
    },
    false,
  );
  assert!(selected_files(&selected).contains("outputs/checkpoint.bin"));
  assert!(selected_files(&selected).contains("code/out/result.csv"));
  assert!(!selected_files(&selected).contains("code/train.py"));
  let broad = parity(
    &root,
    &RunQueryRequest::Files {
      run_id: "run-files".to_string(),
      artifacts: vec!["outputs/".to_string(), "code".to_string()],
      metrics: false,
    },
    false,
  );
  assert!(selected_files(&broad).contains("code/train.py"));
  assert_eq!(broad["warnings"].as_array().unwrap().len(), 5);
  assert!(selected_files(&broad).iter().all(|path| {
    !path
      .split('/')
      .any(|part| EXCLUDED_COMPONENTS.contains(&part))
  }));
}

#[cfg(unix)]
#[test]
fn catalog_rejects_symlink_boundaries_and_selected_artifact_links() {
  use std::os::unix::fs::symlink;
  let (_directory, root) = fixture();
  let runs = root.join(".expri/runs");
  let directory = run(
    &runs,
    "run-links",
    Some(&state("run-links", "running", "2026-10-02T12:00:00Z")),
  );
  let outside = root.join("outside");
  fs::create_dir(&outside).unwrap();
  write_json(
    &outside.join("secret.json"),
    &json!({"secret":"must not read"}),
  );
  fs::create_dir(directory.join("logs")).unwrap();
  fs::create_dir(directory.join("outputs")).unwrap();
  symlink(&outside, directory.join("code")).unwrap();
  symlink(
    outside.join("secret.json"),
    directory.join("outputs/link.json"),
  )
  .unwrap();
  symlink(&outside, directory.join("outputs/nested")).unwrap();
  symlink(outside.join("secret.json"), directory.join("logs/link.log")).unwrap();
  symlink(outside.join("secret.json"), directory.join("snapshot.json")).unwrap();
  symlink(&directory, runs.join("run-alias")).unwrap();
  let listed = parity(&root, &request_list(), false);
  assert_eq!(listed["runs"].as_array().unwrap().len(), 1);
  assert!(
    listed["warnings"]
      .as_array()
      .unwrap()
      .iter()
      .any(|warning| warning["run_id"] == "run-alias")
  );
  let shown = parity(
    &root,
    &RunQueryRequest::Show {
      run_id: "run-links".to_string(),
    },
    false,
  );
  assert!(shown["snapshot"].is_null());
  let default = parity(
    &root,
    &RunQueryRequest::Files {
      run_id: "run-links".to_string(),
      artifacts: Vec::new(),
      metrics: false,
    },
    false,
  );
  assert!(!selected_files(&default).contains("logs/link.log"));
  assert!(!selected_files(&default).contains("snapshot.json"));
  for artifact in ["outputs/link.json", "outputs", "code/result.txt"] {
    parity_error(
      &root,
      &RunQueryRequest::Files {
        run_id: "run-links".to_string(),
        artifacts: vec![artifact.to_string()],
        metrics: false,
      },
      false,
    );
  }
  parity_error(
    &root,
    &RunQueryRequest::Show {
      run_id: "run-alias".to_string(),
    },
    false,
  );
  fs::rename(root.join(".expri"), root.join("real-state")).unwrap();
  symlink(root.join("real-state"), root.join(".expri")).unwrap();
  assert!(parity_error(&root, &request_list(), false).contains("unsafe parent"));
}

#[test]
fn requests_reject_unsafe_ids_filters_and_artifact_selectors() {
  let (_directory, root) = fixture();
  for id in [
    "",
    ".",
    "..",
    "../run",
    "/tmp/run",
    "run/child",
    "run\\child",
    "run\nchild",
  ] {
    assert!(
      parity_error(
        &root,
        &RunQueryRequest::Show {
          run_id: id.to_string()
        },
        false
      )
      .contains("invalid run ID")
    );
  }
  for artifact in [
    "../outputs",
    "/outputs/a",
    "logs/stdout.log",
    "environment/.venv",
    "outputs//a",
    "outputs/../a",
    "code/.venv/python",
    "outputs/cache",
    "code/.expri/state",
    "outputs/a\nb",
    "code\\a",
  ] {
    assert!(
      parity_error(
        &root,
        &RunQueryRequest::Files {
          run_id: "run-safe".to_string(),
          artifacts: vec![artifact.to_string()],
          metrics: false,
        },
        false
      )
      .contains("invalid artifact path")
    );
  }
  parity_error(
    &root,
    &RunQueryRequest::List {
      task: Some(" ".to_string()),
      status: None,
      limit: None,
    },
    false,
  );
  parity_error(
    &root,
    &RunQueryRequest::List {
      task: None,
      status: Some("succeeded".to_string()),
      limit: None,
    },
    false,
  );
  assert!(
    parity(&root, &request_list(), false)["runs"]
      .as_array()
      .unwrap()
      .is_empty()
  );
  assert!(!root.join(".expri").exists());
}

#[test]
fn inspection_does_not_require_a_lease_or_mutate_active_records() {
  let (_directory, root) = fixture();
  let directory = run(
    &root.join(".expri/runs"),
    "run-active",
    Some(&state("run-active", "preparing", "2026-10-02T12:00:00Z")),
  );
  let lease = crate::lock::run_lock(&directory).unwrap();
  let before = fs::read(directory.join("run-state.json")).unwrap();
  let modified = fs::metadata(directory.join("run-state.json"))
    .unwrap()
    .modified()
    .unwrap();
  parity(&root, &request_list(), false);
  parity(
    &root,
    &RunQueryRequest::Show {
      run_id: "run-active".to_string(),
    },
    false,
  );
  parity(
    &root,
    &RunQueryRequest::Files {
      run_id: "run-active".to_string(),
      artifacts: Vec::new(),
      metrics: false,
    },
    false,
  );
  assert_eq!(fs::read(directory.join("run-state.json")).unwrap(), before);
  assert_eq!(
    fs::metadata(directory.join("run-state.json"))
      .unwrap()
      .modified()
      .unwrap(),
    modified
  );
  assert!(!root.join(".expri/worktree.lock").exists());
  assert!(!directory.join("environment").exists());
  assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
  drop(lease);
}

#[test]
fn catalogs_accept_cancelled_and_lost_without_live_probes_in_cached_queries() {
  let (_directory, root) = fixture();
  let cached = root.join("results/gpu/runs");
  for status in ["cancelled", "lost", "running"] {
    let id = format!("run-{status}");
    let mut saved = state(&id, status, "2026-10-03T00:00:00Z");
    saved["detached"] = json!(true);
    if status == "cancelled" {
      saved["finished_at"] = json!("2026-10-03T00:01:00Z");
      saved["exit_code"] = json!(143);
    }
    run(&cached, &id, Some(&saved));
    let report = parity(
      &cached,
      &RunQueryRequest::List {
        task: None,
        status: Some(status.to_string()),
        limit: None,
      },
      true,
    );
    assert_eq!(report["runs"].as_array().unwrap().len(), 1);
    assert_eq!(report["runs"][0]["status"], status);
    assert!(report["warnings"].as_array().unwrap().is_empty());
    assert!(!cached.join(id).join(".run.lock").exists());
  }
}

#[cfg(unix)]
#[test]
fn normal_repository_aliases_work_without_allowing_catalog_boundary_symlinks() {
  use std::os::unix::fs::symlink;
  let (_directory, root) = fixture();
  let actual = root.join("actual-repo");
  fs::create_dir(&actual).unwrap();
  run(
    &actual.join(".expri/runs"),
    "run-alias",
    Some(&state("run-alias", "running", "2026-10-02T12:00:00Z")),
  );
  let alias = root.join("repo-alias");
  symlink(&actual, &alias).unwrap();
  assert_eq!(
    parity(&alias, &request_list(), false)["runs"][0]["run_id"],
    "run-alias"
  );
  let cached_alias = alias.join(".expri/runs");
  parity(
    &cached_alias,
    &RunQueryRequest::Files {
      run_id: "run-alias".to_string(),
      artifacts: Vec::new(),
      metrics: false,
    },
    true,
  );
  fs::rename(
    actual.join(".expri/runs"),
    actual.join(".expri/actual-runs"),
  )
  .unwrap();
  symlink(
    actual.join(".expri/actual-runs"),
    actual.join(".expri/runs"),
  )
  .unwrap();
  assert!(parity_error(&alias, &request_list(), false).contains("must be a real directory"));
}

#[test]
fn protocol_defaults_and_node_request_options_match_the_query_contract() {
  let list: RunQueryRequest = serde_json::from_value(json!({"operation":"list"})).unwrap();
  assert_eq!(
    serde_json::to_value(list).unwrap(),
    json!({"operation":"list", "task":null, "status":null, "limit":null})
  );
  let files: RunQueryRequest =
    serde_json::from_value(json!({"operation":"files", "run_id":"run-a"})).unwrap();
  assert_eq!(
    serde_json::to_value(files).unwrap(),
    json!({"operation":"files", "run_id":"run-a", "artifacts":[]})
  );
  let metric_files: RunQueryRequest =
    serde_json::from_value(json!({"operation":"files", "run_id":"run-a", "metrics":true})).unwrap();
  assert_eq!(
    serde_json::to_value(metric_files).unwrap(),
    json!({"operation":"files", "run_id":"run-a", "artifacts":[], "metrics":true}),
  );
  assert!(crate::Cli::try_parse_from(["expri", "node", "runs"]).is_err());
  assert!(
    crate::Cli::try_parse_from(["expri", "node", "runs", "--request", "a", "--request-stdin"])
      .is_err()
  );
  let cli = crate::Cli::try_parse_from(["expri", "node", "runs", "--request-stdin"]).unwrap();
  assert!(matches!(
    cli.command,
    crate::Command::Node {
      command: crate::node::cli::NodeCommand::Runs(_)
    }
  ));
  crate::node::cli::run(crate::node::cli::NodeCommand::Capabilities(
    crate::node::cli::CapabilitiesCommand {
      has: Some(crate::node::cli::RUN_RECORDS_CAPABILITY.to_string()),
    },
  ))
  .expect("run records capability");
}

#[test]
fn node_runs_child() {
  let Some(mode) = std::env::var_os("EXPRI_TEST_RUN_QUERY_MODE") else {
    return;
  };
  let mut args = vec![
    std::ffi::OsString::from("expri"),
    "node".into(),
    "runs".into(),
  ];
  if mode == "stdin" {
    args.push("--request-stdin".into());
  } else {
    args.push("--request".into());
    args.push(std::env::var_os("EXPRI_TEST_RUN_QUERY_FILE").unwrap());
  }
  let cli = crate::Cli::try_parse_from(args).unwrap();
  let crate::Command::Node { command } = cli.command else {
    panic!("node command");
  };
  crate::node::cli::run(command).expect("node catalog query");
}

#[test]
fn node_runs_prints_json_from_file_and_stdin_requests() {
  let (_directory, root) = fixture();
  run(
    &root.join(".expri/runs"),
    "run-node",
    Some(&state("run-node", "running", "2026-10-02T12:00:00Z")),
  );
  let request = serde_json::to_vec(&request_list()).unwrap();
  let request_file = root.join("query.json");
  fs::write(&request_file, &request).unwrap();
  for mode in ["file", "stdin"] {
    let mut child = Command::new(std::env::current_exe().unwrap())
      .args(["--exact", "runs::tests::node_runs_child", "--nocapture"])
      .current_dir(&root)
      .env("EXPRI_TEST_RUN_QUERY_MODE", mode)
      .env("EXPRI_TEST_RUN_QUERY_FILE", &request_file)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    if mode == "stdin" {
      child.stdin.as_mut().unwrap().write_all(&request).unwrap();
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
      output.status.success(),
      "node query: {}",
      String::from_utf8_lossy(&output.stderr)
    );
    let start = output
      .stdout
      .iter()
      .position(|byte| *byte == b'{')
      .expect("JSON output");
    let report = serde_json::Deserializer::from_slice(&output.stdout[start..])
      .into_iter::<Value>()
      .next()
      .unwrap()
      .unwrap();
    assert_eq!(report, query(&root, &request_list()).unwrap());
  }
}
