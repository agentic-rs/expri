#![cfg(unix)]

//! Real uv/SSH/rsync acceptance checks for the isolated CI host and worker images.
//! These are intentionally ignored by the ordinary unit suite; container CI runs
//! them explicitly, with every prerequisite mandatory.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TRAIN: &str = r#"import json, os, pathlib, subprocess, sys, time
import torch
import fixture_extra
from expri_metrics import MetricsLogger
out = pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'])
mode, rate = sys.argv[1], float(sys.argv[2])
wrapper = json.loads(subprocess.check_output(['torchrun', '--fixture-check'], text=True))
report = {
  'python': sys.executable, 'cwd': str(pathlib.Path.cwd()),
  'torch_version': torch.__version__, 'torch_origin': torch.__file__,
  'extra_origin': fixture_extra.__file__, 'wrapper': wrapper,
  'cache_dir': os.environ['UV_CACHE_DIR'],
  'environment_path': os.environ['UV_PROJECT_ENVIRONMENT'],
  'probe': (torch.ones(1, device='cpu') + 1).item(),
}
(out / 'report.json').write_text(json.dumps(report))
(out / 'started').write_text(mode)
print('training started ' + mode, flush=True)
print('diagnostic ' + mode, file=sys.stderr, flush=True)
with MetricsLogger() as logger:
  logger.params({'learning_rate': rate, 'mode': mode})
  logger.log(0, {'loss': 2.0, 'accuracy': 0.25})
  logger.log(1, {'loss': rate * 10, 'accuracy': 0.75})
if mode == 'cancel':
  while True:
    time.sleep(0.1)
(out / 'checkpoint.pt').write_bytes(b'fixture checkpoint')
if mode == 'fail':
  raise SystemExit(7)
"#;

const BASE_INVENTORY: &str = "import importlib.metadata as m,json,pathlib,torch; print(json.dumps({'packages': sorted((d.metadata['Name'],d.version,str(d.locate_file(''))) for d in m.distributions()), 'torch_version': torch.__version__, 'torch_origin': str(pathlib.Path(torch.__file__).resolve())},sort_keys=True))";

struct Fixture {
  _root: tempfile::TempDir,
  repo: PathBuf,
  remote: String,
  bin: PathBuf,
  case: String,
}

impl Fixture {
  fn new() -> Self {
    let case = std::env::var("EXPRI_TEST_CASE")
      .expect("EXPRI_TEST_CASE is required by the container workflow test");
    assert!(matches!(
      case.as_str(),
      "matched-native"
        | "matched-python"
        | "matched-auto"
        | "torch-mismatch"
        | "dependency-mismatch"
        | "cuda-unavailable"
    ));
    let bin = PathBuf::from(
      std::env::var("EXPRI_TEST_BIN")
        .expect("EXPRI_TEST_BIN must name the expri binary installed in the host image"),
    );
    assert!(bin.is_file(), "installed expri binary is missing");
    for (tool, version_flag) in [
      ("uv", "--version"),
      ("git", "--version"),
      ("ssh", "-V"),
      ("rsync", "--version"),
      ("python3", "--version"),
    ] {
      successful(
        Command::new(tool)
          .arg(version_flag)
          .output()
          .unwrap_or_else(|error| panic!("required {tool}: {error}")),
        tool,
      );
    }
    let root = tempfile::Builder::new()
      .prefix("expri-container-")
      .tempdir()
      .unwrap();
    let repo = fs::canonicalize(root.path()).unwrap().join("host-project");
    fs::create_dir(&repo).unwrap();
    let tag = root.path().file_name().unwrap().to_str().unwrap();
    let remote = format!("/home/tester/project-{tag}");
    let fixture = Self {
      _root: root,
      repo,
      remote,
      bin,
      case,
    };
    fixture.write_project();
    fixture
  }

