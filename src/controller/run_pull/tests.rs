use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Command, Stdio};

use super::*;
use crate::config::{TargetConfig, TransportKind};

struct Fixture {
  _temporary: tempfile::TempDir,
  root: PathBuf,
  repo: PathBuf,
  run: PathBuf,
  ctl: PathBuf,
}

impl Fixture {
  fn new() -> Self {
    let temporary = tempfile::Builder::new()
      .prefix("expri pull's ")
      .tempdir()
      .expect("fixture");
    let root = fs::canonicalize(temporary.path()).expect("canonical fixture");
    let repo = root.join("controller project");
    let run = root.join("remote project's code/.expri/runs/run-one");
    fs::create_dir_all(&repo).expect("controller repository");
    fs::create_dir_all(run.join("environment/.venv/lib")).expect("remote environment");
    fs::create_dir_all(run.join("logs")).expect("remote logs");
    fs::create_dir_all(run.join("outputs")).expect("remote outputs");
    fs::create_dir_all(run.join("code/out")).expect("remote code output");
    fs::write(
      run.join("run-state.json"),
      serde_json::to_vec(&json!({
        "schema_version": 1, "run_id": "run-one", "task": "train", "status": "completed",
        "started_at": "2026-10-02T00:00:00Z", "finished_at": "2026-10-02T01:00:00Z", "exit_code": 0,
      }))
      .expect("state JSON"),
    )
    .expect("remote state");
    fs::write(
      run.join("snapshot.json"),
      b"{\"source\":{\"git_head\":\"abc\"}}",
    )
    .expect("snapshot");
    fs::write(
      run.join("environment/environment-state.json"),
      b"{\"base_python\":\"/base/python\"}",
    )
    .expect("environment manifest");
    fs::write(
      run.join("environment/.venv/lib/heavy-package"),
      b"never transfer",
    )
    .expect("environment bytes");
    fs::write(run.join("logs/stdout.log"), b"loss=0.1\n").expect("stdout log");
    fs::write(run.join("logs/stderr.log"), b"warning\n").expect("stderr log");
    fs::write(run.join("outputs/model one.pt"), b"checkpoint\0binary").expect("checkpoint");
    fs::write(run.join("code/out/metrics.json"), b"{\"loss\":0.1}").expect("metrics");
    let ctl = root.join("ctl bridge");
    fs::write(&ctl, "#!/bin/sh\n[ \"$RSYNC_OLD_ARGS\" = 1 ] || exit 95\n[ \"$RSYNC_PROTECT_ARGS\" = 0 ] || exit 96\n[ \"$1\" = ssh ] || exit 91\nshift\n[ \"$1\" = -q ] && shift\n[ \"$1\" = expri-test-host ] || exit 92\nshift\nexec /bin/sh -c \"$*\"\n").expect("fake ctl");
    fs::set_permissions(&ctl, fs::Permissions::from_mode(0o700)).expect("executable fake ctl");
    Self {
      _temporary: temporary,
      root,
      repo,
      run,
      ctl,
    }
  }

  fn remote(&self, dry_run: bool) -> Remote {
    Remote::new(
      TargetConfig {
        host: "expri-test-host".to_string(),
        remote_dir: self
          .run
          .parent()
          .unwrap()
          .parent()
          .unwrap()
          .parent()
          .unwrap()
          .display()
          .to_string(),
        transport: TransportKind::Ctl,
        port: None,
        protocol: None,
        node_bin: None,
        ctl_bin: Some(self.ctl.display().to_string()),
        ctl_method: None,
        environment: None,
        service: None,
      },
      String::new(),
      String::new(),
      dry_run,
      0,
      true,
    )
    .expect("fake remote")
  }

  fn selection(&self, artifacts: &[&str]) -> Value {
    let mut files = vec![
      "run-state.json",
      "snapshot.json",
      "environment/environment-state.json",
      "logs/stdout.log",
      "logs/stderr.log",
    ];
    files.extend_from_slice(artifacts);
    json!({"run_id":"run-one", "run_dir":self.run, "files":files,
      "warnings":[{"run_id":"run-one", "message":"optional metadata omitted"}]})
  }

  fn destination(&self) -> PathBuf {
    self.repo.join("results/gpu target/runs/run-one")
  }
}

fn rsync_available() -> bool {
  match Command::new("rsync")
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
  {
    Ok(status) => {
      assert!(status.success(), "rsync --version failed");
      true
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      eprintln!("skipping run transfer test because rsync is unavailable");
      false
    }
    Err(error) => panic!("rsync inspection failed: {error}"),
  }
}

