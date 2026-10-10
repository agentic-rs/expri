use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Parser;
use serde_json::{Value, json};

use crate::runs::query;
use expri_core::protocol::RunQueryRequest;

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

#[test]
fn both_catalog_entrypoints_preserve_publishing_status_and_errors() {
  let (_temporary, root) = fixture();
  let runs_dir = root.join(".expri/runs");
  let directory = run(
    &runs_dir,
    "run-publisher",
    Some(&state("run-publisher", "running", "2026-10-02T12:00:00Z")),
  );
  let request = RunQueryRequest::Show {
    run_id: "run-publisher".into(),
  };
  let pure = expri_runs::runs::query_directory(&runs_dir, &request).unwrap();
  assert!(pure.get("publishing").is_none());
  assert_eq!(query(&root, &request).unwrap(), pure);

  write_json(
    &directory.join("publishing-state.json"),
    &json!({
      "schema_version": 1, "run_id": "run-publisher", "status": "published",
      "updated_at": "2026-10-02T12:00:00Z", "last_success_at": null,
      "last_error": null, "retry_after_seconds": null, "dashboard_url": null,
      "progress": {"archive": {"status": "archived"}}
    }),
  );
  let report = query(&root, &request).unwrap();
  assert_eq!(report["publishing"]["status"], "published");
  assert_eq!(report["publishing"]["worker_active"], false);
  assert_eq!(
    report["publishing"]["progress"]["result_upload"]["status"],
    "uploaded"
  );
  assert_eq!(report, super::query_directory(&runs_dir, &request).unwrap());

  fs::write(directory.join("publishing-state.json"), b"{broken").unwrap();
  let report = query(&root, &request).unwrap();
  assert_eq!(report["run"]["status"], "running");
  assert_eq!(report["publishing"]["status"], "error");
  assert!(
    report["warnings"]
      .as_array()
      .unwrap()
      .iter()
      .any(|warning| {
        warning["run_id"] == "run-publisher"
          && warning["message"] == "Service publishing status could not be read."
      })
  );
  assert_eq!(report, super::query_directory(&runs_dir, &request).unwrap());
  assert!(
    expri_runs::runs::query_directory(&runs_dir, &request)
      .unwrap()
      .get("publishing")
      .is_none()
  );
}

#[test]
fn node_request_options_match_the_query_contract() {
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
