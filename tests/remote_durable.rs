#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const UV: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if args[:2] == ['run', '--isolated']:
  request = json.loads(args[-1])
  root = pathlib.Path(request['state_dir'])
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

const TASK: &str = r#"import os, pathlib, signal, sys, time
out = pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'])
if sys.argv[1] == 'cancel':
  signal.signal(signal.SIGTERM, signal.SIG_IGN)
(out / 'started').write_text(str(pathlib.Path.cwd()))
os.write(1, b'first\x00\xff\n')
os.write(2, b'error\x80\n')
deadline = time.monotonic() + 20
while not (out / 'release').exists() and time.monotonic() < deadline:
  time.sleep(0.025)
(out / 'checkpoint.pt').write_bytes(b'checkpoint')
raise SystemExit(7)
"#;

struct Fixture {
  _root: tempfile::TempDir,
  local: PathBuf,
  remote: PathBuf,
  bin: PathBuf,
}

impl Fixture {
  fn new(protocol: &str, old_node: bool) -> Self {
    let root = tempfile::Builder::new()
      .prefix("expri remote 'quoted' ")
      .tempdir()
      .unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let local = base.join("local");
    let remote = base.join("remote project");
    let bin = base.join("bin");
    let home = base.join("home");
    fs::create_dir_all(&local).unwrap();
    fs::create_dir_all(remote.join(".expri")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&home).unwrap();
    executable(&bin.join("uv"), UV);
    executable(
      &bin.join("rsync"),
      r#"#!/usr/bin/env python3
import pathlib, shlex, shutil, sys
source, destination = sys.argv[-2:]
destination = shlex.split(destination.split(':', 1)[1])[0]
if pathlib.Path(source).is_dir():
  shutil.copytree(source, destination, dirs_exist_ok=True)
else:
  shutil.copyfile(source, destination)
if '--progress' in sys.argv:
  print('fake-rsync-progress')
"#,
    );
    executable(
      &bin.join("ctl"),
      &format!(
        "#!/bin/sh\nHOME={}; export HOME\nfor argument do remote_command=$argument; done\nexec /bin/sh -c \"$remote_command\"\n",
        quote(&home)
      ),
    );
    let node = if old_node {
      let path = bin.join("old-expri");
      executable(&path, "#!/bin/sh\nexit 1\n");
      path
    } else {
      PathBuf::from(env!("CARGO_BIN_EXE_expri"))
    };
    fs::write(home.join(".profile"), "echo login-banner\n").unwrap();
    fs::write(remote.join("train.py"), TASK).unwrap();
    fs::write(
      remote.join("pyproject.toml"),
      "[project]\nname='demo'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(remote.join("uv.lock"), "version=1\n").unwrap();
    fs::write(
      remote.join(".expri/checkout.manifest"),
      "train.py\npyproject.toml\nuv.lock\n",
    )
    .unwrap();
    fs::write(local.join("expri.toml"), format!(
      "[environment]\n[tasks]\ntrain=['python3', 'train.py']\n[target.gpu]\nhost='gpu'\ntransport='ctl'\nremote_dir={}\nctl_bin={}\nprotocol={}\nnode_bin={}\n",
      serde_json::to_string(&remote.to_string_lossy()).unwrap(),
      serde_json::to_string(&bin.join("ctl").to_string_lossy()).unwrap(),
      serde_json::to_string(protocol).unwrap(),
      serde_json::to_string(&node.to_string_lossy()).unwrap(),
    )).unwrap();
    Self {
      _root: root,
      local,
      remote,
      bin,
    }
  }

  fn command(&self, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_expri"));
    command
      .current_dir(&self.local)
      .args(["-T", "gpu"])
      .args(args)
      .env(
        "PATH",
        format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
      );
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

  fn start(&self, mode: &str) -> Value {
    let output = self.run(&["run", "--no-push", "--detach", "train", mode]);
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["detached"], true);
    assert!(
      Path::new(receipt["run_dir"].as_str().unwrap()).starts_with(self.remote.join(".expri/runs"))
    );
    receipt
  }

  fn prepare_local_source(&self) {
    for file in ["train.py", "pyproject.toml", "uv.lock"] {
      fs::copy(self.remote.join(file), self.local.join(file)).unwrap();
    }
    for args in [
      vec!["init", "--quiet"],
      vec!["add", "train.py", "pyproject.toml", "uv.lock", "expri.toml"],
      vec![
        "-c",
        "user.name=Expri Test",
        "-c",
        "user.email=expri@example.com",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--quiet",
        "-m",
        "fixture",
      ],
    ] {
      let output = Command::new("git")
        .current_dir(&self.local)
        .args(args)
        .output()
        .unwrap();
      assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
      );
    }
  }

