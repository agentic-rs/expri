use std::collections::BTreeSet;
use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use chrono::Utc;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::archive::sha256_file;
use crate::error::{ExpriError, Result};
use crate::filter::DEFAULT_EXCLUDED_DIRS;
use crate::protocol::SyncIdentity;

#[derive(Debug)]
pub struct RunSnapshot {
  pub run_id: String,
  pub run_dir: PathBuf,
  pub code_dir: PathBuf,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SourceKind {
  Git,
  SyncedCheckout,
}

#[derive(Debug, PartialEq, Serialize)]
struct SourceSelection {
  kind: SourceKind,
  git_head: Option<String>,
  checkout_manifest_sha256: Option<String>,
  sync_state_sha256: Option<String>,
  paths: BTreeSet<PathBuf>,
}

#[derive(Debug, PartialEq, Serialize)]
struct SourceFile {
  path: PathBuf,
  sha256: Option<String>,
  size: Option<u64>,
  symlink_target: Option<PathBuf>,
  #[cfg(unix)]
  mode: u32,
}

#[derive(Serialize)]
struct SnapshotManifest<'a> {
  run_id: &'a str,
  created_at: String,
  source: &'a SourceSelection,
  files: &'a [SourceFile],
}

/// Copies the current checkout into a run without sharing mutable source files.
///
/// Sync and snapshot preparation hold the same checkout lock. The file selection
/// and contents are also checked again before publishing to detect local edits.
#[cfg(test)]
pub fn create(repo_root: &Path, remote_managed: &[String]) -> Result<RunSnapshot> {
  create_expected(repo_root, remote_managed, None)
}

pub fn create_expected(
  repo_root: &Path,
  remote_managed: &[String],
  expected_sync: Option<&SyncIdentity>,
) -> Result<RunSnapshot> {
  let repo_root =
    fs::canonicalize(repo_root).map_err(|source| io_context("resolve", repo_root, source))?;
  let state_dir = repo_root.join(".expri");
  create_private_directory(&state_dir)?;
  let _checkout_lock = crate::lock::worktree_lock(&state_dir)?;
  if let Some(expected) = expected_sync {
    verify_sync_identity(&state_dir, expected)?;
  }
  let source = select_source(&repo_root, remote_managed)?;
  let runs_dir = state_dir.join("runs");
  create_private_directory(&runs_dir)?;
  let temporary = tempfile::Builder::new()
    .prefix("run-")
    .tempdir_in(&runs_dir)?;
  let run_dir = temporary.path().to_path_buf();
  let run_id = run_dir
    .file_name()
    .expect("temporary directory has a file name")
    .to_string_lossy()
    .into_owned();
  let code_dir = run_dir.join("code");
  fs::create_dir(&code_dir).map_err(|error| io_context("create directory", &code_dir, error))?;
  let outputs_dir = run_dir.join("outputs");
  fs::create_dir(&outputs_dir)
    .map_err(|error| io_context("create directory", &outputs_dir, error))?;

  let mut files = Vec::new();
  let mut missing = Vec::new();
  for relative_path in &source.paths {
    ensure_safe_parents(&repo_root, relative_path)?;
    match copy_source_file(&repo_root, &code_dir, relative_path)? {
      Some(file) => files.push(file),
      None
        if source.kind == SourceKind::Git || is_remote_managed(relative_path, remote_managed) =>
      {
        missing.push(relative_path);
      }
      None => return Err(source_changed(relative_path)),
    }
  }

  for copied in &files {
    ensure_safe_parents(&repo_root, &copied.path)?;
    if inspect_source_file(&repo_root, &copied.path)?.as_ref() != Some(copied) {
      return Err(source_changed(&copied.path));
    }
  }
  for relative_path in missing {
    ensure_safe_parents(&repo_root, relative_path)?;
    if inspect_source_file(&repo_root, relative_path)?.is_some() {
      return Err(source_changed(relative_path));
    }
  }
  if select_source(&repo_root, remote_managed)? != source {
    return Err(ExpriError::Message(
      "checkout changed while preparing the run snapshot; retry after edits or source pushing finish"
        .to_string(),
    ));
  }

  let manifest = SnapshotManifest {
    run_id: &run_id,
    created_at: Utc::now().to_rfc3339(),
    source: &source,
    files: &files,
  };
  let manifest_path = run_dir.join("snapshot.json");
  fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)
    .map_err(|error| io_context("write", &manifest_path, error))?;
  let run_dir = temporary.keep();
  Ok(RunSnapshot {
    run_id,
    run_dir,
    code_dir,
  })
}

