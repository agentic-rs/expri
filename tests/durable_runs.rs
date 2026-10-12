#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const UV: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
args = sys.argv[1:]
if args[:2] == ['run', '--isolated']:
  request = json.loads(args[-1])
  root = pathlib.Path(request['state_dir'])
  (root / 'helper-started').write_text('ready')
  if os.environ.get('EXPRI_TEST_PREPARE_BLOCK'):
    time.sleep(60)
  if os.environ.get('EXPRI_TEST_PREPARE_FAIL'):
    raise SystemExit(43)
  venv = root / 'environment' / '.venv'
  venv.mkdir(parents=True)
  manifest = venv.parent / 'environment-state.json'
  manifest.write_text('{}')
  print(json.dumps({'environment_path': str(venv), 'python': sys.executable,
                   'manifest_path': str(manifest), 'run_env': {'PATH': os.environ['PATH']}}))
elif args[:4] == ['run', '--no-sync', '--no-env-file', '--']:
  os.execvp(args[4], args[4:])
else:
  raise SystemExit(71)
"#;

const TASK: &str = r#"import os, pathlib, signal, subprocess, sys, time
out = pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'])
(out / 'started').write_text(str(pathlib.Path.cwd()))
os.write(1, b'first\x00\xff\n')
os.write(2, b'error\x80\n')
mode = sys.argv[1]
if mode == 'success':
  (out / 'model.pt').write_bytes(b'checkpoint')
  raise SystemExit(0)
if mode == 'finish':
  time.sleep(1)
  (out / 'model.pt').write_bytes(b'checkpoint')
  raise SystemExit(7)
if mode == 'graceful':
  time.sleep(60)
if mode in ('cancel', 'orphan'):
  if mode == 'cancel':
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
  script = "import pathlib,signal,sys,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); p=pathlib.Path(sys.argv[1]); exec('while True:\\n  p.write_text(str(time.time_ns()))\\n  time.sleep(0.025)')"
  subprocess.Popen([sys.executable, '-c', script, str(out / 'heartbeat')])
  if mode == 'orphan':
    while not (out / 'heartbeat').exists():
      time.sleep(0.01)
    (out / 'direct-exit').touch()
    raise SystemExit(0)
  while True:
    time.sleep(1)
"#;

struct Fixture {
  _root: tempfile::TempDir,
  repo: PathBuf,
  bin: PathBuf,
}

impl Fixture {
  fn new() -> Self {
    let root = tempfile::Builder::new()
      .prefix("expri durable 'quoted' ")
      .tempdir()
      .unwrap();
    let repo = fs::canonicalize(root.path()).unwrap().join("repo");
    let bin = root.path().join("bin");
    fs::create_dir_all(repo.join(".expri")).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("uv"), UV).unwrap();
    fs::set_permissions(bin.join("uv"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(repo.join("train.py"), TASK).unwrap();
    fs::write(
      repo.join("pyproject.toml"),
      "[project]\nname='demo'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(repo.join("uv.lock"), "version=1\n").unwrap();
    fs::write(
      repo.join(".expri/checkout.manifest"),
      "train.py\npyproject.toml\nuv.lock\n",
    )
    .unwrap();
    fs::write(
      repo.join("expri.toml"),
      "[environment]\n[tasks]\ntrain=['python3', 'train.py']\n",
    )
    .unwrap();
    Self {
      _root: root,
      repo,
      bin,
    }
  }

  fn command(&self) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_expri"));
    command.current_dir(&self.repo).env(
      "PATH",
      format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
    );
    command
  }

  fn enable_service_with_missing_client(&self) {
    let config_path = self.repo.join("expri.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
      "\n[service]\nclient_config = {:?}\nproject_id = 'demo'\norigin = 'worker'\ndashboard_url = 'https://dashboard.example.net'\n",
      self.repo.parent().unwrap().join("missing-client.toml").to_string_lossy()
    ));
    fs::write(config_path, config).unwrap();
  }

  fn run(&self, args: &[&str]) -> Output {
    let output = self.command().args(args).output().unwrap();
    assert!(
      output.status.success(),
      "{}",
      String::from_utf8_lossy(&output.stderr)
    );
    output
  }

  fn start(&self, mode: &str, preparing: bool) -> Value {
    self.start_with_smoke(mode, preparing, false)
  }

