use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use clap::Parser;
use serde_json::{Value, json};

use crate::config::EnvironmentConfig;
use crate::protocol::{
  DoctorRequest, EnvironmentAction, EnvironmentCommandRequest, PruneRequest, RunRequest,
  SetupRequest, SetupStep,
};

const FAKE_UV: &str = r#"#!/usr/bin/env python3
import json
import os
from pathlib import Path
import signal
import subprocess
import sys

def observation(kind, arguments):
  return {
    "kind": kind,
    "argv": arguments,
    "cwd": str(Path.cwd()),
    "env": {key: os.environ.get(key) for key in [
      "UV_PROJECT_ENVIRONMENT", "UV_CACHE_DIR", "PYTHONNOUSERSITE", "CUSTOM_RUNTIME",
      "UV_PROJECT", "UV_PYTHON", "UV_ACTIVE", "VIRTUAL_ENV",
    ]},
  }

def setup_event(event):
  with Path(".expri/setup-events.jsonl").open("a") as handle:
    handle.write(json.dumps(event) + "\n")

arguments = sys.argv[1:]
if arguments[:2] == ["run", "--isolated"]:
  request = json.loads(arguments[-1])
  run_dir = Path(request["state_dir"]).resolve()
  (run_dir / "helper-request.json").write_text(json.dumps(request))
  (run_dir / "helper-argv.json").write_text(json.dumps(arguments[:-2]))
  (run_dir / "helper-environment.json").write_text(json.dumps(observation("helper", [])))
  if request["operation"] == "run":
    os.write(2, b"preparation stderr\x00\xff\n")
  if os.environ.get("EXPRI_TEST_HELPER_SIGNAL") == "1":
    os.kill(os.getpid(), signal.SIGTERM)
  if request["operation"] == "doctor":
    compatible = os.environ.get("EXPRI_TEST_DOCTOR_FAIL") != "1"
    print(json.dumps({
      "compatible": compatible,
      "scope": "base_and_lock",
      "base_python": "/opt/conda/bin/python",
      "reused_packages": ["numpy", "torch"],
      "issues": [] if compatible else [{
        "kind": "locked_version_mismatch", "package": "torch",
        "message": "the base torch version differs from uv.lock",
      }],
      "checks": {"lock": True, "base": True},
      "cache": {"directory": request["cache_dir"], "link_mode": "hardlink",
                "same_filesystem": True, "disabled": False},
    }))
    sys.exit(0)
  if os.environ.get("EXPRI_TEST_PREPARE_FAIL") == "1":
    sys.exit(19)
  environment_path = run_dir / "environment" / ".venv"
  environment_path.mkdir(parents=True)
  (environment_path / "bin").mkdir()
  (environment_path / "bin" / "python").symlink_to(sys.executable)
  manifest = run_dir / "environment" / "environment-state.json"
  manifest.write_text("{}")
  (run_dir / "environment" / "owner.json").write_text(json.dumps({
    "schema_version": 1, "repo_root": request["repo_root"],
  }))
  if request["operation"] == "run":
    # Simulate a subsequent sync after the code snapshot has been prepared.
    (Path(os.environ["EXPRI_TEST_SOURCE_ROOT"]) / "train.py").write_text("later source")
  else:
    setup_event(observation("prepare", arguments[:-2]))
  print(json.dumps({
    "environment_path": str(environment_path),
    "python": str(environment_path / "bin" / "python"),
    "manifest_path": str(manifest),
    "run_env": {
      "UV_PROJECT_ENVIRONMENT": str(environment_path), "PYTHONNOUSERSITE": "1",
      "UV_CACHE_DIR": request.get("cache_dir", ""),
      "PATH": str(environment_path / "bin") + os.pathsep + os.environ["PATH"],
    },
    "env_remove": ["UV_PROJECT", "UV_PYTHON", "UV_ACTIVE", "VIRTUAL_ENV"],
  }))
elif arguments[:4] == ["run", "--no-sync", "--no-env-file", "hf"]:
  setup_event(observation("hf", arguments))
elif arguments[:5] == ["run", "--no-sync", "--no-env-file", "--", "bash"]:
  sys.exit(subprocess.run(arguments[4:]).returncode)
elif arguments[:4] == ["run", "--no-sync", "--no-env-file", "--"]:
  run_dir = Path(os.environ["EXPRI_RUN_DIR"])
  result = {
    "argv": arguments,
    "cwd": str(Path.cwd()),
    "source": Path("train.py").read_text(),
    "env": {key: os.environ.get(key) for key in [
      "EXPRI_RUN_ID", "EXPRI_RUN_DIR", "EXPRI_OUTPUT_DIR", "UV_PROJECT_ENVIRONMENT", "UV_CACHE_DIR",
      "PYTHONNOUSERSITE", "CUSTOM_RUNTIME", "UV_PROJECT", "UV_PYTHON", "UV_ACTIVE", "VIRTUAL_ENV",
      "PYTHONUNBUFFERED",
    ]},
  }
  output_dir = Path(os.environ["EXPRI_OUTPUT_DIR"])
  (output_dir / "task-observation.json").write_text(json.dumps(result))
  os.write(1, b"task stdout\x00\xff\n")
  os.write(2, b"task stderr\x80\x00\n")
  if os.environ.get("EXPRI_TEST_TASK_SIGNAL") == "1":
    os.kill(os.getpid(), signal.SIGTERM)
  sys.exit(int(os.environ.get("EXPRI_TEST_TASK_EXIT", "0")))
else:
  print("unexpected fake uv arguments: " + repr(arguments), file=sys.stderr)
  sys.exit(31)
"#;