fn verify_sync_identity(state_dir: &Path, expected: &SyncIdentity) -> Result<()> {
  let raw = read_optional_regular_file(&state_dir.join("sync-state.json"))?;
  let actual = raw.and_then(|raw| serde_json::from_slice::<SyncIdentity>(&raw).ok());
  if actual.as_ref() != Some(expected) {
    return Err(ExpriError::Message(
      "target checkout changed after push; retry run".to_string(),
    ));
  }
  Ok(())
}

fn select_source(repo_root: &Path, remote_managed: &[String]) -> Result<SourceSelection> {
  let manifest_path = repo_root.join(".expri/checkout.manifest");
  let mut paths = BTreeSet::new();
  let mut source = match read_optional_regular_file(&manifest_path)? {
    Some(raw) => {
      let text = std::str::from_utf8(&raw).map_err(|error| {
        ExpriError::Message(format!(
          "invalid checkout manifest {}: {error}",
          manifest_path.display()
        ))
      })?;
      for line in text.lines().filter(|line| !line.is_empty()) {
        let path = PathBuf::from(line);
        validate_source_path(&path)?;
        if should_include(&path) {
          paths.insert(path);
        }
      }
      let sync_state = read_optional_regular_file(&repo_root.join(".expri/sync-state.json"))?;
      let git_head = sync_state.as_ref().and_then(|raw| {
        serde_json::from_slice::<serde_json::Value>(raw)
          .ok()
          .and_then(|state| state.get("head")?.as_str().map(ToString::to_string))
      });
      SourceSelection {
        kind: SourceKind::SyncedCheckout,
        git_head,
        checkout_manifest_sha256: Some(digest(&raw)),
        sync_state_sha256: sync_state.map(|raw| digest(&raw)),
        paths: BTreeSet::new(),
      }
    }
    None => {
      let output = Command::new("git")
        .current_dir(repo_root)
        .args([
          "ls-files",
          "--cached",
          "--others",
          "--exclude-standard",
          "-z",
        ])
        .output()
        .map_err(|error| io_context("run git in", repo_root, error))?;
      if !output.status.success() {
        return Err(ExpriError::Message(format!(
          "cannot select source files in {}: use a Git checkout or push source to the target to create .expri/checkout.manifest",
          repo_root.display()
        )));
      }
      for value in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
      {
        let path = path_from_git(value)?;
        if !should_include(&path) {
          continue;
        }
        validate_source_path(&path)?;
        paths.insert(path);
      }
      let head = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map_err(|error| io_context("run git in", repo_root, error))?;
      SourceSelection {
        kind: SourceKind::Git,
        git_head: head
          .status
          .success()
          .then(|| String::from_utf8_lossy(&head.stdout).trim().to_string()),
        checkout_manifest_sha256: None,
        sync_state_sha256: None,
        paths: BTreeSet::new(),
      }
    }
  };
  for managed in remote_managed {
    let path = PathBuf::from(managed);
    validate_source_path(&path)?;
    if should_include(&path) {
      paths.insert(path);
    }
  }
  source.paths = paths;
  Ok(source)
}

fn should_include(path: &Path) -> bool {
  !path.components().any(|component| {
    matches!(component, Component::Normal(name) if name == ".expri" || DEFAULT_EXCLUDED_DIRS.iter().any(|excluded| name == *excluded))
  })
}

fn validate_source_path(path: &Path) -> Result<()> {
  if path.as_os_str().is_empty()
    || !path.is_relative()
    || path.components().any(|component| {
      !matches!(component, Component::Normal(name) if name != ".expri" && name != ".venv" && name != ".git")
    })
  {
    return Err(ExpriError::Message(format!("unsafe snapshot source path: {}", path.display())));
  }
  Ok(())
}