  fn start_with_smoke(&self, mode: &str, preparing: bool, smoke: bool) -> Value {
    let mut command = self.command();
    command.args(["run", "--detach", "--json"]);
    if smoke {
      command.arg("--smoke");
    }
    command.args(["train", mode]);
    if preparing {
      command.env("EXPRI_TEST_PREPARE_BLOCK", "1");
    }
    let before = Instant::now();
    let output = command.output().unwrap();
    assert!(
      output.status.success(),
      "{}",
      String::from_utf8_lossy(&output.stderr)
    );
    assert!(
      before.elapsed() < Duration::from_secs(3),
      "launcher waited for the experiment"
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["detached"], true);
    assert!(
      Path::new(receipt["run_dir"].as_str().unwrap())
        .join(".worker-ready")
        .is_file()
    );
    receipt
  }
}

fn await_file(path: &Path) {
  let deadline = Instant::now() + Duration::from_secs(10);
  while !path.is_file() {
    assert!(Instant::now() < deadline, "missing {}", path.display());
    thread::sleep(Duration::from_millis(25));
  }
}

fn terminal(fixture: &Fixture, id: &str) -> Value {
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    let report: Value =
      serde_json::from_slice(&fixture.run(&["runs", "status", id, "--json"]).stdout).unwrap();
    if matches!(
      report["status"].as_str(),
      Some("completed" | "failed" | "cancelled" | "lost")
    ) {
      return report;
    }
    assert!(
      Instant::now() < deadline,
      "run failed to reach a terminal state: {report}"
    );
    thread::sleep(Duration::from_millis(50));
  }
}

fn publishing_error(fixture: &Fixture, id: &str) -> Value {
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    let report: Value =
      serde_json::from_slice(&fixture.run(&["runs", "status", id, "--json"]).stdout).unwrap();
    if report["publishing"]["status"] == "error" {
      return report;
    }
    assert!(
      Instant::now() < deadline,
      "publishing error was not reported"
    );
    thread::sleep(Duration::from_millis(25));
  }
}

fn only_run(fixture: &Fixture) -> PathBuf {
  let mut runs = fs::read_dir(fixture.repo.join(".expri/runs")).unwrap();
  let run = runs.next().unwrap().unwrap().path();
  assert!(runs.next().is_none(), "fixture created more than one run");
  run
}

fn record(run: &Path, name: &str) -> Value {
  serde_json::from_slice(&fs::read(run.join(name)).unwrap()).unwrap()
}

fn assert_smoke_snapshot(run: &Path) -> String {
  let id = run.file_name().unwrap().to_str().unwrap();
  assert!(id.starts_with("smoke-run-"), "unexpected smoke ID: {id}");
  assert!(id.len() > "smoke-run-".len());
  for name in ["snapshot.json", "run-state.json"] {
    let value = record(run, name);
    assert_eq!(value["run_id"], id, "{name} identifies another run");
    assert_eq!(value["smoke"], true, "{name} lost the smoke marker");
  }
  id.to_owned()
}

#[test]
fn foreground_smoke_runs_keep_the_marker_and_local_results_after_finishing() {
  for (mode, preparation_fails, exit_code, status) in [
    ("success", false, 0, "completed"),
    ("finish", false, 7, "failed"),
    ("success", true, 43, "failed"),
  ] {
    let fixture = Fixture::new();
    fixture.enable_service_with_missing_client();
    let mut command = fixture.command();
    command.args(["run", "--smoke", "train", mode]);
    if preparation_fails {
      command.env("EXPRI_TEST_PREPARE_FAIL", "1");
    }
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(exit_code));
    let run = only_run(&fixture);
    let id = assert_smoke_snapshot(&run);
    let report = publishing_error(&fixture, &id);
    assert_eq!(report["status"], status);
    assert_eq!(report["state"]["smoke"], true);
    assert_eq!(report["state"]["exit_code"], exit_code);
    assert!(run.join("publishing-request.json").is_file());
    assert!(run.join("logs/stdout.log").is_file());
    assert!(run.join("logs/stderr.log").is_file());
    assert_eq!(run.join("outputs/model.pt").exists(), !preparation_fails);
    let shown: Value =
      serde_json::from_slice(&fixture.run(&["runs", "show", &id, "--json"]).stdout).unwrap();
    assert_eq!(shown["state"]["smoke"], true);
    assert_eq!(shown["run"]["status"], status);
    assert!(
      run.is_dir(),
      "finishing a smoke run removed its local files"
    );
  }
}