  fn write_project(&self) {
    fs::write(self.repo.join("train.py"), TRAIN).unwrap();
    fs::write(
      self.repo.join("expri_metrics.py"),
      include_str!("../python/expri_metrics.py"),
    )
    .unwrap();
    fs::write(self.repo.join(".gitignore"), ".expri/\nresults/\n.venv/\n").unwrap();
    fs::write(
      self.repo.join("pyproject.toml"),
      r#"[project]
name = "expri-container-experiment"
version = "0.1.0"
requires-python = ">=3.12,<3.13"
dependencies = [
  "torch==2.10.0+cu128",
  "nvidia-cuda-runtime-cu12==12.8.90",
  "nvidia-cublas-cu12==12.8.4.1",
  "fixture-extra==0.1.0",
]
[tool.uv]
package = false
[[tool.uv.index]]
name = "fixture"
url = "http://worker:8000/simple"
default = true
"#,
    )
    .unwrap();
    self.write_config(true);
    successful(
      Command::new("uv")
        .current_dir(&self.repo)
        .args(["lock", "--python", "/usr/local/bin/python3"])
        .output()
        .unwrap(),
      "uv lock",
    );
    for args in [
      vec!["init", "--quiet"],
      vec!["add", "."],
      vec![
        "-c",
        "user.name=Expri CI",
        "-c",
        "user.email=ci@expri.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--quiet",
        "-m",
        "fixture",
      ],
    ] {
      successful(
        Command::new("git")
          .current_dir(&self.repo)
          .args(args)
          .output()
          .unwrap(),
        "git fixture",
      );
    }
  }

  fn write_config(&self, require_cuda: bool) {
    let protocol = match self.case.as_str() {
      "matched-python" => "python",
      "matched-auto" => "auto",
      _ => "expri-node",
    };
    let node = if self.case == "matched-auto" {
      "/usr/local/bin/expri-old"
    } else {
      "/usr/local/bin/expri"
    };
    fs::write(self.repo.join("expri.toml"), format!(
      "[project]\nname='Container workflow'\n[environment]\nbase_python='/usr/local/bin/python3'\nreuse_packages=['torch']\nrequire_cuda={require_cuda}\n[tasks]\ntrain=['python','train.py']\n[download]\nresults_dir='results'\n[target.worker]\nhost='worker'\nremote_dir={}\nprotocol='{protocol}'\nnode_bin='{node}'\n",
      serde_json::to_string(&self.remote).unwrap(),
    )).unwrap();
  }

  fn command(&self, args: &[&str]) -> Command {
    let mut command = Command::new(&self.bin);
    command
      .current_dir(&self.repo)
      .args(["-T", "worker"])
      .args(args);
    command
  }

  fn run(&self, args: &[&str]) -> Output {
    successful(
      self.command(args).output().unwrap(),
      &format!("expri {}", args.join(" ")),
    )
  }

  fn json(&self, args: &[&str]) -> Value {
    parse_json(&self.run(args).stdout)
  }

  fn ssh(&self, script: &str) -> Output {
    successful(
      Command::new("ssh")
        .args(["-o", "BatchMode=yes", "worker", script])
        .output()
        .unwrap(),
      "worker SSH",
    )
  }

  fn base(&self) -> Value {
    parse_json(
      &self
        .ssh(&format!(
          "/usr/local/bin/python3 -I -c {}",
          quote(BASE_INVENTORY)
        ))
        .stdout,
    )
  }

  fn remote_json(&self, path: &str) -> Value {
    parse_json(&self.ssh(&format!("cat -- {}", quote(path))).stdout)
  }

  fn cache(&self, id: &str) -> PathBuf {
    self.repo.join("results/worker/runs").join(id)
  }
}

fn successful(output: Output, label: &str) -> Output {
  assert!(
    output.status.success(),
    "{label} failed ({}):\n{}\n{}",
    output.status,
    bounded(&output.stdout),
    bounded(&output.stderr)
  );
  output
}

fn bounded(bytes: &[u8]) -> String {
  let text = String::from_utf8_lossy(bytes);
  text.chars().take(4096).collect()
}

fn parse_json(bytes: &[u8]) -> Value {
  serde_json::from_slice(bytes)
    .unwrap_or_else(|error| panic!("invalid JSON ({error}): {}", bounded(bytes)))
}

fn quote(value: &str) -> String {
  format!("'{}'", value.replace('\'', "'\\''"))
}

fn start(fixture: &Fixture, mode: &str, rate: &str, synchronize: bool) -> Value {
  let args = if synchronize {
    vec!["run", "--detach", "train", mode, rate]
  } else {
    vec!["run", "--no-sync", "--detach", "train", mode, rate]
  };
  let receipt = fixture.json(&args);
  assert_eq!(receipt["detached"], true);
  assert!(
    receipt["run_dir"]
      .as_str()
      .unwrap()
      .starts_with(&format!("{}/.expri/runs/", fixture.remote))
  );
  receipt
}