const SETUP_SCRIPT: &str = r#"#!/bin/sh
python - "$1" <<'PY'
import json
import os
from pathlib import Path
import sys

event = {
  "kind": sys.argv[1],
  "cwd": str(Path.cwd()),
  "python": sys.executable,
  "env": {key: os.environ.get(key) for key in [
    "UV_PROJECT_ENVIRONMENT", "PYTHONNOUSERSITE", "CUSTOM_RUNTIME",
    "UV_PROJECT", "UV_PYTHON", "UV_ACTIVE", "VIRTUAL_ENV",
  ]},
}
with Path(".expri/setup-events.jsonl").open("a") as handle:
  handle.write(json.dumps(event) + "\n")
PY
"#;

#[derive(Clone, Copy, Debug)]
enum Backend {
  Native,
  Python,
}

struct Fixture {
  _directory: tempfile::TempDir,
  repo_root: PathBuf,
  bin_dir: PathBuf,
  request: RunRequest,
  request_path: PathBuf,
}

impl Fixture {
  fn new() -> Self {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::Builder::new()
      .prefix("expri run test ")
      .tempdir()
      .expect("fixture directory");
    let repo_root = directory.path().join("project with spaces");
    let bin_dir = directory.path().join("bin");
    fs::create_dir(&repo_root).expect("repo directory");
    let repo_root = repo_root.canonicalize().expect("canonical repo directory");
    fs::create_dir(repo_root.join(".expri")).expect("state directory");
    fs::create_dir(&bin_dir).expect("binary directory");
    fs::write(repo_root.join("train.py"), "original source").expect("source file");
    fs::write(
      repo_root.join("pyproject.toml"),
      "[project]\nname = 'demo'\nversion = '0.1.0'\n",
    )
    .expect("project metadata");
    fs::write(repo_root.join("uv.lock"), "version = 1\n").expect("lock file");
    fs::write(
      repo_root.join(".expri/checkout.manifest"),
      "train.py\npyproject.toml\n",
    )
    .expect("synced checkout manifest");
    fs::write(bin_dir.join("uv"), FAKE_UV).expect("fake uv");
    fs::set_permissions(bin_dir.join("uv"), fs::Permissions::from_mode(0o755))
      .expect("fake uv executable permissions");
    let python = Command::new("python3")
      .args(["-c", "import sys; print(sys.executable)"])
      .output()
      .expect("ambient Python executable");
    assert!(
      python.status.success(),
      "ambient Python executable lookup failed"
    );
    let python = String::from_utf8(python.stdout).expect("Python path UTF-8");
    std::os::unix::fs::symlink(python.trim(), bin_dir.join("python"))
      .expect("ambient Python alias");
    let request = RunRequest {
      service: None,
      detach: false,
      name: "train".to_string(),
      command: vec![
        "python".to_string(),
        "train.py".to_string(),
        "argument with spaces".to_string(),
      ],
      environment: EnvironmentConfig {
        base_python: None,
        reuse_packages: Vec::new(),
        require_cuda: false,
        env: BTreeMap::from([("CUSTOM_RUNTIME".to_string(), "active".to_string())]),
      },
      remote_managed: vec!["uv.lock".to_string()],
      extras: vec!["test".to_string()],
      sync_args: vec!["--no-dev".to_string()],
      expected_sync: None,
    };
    let request_path = repo_root.join(".expri/test-request.json");
    fs::write(
      &request_path,
      serde_json::to_vec(&request).expect("request JSON"),
    )
    .expect("request file");
    Self {
      _directory: directory,
      repo_root,
      bin_dir,
      request,
      request_path,
    }
  }

  fn command(&self, backend: Backend, setup: bool) -> Command {
    self.backend_command(backend, if setup { "setup" } else { "run" })
  }

  fn backend_command(&self, backend: Backend, operation: &str) -> Command {
    let mut command = match backend {
      Backend::Native => {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        let test_name = match operation {
          "setup" => "environment::integration_tests::native_setup_child",
          "environment" => "environment::integration_tests::native_environment_child",
          _ => "environment::integration_tests::native_run_child",
        };
        command.args(["--exact", test_name, "--nocapture"]);
        command.env("EXPRI_TEST_NATIVE_REQUEST", &self.request_path);
        command
      }
      Backend::Python => {
        let mut command = Command::new("python3");
        let request_path = self.request_path.to_str().expect("request path UTF-8");
        let script = match operation {
          "setup" => crate::controller::protocol::python_setup_script(request_path),
          "environment" => {
            let request: EnvironmentCommandRequest = serde_json::from_slice(
              &fs::read(&self.request_path).expect("environment command request"),
            )
            .expect("environment command request JSON");
            crate::controller::protocol::python_environment_script(&request)
          }
          _ => crate::controller::protocol::python_run_script(request_path),
        };
        command.args(["-c", &script]);
        command
      }
    };
    let mut paths = vec![self.bin_dir.clone()];
    paths.extend(std::env::split_paths(
      &std::env::var_os("PATH").expect("PATH"),
    ));
    command
      .current_dir(&self.repo_root)
      .env("PATH", std::env::join_paths(paths).expect("test PATH"))
      .env_remove("UV_CACHE_DIR")
      .env_remove("PYTHONUNBUFFERED")
      .env_remove("EXPRI_TEST_CLI_CONFIG")
      .env("EXPRI_TEST_SOURCE_ROOT", &self.repo_root)
      .env("UV_PROJECT", "/ambient/wrong-project")
      .env("UV_PYTHON", "/ambient/wrong-python")
      .env("UV_ACTIVE", "1")
      .env("VIRTUAL_ENV", "/ambient/venv");
    command
  }