#[test]
fn detached_smoke_receipts_and_worker_requests_preserve_the_marker() {
  for (mode, status, exit_code) in [("success", "completed", 0), ("finish", "failed", 7)] {
    let fixture = Fixture::new();
    let receipt = fixture.start_with_smoke(mode, false, true);
    assert_eq!(receipt["smoke"], true);
    let run = Path::new(receipt["run_dir"].as_str().unwrap());
    let id = assert_smoke_snapshot(run);
    assert_eq!(receipt["run_id"], id);
    assert_eq!(record(run, "run-request.json")["smoke"], true);
    let report = terminal(&fixture, &id);
    assert_eq!(report["status"], status);
    assert_eq!(report["state"]["smoke"], true);
    assert_eq!(report["state"]["exit_code"], exit_code);
    assert_eq!(
      fs::read(run.join("outputs/model.pt")).unwrap(),
      b"checkpoint"
    );
    assert!(
      run.is_dir(),
      "finishing a smoke run removed its local files"
    );
  }
}

#[test]
fn cancelled_smoke_runs_remain_available_on_the_worker() {
  let fixture = Fixture::new();
  let receipt = fixture.start_with_smoke("graceful", false, true);
  let run = Path::new(receipt["run_dir"].as_str().unwrap());
  let id = assert_smoke_snapshot(run);
  await_file(&run.join("outputs/started"));
  fixture.run(&["runs", "cancel", &id]);
  let report = terminal(&fixture, &id);
  assert_eq!(report["status"], "cancelled");
  assert_eq!(report["state"]["smoke"], true);
  assert_eq!(report["state"]["exit_code"], 130);
  assert!(run.join("code/train.py").is_file());
  assert!(run.join("outputs/started").is_file());
  assert_eq!(
    fixture.run(&["runs", "logs", &id]).stdout,
    b"first\x00\xff\n"
  );
}