fn status(fixture: &Fixture, id: &str, terminal: bool) -> Value {
  let deadline = Instant::now() + Duration::from_secs(90);
  loop {
    let report = fixture.json(&["runs", "status", id, "--json"]);
    let state = report["status"].as_str().unwrap();
    if if terminal {
      matches!(state, "completed" | "failed" | "cancelled" | "lost")
    } else {
      state == "running"
    } {
      return report;
    }
    assert!(
      terminal || !matches!(state, "completed" | "failed" | "cancelled" | "lost"),
      "run {id} stopped before reaching running: {state}"
    );
    assert!(Instant::now() < deadline, "run {id} stalled at {state}");
    thread::sleep(Duration::from_millis(100));
  }
}

fn run_report(fixture: &Fixture, receipt: &Value, expected_status: &str, base: &Value) -> Value {
  let id = receipt["run_id"].as_str().unwrap();
  let terminal = status(fixture, id, true);
  assert_eq!(
    terminal["status"],
    expected_status,
    "{}",
    bounded(&serde_json::to_vec(&terminal).unwrap())
  );
  assert_eq!(terminal["alive"], false);
  let detail = fixture.json(&["runs", "show", id, "--json"]);
  assert_eq!(detail["run"]["status"], expected_status);
  let run_dir = receipt["run_dir"].as_str().unwrap();
  let report = fixture.remote_json(&format!("{run_dir}/outputs/report.json"));
  assert_eq!(report["cwd"], format!("{run_dir}/code"));
  assert_eq!(report["torch_version"], "2.10.0+cu128");
  assert_eq!(report["torch_origin"], base["torch_origin"]);
  assert_eq!(report["probe"], 2);
  assert_eq!(report["wrapper"]["torch_version"], report["torch_version"]);
  assert_eq!(report["wrapper"]["torch_origin"], report["torch_origin"]);
  // uv's task and console-script launchers can choose python3 and python aliases.
  assert_eq!(
    Path::new(report["wrapper"]["python"].as_str().unwrap()).parent(),
    Path::new(report["python"].as_str().unwrap()).parent()
  );
  assert!(
    report["python"]
      .as_str()
      .unwrap()
      .starts_with(&format!("{run_dir}/environment/.venv/"))
  );
  assert!(
    report["extra_origin"]
      .as_str()
      .unwrap()
      .starts_with(&format!("{run_dir}/environment/.venv/"))
  );
  assert_eq!(
    detail["environment"]["environment_path"],
    report["environment_path"]
  );
  assert_eq!(
    detail["environment"]["cache"]["directory"],
    report["cache_dir"]
  );
  let stdout = fixture.run(&["runs", "logs", id, "--tail", "2"]);
  assert!(String::from_utf8_lossy(&stdout.stdout).contains("training started"));
  let stderr = fixture.run(&["runs", "logs", id, "--stream", "stderr", "--tail", "1"]);
  assert!(String::from_utf8_lossy(&stderr.stdout).contains("diagnostic"));
  report
}

