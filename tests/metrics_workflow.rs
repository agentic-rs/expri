#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

struct Fixture {
  _root: tempfile::TempDir,
  local: PathBuf,
  remote: PathBuf,
  ctl: PathBuf,
}

impl Fixture {
  fn new(protocol: &str, old_node: bool) -> Self {
    let root = tempfile::Builder::new()
      .prefix("expri metrics 'quoted' ")
      .tempdir()
      .unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let local = base.join("local project");
    let remote = base.join("remote project");
    let home = base.join("login home");
    let ctl = base.join("ctl bridge");
    fs::create_dir_all(&local).unwrap();
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join(".profile"), "echo login-banner\n").unwrap();
    executable(
      &ctl,
      &format!(
        r#"#!/bin/sh
HOME={home}; export HOME
[ "$1" = ssh ] || exit 91
shift
[ "$1" = -q ] && shift
[ "$1" = -- ] && shift
[ "$1" = expri-metrics-host ] || exit 92
shift
exec /bin/sh -c "$*"
"#,
        home = quote(&home)
      ),
    );
    let node = if old_node {
      let old = base.join("old expri");
      executable(
        &old,
        "#!/bin/sh\n[ \"$2\" = capabilities ] && [ \"$4\" = run-records-v1 ]\n",
      );
      old
    } else {
      PathBuf::from(env!("CARGO_BIN_EXE_expri"))
    };
    fs::write(local.join("expri.toml"), format!(
      "[download]\nresults_dir='fetched results'\n[target.gpu]\nhost='expri-metrics-host'\ntransport='ctl'\nremote_dir={}\nctl_bin={}\nnode_bin={}\nprotocol={}\n",
      serde_json::to_string(&remote.to_string_lossy()).unwrap(),
      serde_json::to_string(&ctl.to_string_lossy()).unwrap(),
      serde_json::to_string(&node.to_string_lossy()).unwrap(),
      serde_json::to_string(protocol).unwrap(),
    )).unwrap();
    for (id, loss, rate) in [("run-one", 0.25, 0.01), ("run-two", 0.5, 0.02)] {
      let run = remote.join(".expri/runs").join(id);
      fs::create_dir_all(run.join("outputs/checkpoints")).unwrap();
      fs::create_dir_all(run.join("logs")).unwrap();
      fs::write(run.join("run-state.json"), serde_json::to_vec(&json!({
        "schema_version": 1, "run_id": id, "task": "train", "status": "completed",
        "started_at": "2026-10-03T00:00:00Z", "finished_at": "2026-10-03T00:01:00Z", "exit_code": 0,
      })).unwrap()).unwrap();
      fs::write(
        run.join("outputs/metrics.jsonl"),
        format!(
          "{}\n{}\n",
          json!({"schema_version":1,"step":0,"metrics":{"loss":2,"accuracy":0.25}}),
          json!({"schema_version":1,"step":1,"metrics":{"loss":loss,"accuracy":0.75}}),
        ),
      )
      .unwrap();
      fs::write(
        run.join("outputs/params.json"),
        serde_json::to_vec(&json!({"schema_version":1,"params":{"learning_rate":rate}})).unwrap(),
      )
      .unwrap();
      fs::write(
        run.join("outputs/checkpoints/model.pt"),
        b"checkpoint must stay remote",
      )
      .unwrap();
      fs::write(run.join("logs/stdout.log"), b"bulk training diagnostics").unwrap();
    }
    Self {
      _root: root,
      local,
      remote,
      ctl,
    }
  }

  fn command(&self, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_expri"));
    command
      .current_dir(&self.local)
      .args(["-T", "gpu"])
      .args(args);
    command
  }

  fn run(&self, args: &[&str]) -> Output {
    let output = self.command(args).output().unwrap();
    assert!(
      output.status.success(),
      "{}",
      String::from_utf8_lossy(&output.stderr)
    );
    output
  }

  fn json(&self, args: &[&str]) -> Value {
    serde_json::from_slice(&self.run(args).stdout).unwrap()
  }

  fn cache(&self, id: &str) -> PathBuf {
    self.local.join("fetched results/gpu/runs").join(id)
  }
}