#[test]
fn smoke_requires_an_environment_before_starting_a_run() {
  let fixture = Fixture::new();
  fs::write(
    fixture.repo.join("expri.toml"),
    "[tasks]\ntrain=['python3', 'train.py']\n",
  )
  .unwrap();
  let output = fixture
    .command()
    .args(["run", "--smoke", "train", "success"])
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert!(
    String::from_utf8_lossy(&output.stderr).contains("--smoke requires a configured environment"),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(!fixture.repo.join(".expri/runs").exists());
}

#[test]
fn ordinary_runs_keep_their_existing_id_and_metadata_shape() {
  let fixture = Fixture::new();
  let receipt = fixture.start("success", false);
  let run = Path::new(receipt["run_dir"].as_str().unwrap());
  let id = receipt["run_id"].as_str().unwrap();
  assert!(id.starts_with("run-"));
  assert!(receipt.get("smoke").is_none());
  assert_eq!(terminal(&fixture, id)["status"], "completed");
  for name in ["snapshot.json", "run-state.json", "run-request.json"] {
    assert!(
      record(run, name).get("smoke").is_none(),
      "{name} changed its existing shape"
    );
  }
  assert_eq!(
    fs::read(run.join("outputs/model.pt")).unwrap(),
    b"checkpoint"
  );
}

#[test]
fn detached_cli_receipts_default_to_a_table_and_json_overrides_quiet() {
  for (args, json, quiet) in [
    (vec!["run", "--detach", "train", "success"], false, false),
    (
      vec!["-q", "run", "--detach", "train", "success"],
      false,
      true,
    ),
    (
      vec!["-q", "run", "--detach", "--json", "train", "success"],
      true,
      true,
    ),
  ] {
    let fixture = Fixture::new();
    let output = fixture.run(&args);
    let run = fs::read_dir(fixture.repo.join(".expri/runs"))
      .unwrap()
      .next()
      .unwrap()
      .unwrap()
      .path();
    let id = run.file_name().unwrap().to_str().unwrap();
    if json {
      let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
      assert_eq!(receipt["run_id"], id);
      assert_eq!(receipt["run_dir"], run.to_string_lossy().as_ref());
      assert_eq!(receipt["detached"], true);
      assert!(receipt["status"].is_string());
    } else if quiet {
      assert!(output.stdout.is_empty());
    } else {
      let stdout = String::from_utf8(output.stdout).unwrap();
      assert_eq!(
        stdout
          .lines()
          .next()
          .unwrap()
          .split_whitespace()
          .collect::<Vec<_>>(),
        ["FIELD", "VALUE"]
      );
      assert!(stdout.contains("Run id"));
      assert!(stdout.contains(id));
      assert!(stdout.contains("Run dir"));
      assert!(stdout.contains("Status"));
      assert!(
        stdout
          .lines()
          .any(|line| line.contains("Detached") && line.contains("yes"))
      );
      assert!(serde_json::from_str::<Value>(&stdout).is_err());
    }
    assert_eq!(terminal(&fixture, id)["status"], "completed");
  }
}

#[test]
fn detached_node_run_keeps_its_json_wire_receipt() {
  let fixture = Fixture::new();
  let request = fixture.repo.join(".expri/node-request.json");
  fs::write(
    &request,
    serde_json::to_vec(&serde_json::json!({
      "name": "train", "command": ["python3", "train.py", "success"],
      "environment": {}, "service": null, "remote_managed": ["uv.lock"],
      "extras": [], "sync_args": [], "expected_sync": null, "detach": true,
    }))
    .unwrap(),
  )
  .unwrap();
  let output = fixture.run(&["node", "run", "--request", request.to_str().unwrap()]);
  let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(receipt["detached"], true);
  let id = receipt["run_id"].as_str().unwrap();
  assert!(
    Path::new(receipt["run_dir"].as_str().unwrap())
      .join(".worker-ready")
      .is_file()
  );
  assert_eq!(terminal(&fixture, id)["status"], "completed");
}

#[test]
fn publishing_failure_preserves_foreground_success_and_preparation_failure() {
  for prepare_fails in [false, true] {
    let fixture = Fixture::new();
    fixture.enable_service_with_missing_client();
    let mut command = fixture.command();
    command.args(["run", "train", "success"]);
    if prepare_fails {
      command.env("EXPRI_TEST_PREPARE_FAIL", "1");
    }
    let output = command.output().unwrap();
    assert_eq!(
      output.status.code(),
      Some(if prepare_fails { 43 } else { 0 })
    );
    let run = fs::read_dir(fixture.repo.join(".expri/runs"))
      .unwrap()
      .next()
      .unwrap()
      .unwrap()
      .path();
    let id = run.file_name().unwrap().to_str().unwrap();
    let report = publishing_error(&fixture, id);
    assert_eq!(
      report["status"],
      if prepare_fails { "failed" } else { "completed" }
    );
    assert_eq!(
      report["state"]["exit_code"],
      if prepare_fails { 43 } else { 0 }
    );
    assert!(run.join("publishing-request.json").is_file());
    assert!(run.join("logs/stdout.log").is_file());
    assert!(run.join("logs/stderr.log").is_file());
    assert_eq!(run.join("outputs/model.pt").exists(), !prepare_fails);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("dashboard: https://dashboard.example.net"));
    assert!(stderr.contains("available after publishing"));
    let shown: Value =
      serde_json::from_slice(&fixture.run(&["runs", "show", id, "--json"]).stdout).unwrap();
    assert_eq!(shown["publishing"]["status"], "error");
    assert_eq!(shown["state"]["exit_code"], report["state"]["exit_code"]);
  }
}

#[test]
fn detached_publishing_error_keeps_receipt_and_training_result_available() {
  let fixture = Fixture::new();
  fixture.enable_service_with_missing_client();
  let receipt = fixture.start("success", false);
  let id = receipt["run_id"].as_str().unwrap();
  let url = receipt["dashboard_url"].as_str().unwrap();
  assert!(url.starts_with("https://dashboard.example.net"));
  assert!(url.contains(id));
  let report = terminal(&fixture, id);
  assert_eq!(report["status"], "completed");
  assert_eq!(report["state"]["exit_code"], 0);
  let report = publishing_error(&fixture, id);
  assert_eq!(report["status"], "completed");
  let run = Path::new(receipt["run_dir"].as_str().unwrap());
  assert_eq!(
    fs::read(run.join("outputs/model.pt")).unwrap(),
    b"checkpoint"
  );

  fs::write(run.join("publishing-state.json"), b"invalid JSON").unwrap();
  let unreadable: Value =
    serde_json::from_slice(&fixture.run(&["runs", "status", id, "--json"]).stdout).unwrap();
  assert_eq!(unreadable["status"], "completed");
  assert_eq!(unreadable["publishing"]["status"], "error");
  assert_eq!(
    unreadable["publishing"]["last_error"],
    "Service publishing status could not be read."
  );
  let shown: Value =
    serde_json::from_slice(&fixture.run(&["runs", "show", id, "--json"]).stdout).unwrap();
  assert_eq!(shown["run"]["status"], "completed");
  assert_eq!(shown["publishing"], unreadable["publishing"]);
}