  fn execute(&self, backend: Backend, task_exit: i32, prepare_fail: bool) -> Output {
    self
      .command(backend, false)
      .env("EXPRI_TEST_TASK_EXIT", task_exit.to_string())
      .env(
        "EXPRI_TEST_PREPARE_FAIL",
        if prepare_fail { "1" } else { "0" },
      )
      .output()
      .expect("execute run backend")
  }

  fn run_dir(&self) -> PathBuf {
    let paths: Vec<_> = fs::read_dir(self.repo_root.join(".expri/runs"))
      .expect("run directories")
      .map(|entry| entry.expect("run entry").path())
      .collect();
    assert_eq!(paths.len(), 1, "expected exactly one run");
    paths.into_iter().next().expect("run directory")
  }
}

fn read_json(path: &Path) -> Value {
  serde_json::from_slice(&fs::read(path).expect("JSON file")).expect("JSON contents")
}

fn assert_helper_argv(path: &Path) {
  let arguments = read_json(path);
  let arguments = arguments.as_array().expect("helper argv array");
  assert_eq!(&arguments[..2], &[json!("run"), json!("--isolated")]);
  for option in ["--no-project", "--no-config", "--no-env-file"] {
    assert!(
      arguments.contains(&json!(option)),
      "helper missing {option}"
    );
  }
  assert_eq!(
    &arguments[arguments.len() - 3..],
    &[json!("python"), json!("-I"), json!("-c")]
  );
}

fn output_report(output: &Output) -> Value {
  let start = output
    .stdout
    .iter()
    .position(|byte| *byte == b'{')
    .unwrap_or_else(|| {
      panic!(
        "missing JSON report: {}",
        String::from_utf8_lossy(&output.stdout)
      )
    });
  serde_json::Deserializer::from_slice(&output.stdout[start..])
    .into_iter::<Value>()
    .next()
    .expect("JSON report")
    .expect("valid JSON report")
}

fn assert_maintenance_exit(fixture: &Fixture, backend: Backend, output: &Output, expected: i32) {
  match backend {
    Backend::Native => {
      assert!(
        output.status.success(),
        "native child: {}",
        String::from_utf8_lossy(&output.stderr)
      );
      assert_eq!(
        read_json(&fixture.repo_root.join(".expri/maintenance-outcome.json"))["exit_code"],
        expected
      );
    }
    Backend::Python => assert_eq!(
      output.status.code(),
      Some(expected),
      "Python backend: {}",
      String::from_utf8_lossy(&output.stderr)
    ),
  }
}

#[test]
fn native_run_child() {
  let Some(request_path) = std::env::var_os("EXPRI_TEST_NATIVE_REQUEST") else {
    return;
  };
  let request: RunRequest = serde_json::from_slice(&fs::read(request_path).expect("child request"))
    .expect("child request JSON");
  let repo_root = PathBuf::from(std::env::var_os("EXPRI_TEST_SOURCE_ROOT").expect("child root"));
  let result = if let Some(config_path) = std::env::var_os("EXPRI_TEST_CLI_CONFIG") {
    let cli = crate::Cli::try_parse_from([
      std::ffi::OsString::from("expri"),
      std::ffi::OsString::from("run"),
      std::ffi::OsString::from("--config"),
      config_path,
      std::ffi::OsString::from("--repo"),
      repo_root.as_os_str().to_owned(),
      std::ffi::OsString::from("train"),
    ])
    .expect("local run CLI");
    let crate::Command::Run(command) = cli.command else {
      panic!("expected run command");
    };
    crate::run_task(command, cli.target.as_deref(), cli.verbose, cli.quiet)
  } else {
    crate::node::run::apply_request_at(&request, &repo_root)
  };
  let outcome = match result {
    Ok(()) => json!({"exit_code": 0}),
    Err(error) => json!({"exit_code": error.exit_code(), "error": error.to_string()}),
  };
  fs::write(
    repo_root.join(".expri/native-outcome.json"),
    serde_json::to_vec(&outcome).unwrap(),
  )
  .expect("native outcome");
}

#[test]
fn native_setup_child() {
  let Some(request_path) = std::env::var_os("EXPRI_TEST_NATIVE_REQUEST") else {
    return;
  };
  let request: SetupRequest =
    serde_json::from_slice(&fs::read(request_path).expect("child request"))
      .expect("child request JSON");
  let repo_root = PathBuf::from(std::env::var_os("EXPRI_TEST_SOURCE_ROOT").expect("child root"));
  let result = crate::node::setup::apply_request_at(&request, &repo_root);
  let outcome = match result {
    Ok(()) => json!({"exit_code": 0}),
    Err(error) => json!({"exit_code": error.exit_code(), "error": error.to_string()}),
  };
  fs::write(
    repo_root.join(".expri/setup-outcome.json"),
    serde_json::to_vec(&outcome).unwrap(),
  )
  .expect("native setup outcome");
}

#[test]
fn native_environment_child() {
  let Some(request_path) = std::env::var_os("EXPRI_TEST_NATIVE_REQUEST") else {
    return;
  };
  let request: EnvironmentCommandRequest =
    serde_json::from_slice(&fs::read(request_path).expect("child request"))
      .expect("child environment command JSON");
  let repo_root = PathBuf::from(std::env::var_os("EXPRI_TEST_SOURCE_ROOT").expect("child root"));
  let result = crate::node::environment::apply_request_at(&request, &repo_root);
  let outcome = match result {
    Ok(()) => json!({"exit_code": 0}),
    Err(error) => json!({"exit_code": error.exit_code(), "error": error.to_string()}),
  };
  fs::write(
    repo_root.join(".expri/maintenance-outcome.json"),
    serde_json::to_vec(&outcome).unwrap(),
  )
  .expect("native maintenance outcome");
}