#[test]
fn selected_pull_handles_spaces_and_quotes_and_retains_previous_artifacts() {
  if !rsync_available() {
    return;
  }
  let fixture = Fixture::new();
  let remote = fixture.remote(false);
  let selection = fixture.selection(&["outputs/model one.pt", "code/out/metrics.json"]);
  let report =
    pull(&remote, &fixture.repo, "results", "gpu target", &selection).expect("selective pull");
  assert_eq!(report["dry_run"], false);
  assert_eq!(report["warnings"], selection["warnings"]);
  let destination = fixture.destination();
  assert_eq!(
    fs::read(destination.join("outputs/model one.pt")).expect("checkpoint"),
    b"checkpoint\0binary"
  );
  assert!(destination.join("code/out/metrics.json").is_file());
  assert!(!destination.join("environment/.venv").exists());
  assert_eq!(
    serde_json::from_slice::<Value>(&fs::read(destination.join(RECEIPT_FILE)).expect("receipt"))
      .expect("receipt JSON")["selected_files"],
    report["files"]
  );
  assert!(destination.join(OWNER_FILE).is_file());
  fs::write(fixture.run.join("logs/stdout.log"), b"loss=0.05\n").expect("updated log");
  let metadata = fixture.selection(&[]);
  let report =
    pull(&remote, &fixture.repo, "results", "gpu target", &metadata).expect("repeat metadata pull");
  assert_eq!(
    fs::read(destination.join("logs/stdout.log")).expect("updated log"),
    b"loss=0.05\n"
  );
  assert_eq!(
    fs::read(destination.join("outputs/model one.pt")).expect("retained checkpoint"),
    b"checkpoint\0binary"
  );
  assert!(destination.join("code/out/metrics.json").is_file());
  assert_eq!(
    serde_json::from_slice::<Value>(&fs::read(destination.join(RECEIPT_FILE)).expect("receipt"))
      .expect("receipt JSON")["selected_files"],
    report["files"]
  );
  assert!(
    fs::read_dir(destination.parent().unwrap())
      .expect("run cache")
      .all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".pull-"))
  );
}

#[test]
fn dry_run_writes_nothing_even_when_cache_does_not_exist() {
  let fixture = Fixture::new();
  let report = pull(
    &fixture.remote(true),
    &fixture.repo,
    "results",
    "gpu target",
    &fixture.selection(&["outputs/model one.pt"]),
  )
  .expect("pull preview");
  assert_eq!(report["dry_run"], true);
  assert_eq!(
    report["destination"],
    fixture.destination().to_string_lossy().as_ref()
  );
  assert!(!fixture.repo.join("results").exists());
}

#[test]
fn cached_root_is_read_only_and_accepts_trusted_repository_aliases() {
  let fixture = Fixture::new();
  let alias = fixture.root.join("repository alias");
  symlink(&fixture.repo, &alias).expect("trusted repository alias");
  let cache = cached_runs_dir(&alias, "results", "gpu target").expect("absent cache");
  assert_eq!(cache, fixture.repo.join("results/gpu target/runs"));
  assert!(!fixture.repo.join("results").exists());
  let outside = fixture.root.join("outside cache");
  fs::create_dir(&outside).expect("outside directory");
  symlink(&outside, fixture.repo.join("results")).expect("linked results");
  assert!(cached_runs_dir(&alias, "results", "gpu target").is_err());
  assert!(!outside.join("gpu target").exists());
}

#[test]
fn failed_transfer_preserves_cached_files_and_receipt() {
  if !rsync_available() {
    return;
  }
  let fixture = Fixture::new();
  let remote = fixture.remote(false);
  pull(
    &remote,
    &fixture.repo,
    "results",
    "gpu target",
    &fixture.selection(&[]),
  )
  .expect("initial pull");
  let destination = fixture.destination();
  let state = fs::read(destination.join("run-state.json")).expect("old state");
  let log = fs::read(destination.join("logs/stdout.log")).expect("old log");
  let receipt = fs::read(destination.join(RECEIPT_FILE)).expect("old receipt");
  fs::write(fixture.run.join("logs/stdout.log"), b"new bytes\n").expect("changed remote log");
  fs::remove_file(fixture.run.join("snapshot.json")).expect("remote file disappeared");
  assert!(
    pull(
      &remote,
      &fixture.repo,
      "results",
      "gpu target",
      &fixture.selection(&[])
    )
    .is_err()
  );
  assert_eq!(
    fs::read(destination.join("run-state.json")).expect("state"),
    state
  );
  assert_eq!(
    fs::read(destination.join("logs/stdout.log")).expect("log"),
    log
  );
  assert_eq!(
    fs::read(destination.join(RECEIPT_FILE)).expect("receipt"),
    receipt
  );
}