fn matched(fixture: &Fixture, base: &Value) {
  let doctor = fixture.json(&["env", "doctor", "--json"]);
  assert_eq!(
    doctor["compatible"],
    true,
    "{}",
    bounded(&serde_json::to_vec(&doctor).unwrap())
  );
  assert_eq!(doctor["checks"]["torch_details"]["version"], "2.10.0+cu128");
  assert_eq!(doctor["checks"]["torch_details"]["cuda_available"], true);
  assert!(
    doctor["reused_packages"]
      .as_array()
      .unwrap()
      .contains(&json!("nvidia-cuda-runtime-cu12"))
  );
  let first = start(fixture, "success", "0.01", false);
  let first_report = run_report(fixture, &first, "completed", base);
  let first_dir = first["run_dir"].as_str().unwrap();
  let first_source = fixture
    .ssh(&format!(
      "cat -- {}",
      quote(&format!("{first_dir}/code/train.py"))
    ))
    .stdout;
  assert!(
    first_source == TRAIN.as_bytes(),
    "initial snapshot differs from the synchronized source"
  );
  fs::write(
    fixture.repo.join("train.py"),
    format!("{TRAIN}\n# changed after the first snapshot\n"),
  )
  .unwrap();
  let second = start(fixture, "success", "0.02", true);
  let second_report = run_report(fixture, &second, "completed", base);
  let second_source = fixture
    .ssh(&format!(
      "cat -- {}",
      quote(&format!(
        "{}/code/train.py",
        second["run_dir"].as_str().unwrap()
      ))
    ))
    .stdout;
  assert!(
    String::from_utf8_lossy(&second_source).contains("# changed after the first snapshot"),
    "resynchronization did not include the source edit"
  );
  assert_ne!(
    first_report["environment_path"],
    second_report["environment_path"]
  );
  assert_eq!(first_report["cache_dir"], second_report["cache_dir"]);
  assert!(
    first_source
      == fixture
        .ssh(&format!(
          "cat -- {}",
          quote(&format!("{first_dir}/code/train.py"))
        ))
        .stdout
  );
  let failed = start(fixture, "fail", "0.03", false);
  run_report(fixture, &failed, "failed", base);
  assert_eq!(
    status(fixture, failed["run_id"].as_str().unwrap(), true)["state"]["exit_code"],
    7
  );
  let active = start(fixture, "cancel", "0.04", false);
  let active_id = active["run_id"].as_str().unwrap();
  assert_eq!(status(fixture, active_id, false)["alive"], true);
  let prune = fixture.json(&["env", "prune", "--apply", "--keep-last", "0", "--json"]);
  assert_eq!(prune["pruned_runs"], 3);
  for receipt in [&first, &second, &failed] {
    let run_dir = receipt["run_dir"].as_str().unwrap();
    fixture.ssh(&format!(
      "test ! -e {} && test -f {} && test -f {}",
      quote(&format!("{run_dir}/environment/.venv")),
      quote(&format!("{run_dir}/outputs/checkpoint.pt")),
      quote(&format!("{run_dir}/outputs/metrics.jsonl"))
    ));
  }
  fixture.ssh(&format!(
    "test -d {}",
    quote(&format!(
      "{}/environment/.venv",
      active["run_dir"].as_str().unwrap()
    ))
  ));
  fixture.json(&["runs", "cancel", active_id, "--json"]);
  assert_eq!(status(fixture, active_id, true)["status"], "cancelled");
  assert_eq!(fixture.base(), *base, "worker base packages changed");
  review_cached(fixture, &first, &second);
}