#[test]
fn native_and_python_setup_steps_share_prepared_environment_after_bootstrap() {
  for backend in [Backend::Native, Backend::Python] {
    let fixture = Fixture::new();
    fs::write(
      fixture.repo_root.join(".expri/setup-script.sh"),
      SETUP_SCRIPT,
    )
    .expect("setup script");
    let request = SetupRequest {
      state_dir: ".expri".to_string(),
      force: false,
      environment: Some(fixture.request.environment.clone()),
      steps: vec![
        SetupStep::Script {
          path: ".expri/setup-script.sh".to_string(),
          args: vec!["bootstrap".to_string()],
        },
        SetupStep::Uv {
          extras: vec!["test".to_string()],
          args: vec!["--no-dev".to_string()],
        },
        SetupStep::Hf {
          repo: "org/model".to_string(),
          revision: Some("rev-123".to_string()),
          args: vec!["--local-dir".to_string(), "models".to_string()],
        },
        SetupStep::Script {
          path: ".expri/setup-script.sh".to_string(),
          args: vec!["prepared".to_string()],
        },
      ],
    };
    fs::write(
      &fixture.request_path,
      serde_json::to_vec(&request).expect("setup request JSON"),
    )
    .expect("setup request file");
    let output = fixture
      .command(backend, true)
      .output()
      .expect("setup output");
    assert!(
      output.status.success(),
      "{backend:?}: {}",
      String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> =
      fs::read_to_string(fixture.repo_root.join(".expri/setup-events.jsonl"))
        .expect("setup events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event JSON"))
        .collect();
    assert_eq!(
      events
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect::<Vec<_>>(),
      ["bootstrap", "prepare", "hf", "prepared"]
    );
    for event in &events {
      assert_eq!(event["cwd"], fixture.repo_root.to_string_lossy().as_ref());
      assert_eq!(event["env"]["CUSTOM_RUNTIME"], "active");
    }
    assert_eq!(events[0]["env"]["VIRTUAL_ENV"], "/ambient/venv");
    assert_eq!(events[0]["env"]["UV_PROJECT"], "/ambient/wrong-project");
    assert!(events[0]["env"]["UV_PROJECT_ENVIRONMENT"].is_null());
    for event in &events[1..] {
      for key in ["UV_PROJECT", "UV_PYTHON", "UV_ACTIVE", "VIRTUAL_ENV"] {
        assert!(
          event["env"][key].is_null(),
          "{backend:?} setup leaked {key}"
        );
      }
    }
    let environment_path = fixture.repo_root.join(".expri/environment/.venv");
    for event in &events[2..] {
      assert_eq!(
        event["env"]["UV_PROJECT_ENVIRONMENT"],
        environment_path.to_string_lossy().as_ref()
      );
      assert_eq!(event["env"]["PYTHONNOUSERSITE"], "1");
    }
    assert_eq!(
      events[3]["python"],
      environment_path
        .join("bin/python")
        .to_string_lossy()
        .as_ref()
    );
    assert_eq!(
      events[2]["argv"],
      json!([
        "run",
        "--no-sync",
        "--no-env-file",
        "hf",
        "download",
        "org/model",
        "--revision",
        "rev-123",
        "--local-dir",
        "models"
      ])
    );
    let helper = read_json(&fixture.repo_root.join(".expri/helper-request.json"));
    assert_helper_argv(&fixture.repo_root.join(".expri/helper-argv.json"));
    assert_eq!(helper["operation"], "setup");
    assert_eq!(helper["install_project"], false);
    assert_eq!(
      helper["repo_root"],
      fixture.repo_root.to_string_lossy().as_ref()
    );
    assert_eq!(helper["extras"], json!(["test"]));
    assert_eq!(helper["sync_args"], json!(["--no-dev"]));
    assert_eq!(
      read_json(&fixture.repo_root.join(".expri/setup-state.json")),
      serde_json::to_value(request).unwrap()
    );
  }
}

