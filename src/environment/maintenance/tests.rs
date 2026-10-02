use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use super::*;

fn fixture_root() -> (tempfile::TempDir, PathBuf) {
  let temporary = tempfile::tempdir().expect("temporary repository");
  let root = fs::canonicalize(temporary.path()).expect("canonical repository");
  (temporary, root)
}

fn run(root: &Path, id: &str, status: &str, finished_at: &str) -> PathBuf {
  let directory = root.join(".expri/runs").join(id);
  fs::create_dir_all(directory.join("code")).expect("code directory");
  fs::create_dir_all(directory.join("outputs")).expect("outputs directory");
  fs::create_dir_all(directory.join("environment/.venv/lib")).expect("environment");
  fs::write(directory.join("code/train.py"), "print('source')").expect("source");
  fs::write(directory.join("outputs/metrics.json"), "{\"loss\":0.1}").expect("output");
  fs::write(
    directory.join("snapshot.json"),
    "{\"source\":\"preserved\"}",
  )
  .expect("snapshot");
  fs::write(
    directory.join("environment/environment-state.json"),
    "{\"fingerprint\":\"preserved\"}",
  )
  .expect("environment manifest");
  fs::write(
    directory.join("environment/.venv/lib/package.py"),
    b"elevenbytes",
  )
  .expect("package");
  write_json(
    &directory.join("environment/owner.json"),
    &json!({"schema_version": 1, "repo_root": directory.join("code")}),
  );
  write_json(
    &directory.join("run-state.json"),
    &json!({
      "run_id": id,
      "code_dir": directory.join("code"),
      "output_dir": directory.join("outputs"),
      "status": status,
      "finished_at": finished_at,
    }),
  );
  directory
}

fn write_json(path: &Path, value: &Value) {
  fs::write(path, serde_json::to_vec(value).expect("JSON")).expect("metadata");
}

fn request(apply: bool, keep_last: usize) -> PruneRequest {
  PruneRequest { apply, keep_last }
}

fn tree(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
  fn collect(path: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) {
    let metadata = fs::symlink_metadata(path).expect("tree metadata");
    if metadata.is_file() {
      output.push((path.to_path_buf(), fs::read(path).expect("tree file")));
    } else if metadata.is_dir() {
      output.push((path.to_path_buf(), Vec::new()));
      for entry in fs::read_dir(path).expect("tree directory") {
        collect(&entry.expect("tree entry").path(), output);
      }
    } else {
      output.push((path.to_path_buf(), Vec::new()));
    }
  }
  let mut output = Vec::new();
  collect(path, &mut output);
  output.sort();
  output
}