  fn status(&self, id: &str) -> Value {
    serde_json::from_slice(&self.run(&["runs", "status", id, "--json"]).stdout).unwrap()
  }

  fn terminal(&self, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let report = self.status(id);
      if matches!(
        report["status"].as_str(),
        Some("completed" | "failed" | "cancelled" | "lost")
      ) {
        return report;
      }
      assert!(Instant::now() < deadline, "run did not finish: {report}");
      thread::sleep(Duration::from_millis(50));
    }
  }
}

fn executable(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap();
  fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn quote(path: &Path) -> String {
  format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn await_file(path: &Path) {
  let deadline = Instant::now() + Duration::from_secs(10);
  while !path.is_file() {
    assert!(Instant::now() < deadline, "missing {}", path.display());
    thread::sleep(Duration::from_millis(25));
  }
}

#[test]
fn remote_backends_detach_follow_logs_finish_and_cancel_without_profile_contamination() {
  for (protocol, old_node) in [("expri-node", false), ("python", false), ("auto", true)] {
    let fixture = Fixture::new(protocol, old_node);
    let receipt = fixture.start("finish");
    let id = receipt["run_id"].as_str().unwrap();
    let run = Path::new(receipt["run_dir"].as_str().unwrap());
    await_file(&run.join("outputs/started"));
    assert_eq!(fixture.status(id)["status"], "running");
    assert_eq!(
      fs::read_to_string(run.join("outputs/started")).unwrap(),
      run.join("code").to_string_lossy()
    );
    let mut follower = fixture
      .command(&["runs", "logs", id, "--follow"])
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(follower.try_wait().unwrap().is_none());
    fs::write(run.join("outputs/release"), "finish").unwrap();
    let followed = follower.wait_with_output().unwrap();
    assert!(
      followed.status.success(),
      "{}",
      String::from_utf8_lossy(&followed.stderr)
    );
    assert_eq!(followed.stdout, b"first\x00\xff\n");
    let terminal = fixture.terminal(id);
    assert_eq!(terminal["status"], "failed");
    assert_eq!(terminal["state"]["exit_code"], 7);
    assert_eq!(
      fs::read(run.join("outputs/checkpoint.pt")).unwrap(),
      b"checkpoint"
    );
    assert_eq!(
      fixture
        .run(&["runs", "logs", id, "--stream", "stderr"])
        .stdout,
      b"error\x80\n"
    );

    let receipt = fixture.start("cancel");
    let id = receipt["run_id"].as_str().unwrap();
    let run = Path::new(receipt["run_dir"].as_str().unwrap());
    await_file(&run.join("outputs/started"));
    let requested: Value =
      serde_json::from_slice(&fixture.run(&["runs", "cancel", id, "--json"]).stdout).unwrap();
    assert_eq!(requested["cancel_requested"], true);
    let terminal = fixture.terminal(id);
    assert_eq!(terminal["status"], "cancelled");
    assert_eq!(terminal["state"]["exit_code"], 130);
    assert!(!run.join("outputs/checkpoint.pt").exists());
  }
}

#[test]
fn detached_default_sync_and_verbose_transfer_keep_stdout_a_json_receipt() {
  for protocol in ["expri-node", "python"] {
    let fixture = Fixture::new(protocol, false);
    fixture.prepare_local_source();
    let output = fixture.run(&["-v", "run", "--detach", "train", "finish"]);
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["detached"], true);
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostics.contains("login-banner"));
    assert!(diagnostics.contains("fake-rsync-progress"));
    let id = receipt["run_id"].as_str().unwrap();
    let run = Path::new(receipt["run_dir"].as_str().unwrap());
    await_file(&run.join("outputs/started"));
    fs::write(run.join("outputs/release"), "finish").unwrap();
    assert_eq!(fixture.terminal(id)["state"]["exit_code"], 7);
  }
}