#[test]
fn native_and_python_runs_preserve_snapshot_request_environment_and_outputs() {
  for backend in [Backend::Native, Backend::Python] {
    let fixture = Fixture::new();
    let output = fixture.execute(backend, 0, false);
    assert!(
      output.status.success(),
      "{backend:?}: {}",
      String::from_utf8_lossy(&output.stderr)
    );
    let run_dir = fixture.run_dir();
    let state = read_json(&run_dir.join("run-state.json"));
    assert_eq!(state["status"], "completed");
    assert_eq!(state["exit_code"], 0);
    assert_eq!(state["task_exit_code"], 0);
    assert_eq!(state["schema_version"], 1);
    assert_eq!(
      state["logs"],
      json!({"stdout": "logs/stdout.log", "stderr": "logs/stderr.log"})
    );
    assert_eq!(
      fs::read(run_dir.join("logs/stdout.log")).unwrap(),
      b"task stdout\x00\xff\n"
    );
    assert_eq!(
      fs::read(run_dir.join("logs/stderr.log")).unwrap(),
      b"preparation stderr\x00\xff\ntask stderr\x80\x00\n"
    );
    assert!(
      output
        .stdout
        .windows(b"task stdout\x00\xff\n".len())
        .any(|bytes| bytes == b"task stdout\x00\xff\n")
    );
    assert!(
      output
        .stderr
        .windows(b"preparation stderr\x00\xff\n".len())
        .any(|bytes| bytes == b"preparation stderr\x00\xff\n")
    );
    assert!(
      output
        .stderr
        .windows(b"task stderr\x80\x00\n".len())
        .any(|bytes| bytes == b"task stderr\x80\x00\n")
    );
    assert!(state["finished_at"].is_string());
    let helper = read_json(&run_dir.join("helper-request.json"));
    assert_helper_argv(&run_dir.join("helper-argv.json"));
    assert_eq!(helper["operation"], "run");
    assert_eq!(helper["install_project"], true);
    assert_eq!(
      helper["repo_root"],
      run_dir.join("code").to_string_lossy().as_ref()
    );
    assert_eq!(helper["state_dir"], run_dir.to_string_lossy().as_ref());
    assert_eq!(helper["extras"], json!(fixture.request.extras));
    assert_eq!(helper["sync_args"], json!(fixture.request.sync_args));
    assert_eq!(
      helper["environment"],
      serde_json::to_value(&fixture.request.environment).unwrap()
    );
    let observation = read_json(&run_dir.join("outputs/task-observation.json"));
    assert_eq!(
      observation["cwd"],
      run_dir.join("code").to_string_lossy().as_ref()
    );
    assert_eq!(observation["source"], "original source");
    assert_eq!(
      fs::read_to_string(fixture.repo_root.join("train.py")).unwrap(),
      "later source"
    );
    assert_eq!(
      fs::read_to_string(run_dir.join("code/uv.lock")).unwrap(),
      "version = 1\n"
    );
    let mut expected_argv = vec!["run", "--no-sync", "--no-env-file", "--"]
      .into_iter()
      .map(str::to_string)
      .collect::<Vec<_>>();
    expected_argv.extend(fixture.request.command);
    assert_eq!(observation["argv"], json!(expected_argv));
    let env = &observation["env"];
    assert_eq!(env["EXPRI_RUN_ID"], state["run_id"]);
    assert_eq!(env["EXPRI_RUN_DIR"], run_dir.to_string_lossy().as_ref());
    assert_eq!(
      env["EXPRI_OUTPUT_DIR"],
      run_dir.join("outputs").to_string_lossy().as_ref()
    );
    assert_eq!(
      env["UV_PROJECT_ENVIRONMENT"],
      run_dir.join("environment/.venv").to_string_lossy().as_ref()
    );
    assert_eq!(env["PYTHONNOUSERSITE"], "1");
    assert_eq!(env["CUSTOM_RUNTIME"], "active");
    assert_eq!(env["PYTHONUNBUFFERED"], "1");
    for key in ["UV_PROJECT", "UV_PYTHON", "UV_ACTIVE", "VIRTUAL_ENV"] {
      assert!(env[key].is_null(), "{backend:?} leaked {key}");
    }
  }
}

#[test]
fn native_and_python_runs_preserve_task_failure_exit_code() {
  for backend in [Backend::Native, Backend::Python] {
    let fixture = Fixture::new();
    let output = fixture.execute(backend, 7, false);
    if matches!(backend, Backend::Python) {
      assert_eq!(output.status.code(), Some(7));
    }
    let state = read_json(&fixture.run_dir().join("run-state.json"));
    assert_eq!(state["status"], "failed");
    assert_eq!(state["exit_code"], 7);
    assert_eq!(state["task_exit_code"], 7);
    assert!(state["error"].is_string());
    assert!(state["finished_at"].is_string());
    if matches!(backend, Backend::Native) {
      assert_eq!(
        read_json(&fixture.repo_root.join(".expri/native-outcome.json"))["exit_code"],
        7
      );
    }
  }
}

#[test]
fn native_and_python_runs_preserve_configured_python_buffering() {
  for backend in [Backend::Native, Backend::Python] {
    let mut fixture = Fixture::new();
    fixture
      .request
      .environment
      .env
      .insert("PYTHONUNBUFFERED".to_string(), "0".to_string());
    fs::write(
      &fixture.request_path,
      serde_json::to_vec(&fixture.request).unwrap(),
    )
    .expect("configured buffering request");
    let output = fixture.execute(backend, 0, false);
    assert!(
      output.status.success(),
      "{backend:?}: {}",
      String::from_utf8_lossy(&output.stderr)
    );
    let observation = read_json(&fixture.run_dir().join("outputs/task-observation.json"));
    assert_eq!(observation["env"]["PYTHONUNBUFFERED"], "0");
  }
}

#[test]
fn native_and_python_signal_failures_keep_shell_exit_code_and_release_run_lease() {
  for backend in [Backend::Native, Backend::Python] {
    let fixture = Fixture::new();
    let output = fixture
      .command(backend, false)
      .env("EXPRI_TEST_TASK_SIGNAL", "1")
      .output()
      .expect("signaled task output");
    match backend {
      Backend::Native => {
        assert!(
          output.status.success(),
          "native run child: {}",
          String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
          read_json(&fixture.repo_root.join(".expri/native-outcome.json"))["exit_code"],
          143
        );
      }
      Backend::Python => assert_eq!(output.status.code(), Some(143)),
    }
    let run_dir = fixture.run_dir();
    let state = read_json(&run_dir.join("run-state.json"));
    assert_eq!(state["status"], "failed");
    assert_eq!(state["exit_code"], 143);
    assert_eq!(state["task_exit_code"], 143);
    assert_eq!(
      fs::read(run_dir.join("logs/stdout.log")).unwrap(),
      b"task stdout\x00\xff\n"
    );
    assert_eq!(
      fs::read(run_dir.join("logs/stderr.log")).unwrap(),
      b"preparation stderr\x00\xff\ntask stderr\x80\x00\n"
    );
    assert!(state["finished_at"].is_string());
    assert!(run_dir.join(".run.lock").is_file());
    let lease =
      crate::lock::try_lock_file(&run_dir.join(".run.lock"), false).expect("released run lease");
    let crate::lock::LockAttempt::Acquired(lease) = lease else {
      panic!("signaled task retained run lease");
    };
    drop(lease);
    let report = crate::environment::maintenance::prune(
      &fixture.repo_root,
      &PruneRequest {
        apply: true,
        keep_last: 0,
      },
    )
    .expect("prune signaled failed run");
    assert_eq!(
      report.pruned_runs, 1,
      "{backend:?} prune report: {report:?}"
    );
    assert!(!run_dir.join("environment/.venv").exists());
    assert!(run_dir.join("code/train.py").is_file());
    assert!(run_dir.join("outputs/task-observation.json").is_file());
    assert_eq!(read_json(&run_dir.join("run-state.json")), state);
  }
}

