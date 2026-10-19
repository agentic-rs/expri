#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

mod support;

const UV: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if args[:2] == ['run', '--isolated']:
  root = pathlib.Path(json.loads(args[-1])['state_dir'])
  venv = root / 'environment/.venv'
  venv.mkdir(parents=True, exist_ok=True)
  manifest = venv.parent / 'environment-state.json'
  manifest.write_text('{}')
  print(json.dumps({'environment_path': str(venv), 'python': sys.executable,
    'manifest_path': str(manifest), 'run_env': {'PATH': os.environ['PATH']}}))
elif args[:4] == ['run', '--no-sync', '--no-env-file', '--']:
  os.execvp(args[4], args[4:])
else:
  raise SystemExit(71)
"#;

const TASK: &str = r#"import argparse, fcntl, json, os, pathlib, sys, time
from expri_metrics import MetricsLogger
from expri_run import set_run_status
parser = argparse.ArgumentParser()
parser.add_argument('--mode', default='running')
parser.add_argument('--limit', type=int, default=100)
parser.add_argument('--startup-delay', type=float, default=0)
args = parser.parse_args()
out = pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'])
attempt = int(os.environ.get('EXPRI_ATTEMPT', '1'))
if attempt > 1:
  time.sleep(args.startup_delay)
set_run_status('running')
exclusive = open(out / '.task-exclusive', 'a')
fcntl.flock(exclusive, fcntl.LOCK_EX | fcntl.LOCK_NB)
if attempt == 1:
  (out / 'checkpoint.bin').write_bytes(b'immutable-checkpoint')
else:
  assert pathlib.Path(os.environ['EXPRI_RESUME_CHECKPOINT']).read_bytes() == b'immutable-checkpoint'
logger = MetricsLogger()
logger.params({'max_steps': args.limit})
for step in ([100, 5] if attempt == 1 else [10, 12]):
  if attempt == 1:
    logger.log(step, {'loss': 1 / (step + 1)}, step_mode='global')
  else:
    logger.log(step, {'loss': 1 / (step + 1)})
with MetricsLogger() as another_logger:
  assert another_logger.initial_step == logger.initial_step
os.write(1, ('attempt %s stdout\n' % attempt).encode())
os.write(2, ('attempt %s stderr\n' % attempt).encode())
(out / ('started-%s.json' % attempt)).write_text(json.dumps({
  'pid': os.getpid(), 'revision': pathlib.Path('revision.txt').read_text(),
  'initial_step': logger.initial_step,
  'checkpoint': os.environ.get('EXPRI_RESUME_CHECKPOINT')}))
if args.mode == 'orphan':
  child = os.fork()
  if child == 0:
    os.close(1)
    os.close(2)
    time.sleep(45)
    os._exit(0)
  (out / 'descendant-pid').write_text(str(child))
if args.mode in ('running', 'waiting'):
  set_run_status(args.mode)
  deadline = time.monotonic() + 60
  while not (out / ('release-%s' % attempt)).exists():
    if time.monotonic() > deadline:
      raise RuntimeError('test release timeout')
    time.sleep(0.02)
elif args.mode == 'failed':
  raise SystemExit(7)
"#;

struct Fixture {
  _root: tempfile::TempDir,
  repo: PathBuf,
  home: PathBuf,
  bin: PathBuf,
  run_dir: Option<PathBuf>,
}

impl Fixture {
  fn new() -> Self {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    let repo = base.join("project");
    let home = base.join("home");
    let bin = base.join("bin");
    fs::create_dir_all(repo.join(".expri")).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    // These scenarios never replace the installed executable. Linking avoids
    // executable copies briefly held writable by concurrent fixture forks.
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_expri"), bin.join("expri")).unwrap();
    fs::write(bin.join("uv"), UV).unwrap();
    fs::set_permissions(bin.join("uv"), fs::Permissions::from_mode(0o755)).unwrap();
    for (name, source) in [
      ("train.py", TASK),
      ("revision.txt", "initial"),
      ("expri_metrics.py", expri_worker::METRICS_PRODUCER_SCRIPT),
      ("expri_run.py", expri_worker::RUN_STATUS_HELPER_SCRIPT),
      (
        "pyproject.toml",
        "[project]\nname='continuation-test'\nversion='0.1.0'\n",
      ),
      ("uv.lock", "version=1\n"),
      (
        "expri.toml",
        "[environment]\n[tasks]\ntrain=['python3', 'train.py']\n",
      ),
      (
        ".expri/checkout.manifest",
        "train.py\nrevision.txt\nexpri_metrics.py\nexpri_run.py\npyproject.toml\nuv.lock\n",
      ),
    ] {
      fs::write(repo.join(name), source).unwrap();
    }
    Self {
      _root: root,
      repo,
      home,
      bin,
      run_dir: None,
    }
  }

