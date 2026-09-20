use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use super::ssh_sync_apply_script;
use crate::archive::{build_patch_archive, sha256_file};
use crate::filter::SyncRules;
use crate::git;
use crate::protocol::SyncApplyRequest;

struct SyncFixture {
  root: TempDir,
  source: PathBuf,
  worktree: PathBuf,
}

impl SyncFixture {
  fn new(files: &[(&str, &str)]) -> Self {
    let root = tempfile::tempdir().expect("fixture directory");
    let source = root.path().join("source");
    let worktree = root.path().join("worktree");
    fs::create_dir(&source).expect("source directory");
    fs::create_dir(&worktree).expect("worktree directory");
    let fixture = Self {
      root,
      source,
      worktree,
    };
    fixture.git(&["init", "--quiet"]);
    for (path, contents) in files {
      fixture.write_source(path, contents);
    }
    fixture.commit();
    fixture
  }

  fn git(&self, args: &[&str]) {
    let output = Command::new("git")
      .current_dir(&self.source)
      .args(args)
      .output()
      .expect("run source git");
    assert!(
      output.status.success(),
      "git {args:?} failed: {}",
      String::from_utf8_lossy(&output.stderr)
    );
  }

  fn commit(&self) {
    self.git(&["add", "--all"]);
    self.git(&[
      "-c",
      "user.name=Expri Test",
      "-c",
      "user.email=expri@example.com",
      "-c",
      "commit.gpgsign=false",
      "commit",
      "--quiet",
      "-m",
      "fixture commit",
    ]);
  }

  fn write_source(&self, path: &str, contents: &str) {
    write_file(self.source.join(path), contents);
  }

  fn write_remote(&self, path: &str, contents: &str) {
    write_file(self.worktree.join(path), contents);
  }