fn review_cached(fixture: &Fixture, first: &Value, second: &Value) {
  let ids = [
    first["run_id"].as_str().unwrap(),
    second["run_id"].as_str().unwrap(),
  ];
  for (id, rate) in ids.iter().zip([0.01, 0.02]) {
    fixture.json(&["runs", "pull", id, "--json"]);
    fixture.json(&["runs", "pull", id, "--metrics", "--json"]);
    let cache = fixture.cache(id);
    assert!(cache.join("logs/stdout.log").is_file());
    assert!(cache.join("outputs/metrics.jsonl").is_file());
    assert!(cache.join("outputs/params.json").is_file());
    for artifact in ["checkpoint.pt", "report.json", "started"] {
      assert!(
        !cache.join("outputs").join(artifact).exists(),
        "unselected {artifact} was downloaded"
      );
    }
    assert!(!cache.join("environment/.venv").exists());
    let metrics = fixture.json(&["runs", "metrics", id, "--json"]);
    assert_eq!(metrics["params"]["learning_rate"], rate);
    assert_eq!(
      metrics["metrics"]["loss"]["summary"]["last"]["value"],
      rate * 10.0
    );
  }
  fs::write(
    fixture.repo.join("expri.toml"),
    "[project]\nname='Offline container review'\n[download]\nresults_dir='results'\n",
  )
  .unwrap();
  let before = tree(&fixture.repo);
  let catalog = fixture.json(&["runs", "list", "--cached", "--json"]);
  assert_eq!(catalog["runs"].as_array().unwrap().len(), 2);
  let detail = fixture.json(&["runs", "show", ids[0], "--cached", "--json"]);
  assert_eq!(detail["run"]["status"], "completed");
  let metrics = fixture.json(&["runs", "metrics", ids[0], "--cached", "--json"]);
  assert_eq!(metrics["metrics"]["loss"]["summary"]["last"]["value"], 0.1);
  let compare = fixture.json(&[
    "runs",
    "compare",
    ids[0],
    ids[1],
    "--cached",
    "--metric",
    "loss",
    "--reduction",
    "min",
    "--json",
  ]);
  assert_eq!(compare["runs"][0]["values"]["loss"]["value"], 0.1);
  assert_eq!(compare["runs"][1]["values"]["loss"]["value"], 0.2);
  {
    let server = Dashboard::start(fixture);
    let catalog = server.json("/api/catalog");
    assert_eq!(catalog["initial_source"], "cached:worker");
    let list = server.json("/api/runs?source=cached%3Aworker&limit=1");
    assert_eq!(list["total_count"], 2);
    assert_eq!(list["runs"].as_array().unwrap().len(), 1);
    assert_eq!(list["next_offset"], 1);
    let next = server.json("/api/runs?source=cached%3Aworker&limit=1&offset=1");
    assert_eq!(next["runs"].as_array().unwrap().len(), 1);
    assert_eq!(next["next_offset"], Value::Null);
    assert_ne!(list["runs"][0]["run_id"], next["runs"][0]["run_id"]);
    let detail = server.json(&format!(
      "/api/run?source=cached%3Aworker&run_id={}",
      ids[0]
    ));
    assert_eq!(detail["run"]["status"], "completed");
    assert_eq!(detail["params"]["learning_rate"], 0.01);
    assert_eq!(detail["metrics"]["loss"]["last"]["value"], 0.1);
    let log = server.json(&format!(
      "/api/log?source=cached%3Aworker&run_id={}&stream=stdout&tail=1",
      ids[0]
    ));
    assert!(
      log["content"]
        .as_str()
        .unwrap()
        .contains("training started success")
    );
    let comparison = server.json(&format!(
      "/api/compare?source=cached%3Aworker&run_id={}&run_id={}&metric=loss&reduction=min",
      ids[0], ids[1]
    ));
    assert_eq!(
      comparison["comparison"]["runs"][0]["values"]["loss"]["value"],
      0.1
    );
    let chart = server.get(&format!(
      "/api/chart?source=cached%3Aworker&run_id={}&run_id={}&metric=loss",
      ids[0], ids[1]
    ));
    assert!(String::from_utf8_lossy(&chart).contains("<svg"));
    assert!(chart.len() < 2 * 1024 * 1024);
  }
  assert!(
    before == tree(&fixture.repo),
    "offline inspection changed the project or cache"
  );
}

fn incompatible(fixture: &Fixture, base: &Value) {
  let output = fixture
    .command(&["env", "doctor", "--json"])
    .output()
    .unwrap();
  let doctor = parse_json(&output.stdout);
  assert!(
    !output.status.success(),
    "incompatible doctor returned a successful exit status"
  );
  assert_eq!(doctor["compatible"], false);
  let (kind, package, message_fragment) = match fixture.case.as_str() {
    "torch-mismatch" => ("locked_version", "torch", "2.10.0+cu126"),
    "dependency-mismatch" => ("locked_version", "nvidia-cuda-runtime-cu12", "12.8.91"),
    "cuda-unavailable" => ("cuda", "torch", "is_available() is false"),
    _ => unreachable!(),
  };
  assert!(
    doctor["issues"].as_array().unwrap().iter().any(|issue| {
      issue["kind"] == kind
        && issue["package"] == package
        && issue["message"]
          .as_str()
          .is_some_and(|message| message.contains(message_fragment))
    }),
    "expected {kind} for {package}: {}",
    bounded(&serde_json::to_vec(&doctor).unwrap())
  );
  let failed = fixture
    .command(&["run", "--no-sync", "train", "success", "0.01"])
    .output()
    .unwrap();
  assert!(
    !failed.status.success(),
    "incompatible run unexpectedly succeeded"
  );
  assert!(
    String::from_utf8_lossy(&failed.stderr).contains(message_fragment),
    "run did not report its compatibility failure: {}",
    bounded(&failed.stderr)
  );
  let report = fixture.json(&["runs", "list", "--json"]);
  let runs = report["runs"].as_array().unwrap();
  assert_eq!(runs.len(), 1);
  assert_eq!(runs[0]["status"], "failed");
  let id = runs[0]["run_id"].as_str().unwrap();
  fixture.ssh(&format!(
    "test ! -e {}",
    quote(&format!(
      "{}/.expri/runs/{id}/outputs/started",
      fixture.remote
    ))
  ));
  assert_eq!(
    fixture.base(),
    *base,
    "failed preparation changed the base packages"
  );
  if fixture.case == "cuda-unavailable" {
    fixture.write_config(false);
    let doctor = fixture.json(&["env", "doctor", "--json"]);
    assert_eq!(doctor["compatible"], true);
    let receipt = start(fixture, "success", "0.01", false);
    run_report(fixture, &receipt, "completed", base);
    assert_eq!(fixture.base(), *base);
  }
}

