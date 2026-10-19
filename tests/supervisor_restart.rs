#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use expri_core::lock::{LockAttempt, try_lock_file_readonly};
use serde_json::Value;

mod support;

const UV: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if args[:2] == ['run', '--isolated']:
  root = pathlib.Path(json.loads(args[-1])['state_dir'])
  venv = root / 'environment/.venv'
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

const TASK: &str = r#"import json, os, pathlib, sys, time
from expri_metrics import MetricsLogger
from expri_run import set_run_status
out = pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'])
logger = MetricsLogger()
logger.params({'batch_size': 32})
(out / 'checkpoint.bin').write_bytes(b'checkpoint-prefix')
set_run_status(sys.argv[1])
os.write(1, b'initial\x00\xff\n')
os.write(2, b'stderr\x80\n')
logger.log(0, {'tick': 0}, step_mode='global')
(out / 'started.json').write_text(json.dumps({'pid': os.getpid(), 'supervisor_pid': os.getppid()}))
deadline = time.monotonic() + 45
step = 1
while not (out / 'release').exists():
  if time.monotonic() > deadline:
    raise RuntimeError('test release timed out')
  phase = out / 'desired_status'
  if phase.exists():
    set_run_status(phase.read_text())
    phase.unlink()
  logger.log(step, {'tick': step, 'task_pid': os.getpid(), 'supervisor_pid': os.getppid()}, step_mode='global')
  os.write(1, ('tick %s\n' % step).encode())
  step += 1
  time.sleep(0.025)
"#;