fn is_remote_managed(path: &Path, remote_managed: &[String]) -> bool {
  remote_managed
    .iter()
    .any(|managed| path == Path::new(managed))
}

fn ensure_safe_parents(root: &Path, relative_path: &Path) -> Result<()> {
  let Some(parent) = relative_path.parent() else {
    return Ok(());
  };
  let mut current = root.to_path_buf();
  for component in parent.components() {
    current.push(component);
    match fs::symlink_metadata(&current) {
      Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
      Ok(_) => {
        return Err(ExpriError::Message(format!(
          "snapshot source parent must be a directory without symlinks: {}",
          current.display()
        )));
      }
      Err(error) if error.kind() == io::ErrorKind::NotFound => break,
      Err(error) => return Err(io_context("inspect", &current, error)),
    }
  }
  Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
  match fs::create_dir(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
      let metadata =
        fs::symlink_metadata(path).map_err(|error| io_context("inspect", path, error))?;
      if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
      } else {
        Err(ExpriError::Message(format!(
          "run state directory must not be a symlink or file: {}",
          path.display()
        )))
      }
    }
    Err(error) => Err(io_context("create directory", path, error)),
  }
}

fn copy_source_file(
  root: &Path,
  code_dir: &Path,
  relative_path: &Path,
) -> Result<Option<SourceFile>> {
  let source_path = root.join(relative_path);
  let Some(metadata) = source_metadata(&source_path)? else {
    return Ok(None);
  };
  let destination = code_dir.join(relative_path);
  if let Some(parent) = destination.parent() {
    // A copied symlink is never permitted as a parent for a later entry.
    ensure_safe_parents(code_dir, relative_path)?;
    fs::create_dir_all(parent).map_err(|error| io_context("create directory", parent, error))?;
  }
  if metadata.file_type().is_symlink() {
    let target = fs::read_link(&source_path)
      .map_err(|error| io_context("read symlink", &source_path, error))?;
    copy_symlink(&source_path, &target, &destination)?;
    return Ok(Some(source_file(
      relative_path,
      &metadata,
      None,
      None,
      Some(target),
    )));
  }
  if !metadata.is_file() {
    return Err(ExpriError::Message(format!(
      "snapshot source must be a file or symlink: {} (Git submodules need their own source files pushed)",
      source_path.display()
    )));
  }
  let mut source =
    File::open(&source_path).map_err(|error| io_context("open", &source_path, error))?;
  if !same_file(&metadata, &source.metadata()?) {
    return Err(source_changed(relative_path));
  }
  let mut output =
    File::create(&destination).map_err(|error| io_context("create", &destination, error))?;
  io::copy(&mut source, &mut output).map_err(|error| io_context("copy", &source_path, error))?;
  output
    .set_permissions(metadata.permissions())
    .map_err(|error| io_context("set permissions on", &destination, error))?;
  let (sha256, size) = sha256_file(&destination)?;
  Ok(Some(source_file(
    relative_path,
    &metadata,
    Some(sha256),
    Some(size),
    None,
  )))
}

fn inspect_source_file(root: &Path, relative_path: &Path) -> Result<Option<SourceFile>> {
  let path = root.join(relative_path);
  let Some(metadata) = source_metadata(&path)? else {
    return Ok(None);
  };
  if metadata.file_type().is_symlink() {
    let target = fs::read_link(&path).map_err(|error| io_context("read symlink", &path, error))?;
    return Ok(Some(source_file(
      relative_path,
      &metadata,
      None,
      None,
      Some(target),
    )));
  }
  if !metadata.is_file() {
    return Err(source_changed(relative_path));
  }
  let (sha256, size) = sha256_file(&path)?;
  Ok(Some(source_file(
    relative_path,
    &metadata,
    Some(sha256),
    Some(size),
    None,
  )))
}

fn source_metadata(path: &Path) -> Result<Option<Metadata>> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(io_context("inspect", path, error)),
  }
}

fn source_file(
  path: &Path,
  metadata: &Metadata,
  sha256: Option<String>,
  size: Option<u64>,
  symlink_target: Option<PathBuf>,
) -> SourceFile {
  #[cfg(unix)]
  use std::os::unix::fs::PermissionsExt;
  SourceFile {
    path: path.to_path_buf(),
    sha256,
    size,
    symlink_target,
    #[cfg(unix)]
    mode: metadata.permissions().mode(),
  }
}

