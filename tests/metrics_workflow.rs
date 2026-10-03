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
fn remote_metrics_compare_use_selective_pulls_and_remain_available_offline() {
  if !rsync_available() {
    eprintln!("skipping metrics transfer test: rsync unavailable");
    return;
  }
  for (protocol, old_node) in [("expri-node", false), ("python", false), ("auto", true)] {
    let fixture = Fixture::new(protocol, old_node);
    let preview = fixture.json(&[
      "runs",
      "pull",
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
