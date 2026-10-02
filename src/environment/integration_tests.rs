use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use clap::Parser;
use serde_json::{Value, json};

use crate::config::EnvironmentConfig;
use crate::protocol::{RunRequest, SetupRequest, SetupStep};

const FAKE_UV: &str = r#"#!/usr/bin/env python3
import json
import os
from pathlib import Path
import subprocess
import sys

def observation(kind, arguments):
  return {
    "kind": kind,
    "argv": arguments,
    "cwd": str(Path.cwd()),
    "env": {key: os.environ.get(key) for key in [
      "UV_PROJECT_ENVIRONMENT", "PYTHONNOUSERSITE", "CUSTOM_RUNTIME",
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
  if os.environ.get("EXPRI_TEST_PREPARE_FAIL") == "1":
    sys.exit(19)
  environment_path = run_dir / "environment" / ".venv"
  environment_path.mkdir(parents=True)
  (environment_path / "bin").mkdir()
  (environment_path / "bin" / "python").symlink_to(sys.executable)
  manifest = run_dir / "environment" / "environment-state.json"
  manifest.write_text("{}")
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
      "EXPRI_RUN_ID", "EXPRI_RUN_DIR", "EXPRI_OUTPUT_DIR", "UV_PROJECT_ENVIRONMENT",
      "PYTHONNOUSERSITE", "CUSTOM_RUNTIME", "UV_PROJECT", "UV_PYTHON", "UV_ACTIVE", "VIRTUAL_ENV",
    ]},
  }
  output_dir = Path(os.environ["EXPRI_OUTPUT_DIR"])
  (output_dir / "task-observation.json").write_text(json.dumps(result))
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
    let mut command = match backend {
      Backend::Native => {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        let test_name = if setup {
          "environment::integration_tests::native_setup_child"
        } else {
          "environment::integration_tests::native_run_child"
        };
        command.args(["--exact", test_name, "--nocapture"]);
        command.env("EXPRI_TEST_NATIVE_REQUEST", &self.request_path);
        command
      }
      Backend::Python => {
        let mut command = Command::new("python3");
        let request_path = self.request_path.to_str().expect("request path UTF-8");
        let script = if setup {
          crate::controller::protocol::python_setup_script(request_path)
        } else {
          crate::controller::protocol::python_run_script(request_path)
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
  crate::node::setup::apply_request_at(&request, &repo_root).expect("native setup result");
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
    if matches!(backend, Backend::Native) {
      assert_eq!(
        read_json(&fixture.repo_root.join(".expri/native-outcome.json"))["exit_code"],
        19
      );
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

[sync]
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