#[test]
fn detached_launcher_returns_closes_ssh_style_pipes_and_preserves_results_and_failure() {
  let fixture = Fixture::new();
  let receipt = fixture.start("finish", false);
  let id = receipt["run_id"].as_str().unwrap();
  let run = Path::new(receipt["run_dir"].as_str().unwrap());
  let report = terminal(&fixture, id);
  assert_eq!(report["status"], "failed");
  assert_eq!(report["state"]["task_exit_code"], 7);
  assert_eq!(report["state"]["exit_code"], 7);
  assert_eq!(
    fs::read(run.join("outputs/model.pt")).unwrap(),
    b"checkpoint"
  );
  assert_eq!(
    fs::read_to_string(run.join("outputs/started")).unwrap(),
    run.join("code").to_string_lossy()
  );
  assert_eq!(
    fixture.run(&["runs", "logs", id]).stdout,
    b"first\x00\xff\n"
  );
  assert_eq!(
    fixture
      .run(&["runs", "logs", id, "--stream", "stderr"])
      .stdout,
    b"error\x80\n"
  );
  let cancelled: Value =
    serde_json::from_slice(&fixture.run(&["runs", "cancel", id, "--json"]).stdout).unwrap();
  assert_eq!(cancelled["status"], "failed");
}

#[test]
fn cancellation_escalates_for_task_and_descendant_and_remains_idempotent() {
  let fixture = Fixture::new();
  let receipt = fixture.start("cancel", false);
  let id = receipt["run_id"].as_str().unwrap();
  let heartbeat = Path::new(receipt["run_dir"].as_str().unwrap()).join("outputs/heartbeat");
  await_file(&heartbeat);
  fixture.run(&["runs", "cancel", id, "--json"]);
  fixture.run(&["runs", "cancel", id, "--json"]);
  let report = terminal(&fixture, id);
  assert_eq!(report["status"], "cancelled");
  assert_eq!(report["state"]["exit_code"], 130);
  let before = fs::read(&heartbeat).unwrap();
  thread::sleep(Duration::from_millis(150));
  assert_eq!(
    fs::read(&heartbeat).unwrap(),
    before,
    "descendant survived cancellation"
  );
  fixture.run(&["runs", "cancel", id]);
}

#[test]
fn preparation_is_detached_and_can_be_cancelled_before_task_launch() {
  let fixture = Fixture::new();
  let receipt = fixture.start("finish", true);
  let id = receipt["run_id"].as_str().unwrap();
  let run = Path::new(receipt["run_dir"].as_str().unwrap());
  await_file(&run.join("helper-started"));
  let status: Value =
    serde_json::from_slice(&fixture.run(&["runs", "status", id, "--json"]).stdout).unwrap();
  assert_eq!(status["status"], "preparing");
  assert_eq!(status["alive"], true);
  fixture.run(&["runs", "cancel", id, "--json"]);
  assert_eq!(terminal(&fixture, id)["status"], "cancelled");
  assert!(!run.join("outputs/started").exists());
}

#[test]
fn graceful_cancellation_preserves_child_signal_status() {
  let fixture = Fixture::new();
  let receipt = fixture.start("graceful", false);
  let id = receipt["run_id"].as_str().unwrap();
  let run = Path::new(receipt["run_dir"].as_str().unwrap());
  await_file(&run.join("outputs/started"));
  fixture.run(&["runs", "cancel", id]);
  let report = terminal(&fixture, id);
  assert_eq!(report["status"], "cancelled");
  assert_eq!(report["state"]["task_exit_code"], 143);
}

#[test]
fn cancellation_during_log_drain_stops_surviving_descendants() {
  let fixture = Fixture::new();
  let receipt = fixture.start("orphan", false);
  let id = receipt["run_id"].as_str().unwrap();
  let run = Path::new(receipt["run_dir"].as_str().unwrap());
  await_file(&run.join("outputs/direct-exit"));
  thread::sleep(Duration::from_millis(150));
  fixture.run(&["runs", "cancel", id]);
  let report = terminal(&fixture, id);
  assert_eq!(report["status"], "cancelled");
  assert_eq!(report["state"]["task_exit_code"], 0);
  let heartbeat = run.join("outputs/heartbeat");
  let before = fs::read(&heartbeat).unwrap();
  thread::sleep(Duration::from_millis(100));
  assert_eq!(fs::read(&heartbeat).unwrap(), before);
}