fn python_prune(root: &Path, apply: bool, keep_last: usize) -> Value {
  let output = Command::new("python3")
    .args([
      "-c",
      &format!(
        "{}\nprint(_maintenance_json.dumps(prune_environments({:?}, apply={}, keep_last={})))",
        include_str!("../maintenance.py"),
        root.to_str().expect("UTF-8 root"),
        if apply { "True" } else { "False" },
        keep_last,
      ),
    ])
    .output()
    .expect("Python prune");
  assert!(
    output.status.success(),
    "Python prune failed: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  serde_json::from_slice(&output.stdout).expect("Python report")
}

#[test]
fn preview_is_read_only_and_apply_keeps_newest_existing_environment() {
  let (_temporary, root) = fixture_root();
  let older = run(&root, "run-old", "completed", "2026-10-01T00:00:00Z");
  let failed = run(&root, "run-failed", "failed", "2026-10-01T01:00:00Z");
  let newest = run(&root, "run-new", "completed", "2026-10-01T02:00:00Z");
  let absent = run(&root, "run-absent", "completed", "2026-10-01T03:00:00Z");
  fs::remove_dir_all(absent.join("environment/.venv")).expect("already absent");
  let before = tree(&root);
  let preview = prune(&root, &request(false, 1)).expect("preview");
  assert_eq!(
    before,
    tree(&root),
    "preview must not create leases or metadata"
  );
  assert_eq!(preview.logical_bytes, 22);
  assert_eq!(preview.pruned_runs, 0);
  assert_eq!(
    preview
      .runs
      .iter()
      .filter(|entry| entry.action == "preview")
      .count(),
    2
  );
  assert_eq!(
    preview
      .runs
      .iter()
      .find(|entry| entry.run_id == "run-new")
      .expect("newest")
      .action,
    "kept"
  );
  let result = prune(&root, &request(true, 1)).expect("apply");
  assert_eq!(result.logical_bytes, 22);
  assert_eq!(result.pruned_runs, 2);
  assert!(newest.join("environment/.venv/lib/package.py").is_file());
  for directory in [older, failed] {
    assert!(!directory.join("environment/.venv").exists());
    for file in [
      "code/train.py",
      "outputs/metrics.json",
      "snapshot.json",
      "run-state.json",
      "environment/owner.json",
      "environment/environment-state.json",
      ".run.lock",
      "environment/.prepare.lock",
      "environment/prune-state.json",
    ] {
      assert!(directory.join(file).is_file(), "must retain {file}");
    }
    assert_eq!(
      fs::read_to_string(directory.join("code/train.py")).expect("source"),
      "print('source')"
    );
    assert_eq!(
      read_json(&directory.join("environment/prune-state.json")).expect("audit")["logical_bytes"],
      11
    );
  }
  let repeated = prune(&root, &request(true, 1)).expect("repeat");
  assert_eq!(repeated.pruned_runs, 0);
  assert_eq!(
    repeated
      .runs
      .iter()
      .filter(|entry| entry.action == "already_pruned")
      .count(),
    3
  );
}

#[test]
fn unknown_stale_or_unowned_states_are_preserved() {
  let (_temporary, root) = fixture_root();
  let mut directories = Vec::new();
  for status in ["preparing", "running", "cancelled", "unknown"] {
    directories.push(run(
      &root,
      &format!("run-{status}"),
      status,
      "2000-01-01T00:00:00Z",
    ));
  }
  let missing = run(&root, "run-missing", "completed", "2026-10-01T00:00:00Z");
  fs::remove_file(missing.join("run-state.json")).expect("missing state");
  directories.push(missing);
  let malformed = run(&root, "run-malformed", "completed", "2026-10-01T00:00:00Z");
  fs::write(malformed.join("run-state.json"), "{").expect("malformed state");
  directories.push(malformed);
  directories.push(run(&root, "run-invalid-time", "completed", "yesterday"));
  let mismatched = run(&root, "run-mismatched", "completed", "2026-10-01T00:00:00Z");
  write_json(
    &mismatched.join("run-state.json"),
    &json!({"run_id":"another", "status":"completed"}),
  );
  directories.push(mismatched);
  let unowned = run(&root, "run-unowned", "failed", "2026-10-01T00:00:00Z");
  fs::remove_file(unowned.join("environment/owner.json")).expect("missing owner");
  directories.push(unowned);
  let wrong_owner = run(
    &root,
    "run-wrong-owner",
    "completed",
    "2026-10-01T00:00:00Z",
  );
  write_json(
    &wrong_owner.join("environment/owner.json"),
    &json!({"schema_version":1,"repo_root":"/different/code"}),
  );
  directories.push(wrong_owner);
  let result = prune(&root, &request(true, 0)).expect("apply");
  assert_eq!(result.pruned_runs, 0);
  assert!(result.runs.iter().all(|entry| entry.action == "skipped"));
  for directory in directories {
    assert!(directory.join("environment/.venv/lib/package.py").is_file());
    assert!(
      !directory.join(".run.lock").exists(),
      "uncertain states need no new lock file"
    );
  }
}

#[test]
fn failed_partial_preparation_can_be_pruned_with_valid_owner() {
  let (_temporary, root) = fixture_root();
  let directory = run(&root, "run-partial", "failed", "2026-10-01T00:00:00Z");
  fs::remove_file(directory.join("environment/environment-state.json"))
    .expect("partial preparation");
  let result = prune(&root, &request(true, 0)).expect("apply");
  assert_eq!(result.pruned_runs, 1);
  assert!(!directory.join("environment/.venv").exists());
  assert!(directory.join("environment/owner.json").is_file());
}

#[cfg(unix)]
#[test]
fn native_run_and_prepare_leases_protect_terminal_looking_runs_from_both_pruners() {
  let (_temporary, root) = fixture_root();
  let directory = run(&root, "run-active", "completed", "2026-10-01T00:00:00Z");
  let lease = lock::run_lock(&directory).expect("run lease");
  assert_eq!(
    prune(&root, &request(true, 0))
      .expect("native prune")
      .pruned_runs,
    0
  );
  assert_eq!(python_prune(&root, true, 0)["pruned_runs"], 0);
  drop(lease);
  let LockAttempt::Acquired(lease) =
    lock::try_lock_file(&directory.join("environment/.prepare.lock"), true).expect("prepare lease")
  else {
    panic!("prepare lease should be free");
  };
  assert_eq!(
    prune(&root, &request(true, 0))
      .expect("native prune")
      .pruned_runs,
    0
  );
  assert_eq!(python_prune(&root, true, 0)["pruned_runs"], 0);
  assert!(directory.join("environment/.venv/lib/package.py").is_file());
  drop(lease);
  let released = prune(&root, &request(true, 0)).expect("released prune");
  assert_eq!(
    released.pruned_runs, 1,
    "released prune report: {released:?}"
  );
}

#[cfg(unix)]
#[test]
fn python_run_lease_protects_run_from_native_pruning() {
  let (_temporary, root) = fixture_root();
  let directory = run(&root, "run-python-active", "failed", "2026-10-01T00:00:00Z");
  let mut child = Command::new("python3")
    .args([
      "-c",
      &format!(
        "{}\nhandle = acquire_run_lock({:?})\nprint('ready', flush=True)\ninput()",
        include_str!("../maintenance.py"),
        directory.to_str().expect("UTF-8 path"),
      ),
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .expect("Python lease process");
  let mut ready = String::new();
  BufReader::new(child.stdout.take().expect("child stdout"))
    .read_line(&mut ready)
    .expect("lease ready");
  let result = prune(&root, &request(true, 0));
  child
    .stdin
    .take()
    .expect("child stdin")
    .write_all(b"release\n")
    .expect("release lease");
  let status = child.wait().expect("lease process exit");
  assert_eq!(ready.trim(), "ready");
  assert!(status.success());
  assert_eq!(result.expect("native prune").pruned_runs, 0);
  assert!(directory.join("environment/.venv").is_dir());
}

#[cfg(unix)]
#[test]
fn prune_rejects_symlink_boundaries_and_never_follows_environment_links() {
  use std::os::unix::fs::symlink;

  let (_temporary, root) = fixture_root();
  let (_outside_temporary, outside) = fixture_root();
  fs::write(outside.join("precious"), "keep me").expect("outside sentinel");
  let venv_link = run(&root, "run-venv-link", "completed", "2026-10-01T00:00:00Z");
  fs::remove_dir_all(venv_link.join("environment/.venv")).expect("remove fixture venv");
  symlink(&outside, venv_link.join("environment/.venv")).expect("venv symlink");
  let owner_link = run(&root, "run-owner-link", "completed", "2026-10-01T00:00:00Z");
  fs::remove_file(owner_link.join("environment/owner.json")).expect("remove owner");
  symlink(
    outside.join("precious"),
    owner_link.join("environment/owner.json"),
  )
  .expect("owner symlink");
  let lock_link = run(&root, "run-lock-link", "completed", "2026-10-01T00:00:00Z");
  symlink(outside.join("precious"), lock_link.join(".run.lock")).expect("lock symlink");
  let parent_link = run(
    &root,
    "run-parent-link",
    "completed",
    "2026-10-01T00:00:00Z",
  );
  fs::remove_dir_all(parent_link.join("environment")).expect("remove environment parent");
  symlink(&outside, parent_link.join("environment")).expect("environment parent symlink");
  symlink(&outside, root.join(".expri/runs/run-directory-link")).expect("run symlink");
  let result = prune(&root, &request(true, 0)).expect("prune unsafe runs");
  assert_eq!(result.pruned_runs, 0);
  assert!(result.runs.iter().all(|entry| entry.action == "skipped"));
  assert_eq!(python_prune(&root, true, 0)["pruned_runs"], 0);
  assert_eq!(
    fs::read_to_string(outside.join("precious")).expect("outside sentinel"),
    "keep me"
  );

  let (_state_temporary, state_root) = fixture_root();
  symlink(&outside, state_root.join(".expri")).expect("state symlink");
  assert!(prune(&state_root, &request(true, 0)).is_err());
}

#[cfg(unix)]
#[test]
fn native_and_python_reports_match_for_preview_and_apply() {
  let (_temporary, root) = fixture_root();
  let older = run(
    &root,
    "run-old",
    "failed",
    "2026-10-01T00:00:00.000000001+00:00",
  );
  run(
    &root,
    "run-new",
    "completed",
    "2026-10-01T00:00:00.000000009Z",
  );
  run(&root, "run-stale", "running", "2000-01-01T00:00:00Z");
  let native = prune(&root, &request(false, 1)).expect("native preview");
  assert_eq!(
    serde_json::to_value(native).expect("native JSON"),
    python_prune(&root, false, 1)
  );
  let python = python_prune(&root, true, 1);
  fs::create_dir_all(older.join("environment/.venv/lib")).expect("restore test fixture");
  fs::write(
    older.join("environment/.venv/lib/package.py"),
    b"elevenbytes",
  )
  .expect("restore package");
  let native = prune(&root, &request(true, 1)).expect("native apply");
  assert_eq!(serde_json::to_value(native).expect("native JSON"), python);
}

#[cfg(unix)]
#[test]
fn prune_leaves_reused_package_and_interpreter_symlink_targets_untouched() {
  use std::os::unix::fs::symlink;

  let (_temporary, root) = fixture_root();
  let (_outside_temporary, outside) = fixture_root();
  fs::write(
    outside.join("base_python"),
    "shared heavy base installation",
  )
  .expect("base installation");
  let directory = run(
    &root,
    "run-reused-base",
    "completed",
    "2026-10-01T00:00:00Z",
  );
  symlink(
    &outside,
    directory.join("environment/.venv/lib/base_packages"),
  )
  .expect("base package link");
  symlink(
    outside.join("base_python"),
    directory.join("environment/.venv/python"),
  )
  .expect("interpreter link");
  assert_eq!(python_prune(&root, false, 0)["logical_bytes"], 11);
  let report = prune(&root, &request(true, 0)).expect("prune overlay");
  assert_eq!(report.logical_bytes, 11);
  assert_eq!(report.pruned_runs, 1);
  assert_eq!(
    fs::read_to_string(outside.join("base_python")).expect("preserved base"),
    "shared heavy base installation"
  );
  assert!(!directory.join("environment/.venv").exists());
}

#[test]
fn no_state_directory_preview_and_apply_create_nothing() {
  let (_temporary, root) = fixture_root();
  for apply in [false, true] {
    assert!(
      prune(&root, &request(apply, 1))
        .expect("no runs")
        .runs
        .is_empty()
    );
    assert!(!root.join(".expri").exists());
  }
}