fn same_file(before: &Metadata, opened: &Metadata) -> bool {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    before.dev() == opened.dev() && before.ino() == opened.ino()
  }
  #[cfg(not(unix))]
  {
    before.len() == opened.len() && before.modified().ok() == opened.modified().ok()
  }
}

fn copy_symlink(_source: &Path, target: &Path, destination: &Path) -> Result<()> {
  #[cfg(unix)]
  let result = std::os::unix::fs::symlink(target, destination);
  #[cfg(windows)]
  let result = if _source.is_dir() {
    std::os::windows::fs::symlink_dir(target, destination)
  } else {
    std::os::windows::fs::symlink_file(target, destination)
  };
  #[cfg(not(any(unix, windows)))]
  let result = Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "symlinks are unsupported on this platform",
  ));
  result.map_err(|error| io_context("create symlink", destination, error))
}

fn read_optional_regular_file(path: &Path) -> Result<Option<Vec<u8>>> {
  let Some(metadata) = source_metadata(path)? else {
    return Ok(None);
  };
  if !metadata.is_file() || metadata.file_type().is_symlink() {
    return Err(ExpriError::Message(format!(
      "snapshot metadata must be a regular file: {}",
      path.display()
    )));
  }
  fs::read(path)
    .map(Some)
    .map_err(|error| io_context("read", path, error))
}

#[cfg(unix)]
fn path_from_git(bytes: &[u8]) -> Result<PathBuf> {
  use std::os::unix::ffi::OsStringExt;
  Ok(std::ffi::OsString::from_vec(bytes.to_vec()).into())
}

#[cfg(not(unix))]
fn path_from_git(bytes: &[u8]) -> Result<PathBuf> {
  std::str::from_utf8(bytes)
    .map(PathBuf::from)
    .map_err(|error| ExpriError::Message(format!("Git source path is not UTF-8: {error}")))
}

fn digest(bytes: &[u8]) -> String {
  use std::fmt::Write;
  let mut digest = String::with_capacity(64);
  for byte in Sha256::digest(bytes) {
    write!(&mut digest, "{byte:02x}").expect("write to string");
  }
  digest
}

fn source_changed(path: &Path) -> ExpriError {
  ExpriError::Message(format!(
    "source changed while preparing the run snapshot: {}; retry after edits or source pushing finish",
    path.display()
  ))
}