#[test]
fn traversal_unsupported_and_environment_paths_fail_before_local_writes() {
  let fixture = Fixture::new();
  for relative in [
    "../secret",
    "/etc/passwd",
    "outputs/../secret",
    "outputs/.venv/bin/python",
    "code/cache/wheel",
    "environment/.venv/bin/python",
    "environment/owner.json",
    "logs/../../outside",
    "code/./train.py",
    "outputs//model.pt",
    "pull-state.json",
  ] {
    assert!(
      pull(
        &fixture.remote(true),
        &fixture.repo,
        "results",
        "gpu target",
        &fixture.selection(&[relative])
      )
      .is_err(),
      "accepted {relative}"
    );
  }
  for component in ["", ".", "..", "../gpu", "gpu/other", "gpu\\other", "gpu\n"] {
    assert!(
      validate_component(component, "target").is_err(),
      "accepted component {component:?}"
    );
  }
  for result in [
    "",
    "../results",
    "/absolute",
    "results/../outside",
    "results/./nested",
  ] {
    assert!(
      validate_results_dir(result).is_err(),
      "accepted results_dir {result}"
    );
  }
  let mut selection = fixture.selection(&[]);
  selection["run_dir"] = json!(fixture.root.join("outside/run-one"));
  assert!(
    pull(
      &fixture.remote(true),
      &fixture.repo,
      "results",
      "gpu target",
      &selection
    )
    .is_err()
  );
  assert!(!fixture.repo.join("results").exists());
}

#[test]
fn unowned_or_symlink_destination_and_staged_symlinks_are_rejected() {
  if !rsync_available() {
    return;
  }
  let fixture = Fixture::new();
  fs::create_dir_all(fixture.destination()).expect("unowned destination");
  fs::write(fixture.destination().join("precious"), b"user content").expect("unowned content");
  assert!(
    pull(
      &fixture.remote(false),
      &fixture.repo,
      "results",
      "gpu target",
      &fixture.selection(&[])
    )
    .is_err()
  );
  assert_eq!(
    fs::read(fixture.destination().join("precious")).expect("preserved content"),
    b"user content"
  );
  fs::remove_dir_all(fixture.repo.join("results")).expect("remove scratch cache");
  let outside = fixture.root.join("outside");
  fs::create_dir(&outside).expect("outside directory");
  fs::write(outside.join("precious"), b"keep").expect("outside content");
  symlink(&outside, fixture.repo.join("results")).expect("results symlink");
  assert!(
    pull(
      &fixture.remote(false),
      &fixture.repo,
      "results",
      "gpu target",
      &fixture.selection(&[])
    )
    .is_err()
  );
  assert!(!outside.join("gpu target").exists());
  fs::remove_file(fixture.repo.join("results")).expect("remove scratch symlink");
  fs::remove_file(fixture.run.join("logs/stdout.log")).expect("remove fixture log");
  symlink(
    outside.join("precious"),
    fixture.run.join("logs/stdout.log"),
  )
  .expect("remote log symlink");
  assert!(
    pull(
      &fixture.remote(false),
      &fixture.repo,
      "results",
      "gpu target",
      &fixture.selection(&[])
    )
    .is_err()
  );
  assert!(!fixture.destination().exists());
  assert_eq!(
    fs::read(outside.join("precious")).expect("outside content"),
    b"keep"
  );
}

#[test]
fn missing_optional_metadata_can_publish_an_owned_partial_cache() {
  if !rsync_available() {
    return;
  }
  let fixture = Fixture::new();
  let mut selection = fixture.selection(&[]);
  selection["files"] = json!(["logs/stdout.log"]);
  pull(
    &fixture.remote(false),
    &fixture.repo,
    "results",
    "gpu target",
    &selection,
  )
  .expect("partial metadata pull");
  assert!(fixture.destination().join("logs/stdout.log").is_file());
  assert!(!fixture.destination().join("run-state.json").exists());
  assert!(fixture.destination().join(OWNER_FILE).is_file());
  assert!(fixture.destination().join(RECEIPT_FILE).is_file());
}