struct Fixture {
  _root: tempfile::TempDir,
  repo: PathBuf,
  home: PathBuf,
  bin: PathBuf,
  run: Option<PathBuf>,
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
    install_binary(&bin.join("expri"));
    fs::write(bin.join("uv"), UV).unwrap();
    fs::set_permissions(bin.join("uv"), fs::Permissions::from_mode(0o755)).unwrap();
    for (name, text) in [
      ("train.py", TASK),
      ("expri_metrics.py", expri_worker::METRICS_PRODUCER_SCRIPT),
      ("expri_run.py", expri_worker::RUN_STATUS_HELPER_SCRIPT),
      (
        "pyproject.toml",
        "[project]\nname='restart-test'\nversion='0.1.0'\n",
      ),
      ("uv.lock", "version=1\n"),
    ] {
      fs::write(repo.join(name), text).unwrap();
    }
    fs::write(
      repo.join("expri.toml"),
      "[environment]\n[tasks]\ntrain=['python3', 'train.py']\n",
    )
    .unwrap();
    fs::write(
      repo.join(".expri/checkout.manifest"),
      "train.py\nexpri_metrics.py\nexpri_run.py\npyproject.toml\nuv.lock\n",
    )
    .unwrap();
    Self {
      _root: root,
      repo,
      home,
      bin,
      run: None,
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
      .env_remove("EXPRI_RUN_ID");
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

  fn start(&mut self, phase: &str) -> String {
    let output = self.run(&["run", "--detach", "--no-publish", "--json", "train", phase]);
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    let run = PathBuf::from(receipt["run_dir"].as_str().unwrap());
    wait_for(|| run.join("outputs/started.json").is_file(), "task start");
    self.run = Some(run);
    receipt["run_id"].as_str().unwrap().into()
  }

  fn state(&self) -> Value {
    serde_json::from_slice(&fs::read(self.run.as_ref().unwrap().join("run-state.json")).unwrap())
      .unwrap()
  }

  fn finish(&self, id: &str) -> Value {
    fs::write(
      self.run.as_ref().unwrap().join("outputs/release"),
      "release",
    )
    .unwrap();
    wait_for(
      || self.state()["finished_at"].is_string(),
      "task completion",
    );
    let output = self.run(&["runs", "status", id, "--json"]);
    serde_json::from_slice(&output.stdout).unwrap()
  }
}

impl Drop for Fixture {
  fn drop(&mut self) {
    if let Some(run) = &self.run {
      let _ = fs::write(run.join("outputs/release"), "release");
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

fn wait_for(mut condition: impl FnMut() -> bool, label: &str) {
  let deadline = Instant::now() + Duration::from_secs(15);
  while !condition() {
    assert!(Instant::now() < deadline, "timed out awaiting {label}");
    thread::sleep(Duration::from_millis(10));
  }
}

fn install_binary(destination: &Path) {
  fs::copy(env!("CARGO_BIN_EXE_expri"), destination).unwrap();
  // Cargo's debug ELF can exceed the production supervisor image limit.
  // Remove only fixture debug sections, as deployment strips its binary too.
  let output = Command::new("strip")
    .arg("--strip-debug")
    .arg(destination)
    .output()
    .expect("supervisor restart tests require the Linux binutils strip tool");
  assert!(
    output.status.success(),
    "strip test fixture: {}",
    String::from_utf8_lossy(&output.stderr)
  );
}

struct LeaseWatch {
  stop: Arc<AtomicBool>,
  gap: Arc<AtomicBool>,
  task: Option<thread::JoinHandle<()>>,
}

impl LeaseWatch {
  fn start(run: &Path) -> Self {
    let stop = Arc::new(AtomicBool::new(false));
    let gap = Arc::new(AtomicBool::new(false));
    let (stopped, lost) = (stop.clone(), gap.clone());
    let path = run.join(".run.lock");
    let task = thread::spawn(move || {
      while !stopped.load(Ordering::Acquire) {
        if !matches!(try_lock_file_readonly(&path), Ok(LockAttempt::Busy)) {
          lost.store(true, Ordering::Release);
        }
        thread::sleep(Duration::from_millis(1));
      }
    });
    Self {
      stop,
      gap,
      task: Some(task),
    }
  }

  fn finish(mut self) {
    self.stop.store(true, Ordering::Release);
    self.task.take().unwrap().join().unwrap();
    assert!(
      !self.gap.load(Ordering::Acquire),
      "run lease became available during handoff"
    );
  }
}

impl Drop for LeaseWatch {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    if let Some(task) = self.task.take() {
      task.join().unwrap();
    }
  }
}

#[test]
fn running_and_waiting_handoffs_preserve_task_identity_prefixes_and_continuous_ownership() {
  for phase in ["running", "waiting"] {
    let mut fixture = Fixture::new();
    let id = fixture.start(phase);
    let run = fixture.run.as_ref().unwrap();
    let before = fixture.state();
    assert!(
      before["system_capture_id"].is_string(),
      "capture was not initialized: {before}"
    );
    let identity = fs::read(run.join("outputs/started.json")).unwrap();
    let paths = [
      "logs/stdout.log",
      "logs/stderr.log",
      "outputs/metrics.jsonl",
      "outputs/params.json",
      "outputs/system-profile.jsonl",
    ];
    let prefixes: Vec<_> = paths
      .iter()
      .map(|path| fs::read(run.join(path)).unwrap())
      .collect();
    fixture.run(&[
      "artifact",
      "register",
      "outputs/checkpoint.bin",
      "--run-dir",
      run.to_str().unwrap(),
      "--label",
      "latest",
      "--json",
    ]);
    let registrations = fs::read(run.join("checkpoint-registrations.json")).unwrap();
    let watch = LeaseWatch::start(run);
    let output = fixture.run(&[
      "runs",
      "restart-supervisor",
      &id,
      "--reason",
      "upgrade runtime",
      "--request-id",
      "handoff-one",
      "--expected-generation",
      "1",
      "--json",
    ]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["restart"]["status"], "completed");
    assert_eq!(report["restart"]["worker_restarted"], false);
    let duplicate = fixture.run(&[
      "runs",
      "restart-supervisor",
      &id,
      "--reason",
      "upgrade runtime",
      "--request-id",
      "handoff-one",
      "--expected-generation",
      "1",
      "--json",
    ]);
    let duplicate: Value = serde_json::from_slice(&duplicate.stdout).unwrap();
    assert_eq!(
      duplicate["restart"]["generation"],
      report["restart"]["generation"]
    );
    let after = fixture.state();
    assert_eq!(after["run_id"], before["run_id"]);
    assert_eq!(after["status"], phase);
    assert_eq!(after["started_at"], before["started_at"]);
    assert_eq!(after["task_started_at"], before["task_started_at"]);
    assert_eq!(after["system_capture_id"], before["system_capture_id"]);
    assert_eq!(after["supervisor"]["generation"], 2);
    assert_eq!(after["supervisor"]["events"].as_array().unwrap().len(), 1);
    // Replace the installed path atomically, then request another default
    // upgrade while the current supervisor is executing its sealed image.
    install_binary(&fixture.bin.join("expri.next"));
    fs::rename(fixture.bin.join("expri.next"), fixture.bin.join("expri")).unwrap();
    let second = fixture.run(&[
      "runs",
      "restart-supervisor",
      &id,
      "--reason",
      "second installed upgrade",
      "--request-id",
      "handoff-two",
      "--expected-generation",
      "2",
      "--json",
    ]);
    let second: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["restart"]["status"], "completed");
    assert_eq!(fixture.state()["supervisor"]["generation"], 3);
    assert_eq!(
      fixture.state()["supervisor"]["events"]
        .as_array()
        .unwrap()
        .len(),
      2
    );
    assert_eq!(
      fs::read(run.join("outputs/started.json")).unwrap(),
      identity
    );
    assert_eq!(
      fs::read(run.join("checkpoint-registrations.json")).unwrap(),
      registrations
    );
    for (path, prefix) in paths.iter().zip(&prefixes) {
      assert!(
        fs::read(run.join(path)).unwrap().starts_with(prefix),
        "prefix changed: {path}"
      );
    }
    let task: Value = serde_json::from_slice(&identity).unwrap();
    wait_for(
      || fs::read(run.join("outputs/metrics.jsonl")).unwrap().len() > prefixes[2].len() + 100,
      "continued metrics",
    );
    let events = fs::read_to_string(run.join("outputs/metrics.jsonl")).unwrap();
    for line in events.lines().skip(1) {
      let event: Value = serde_json::from_str(line).unwrap();
      assert_eq!(event["metrics"]["task_pid"].as_f64(), task["pid"].as_f64());
      assert_eq!(
        event["metrics"]["supervisor_pid"].as_f64(),
        task["supervisor_pid"].as_f64()
      );
    }
    let next_phase = if phase == "running" {
      "waiting"
    } else {
      "running"
    };
    fs::write(run.join("outputs/desired_status.next"), next_phase).unwrap();
    fs::rename(
      run.join("outputs/desired_status.next"),
      run.join("outputs/desired_status"),
    )
    .unwrap();
    wait_for(
      || fixture.state()["status"] == next_phase,
      "task phase transition after handoff",
    );
    assert_eq!(fixture.state()["started_at"], before["started_at"]);
    assert_eq!(fixture.state()["supervisor"]["generation"], 3);
    watch.finish();
    let terminal = fixture.finish(&id);
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["state"]["supervisor"]["generation"], 3);
    assert_eq!(terminal["state"]["started_at"], before["started_at"]);
    let rejected = fixture
      .command(&[
        "runs",
        "restart-supervisor",
        &id,
        "--reason",
        "terminal",
        "--request-id",
        "terminal-request",
        "--json",
      ])
      .output()
      .unwrap();
    assert!(!rejected.status.success());
    assert_eq!(fixture.state(), terminal["state"]);
  }
}

#[test]
fn rejected_replacement_and_stale_requests_leave_the_original_worker_live_and_retryable() {
  let mut fixture = Fixture::new();
  let id = fixture.start("waiting");
  let before = fixture.state();
  let replacement = fixture.repo.join("unsupported-expri");
  fs::write(&replacement, "#!/bin/sh\nexit 0\n").unwrap();
  fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755)).unwrap();
  let rejected = fixture
    .command(&[
      "runs",
      "restart-supervisor",
      &id,
      "--reason",
      "bad target",
      "--request-id",
      "bad-target",
      "--binary",
      replacement.to_str().unwrap(),
      "--json",
    ])
    .output()
    .unwrap();
  assert!(!rejected.status.success());
  let failure: Value = serde_json::from_slice(&rejected.stdout).unwrap();
  assert_eq!(failure["restart"]["status"], "failed");
  assert!(
    failure["restart"]["error"]
      .as_str()
      .unwrap()
      .contains("ELF")
  );
  assert_eq!(fixture.state()["status"], "waiting");
  assert_eq!(fixture.state()["supervisor"]["generation"], 1);
  // A successful unrelated ELF must not be mistaken for a compatible expri.
  let unrelated = fixture
    .command(&[
      "runs",
      "restart-supervisor",
      &id,
      "--reason",
      "unrelated executable",
      "--request-id",
      "unrelated-target",
      "--binary",
      "/bin/true",
      "--json",
    ])
    .output()
    .unwrap();
  assert!(!unrelated.status.success());
  let unrelated: Value = serde_json::from_slice(&unrelated.stdout).unwrap();
  assert_eq!(unrelated["restart"]["status"], "failed");
  assert_eq!(fixture.state()["supervisor"]["generation"], 1);
  assert_eq!(fixture.state()["status"], "waiting");
  let output = fixture.run(&[
    "runs",
    "restart-supervisor",
    &id,
    "--reason",
    "retry after rejected target",
    "--request-id",
    "good-target",
    "--json",
  ]);
  let report: Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(report["restart"]["status"], "completed");
  for arguments in [
    vec![
      "--request-id",
      "stale-request",
      "--expected-generation",
      "1",
    ],
    vec!["--request-id", "good-target"],
  ] {
    let mut command = fixture.command(&[
      "runs",
      "restart-supervisor",
      &id,
      "--reason",
      "conflicting request",
      "--json",
    ]);
    let output = command.args(arguments).output().unwrap();
    assert!(!output.status.success());
  }
  assert_eq!(fixture.state()["started_at"], before["started_at"]);
  assert_eq!(fixture.state()["status"], "waiting");
  let cancelled = fixture.run(&["runs", "cancel", &id, "--json"]);
  assert_eq!(
    serde_json::from_slice::<Value>(&cancelled.stdout).unwrap()["cancel_requested"],
    true
  );
  wait_for(
    || fixture.state()["finished_at"].is_string(),
    "cancellation after handoff",
  );
  assert_eq!(fixture.state()["status"], "cancelled");
  assert_eq!(fixture.state()["supervisor"]["generation"], 2);
}