  fn command(&self, args: &[&str]) -> Command {
    let mut command = Command::new(self.bin.join("expri"));
    command
      .current_dir(&self.repo)
      .args(args)
      .env("HOME", &self.home)
      .env("EXPRI_STORAGE_DIR", self.home.join(".tokn/expri"))
      .env(
        "PATH",
        format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
      )
      .env_remove("EXPRI_RUN_DIR")
      .env_remove("EXPRI_RUN_ID")
      .env_remove("EXPRI_ATTEMPT")
      .env_remove("EXPRI_STEP_OFFSET");
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

  fn start(&mut self, mode: &str) -> String {
    let output = self.run(&[
      "run",
      "--detach",
      "--no-publish",
      "--json",
      "train",
      "--mode",
      mode,
    ]);
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    self.run_dir = Some(PathBuf::from(receipt["run_dir"].as_str().unwrap()));
    wait_for(
      || self.path("outputs/started-1.json").exists(),
      "initial task",
    );
    if matches!(mode, "running" | "waiting") {
      wait_for(|| self.state()["status"] == mode, "initial task phase");
    }
    receipt["run_id"].as_str().unwrap().to_owned()
  }

  fn path(&self, relative: &str) -> PathBuf {
    self.run_dir.as_ref().unwrap().join(relative)
  }
  fn state(&self) -> Value {
    serde_json::from_slice(&fs::read(self.path("run-state.json")).unwrap()).unwrap()
  }
  fn json(&self, relative: &str) -> Value {
    serde_json::from_slice(&fs::read(self.path(relative)).unwrap()).unwrap()
  }
  fn terminal(&self) {
    wait_for(|| self.state()["finished_at"].is_string(), "terminal state");
  }
  fn started(&self, attempt: u64, label: &str) {
    wait_for(
      || {
        if self
          .path(&format!("outputs/started-{attempt}.json"))
          .exists()
        {
          return true;
        }
        let state = self.state();
        assert!(
          !state["finished_at"].is_string(),
          "task ended before {label}: {state}\nstderr: {}",
          fs::read_to_string(self.path("logs/stderr.log")).unwrap_or_default()
        );
        false
      },
      label,
    );
  }
  fn continue_run(&self, id: &str, request: &str) -> Value {
    let output = self.continuation_command(id, request).output().unwrap();
    assert!(
      output.status.success(),
      "{}",
      String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
  }

  fn continuation_command(&self, id: &str, request: &str) -> Command {
    self.command(&[
      "runs",
      "continue",
      id,
      "--checkpoint",
      "outputs/checkpoint.bin",
      "--reason",
      "Extend training",
      "--request-id",
      request,
      "--expected-attempt",
      "1",
      "--json",
      "--",
      "--mode",
      "running",
      "--limit",
      "300",
    ])
  }
}

impl Drop for Fixture {
  fn drop(&mut self) {
    if let Some(run) = &self.run_dir {
      for attempt in 1..=8 {
        let _ = fs::write(run.join(format!("outputs/release-{attempt}")), "release");
      }
      let deadline = Instant::now() + Duration::from_secs(5);
      while Instant::now() < deadline
        && fs::read(run.join("run-state.json"))
          .ok()
          .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
          .is_some_and(|state| state["finished_at"].is_null())
      {
        thread::sleep(Duration::from_millis(25));
      }
    }
    support::stop_worker_service(&self.home.join(".tokn/expri"));
  }
}

fn wait_for(mut ready: impl FnMut() -> bool, label: &str) {
  let deadline = Instant::now() + Duration::from_secs(20);
  while !ready() {
    assert!(Instant::now() < deadline, "timed out waiting for {label}");
    thread::sleep(Duration::from_millis(10));
  }
}

struct PausedSupervisor(Option<u64>);

impl PausedSupervisor {
  fn signal(&self, name: &str) {
    let output = Command::new("python3")
      .args([
        "-c",
        "import os, signal, sys; os.kill(int(sys.argv[1]), getattr(signal,sys.argv[2]))",
        &self.0.unwrap().to_string(),
        name,
      ])
      .output()
      .unwrap();
    assert!(
      output.status.success(),
      "{}",
      String::from_utf8_lossy(&output.stderr)
    );
  }

  fn resume(mut self) {
    self.signal("SIGCONT");
    self.0 = None;
  }
}

impl Drop for PausedSupervisor {
  fn drop(&mut self) {
    let Some(pid) = self.0 else { return };
    let _ = Command::new("python3")
      .args([
        "-c",
        "import os, signal, sys; os.kill(int(sys.argv[1]), signal.SIGCONT)",
        &pid.to_string(),
      ])
      .output();
  }
}

#[test]
fn natural_exit_with_a_ready_continuation_still_launches_exactly_one_successor() {
  let mut fixture = Fixture::new();
  let id = fixture.start("running");
  let state = fixture.state();
  let supervisor = PausedSupervisor(Some(state["supervisor"]["owner_pid"].as_u64().unwrap()));
  supervisor.signal("SIGSTOP");
  let child = fixture
    .continuation_command(&id, "natural-exit")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  wait_for(
    || {
      fs::read(fixture.path(".task-continuation.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|intent| intent["stage"] == "ready")
    },
    "ready continuation while supervisor paused",
  );
  fs::write(fixture.path("outputs/release-1"), "release").unwrap();
  let task = fixture.json("outputs/started-1.json")["pid"]
    .as_u64()
    .unwrap();
  wait_for(
    || {
      fs::read_to_string(format!("/proc/{task}/stat"))
        .ok()
        .is_some_and(|stat| {
          stat
            .rsplit_once(')')
            .is_some_and(|(_, fields)| fields.trim_start().starts_with('Z'))
        })
    },
    "natural task exit before supervisor resumes",
  );
  supervisor.resume();
  let output = child.wait_with_output().unwrap();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  wait_for(
    || fixture.path("outputs/started-2.json").exists(),
    "successor after natural exit",
  );
  assert_eq!(fixture.state()["task_attempts"]["current_attempt"], 2);
  assert_eq!(
    fixture.state()["task_attempts"]["attempts"][0]["status"],
    "completed"
  );
  assert_eq!(
    fixture.state()["task_attempts"]["attempts"]
      .as_array()
      .unwrap()
      .len(),
    2
  );
  fs::write(fixture.path("outputs/release-2"), "release").unwrap();
  fixture.terminal();
}

#[test]
fn active_continuation_preserves_history_and_extends_steps_with_new_source() {
  for mode in ["running", "waiting"] {
    let mut fixture = Fixture::new();
    let id = fixture.start(mode);
    let before = fixture.state();
    let initial = fixture.json("outputs/started-1.json");
    assert_eq!(initial["initial_step"], 0);
    let paths = [
      "outputs/metrics.jsonl",
      "outputs/params.json",
      "logs/stdout.log",
      "logs/stderr.log",
      "outputs/system-profile.jsonl",
    ];
    let prefixes: Vec<_> = paths
      .iter()
      .map(|path| fs::read(fixture.path(path)).unwrap())
      .collect();
    fs::write(fixture.repo.join("revision.txt"), "updated").unwrap();
    let report = fixture.continue_run(&id, "extend-1");
    fixture.started(2, "continued task");
    assert_eq!(report["run_id"], id);
    assert_eq!(report["state"]["started_at"], before["started_at"]);
    let current = fixture.json("outputs/started-2.json");
    assert_eq!(current["initial_step"], 101);
    assert_ne!(current["pid"], initial["pid"]);
    assert_eq!(current["revision"], "updated");
    assert_eq!(
      fs::read_to_string(fixture.path("code/revision.txt")).unwrap(),
      "initial"
    );
    for (path, prefix) in paths.iter().zip(&prefixes) {
      assert!(
        fs::read(fixture.path(path)).unwrap().starts_with(prefix),
        "changed prefix: {path}"
      );
    }
    let rows: Vec<Value> = fs::read_to_string(fixture.path("outputs/metrics.jsonl"))
      .unwrap()
      .lines()
      .map(|line| serde_json::from_str(line).unwrap())
      .collect();
    assert_eq!(
      rows
        .iter()
        .map(|row| row["step"].as_u64().unwrap())
        .collect::<Vec<_>>(),
      [100, 5, 111, 113]
    );
    assert_eq!(rows[0]["step_mode"], "global");
    assert!(rows[0].get("local_step").is_none());
    assert_eq!(rows[2]["step_mode"], "relative");
    assert_eq!(rows[2]["local_step"], 10);
    assert_eq!(rows[2]["attempt"], 2);
    assert_eq!(
      fixture.json("outputs/params.json")["params"]["max_steps"],
      100
    );
    assert_eq!(
      fixture.json("outputs/attempts/2/params.json")["params"]["max_steps"],
      300
    );
    let replay = fixture.continue_run(&id, "extend-1");
    assert_eq!(replay["state"]["task_attempts"]["current_attempt"], 2);
    assert_eq!(
      fixture.json("outputs/started-2.json")["pid"],
      current["pid"]
    );
    fs::write(fixture.path("outputs/release-2"), "release").unwrap();
    fixture.terminal();
    let after = fixture.state();
    assert_eq!(after["status"], "completed");
    assert_eq!(
      after["task_attempts"]["attempts"].as_array().unwrap().len(),
      2
    );
    assert_eq!(after["task_attempts"]["attempts"][0]["status"], "continued");
    assert_eq!(after["task_attempts"]["attempts"][1]["status"], "completed");
  }
}

#[test]
fn an_active_successor_survives_startup_beyond_the_previous_task_stop_grace() {
  let mut fixture = Fixture::new();
  let id = fixture.start("running");
  let output = fixture
    .continuation_command(&id, "delayed-successor")
    .args(["--startup-delay", "3"])
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  fixture.started(2, "successor startup beyond the old two-second stop grace");
  let state = fixture.state();
  assert_eq!(state["status"], "running");
  assert_eq!(state["task_attempts"]["current_attempt"], 2);
  let rows: Vec<Value> = fs::read_to_string(fixture.path("outputs/metrics.jsonl"))
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
  assert_eq!(
    rows
      .iter()
      .map(|row| row["step"].as_u64().unwrap())
      .collect::<Vec<_>>(),
    [100, 5, 111, 113]
  );
  assert_eq!(rows[3]["attempt"], 2);
  fs::write(fixture.path("outputs/release-2"), "release").unwrap();
  fixture.terminal();
  assert_eq!(fixture.state()["status"], "completed");
}

#[test]
fn completed_failed_and_cancelled_runs_can_continue_without_changing_prior_outcome() {
  for mode in ["completed", "failed", "cancelled", "orphan"] {
    let mut fixture = Fixture::new();
    let id = fixture.start(if mode == "cancelled" { "running" } else { mode });
    if mode == "cancelled" {
      fixture.run(&["runs", "cancel", &id, "--json"]);
    }
    fixture.terminal();
    let before = fixture.state();
    let expected = if mode == "orphan" { "completed" } else { mode };
    assert_eq!(before["status"], expected);
    let metrics = fs::read(fixture.path("outputs/metrics.jsonl")).unwrap();
    fixture.continue_run(&id, "resume-terminal");
    wait_for(
      || fixture.path("outputs/started-2.json").exists(),
      "terminal continuation",
    );
    assert_eq!(fixture.state()["started_at"], before["started_at"]);
    assert_eq!(
      fixture.state()["task_attempts"]["attempts"][0]["status"],
      expected
    );
    assert!(
      fs::read(fixture.path("outputs/metrics.jsonl"))
        .unwrap()
        .starts_with(&metrics)
    );
    fs::write(fixture.path("outputs/release-2"), "release").unwrap();
    fixture.terminal();
    assert_eq!(fixture.state()["status"], "completed");
  }
}

#[test]
fn failed_checkpoint_preparation_leaves_the_current_task_and_history_running() {
  let mut fixture = Fixture::new();
  let id = fixture.start("waiting");
  let original = fixture.json("outputs/started-1.json");
  let metrics = fs::read(fixture.path("outputs/metrics.jsonl")).unwrap();
  let output = fixture
    .command(&[
      "runs",
      "continue",
      &id,
      "--checkpoint",
      "outputs/missing.bin",
      "--reason",
      "Invalid checkpoint",
      "--request-id",
      "bad-checkpoint",
      "--json",
    ])
    .output()
    .unwrap();
  assert!(!output.status.success());
  let after = fixture.state();
  assert_eq!(after["status"], "waiting");
  assert_eq!(after["task_attempts"]["current_attempt"], 1);
  assert_eq!(after["task_attempts"]["attempts"][1]["status"], "failed");
  assert!(after["task_attempts"]["attempts"][1]["started_at"].is_null());
  assert_eq!(
    fixture.json("outputs/started-1.json")["pid"],
    original["pid"]
  );
  assert_eq!(
    fs::read(fixture.path("outputs/metrics.jsonl")).unwrap(),
    metrics
  );
  fixture.continue_run(&id, "valid-after-failure");
  fixture.started(3, "continuation after failed preparation");
  assert_eq!(fixture.state()["task_attempts"]["current_attempt"], 3);
  assert_eq!(
    fixture.state()["task_attempts"]["attempts"][1]["status"],
    "failed"
  );
  fs::write(fixture.path("outputs/release-3"), "release").unwrap();
  fixture.terminal();
}

#[test]
fn a_failed_prelaunch_attempt_does_not_move_the_sealed_previous_capture_end() {
  use expri_core::task_attempts;
  use serde_json::json;

  let mut fixture = Fixture::new();
  let id = fixture.start("completed");
  fixture.terminal();
  let original = fixture.state();
  let original_end = original["finished_at"].as_str().unwrap().to_owned();
  let failure_end = (chrono::DateTime::parse_from_rfc3339(&original_end).unwrap()
    + chrono::Duration::milliseconds(500))
  .to_rfc3339();
  let profile = fs::read(fixture.path("outputs/system-profile.jsonl")).unwrap();
  expri_core::run_state::update_state(&fixture.path("run-state.json"),|state| {
    task_attempts::ensure_initial(state)?;
    task_attempts::finish_attempt(state,1,"continued",&original_end,Some(0),None)?;
    task_attempts::begin_attempt(state,json!({"attempt":2,"request_id":"failed-cutover","reason":"Publication interrupted","requested_at":original_end,"metric_step_offset":"101"}))?;
    task_attempts::finish_attempt(state,2,"failed",&failure_end,None,Some("Publication interrupted"))?;
    state["status"]=json!("failed");state["finished_at"]=json!(failure_end);
    state["system_capture_finished_at"]=json!(original_end);
    Ok(())
  }).unwrap();
  fixture.continue_run(&id, "after-failed-cutover");
  wait_for(
    || fixture.path("outputs/started-3.json").exists(),
    "successor after failed cutover",
  );
  let state = fixture.state();
  assert_eq!(
    state["task_attempts"]["attempts"][0]["system_capture_finished_at"],
    original_end
  );
  assert_eq!(state["task_attempts"]["attempts"][1]["status"], "failed");
  assert!(
    fs::read(fixture.path("outputs/system-profile.jsonl"))
      .unwrap()
      .starts_with(&profile)
  );
  fs::write(fixture.path("outputs/release-3"), "release").unwrap();
  fixture.terminal();
}

#[test]
fn a_failed_environment_preparation_can_continue_after_the_environment_is_fixed() {
  let mut fixture = Fixture::new();
  let failing=UV.replace("args = sys.argv[1:]","args = sys.argv[1:]\nif args[:2] == ['run', '--isolated']:\n  raise SystemExit('environment unavailable')");
  fs::write(fixture.bin.join("uv"), failing).unwrap();
  let output = fixture.run(&["run", "--detach", "--no-publish", "--json", "train"]);
  let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
  fixture.run_dir = Some(PathBuf::from(receipt["run_dir"].as_str().unwrap()));
  fixture.terminal();
  assert_eq!(fixture.state()["status"], "failed");
  assert_eq!(fixture.state()["task_process_group_clean"], true);
  fs::write(
    fixture.path("outputs/checkpoint.bin"),
    b"immutable-checkpoint",
  )
  .unwrap();
  fs::write(fixture.bin.join("uv"), UV).unwrap();
  let id = receipt["run_id"].as_str().unwrap();
  fixture.continue_run(id, "fixed-environment");
  wait_for(
    || fixture.path("outputs/started-2.json").exists(),
    "continued task after failed environment",
  );
  assert_eq!(fixture.json("outputs/started-2.json")["initial_step"], 0);
  let rows: Vec<Value> = fs::read_to_string(fixture.path("outputs/metrics.jsonl"))
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
  assert_eq!(
    rows
      .iter()
      .map(|row| row["step"].as_u64().unwrap())
      .collect::<Vec<_>>(),
    [10, 12]
  );
  fs::write(fixture.path("outputs/release-2"), "release").unwrap();
  fixture.terminal();
}