#[test]
fn native_and_python_runs_record_preparation_failure_without_launching_task() {
  for backend in [Backend::Native, Backend::Python] {
    let fixture = Fixture::new();
    let output = fixture.execute(backend, 0, true);
    if matches!(backend, Backend::Python) {
      assert_eq!(output.status.code(), Some(19));
    }
    let run_dir = fixture.run_dir();
    let state = read_json(&run_dir.join("run-state.json"));
    assert_eq!(state["status"], "failed");
    assert_eq!(state["exit_code"], 19);
    assert!(state["finished_at"].is_string());
    assert!(state["error"].is_string());
    assert!(!run_dir.join("outputs/task-observation.json").exists());
    assert!(state["environment_manifest"].is_null());
    assert!(state["task_exit_code"].is_null());
    assert!(
      fs::read(run_dir.join("logs/stdout.log"))
        .unwrap()
        .is_empty()
    );
    assert_eq!(
      fs::read(run_dir.join("logs/stderr.log")).unwrap(),
      b"preparation stderr\x00\xff\n"
    );
    assert!(
      output
        .stderr
        .windows(b"preparation stderr\x00\xff\n".len())
        .any(|bytes| bytes == b"preparation stderr\x00\xff\n")
    );
    let lease = crate::lock::try_lock_file(&run_dir.join(".run.lock"), false)
      .expect("released preparation lease");
    let crate::lock::LockAttempt::Acquired(lease) = lease else {
      panic!("failed preparation retained run lease");
    };
    drop(lease);
    if matches!(backend, Backend::Native) {
      assert_eq!(
        read_json(&fixture.repo_root.join(".expri/native-outcome.json"))["exit_code"],
        19
      );
    }
  }
}

#[test]
fn native_and_python_preparation_signals_preserve_status_for_run_setup_and_doctor() {
  for backend in [Backend::Native, Backend::Python] {
    for operation in ["run", "setup", "doctor"] {
      let fixture = Fixture::new();
      match operation {
        "setup" => {
          let request = SetupRequest {
            state_dir: ".expri".to_string(),
            force: false,
            environment: Some(fixture.request.environment.clone()),
            steps: vec![SetupStep::Uv {
              extras: fixture.request.extras.clone(),
              args: fixture.request.sync_args.clone(),
            }],
          };
          fs::write(&fixture.request_path, serde_json::to_vec(&request).unwrap())
            .expect("signaled setup request");
        }
        "doctor" => {
          let request = EnvironmentCommandRequest {
            json: true,
            action: EnvironmentAction::Doctor(DoctorRequest {
              environment: fixture.request.environment.clone(),
              extras: fixture.request.extras.clone(),
              sync_args: fixture.request.sync_args.clone(),
            }),
          };
          fs::write(&fixture.request_path, serde_json::to_vec(&request).unwrap())
            .expect("signaled doctor request");
        }
        _ => {}
      }
      let output = fixture
        .backend_command(
          backend,
          if operation == "doctor" {
            "environment"
          } else {
            operation
          },
        )
        .env("EXPRI_TEST_HELPER_SIGNAL", "1")
        .output()
        .expect("signaled preparation output");
      match backend {
        Backend::Native => {
          assert!(
            output.status.success(),
            "native {operation} child: {}",
            String::from_utf8_lossy(&output.stderr)
          );
          let outcome_path = match operation {
            "setup" => ".expri/setup-outcome.json",
            "doctor" => ".expri/maintenance-outcome.json",
            _ => ".expri/native-outcome.json",
          };
          assert_eq!(
            read_json(&fixture.repo_root.join(outcome_path))["exit_code"],
            143,
            "{backend:?}/{operation}"
          );
        }
        Backend::Python => assert_eq!(
          output.status.code(),
          Some(143),
          "{backend:?}/{operation}: {}",
          String::from_utf8_lossy(&output.stderr)
        ),
      }
      let helper_dir = if operation == "run" {
        let run_dir = fixture.run_dir();
        let state = read_json(&run_dir.join("run-state.json"));
        assert_eq!(state["status"], "failed");
        assert_eq!(state["exit_code"], 143);
        assert!(state["finished_at"].is_string());
        assert!(state["error"].is_string());
        assert!(state["environment_manifest"].is_null());
        assert!(!run_dir.join("outputs/task-observation.json").exists());
        let lease = crate::lock::try_lock_file(&run_dir.join(".run.lock"), false)
          .expect("preparation failure releases run lease");
        let crate::lock::LockAttempt::Acquired(lease) = lease else {
          panic!("signaled preparation retained run lease");
        };
        drop(lease);
        run_dir
      } else {
        assert!(!fixture.repo_root.join(".expri/runs").exists());
        assert!(!fixture.repo_root.join(".expri/setup-state.json").exists());
        fixture.repo_root.join(".expri")
      };
      assert_eq!(
        read_json(&helper_dir.join("helper-request.json"))["operation"],
        operation
      );
      assert!(!helper_dir.join("environment").exists());
    }
  }
}