fn executable(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap();
  fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn quote(path: &Path) -> String {
  format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn rsync_available() -> bool {
  Command::new("rsync")
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|status| status.success())
}

#[test]
fn remote_metrics_compare_use_selective_fetches_and_remain_available_offline() {
  if !rsync_available() {
    eprintln!("skipping metrics transfer test: rsync unavailable");
    return;
  }
  for (protocol, old_node) in [("expri-node", false), ("python", false), ("auto", true)] {
    let fixture = Fixture::new(protocol, old_node);
    let preview = fixture.json(&[
      "runs",
      "fetch",
      "run-one",
      "--metrics",
      "--dry-run",
      "--json",
    ]);
    let files = preview["files"].as_array().unwrap();
    assert!(files.contains(&json!("outputs/metrics.jsonl")));
    assert!(files.contains(&json!("outputs/params.json")));
    assert!(!files.contains(&json!("logs/stdout.log")));
    assert!(!fixture.cache("run-one").exists());
    let metrics = fixture.json(&["runs", "metrics", "run-one", "--json"]);
    assert_eq!(metrics["metrics"]["loss"]["summary"]["last"]["value"], 0.25);
    assert_eq!(metrics["params"]["learning_rate"], 0.01);
    let cache = fixture.cache("run-one");
    assert!(cache.join("outputs/metrics.jsonl").is_file());
    assert!(!cache.join("logs").exists());
    assert!(!cache.join("outputs/checkpoints").exists());
    let comparison = fixture.json(&[
      "runs",
      "compare",
      "run-one",
      "run-two",
      "--metric",
      "loss",
      "--reduction",
      "min",
      "--chart",
      "comparison.html",
      "--json",
    ]);
    assert_eq!(comparison["runs"][0]["values"]["loss"]["value"], 0.25);
    assert_eq!(comparison["runs"][1]["values"]["loss"]["value"], 0.5);
    assert!(fixture.local.join("comparison.html").is_file());
    assert_eq!(comparison["metric_names"], json!(["loss"]));

    // Online inspection must not report an older retained file as freshly fetched.
    let remote_run = fixture.remote.join(".expri/runs/run-one/outputs");
    fs::remove_file(remote_run.join("metrics.jsonl")).unwrap();
    fs::remove_file(remote_run.join("params.json")).unwrap();
    let missing = fixture.json(&["runs", "metrics", "run-one", "--json"]);
    assert_eq!(missing["metrics"], json!({}));
    assert_eq!(missing["params"], Value::Null);
    assert!(
      missing["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["message"]
          .as_str()
          .unwrap()
          .contains("outputs/metrics.jsonl is missing"))
    );
    assert!(cache.join("outputs/metrics.jsonl").is_file());

    fs::remove_file(&fixture.ctl).unwrap();
    // An offline cache label does not require any configured target credentials.
    fs::write(
      fixture.local.join("expri.toml"),
      "[download]\nresults_dir='fetched results'\n",
    )
    .unwrap();
    let cached = fixture.json(&[
      "runs", "metrics", "run-one", "--cached", "--metric", "loss", "--json",
    ]);
    assert_eq!(cached["metrics"]["loss"]["summary"]["last"]["value"], 0.25);
    assert!(cached["metrics"].get("accuracy").is_none());
    let offline = fixture.json(&[
      "runs", "compare", "run-one", "run-two", "--cached", "--metric", "loss", "--json",
    ]);
    assert_eq!(offline["runs"][1]["values"]["loss"]["value"], 0.5);
  }
}

#[test]
fn continued_run_metrics_fetch_current_attempt_parameters_and_do_not_reuse_missing_remote_values() {
  if !rsync_available() {
    return;
  }
  for (protocol, old_node) in [("expri-node", false), ("python", false), ("auto", true)] {
    let fixture = Fixture::new(protocol, old_node);
    let run = fixture.remote.join(".expri/runs/run-one");
    let mut state: Value =
      serde_json::from_slice(&fs::read(run.join("run-state.json")).unwrap()).unwrap();
    state["task_attempts"] = json!({"protocol_version":1,"current_attempt":2,"attempts":[{"attempt":1},{"attempt":2}],"total_count":2,"truncated":false});
    fs::write(
      run.join("run-state.json"),
      serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    fs::create_dir_all(run.join("outputs/attempts/2")).unwrap();
    fs::write(
      run.join("outputs/attempts/2/params.json"),
      br#"{"schema_version":1,"params":{"learning_rate":0.3,"max_steps":300}}"#,
    )
    .unwrap();
    let preview = fixture.json(&[
      "runs",
      "fetch",
      "run-one",
      "--metrics",
      "--dry-run",
      "--json",
    ]);
    assert!(
      preview["files"]
        .as_array()
        .unwrap()
        .contains(&json!("outputs/attempts/2/params.json"))
    );
    assert!(
      !preview["files"]
        .as_array()
        .unwrap()
        .contains(&json!("outputs/params.json"))
    );
    let metrics = fixture.json(&["runs", "metrics", "run-one", "--json"]);
    assert_eq!(metrics["params"]["max_steps"], 300);
    let cached = fixture.json(&["runs", "metrics", "run-one", "--cached", "--json"]);
    assert_eq!(cached["params"]["learning_rate"], 0.3);
    fs::remove_file(run.join("outputs/attempts/2/params.json")).unwrap();
    let missing = fixture.json(&["runs", "metrics", "run-one", "--json"]);
    assert!(missing["params"].is_null());
    assert!(
      fixture
        .cache("run-one")
        .join("outputs/attempts/2/params.json")
        .exists()
    );
  }
}

#[test]
fn forced_older_nodes_refuse_metric_selection_before_creating_a_cache() {
  let fixture = Fixture::new("expri-node", true);
  let output = fixture
    .command(&["runs", "metrics", "run-one", "--json"])
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("run-metrics-v1"));
  assert!(!fixture.cache("run-one").exists());
}

#[cfg(unix)]
mod python_producer {
  use expri_runs::metrics::read;
  use serde_json::json;
  use std::fs;
  use std::path::PathBuf;

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

  #[cfg(unix)]
  #[test]
  fn vendored_python_producer_is_readable_without_site_packages_or_pythonpath() {
    let fixture = Fixture::new();
    let run = fixture.run("run-python", b"");
    let code = run.join("code");
    fs::create_dir(&code).unwrap();
    fs::write(
      code.join("expri_metrics.py"),
      expri_worker::METRICS_PRODUCER_SCRIPT,
    )
    .unwrap();
    fs::write(
      code.join("train.py"),
      r#"from expri_metrics import MetricsLogger
with MetricsLogger() as logger:
  logger.params({"learning_rate": 0.01, "model": {"width": 64}, "seed": 42})
  logger.log(0, {"train/loss": 4, "train/accuracy": 0.25}, step_mode="global")
  logger.log(18446744073709551615, {"train/loss": 0.5}, step_mode="global")
  "#,
    )
    .unwrap();
    let output = std::process::Command::new("python3")
      .args(["-B", "-S", "train.py"])
      .current_dir(&code)
      .env_remove("PYTHONPATH")
      .env_remove("EXPRI_RUN_DIR")
      .env_remove("EXPRI_RUN_ID")
      .env_remove("EXPRI_ATTEMPT")
      .env_remove("EXPRI_STEP_OFFSET")
      .env_remove("EXPRI_RESUME_CHECKPOINT")
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
  fn virtual_resume_appends_relative_and_global_steps_to_source_agnostic_history() {
    let fixture = Fixture::new();
    let prefix =
      b"{\"step\":1000,\"metrics\":{\"loss\":9}}\n{\"step\":7,\"metrics\":{\"loss\":8}}\n";
    let run = fixture.run("run-virtual", prefix);
    let original_state = fs::read(run.join("run-state.json")).unwrap();
    let code = run.join("code");
    fs::create_dir(&code).unwrap();
    fs::write(
      code.join("expri_metrics.py"),
      expri_worker::METRICS_PRODUCER_SCRIPT,
    )
    .unwrap();
    fs::write(code.join("resume.py"), r#"import json, os
from expri_metrics import MetricsLogger
assert not any(name in os.environ for name in ('EXPRI_RUN_DIR', 'EXPRI_RUN_ID', 'EXPRI_ATTEMPT', 'EXPRI_STEP_OFFSET', 'EXPRI_RESUME_CHECKPOINT'))
with MetricsLogger() as logger:
  initial_step = logger.initial_step
  logger.params({'history_source': 'external'})
  logger.log(3, {'loss': 7})
  logger.log(7, {'loss': 6})
  logger.log(42, {'loss': 5}, step_mode='global')
  logger.log(9, {'loss': 4})
  print(json.dumps({'initial_step': initial_step, 'unchanged_initial_step': logger.initial_step}))
"#).unwrap();
    let output = std::process::Command::new("python3")
      .args(["-B", "-S", "resume.py"])
      .current_dir(&code)
      .env_remove("PYTHONPATH")
      .env_remove("EXPRI_RUN_DIR")
      .env_remove("EXPRI_RUN_ID")
      .env_remove("EXPRI_ATTEMPT")
      .env_remove("EXPRI_STEP_OFFSET")
      .env_remove("EXPRI_RESUME_CHECKPOINT")
      .env("EXPRI_OUTPUT_DIR", run.join("outputs"))
      .output()
      .unwrap();
    assert!(
      output.status.success(),
      "{}",
      String::from_utf8_lossy(&output.stderr)
    );
    let initialized: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(initialized["initial_step"], 1001);
    assert_eq!(initialized["unchanged_initial_step"], 1001);
    let bytes = fs::read(run.join("outputs/metrics.jsonl")).unwrap();
    assert!(
      bytes.starts_with(prefix),
      "virtual resume rewrote imported history"
    );
    let rows: Vec<serde_json::Value> = bytes
      .split(|byte| *byte == b'\n')
      .filter(|row| !row.is_empty())
      .map(|row| serde_json::from_slice(row).unwrap())
      .collect();
    assert_eq!(
      rows
        .iter()
        .map(|row| row["step"].as_u64().unwrap())
        .collect::<Vec<_>>(),
      [1000, 7, 1004, 1008, 42, 1010]
    );
    assert_eq!(rows[2]["local_step"], 3);
    assert_eq!(rows[3]["local_step"], 7);
    assert_eq!(rows[4]["step_mode"], "global");
    assert!(rows[4].get("local_step").is_none());
    assert!(rows[2..].iter().all(|row| row.get("attempt").is_none()));
    let metrics = read(&fixture.runs, "run-virtual", &[]).unwrap();
    assert_eq!(metrics.warnings.len(), 1, "{:?}", metrics.warnings);
    assert!(
      metrics.warnings[0]["message"]
        .as_str()
        .is_some_and(|message| message.contains("steps decreased; last values use recorded order")),
      "{:?}",
      metrics.warnings
    );
    assert_eq!(metrics.metrics["loss"].summary.count, 6);
    assert_eq!(metrics.metrics["loss"].summary.last.step, 1010);
    assert_eq!(metrics.metrics["loss"].summary.last.value, 4.0);
    assert_eq!(
      metrics.metrics["loss"]
        .points
        .iter()
        .map(|point| point.step)
        .collect::<Vec<_>>(),
      [1000, 7, 1004, 1008, 42, 1010]
    );
    assert!(!run.join("outputs/checkpoint.bin").exists());
    assert!(!run.join("outputs/attempts").exists());
    assert_eq!(
      fs::read(run.join("run-state.json")).unwrap(),
      original_state
    );
  }
}