fn io_context(action: &'static str, path: &Path, source: io::Error) -> ExpriError {
  ExpriError::IoContext {
    action,
    path: path.display().to_string(),
    source,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
      .current_dir(root)
      .args(args)
      .output()
      .expect("run git");
    assert!(
      output.status.success(),
      "git failed: {}",
      String::from_utf8_lossy(&output.stderr)
    );
  }

  fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("repository");
    git(root.path(), &["init", "--quiet"]);
    root
  }

  fn synced_repository(manifest: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("synced repository");
    fs::create_dir(root.path().join(".expri")).expect("state directory");
    fs::write(root.path().join(".expri/checkout.manifest"), manifest).expect("manifest");
    root
  }

  #[test]
  fn independent_snapshots_capture_dirty_untracked_and_deleted_files() {
    let root = repository();
    fs::write(root.path().join("tracked.py"), "original").expect("source");
    fs::write(root.path().join("deleted.py"), "deleted").expect("deleted source");
    fs::write(root.path().join(".gitignore"), "ignored/\n").expect("ignore file");
    git(root.path(), &["add", "."]);
    fs::write(root.path().join("tracked.py"), "dirty").expect("dirty source");
    fs::remove_file(root.path().join("deleted.py")).expect("delete tracked file");
    fs::write(root.path().join("untracked.py"), "untracked").expect("untracked source");
    fs::create_dir(root.path().join("ignored")).expect("ignored directory");
    fs::write(root.path().join("ignored/large.bin"), "ignored").expect("ignored output");
    fs::create_dir(root.path().join("results")).expect("results directory");
    fs::write(root.path().join("results/source.py"), "actual source").expect("results source");

    let first = create(root.path(), &[]).expect("first snapshot");
    fs::write(root.path().join("tracked.py"), "later").expect("later source");
    let second = create(root.path(), &[]).expect("second snapshot");

    assert_ne!(first.run_id, second.run_id);
    assert_eq!(
      fs::read_to_string(first.code_dir.join("tracked.py")).expect("first source"),
      "dirty"
    );
    assert_eq!(
      fs::read_to_string(second.code_dir.join("tracked.py")).expect("second source"),
      "later"
    );
    assert!(first.code_dir.join("untracked.py").is_file());
    assert!(!first.code_dir.join("deleted.py").exists());
    assert!(!first.code_dir.join("ignored").exists());
    assert!(!first.code_dir.join(".expri").exists());
    assert!(first.code_dir.join("results/source.py").is_file());
    assert!(first.run_dir.join("outputs").is_dir());
    assert!(first.run_dir.join("snapshot.json").is_file());
  }

  #[test]
  fn synced_snapshot_adds_remote_managed_lock_without_copying_other_remote_files() {
    let root = synced_repository("train.py\n");
    fs::write(root.path().join("train.py"), "train").expect("source");
    fs::write(root.path().join("uv.lock"), "remote lock").expect("lock");
    fs::write(root.path().join("result.bin"), "heavy output").expect("output");
    fs::write(
      root.path().join(".expri/sync-state.json"),
      r#"{"head":"synced-head"}"#,
    )
    .expect("sync state");
    let snapshot = create(root.path(), &["uv.lock".to_string()]).expect("snapshot");
    let provenance: serde_json::Value = serde_json::from_slice(
      &fs::read(snapshot.run_dir.join("snapshot.json")).expect("snapshot manifest"),
    )
    .expect("snapshot manifest JSON");
    assert_eq!(provenance["source"]["git_head"], "synced-head");
    assert_eq!(
      fs::read_to_string(snapshot.code_dir.join("uv.lock")).expect("snapshot lock"),
      "remote lock"
    );
    fs::write(root.path().join("uv.lock"), "new lock").expect("later lock");
    assert_eq!(
      fs::read_to_string(snapshot.code_dir.join("uv.lock")).expect("stable lock"),
      "remote lock"
    );
    assert!(!snapshot.code_dir.join("result.bin").exists());
  }

  #[test]
  fn unsafe_manifests_and_remote_managed_paths_are_rejected() {
    for unsafe_path in [
      "../outside.py",
      "/outside.py",
      ".expri/sync-state.json",
      ".venv/bin/python",
      ".git/config",
    ] {
      let root = synced_repository(&format!("{unsafe_path}\n"));
      assert!(create(root.path(), &[]).is_err(), "accepted {unsafe_path}");
      let root = synced_repository("");
      assert!(
        create(root.path(), &[unsafe_path.to_string()]).is_err(),
        "accepted managed {unsafe_path}"
      );
    }
  }

  #[test]
  fn missing_manifest_source_fails_and_cleans_partial_run() {
    let root = synced_repository("missing.py\n");
    assert!(create(root.path(), &[]).is_err());
    assert_eq!(
      fs::read_dir(root.path().join(".expri/runs"))
        .expect("runs")
        .count(),
      0
    );
  }

  #[test]
  fn missing_source_selection_gives_actionable_error() {
    let root = tempfile::tempdir().expect("directory");
    let error = create(root.path(), &[]).expect_err("source selection should fail");
    assert!(
      error
        .to_string()
        .contains("Git checkout or push source to the target")
    );
  }

  #[cfg(unix)]
  #[test]
  fn native_and_python_snapshots_reject_checkout_replaced_after_sync() {
    let expected = SyncIdentity {
      head: "launch-a".to_string(),
      patch_sha256: "patch-a".to_string(),
    };
    for state in [
      r#"{"head":"launch-b","patch_sha256":"patch-b"}"#,
      r#"{"head":"launch-a","patch_sha256":"patch-b"}"#,
      r#"{"head":"launch-a"}"#,
    ] {
      let root = synced_repository("train.py\n");
      fs::write(root.path().join("train.py"), "source b").expect("source");
      fs::write(root.path().join(".expri/sync-state.json"), state).expect("sync state");
      let native =
        create_expected(root.path(), &[], Some(&expected)).expect_err("wrong native checkout");
      assert!(
        native
          .to_string()
          .contains("target checkout changed after push; retry run")
      );
      let python = python_snapshot_expected(root.path(), &[], Some(&expected))
        .expect_err("wrong Python checkout");
      assert!(python.contains("target checkout changed after push; retry run"));
      assert!(!root.path().join(".expri/runs").exists());
    }
  }

  #[cfg(unix)]
  #[test]
  fn native_and_python_snapshots_accept_matching_sync_receipt() {
    let root = synced_repository("train.py\n");
    fs::write(root.path().join("train.py"), "source a").expect("source");
    let expected = SyncIdentity {
      head: "launch-a".to_string(),
      patch_sha256: "patch-a".to_string(),
    };
    fs::write(
      root.path().join(".expri/sync-state.json"),
      serde_json::to_vec(&expected).expect("sync state JSON"),
    )
    .expect("sync state");
    let native =
      create_expected(root.path(), &[], Some(&expected)).expect("matching native checkout");
    let python = python_snapshot_expected(root.path(), &[], Some(&expected))
      .expect("matching Python checkout");
    assert!(native.code_dir.join("train.py").is_file());
    assert!(
      Path::new(python["code_dir"].as_str().expect("Python code path"))
        .join("train.py")
        .is_file()
    );
  }

  #[cfg(unix)]
  #[test]
  fn sync_receipt_requires_regular_state_file() {
    use std::os::unix::fs::symlink;
    let root = synced_repository("train.py\n");
    fs::write(root.path().join("train.py"), "source a").expect("source");
    let expected = SyncIdentity {
      head: "launch-a".to_string(),
      patch_sha256: "patch-a".to_string(),
    };
    let outside = tempfile::NamedTempFile::new().expect("external state");
    fs::write(
      outside.path(),
      serde_json::to_vec(&expected).expect("sync state JSON"),
    )
    .expect("external sync state");
    symlink(outside.path(), root.path().join(".expri/sync-state.json")).expect("state symlink");
    assert!(
      create_expected(root.path(), &[], Some(&expected))
        .expect_err("native symlink state")
        .to_string()
        .contains("regular file")
    );
    assert!(
      python_snapshot_expected(root.path(), &[], Some(&expected))
        .expect_err("Python symlink state")
        .contains("regular file")
    );
    assert!(!root.path().join(".expri/runs").exists());
  }

  #[cfg(unix)]
  #[test]
  fn python_fallback_matches_native_snapshot_files_and_provenance() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = synced_repository("train.sh\nentry.sh\n");
    fs::write(
      root.path().join(".expri/sync-state.json"),
      r#"{"head":"synced-head"}"#,
    )
    .expect("sync state");
    fs::write(root.path().join("train.sh"), "#!/bin/sh\n").expect("script");
    fs::set_permissions(
      root.path().join("train.sh"),
      fs::Permissions::from_mode(0o750),
    )
    .expect("script permissions");
    symlink("train.sh", root.path().join("entry.sh")).expect("entry symlink");
    fs::write(root.path().join("uv.lock"), "remote lock").expect("lock");
    fs::write(root.path().join("generated.bin"), "excluded").expect("generated output");
    let managed = ["uv.lock".to_string()];
    let native = create(root.path(), &managed).expect("native snapshot");
    let python = python_snapshot(root.path(), &managed).expect("Python snapshot");
    let python_dir = PathBuf::from(python["run_dir"].as_str().expect("Python run directory"));
    let native_manifest: serde_json::Value = serde_json::from_slice(
      &fs::read(native.run_dir.join("snapshot.json")).expect("native manifest"),
    )
    .expect("native manifest JSON");
    let python_manifest: serde_json::Value =
      serde_json::from_slice(&fs::read(python_dir.join("snapshot.json")).expect("Python manifest"))
        .expect("Python manifest JSON");
    assert_eq!(native_manifest["source"], python_manifest["source"]);
    assert_eq!(native_manifest["files"], python_manifest["files"]);
    assert!(!python_dir.join("code/generated.bin").exists());
    assert_eq!(
      fs::read_link(python_dir.join("code/entry.sh")).expect("Python copied symlink"),
      Path::new("train.sh")
    );
  }

  #[cfg(unix)]
  #[test]
  fn python_fallback_rejects_unsafe_manifest_and_cleans_failed_run() {
    let root = synced_repository("../outside.py\n");
    assert!(python_snapshot(root.path(), &[]).is_err());
    let root = synced_repository("missing.py\n");
    assert!(python_snapshot(root.path(), &[]).is_err());
    assert_eq!(
      fs::read_dir(root.path().join(".expri/runs"))
        .expect("runs")
        .count(),
      0
    );
  }

  #[cfg(unix)]
  fn python_snapshot(
    root: &Path,
    remote_managed: &[String],
  ) -> std::result::Result<serde_json::Value, String> {
    python_snapshot_expected(root, remote_managed, None)
  }

  #[cfg(unix)]
  fn python_snapshot_expected(
    root: &Path,
    remote_managed: &[String],
    expected_sync: Option<&SyncIdentity>,
  ) -> std::result::Result<serde_json::Value, String> {
    use std::io::Write;
    use std::process::Stdio;
    let script = format!(
      "{}\nimport sys\nrequest = _snapshot_json.load(sys.stdin)\n_snapshot_json.dump(create_snapshot(request['repo_root'], request['remote_managed'], request.get('expected_sync')), sys.stdout)\n",
      include_str!("snapshot.py")
    );
    let mut child = Command::new("python3")
      .args(["-c", &script])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .expect("spawn Python snapshot");
    let request = serde_json::json!({"repo_root": root, "remote_managed": remote_managed, "expected_sync": expected_sync});
    child
      .stdin
      .take()
      .expect("stdin")
      .write_all(&serde_json::to_vec(&request).expect("request JSON"))
      .expect("write request");
    let output = child.wait_with_output().expect("Python snapshot output");
    if !output.status.success() {
      return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(serde_json::from_slice(&output.stdout).expect("Python snapshot JSON"))
  }

  #[cfg(unix)]
  #[test]
  fn snapshots_preserve_executable_permissions_and_leaf_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = synced_repository("train.sh\nentry.sh\nbroken-link\n");
    fs::write(root.path().join("train.sh"), "#!/bin/sh\n").expect("script");
    fs::set_permissions(
      root.path().join("train.sh"),
      fs::Permissions::from_mode(0o751),
    )
    .expect("script permissions");
    symlink("train.sh", root.path().join("entry.sh")).expect("source symlink");
    symlink("missing", root.path().join("broken-link")).expect("broken symlink");
    let snapshot = create(root.path(), &[]).expect("snapshot");
    assert_eq!(
      fs::metadata(snapshot.code_dir.join("train.sh"))
        .expect("script metadata")
        .permissions()
        .mode()
        & 0o777,
      0o751
    );
    assert_eq!(
      fs::read_link(snapshot.code_dir.join("entry.sh")).expect("copied symlink"),
      Path::new("train.sh")
    );
    assert_eq!(
      fs::read_link(snapshot.code_dir.join("broken-link")).expect("copied broken symlink"),
      Path::new("missing")
    );
    fs::write(snapshot.code_dir.join("train.sh"), "snapshot edit").expect("change copy");
    assert_eq!(
      fs::read_to_string(root.path().join("train.sh")).expect("original"),
      "#!/bin/sh\n"
    );
  }

  #[cfg(unix)]
  #[test]
  fn snapshots_reject_symlink_parents_and_state_directories() {
    use std::os::unix::fs::symlink;
    let root = synced_repository("linked/train.py\n");
    let outside = tempfile::tempdir().expect("outside");
    fs::write(outside.path().join("train.py"), "outside").expect("outside source");
    symlink(outside.path(), root.path().join("linked")).expect("parent symlink");
    assert!(
      create(root.path(), &[])
        .expect_err("symlink parent")
        .to_string()
        .contains("without symlinks")
    );
    let root = repository();
    symlink(outside.path(), root.path().join(".expri")).expect("state symlink");
    assert!(create(root.path(), &[]).is_err());
  }
}