fn tree(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32, std::time::SystemTime)> {
  use std::os::unix::fs::PermissionsExt;
  fn visit(
    root: &Path,
    path: &Path,
    result: &mut BTreeMap<PathBuf, (Vec<u8>, u32, std::time::SystemTime)>,
  ) {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert!(
      !metadata.file_type().is_symlink(),
      "unexpected fixture symlink"
    );
    let contents = if metadata.is_file() {
      fs::read(path).unwrap()
    } else {
      Vec::new()
    };
    result.insert(
      path.strip_prefix(root).unwrap().to_path_buf(),
      (
        contents,
        metadata.permissions().mode(),
        metadata.modified().unwrap(),
      ),
    );
    if metadata.is_dir() {
      for entry in fs::read_dir(path).unwrap() {
        visit(root, &entry.unwrap().path(), result);
      }
    }
  }
  let mut result = BTreeMap::new();
  visit(root, root, &mut result);
  result
}

struct Dashboard {
  child: Child,
  address: String,
}

impl Dashboard {
  fn start(fixture: &Fixture) -> Self {
    let mut child = fixture
      .command(&["dashboard", "--port", "0"])
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit())
      .spawn()
      .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
      let mut line = String::new();
      let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
      let _ = sender.send(result);
    });
    let address = match receiver.recv_timeout(Duration::from_secs(10)) {
      Ok(Ok(line)) => line
        .trim()
        .strip_prefix("Dashboard: http://")
        .map(str::to_string),
      _ => None,
    };
    if let Some(address) = address {
      Self { child, address }
    } else {
      let _ = child.kill();
      let _ = child.wait();
      panic!("dashboard did not publish its loopback URL");
    }
  }

  fn get(&self, path: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(&self.address).unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(10)))
      .unwrap();
    stream
      .set_write_timeout(Some(Duration::from_secs(10)))
      .unwrap();
    write!(
      stream,
      "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
      self.address
    )
    .unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    stream
      .take(2 * 1024 * 1024 + 4096)
      .read_to_end(&mut response)
      .unwrap();
    let boundary = response
      .windows(4)
      .position(|window| window == b"\r\n\r\n")
      .expect("HTTP headers");
    assert!(
      response.starts_with(b"HTTP/1.1 200 "),
      "dashboard request failed: {}",
      bounded(&response)
    );
    response[boundary + 4..].to_vec()
  }

  fn json(&self, path: &str) -> Value {
    let body = self.get(path);
    assert!(
      body.len() <= 512 * 1024,
      "dashboard JSON exceeded its response bound"
    );
    parse_json(&body)
  }
}

impl Drop for Dashboard {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

#[test]
#[ignore = "requires the isolated Docker host/worker images and EXPRI_TEST_CASE"]
fn host_worker_workflow() {
  let fixture = Fixture::new();
  let host_before = parse_json(
    &successful(
      Command::new("/usr/local/bin/python3")
        .args(["-I", "-c", BASE_INVENTORY])
        .output()
        .unwrap(),
      "host base inventory",
    )
    .stdout,
  );
  assert_eq!(
    host_before["torch_version"], "2.9.0+cpu",
    "host image must differ from the locked worker Torch"
  );
  let base = fixture.base();
  assert_ne!(base["torch_version"], host_before["torch_version"]);
  fixture.run(&["sync"]);
  if fixture.case.starts_with("matched-") {
    matched(&fixture, &base);
  } else {
    incompatible(&fixture, &base);
  }
  let host_after = parse_json(
    &successful(
      Command::new("/usr/local/bin/python3")
        .args(["-I", "-c", BASE_INVENTORY])
        .output()
        .unwrap(),
      "host base inventory",
    )
    .stdout,
  );
  assert_eq!(host_before, host_after, "host base packages changed");
}