#[test]
fn local_cli_run_captures_configured_ignored_file_in_code_snapshot() {
  let fixture = Fixture::new();
  fs::remove_file(fixture.repo_root.join(".expri/checkout.manifest"))
    .expect("select Git source instead of synced source");
  let init = Command::new("git")
    .args(["init", "--quiet"])
    .current_dir(&fixture.repo_root)
    .output()
    .expect("initialize Git source");
  assert!(init.status.success(), "Git initialization failed");
  fs::write(
    fixture.repo_root.join(".gitignore"),
    "ignored.cfg\nnot-selected.cfg\n",
  )
  .expect("ignore file");
  fs::write(
    fixture.repo_root.join("ignored.cfg"),
    "explicit experiment settings",
  )
  .expect("configured ignored file");
  fs::write(
    fixture.repo_root.join("not-selected.cfg"),
    "unrelated ignored file",
  )
  .expect("unselected ignored file");
  let ignored = Command::new("git")
    .args(["check-ignore", "ignored.cfg"])
    .current_dir(&fixture.repo_root)
    .output()
    .expect("confirm source is ignored");
  assert!(ignored.status.success(), "fixture file must be Git-ignored");
  let config_path = fixture.repo_root.join("expri.toml");
  fs::write(
    &config_path,
    r#"
[environment]

[environment.env]
CUSTOM_RUNTIME = "active"

[push]
include_ignored = ["ignored.cfg"]
remote_managed = ["uv.lock"]

[tasks]
train = ["python", "train.py"]
"#,
  )
  .expect("CLI project configuration");
  let output = fixture
    .command(Backend::Native, false)
    .env("EXPRI_TEST_CLI_CONFIG", config_path)
    .output()
    .expect("local CLI run output");
  assert!(
    output.status.success(),
    "local CLI child: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  let run_dir = fixture.run_dir();
  let state = read_json(&run_dir.join("run-state.json"));
  assert_eq!(state["status"], "completed");
  assert_eq!(state["exit_code"], 0);
  assert_eq!(
    fs::read_to_string(run_dir.join("code/ignored.cfg")).expect("snapshot ignored file"),
    "explicit experiment settings"
  );
  assert!(!run_dir.join("code/not-selected.cfg").exists());
}

#[test]
fn native_and_python_run_caches_stay_in_original_checkout_across_snapshots() {
  for backend in [Backend::Native, Backend::Python] {
    for selection in ["default", "ambient", "explicit"] {
      let mut fixture = Fixture::new();
      let expected_cache = fixture.repo_root.join(match selection {
        "ambient" => "ambient-cache",
        "explicit" => "explicit-cache",
        _ => ".expri/cache/uv",
      });
      if selection == "explicit" {
        fixture
          .request
          .sync_args
          .extend(["--cache-dir".to_string(), "explicit-cache".to_string()]);
        fs::write(
          &fixture.request_path,
          serde_json::to_vec(&fixture.request).unwrap(),
        )
        .expect("cache request");
      }
      for _ in 0..2 {
        let mut command = fixture.command(backend, false);
        if selection != "default" {
          command.env("UV_CACHE_DIR", "ambient-cache");
        }
        let output = command.output().expect("cached run output");
        assert!(
          output.status.success(),
          "{backend:?}/{selection}: {}",
          String::from_utf8_lossy(&output.stderr)
        );
      }
      let runs: Vec<_> = fs::read_dir(fixture.repo_root.join(".expri/runs"))
        .expect("cache run directories")
        .map(|entry| entry.expect("run entry").path())
        .collect();
      assert_eq!(runs.len(), 2);
      assert_ne!(runs[0], runs[1]);
      for run_dir in &runs {
        assert_eq!(
          read_json(&run_dir.join("run-state.json"))["status"],
          "completed"
        );
        let helper = read_json(&run_dir.join("helper-request.json"));
        assert_eq!(
          helper["cache_dir"],
          expected_cache.to_string_lossy().as_ref(),
          "{backend:?}/{selection}"
        );
        assert_eq!(
          read_json(&run_dir.join("helper-environment.json"))["env"]["UV_CACHE_DIR"],
          helper["cache_dir"]
        );
        let task = read_json(&run_dir.join("outputs/task-observation.json"));
        assert_eq!(task["env"]["UV_CACHE_DIR"], helper["cache_dir"]);
        assert_eq!(
          task["env"]["UV_PROJECT_ENVIRONMENT"],
          run_dir.join("environment/.venv").to_string_lossy().as_ref()
        );
      }
      assert_eq!(
        read_json(&runs[0].join("helper-request.json"))["cache_dir"],
        read_json(&runs[1].join("helper-request.json"))["cache_dir"]
      );
    }
  }
}

#[test]
fn native_and_python_doctor_preserve_json_report_without_preparing_environment() {
  for backend in [Backend::Native, Backend::Python] {
    for compatible in [true, false] {
      let fixture = Fixture::new();
      let request = EnvironmentCommandRequest {
        json: true,
        action: EnvironmentAction::Doctor(DoctorRequest {
          environment: fixture.request.environment.clone(),
          extras: fixture.request.extras.clone(),
          sync_args: fixture.request.sync_args.clone(),
        }),
      };
      fs::write(&fixture.request_path, serde_json::to_vec(&request).unwrap())
        .expect("doctor request");
      let output = fixture
        .backend_command(backend, "environment")
        .env("EXPRI_TEST_DOCTOR_FAIL", if compatible { "0" } else { "1" })
        .output()
        .expect("doctor output");
      assert_maintenance_exit(&fixture, backend, &output, if compatible { 0 } else { 1 });
      let report = output_report(&output);
      assert_eq!(report["scope"], "base_and_lock");
      assert_eq!(report["compatible"], compatible);
      assert_eq!(report["base_python"], "/opt/conda/bin/python");
      assert_eq!(report["reused_packages"], json!(["numpy", "torch"]));
      assert_eq!(
        report["cache"]["directory"],
        fixture
          .repo_root
          .join(".expri/cache/uv")
          .to_string_lossy()
          .as_ref()
      );
      assert_eq!(
        report["issues"].as_array().unwrap().len(),
        if compatible { 0 } else { 1 }
      );
      if !compatible {
        assert_eq!(report["issues"][0]["kind"], "locked_version_mismatch");
      }
      let helper = read_json(&fixture.repo_root.join(".expri/helper-request.json"));
      assert_eq!(helper["operation"], "doctor");
      assert_eq!(helper["install_project"], false);
      assert_eq!(helper["extras"], json!(fixture.request.extras));
      assert_eq!(helper["sync_args"], json!(fixture.request.sync_args));
      assert_eq!(
        read_json(&fixture.repo_root.join(".expri/helper-environment.json"))["env"]["UV_CACHE_DIR"],
        helper["cache_dir"]
      );
      assert_helper_argv(&fixture.repo_root.join(".expri/helper-argv.json"));
      assert!(!fixture.repo_root.join(".expri/environment").exists());
      assert!(!fixture.repo_root.join(".expri/runs").exists());
    }
  }
}

fn finished_environment(root: &Path, run_id: &str, status: &str, timestamp: &str) -> PathBuf {
  let run_dir = root.join(".expri/runs").join(run_id);
  fs::create_dir_all(run_dir.join("code")).expect("prune code directory");
  fs::create_dir_all(run_dir.join("outputs")).expect("prune output directory");
  fs::create_dir_all(run_dir.join("environment/.venv")).expect("prune environment directory");
  fs::write(run_dir.join("code/train.py"), "preserved source").expect("prune source");
  fs::write(run_dir.join("outputs/checkpoint.bin"), "preserved result").expect("prune output");
  fs::write(
    run_dir.join("environment/.venv/package.bin"),
    "cached package bytes",
  )
  .expect("prune package");
  fs::write(
    run_dir.join("environment/owner.json"),
    serde_json::to_vec(&json!({
      "schema_version": 1, "repo_root": run_dir.join("code"),
    }))
    .unwrap(),
  )
  .expect("prune owner");
  fs::write(run_dir.join("run-state.json"), serde_json::to_vec(&json!({
    "run_id": run_id, "status": status, "code_dir": run_dir.join("code"), "finished_at": timestamp,
  })).unwrap()).expect("prune run state");
  run_dir
}

#[test]
fn native_and_python_prune_preview_and_apply_preserve_run_artifacts() {
  let mut reports = Vec::new();
  for backend in [Backend::Native, Backend::Python] {
    let fixture = Fixture::new();
    let old = finished_environment(
      &fixture.repo_root,
      "run-old",
      "completed",
      "2026-10-02T01:00:00Z",
    );
    let newest = finished_environment(
      &fixture.repo_root,
      "run-new",
      "failed",
      "2026-10-02T02:00:00Z",
    );
    let active = finished_environment(
      &fixture.repo_root,
      "run-active",
      "preparing",
      "2026-10-02T03:00:00Z",
    );
    for apply in [false, true] {
      let request = EnvironmentCommandRequest {
        json: true,
        action: EnvironmentAction::Prune(PruneRequest {
          apply,
          keep_last: 1,
        }),
      };
      fs::write(&fixture.request_path, serde_json::to_vec(&request).unwrap())
        .expect("prune request");
      let output = fixture
        .backend_command(backend, "environment")
        .output()
        .expect("prune output");
      assert_maintenance_exit(&fixture, backend, &output, 0);
      let report = output_report(&output);
      assert_eq!(report["apply"], apply);
      assert_eq!(report["keep_last"], 1);
      assert_eq!(report["pruned_runs"], if apply { 1 } else { 0 });
      let runs = report["runs"].as_array().expect("reported runs");
      let entry = |id: &str| {
        runs
          .iter()
          .find(|entry| entry["run_id"] == id)
          .expect("run report")
      };
      assert_eq!(
        entry("run-old")["action"],
        if apply { "pruned" } else { "preview" }
      );
      assert_eq!(entry("run-new")["action"], "kept");
      assert_eq!(entry("run-active")["action"], "skipped");
      assert_eq!(report["logical_bytes"], "cached package bytes".len());
      assert_eq!(old.join("environment/.venv").exists(), !apply);
      assert!(newest.join("environment/.venv").is_dir());
      assert!(active.join("environment/.venv").is_dir());
      for run_dir in [&old, &newest, &active] {
        assert_eq!(
          fs::read_to_string(run_dir.join("code/train.py")).unwrap(),
          "preserved source"
        );
        assert_eq!(
          fs::read_to_string(run_dir.join("outputs/checkpoint.bin")).unwrap(),
          "preserved result"
        );
        assert!(run_dir.join("run-state.json").is_file());
        assert!(run_dir.join("environment/owner.json").is_file());
      }
      reports.push(report);
    }
  }
  assert_eq!(
    reports[0], reports[2],
    "prune previews differ between protocols"
  );
  assert_eq!(
    reports[1], reports[3],
    "prune apply reports differ between protocols"
  );
}