  fn sync(&self, remote_managed: &[&str]) {
    let bundle = git::build_source_bundle(&self.source, None).expect("source bundle");
    let rules = SyncRules::defaults().expect("sync rules");
    let dirty = git::dirty_paths(&self.source, &rules).expect("dirty paths");
    let patch = build_patch_archive(&self.source, &dirty).expect("patch archive");
    let request = SyncApplyRequest {
      head: git::head(&self.source).expect("source HEAD"),
      remote_url: None,
      source_bundle: Some(bundle.path.to_string_lossy().into_owned()),
      source_bundle_sha256: Some(bundle.digest),
      patch: patch.path.to_string_lossy().into_owned(),
      patch_sha256: patch.digest,
      state_dir: ".expri".to_string(),
      remote_managed: remote_managed.iter().map(|path| path.to_string()).collect(),
      force: false,
    };
    let request_path = self.root.path().join("sync-request.json");
    fs::write(
      &request_path,
      serde_json::to_vec(&request).expect("serialize sync request"),
    )
    .expect("write sync request");
    let output = Command::new("python3")
      .current_dir(&self.worktree)
      .arg("-c")
      .arg(ssh_sync_apply_script(
        request_path.to_str().expect("request path"),
      ))
      .output()
      .expect("run SSH sync script");
    assert!(
      output.status.success(),
      "SSH sync failed:\nstdout: {}\nstderr: {}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }

  fn assert_remote(&self, path: &str, contents: &str) {
    assert_eq!(
      fs::read_to_string(self.worktree.join(path))
        .unwrap_or_else(|error| panic!("read remote {path}: {error}")),
      contents,
      "remote {path}"
    );
  }

  fn assert_absent(&self, path: &str) {
    assert!(
      !self.worktree.join(path).exists(),
      "remote {path} should be absent"
    );
  }

  fn assert_manifest(&self, contents: &str) {
    self.assert_remote(".expri/checkout.manifest", contents);
    let manifest = self.worktree.join(".expri/checkout.manifest");
    let (digest, _) = sha256_file(&manifest).expect("manifest digest");
    let state: serde_json::Value = serde_json::from_slice(
      &fs::read(self.worktree.join(".expri/sync-state.json")).expect("sync state"),
    )
    .expect("parse sync state");
    assert_eq!(state["checkout_manifest_sha256"], digest);
    self.assert_absent(".expri/patch.manifest");
  }

  fn mark_state_as_legacy(&self) {
    let state_path = self.worktree.join(".expri/sync-state.json");
    let mut state: serde_json::Value =
      serde_json::from_slice(&fs::read(&state_path).expect("read state")).expect("parse state");
    state
      .as_object_mut()
      .expect("state object")
      .remove("checkout_manifest_sha256");
    fs::write(
      state_path,
      serde_json::to_vec(&state).expect("legacy state"),
    )
    .expect("write legacy state");
  }
}

fn write_file(path: impl AsRef<Path>, contents: &str) {
  let path = path.as_ref();
  fs::create_dir_all(path.parent().expect("file parent")).expect("file directory");
  fs::write(path, contents).expect("write fixture file");
}

#[test]
fn ssh_sync_keeps_dirty_file_after_it_is_committed() {
  let fixture = SyncFixture::new(&[("tracked.txt", "initial\n")]);
  fixture.write_source("tracked.txt", "dirty\n");
  fixture.sync(&[]);
  fixture.assert_remote("tracked.txt", "dirty\n");

  fixture.commit();
  fixture.sync(&[]);
  fixture.assert_remote("tracked.txt", "dirty\n");
  fixture.assert_manifest("tracked.txt\n");
}

#[test]
fn ssh_sync_restores_file_when_local_changes_are_discarded() {
  let fixture = SyncFixture::new(&[("tracked.txt", "initial\n")]);
  fixture.write_source("tracked.txt", "dirty\n");
  fixture.sync(&[]);
  fixture.assert_remote("tracked.txt", "dirty\n");

  fixture.git(&["restore", "tracked.txt"]);
  fixture.sync(&[]);
  fixture.assert_remote("tracked.txt", "initial\n");
}

#[test]
fn ssh_sync_keeps_untracked_file_after_it_is_committed() {
  let fixture = SyncFixture::new(&[("tracked.txt", "initial\n")]);
  fixture.write_source("new.txt", "new file\n");
  fixture.sync(&[]);
  fixture.assert_remote("new.txt", "new file\n");

  fixture.commit();
  fixture.sync(&[]);
  fixture.assert_remote("new.txt", "new file\n");
  fixture.assert_manifest("new.txt\ntracked.txt\n");
}

#[test]
fn ssh_sync_removes_stale_files_and_preserves_generated_output() {
  let fixture = SyncFixture::new(&[("kept.txt", "kept\n"), ("gone.txt", "gone\n")]);
  fixture.write_source("stale.txt", "untracked\n");
  fixture.sync(&[]);
  fixture.assert_remote("gone.txt", "gone\n");
  fixture.assert_remote("stale.txt", "untracked\n");
  fixture.write_remote("out/result.txt", "generated\n");
  fixture.write_remote("unrelated.txt", "remote only\n");

  fs::remove_file(fixture.source.join("gone.txt")).expect("remove tracked file");
  fs::remove_file(fixture.source.join("stale.txt")).expect("remove untracked file");
  fixture.commit();
  fixture.sync(&[]);
  fixture.assert_absent("gone.txt");
  fixture.assert_absent("stale.txt");
  fixture.assert_remote("kept.txt", "kept\n");
  fixture.assert_remote("out/result.txt", "generated\n");
  fixture.assert_remote("unrelated.txt", "remote only\n");
  fixture.assert_manifest("kept.txt\n");
}

#[test]
fn ssh_sync_preserves_present_and_absent_remote_managed_files() {
  let fixture = SyncFixture::new(&[
    ("tracked.txt", "tracked\n"),
    ("present.lock", "from git\n"),
    ("absent.lock", "from git\n"),
  ]);
  fixture.write_remote("present.lock", "remote version\n");
  fixture.write_source("present.lock", "from patch\n");
  fixture.write_source("absent.lock", "from patch\n");
  let remote_managed = ["present.lock", "absent.lock"];
  fixture.sync(&remote_managed);
  fixture.assert_remote("present.lock", "remote version\n");
  fixture.assert_absent("absent.lock");

  fs::remove_file(fixture.source.join("present.lock")).expect("remove present source");
  fs::remove_file(fixture.source.join("absent.lock")).expect("remove absent source");
  fixture.sync(&remote_managed);
  fixture.assert_remote("present.lock", "remote version\n");
  fixture.assert_absent("absent.lock");
  fixture.assert_manifest("tracked.txt\n");
}

#[test]
fn ssh_sync_migrates_legacy_patch_manifest_and_previous_head() {
  let fixture = SyncFixture::new(&[("tracked.txt", "initial\n"), ("gone.txt", "gone\n")]);
  fixture.write_source("tracked.txt", "dirty\n");
  fixture.write_source("stale.txt", "untracked\n");
  fixture.sync(&[]);

  fs::remove_file(fixture.worktree.join(".expri/checkout.manifest")).expect("remove new manifest");
  fixture.write_remote(".expri/patch.manifest", "stale.txt\ntracked.txt\n");
  fixture.mark_state_as_legacy();
  fixture.write_remote("out/result.txt", "generated\n");

  fs::remove_file(fixture.source.join("gone.txt")).expect("remove tracked file");
  fs::remove_file(fixture.source.join("stale.txt")).expect("remove untracked file");
  fixture.commit();
  fixture.sync(&[]);
  fixture.assert_remote("tracked.txt", "dirty\n");
  fixture.assert_absent("gone.txt");
  fixture.assert_absent("stale.txt");
  fixture.assert_remote("out/result.txt", "generated\n");
  fixture.assert_manifest("tracked.txt\n");
}

#[test]
fn ssh_sync_cleans_files_listed_in_native_checkout_manifest() {
  let fixture = SyncFixture::new(&[("tracked.txt", "tracked\n")]);
  fixture.write_remote("native-only.txt", "previous native patch\n");
  fixture.write_remote("tracked.txt", "previous checkout\n");
  fixture.write_remote("generated.txt", "remote output\n");
  // The native protocol records all installed files as sorted, newline-delimited paths.
  fixture.write_remote(".expri/checkout.manifest", "native-only.txt\ntracked.txt\n");

  fixture.sync(&[]);
  fixture.assert_absent("native-only.txt");
  fixture.assert_remote("tracked.txt", "tracked\n");
  fixture.assert_remote("generated.txt", "remote output\n");
  fixture.assert_manifest("tracked.txt\n");
}

#[test]
fn ssh_sync_recovers_ownership_when_legacy_ssh_left_native_manifest() {
  let fixture = SyncFixture::new(&[("tracked.txt", "tracked\n"), ("gone.txt", "gone\n")]);
  fixture.sync(&[]);
  fixture.mark_state_as_legacy();
  fixture.write_remote("native-stale.txt", "from earlier native sync\n");
  fixture.write_remote(
    ".expri/checkout.manifest",
    "native-stale.txt\ntracked.txt\n",
  );
  fixture.write_remote("generated.txt", "remote output\n");

  fs::remove_file(fixture.source.join("gone.txt")).expect("remove tracked file");
  fixture.commit();
  fixture.sync(&[]);
  fixture.assert_absent("gone.txt");
  fixture.assert_absent("native-stale.txt");
  fixture.assert_remote("tracked.txt", "tracked\n");
  fixture.assert_remote("generated.txt", "remote output\n");
  fixture.assert_manifest("tracked.txt\n");
}

#[test]
fn ssh_sync_replaces_tracked_file_with_directory() {
  let fixture = SyncFixture::new(&[("config", "old file\n")]);
  fixture.sync(&[]);
  fixture.assert_remote("config", "old file\n");
  fixture.write_remote("out/result.txt", "generated\n");

  fs::remove_file(fixture.source.join("config")).expect("remove source file");
  fixture.write_source("config/settings.txt", "new settings\n");
  fixture.commit();
  fixture.sync(&[]);
  fixture.assert_remote("config/settings.txt", "new settings\n");
  fixture.assert_remote("out/result.txt", "generated\n");
  fixture.assert_manifest("config/settings.txt\n");
}

#[cfg(unix)]
#[test]
fn ssh_sync_replaces_tracked_symlink_with_directory_without_writing_through_it() {
  use std::os::unix::fs::symlink;

  let fixture = SyncFixture::new(&[("tracked.txt", "tracked\n")]);
  let target = fixture.root.path().join("symlink-target");
  write_file(target.join("result.txt"), "generated\n");
  symlink(&target, fixture.source.join("config")).expect("source symlink");
  fixture.commit();
  fixture.sync(&[]);
  assert_eq!(
    fs::read_link(fixture.worktree.join("config")).expect("remote symlink"),
    target
  );

  fs::remove_file(fixture.source.join("config")).expect("remove source symlink");
  fixture.write_source("config/settings.txt", "new settings\n");
  fixture.commit();
  fixture.sync(&[]);
  assert!(fixture.worktree.join("config").is_dir());
  assert!(!fixture.worktree.join("config").is_symlink());
  fixture.assert_remote("config/settings.txt", "new settings\n");
  assert!(!target.join("settings.txt").exists());
  assert_eq!(
    fs::read_to_string(target.join("result.txt")).expect("generated output"),
    "generated\n"
  );
  fixture.assert_manifest("config/settings.txt\ntracked.txt\n");
}

#[cfg(unix)]
#[test]
fn ssh_sync_preserves_tracked_executable_and_symlink() {
  use std::os::unix::fs::{PermissionsExt, symlink};

  let fixture = SyncFixture::new(&[("run.sh", "#!/bin/sh\necho initial\n")]);
  fs::set_permissions(
    fixture.source.join("run.sh"),
    fs::Permissions::from_mode(0o755),
  )
  .expect("executable source");
  symlink("run.sh", fixture.source.join("run-link")).expect("source symlink");
  fixture.commit();
  fixture.sync(&[]);

  fixture.write_source("run.sh", "#!/bin/sh\necho changed\n");
  fixture.sync(&[]);
  assert_ne!(
    fs::metadata(fixture.worktree.join("run.sh"))
      .expect("remote script metadata")
      .permissions()
      .mode()
      & 0o111,
    0
  );
  assert_eq!(
    fs::read_link(fixture.worktree.join("run-link")).expect("remote symlink"),
    Path::new("run.sh")
  );
  fixture.assert_remote("run-link", "#!/bin/sh\necho changed\n");
}
