use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::archive::sha256_file;
use crate::error::{ExpriError, Result};
use crate::git;
use crate::protocol::{PullArtifacts, SyncApplyRequest};

#[derive(Debug, Deserialize, Serialize)]
struct SyncState {
  head: String,
  source_bundle_sha256: Option<String>,
  patch_sha256: String,
  checkout_manifest_sha256: Option<String>,
}

pub fn apply_request_file(path: &Path) -> Result<()> {
  let raw = fs::read_to_string(path).map_err(|source| ExpriError::IoContext {
    action: "read",
    path: path.display().to_string(),
    source,
  })?;
  let request = serde_json::from_str(&raw)?;
  apply_request(&request)
}

pub fn apply_request(request: &SyncApplyRequest) -> Result<()> {
  if let (Some(source_bundle), Some(source_bundle_sha256)) =
    (&request.source_bundle, &request.source_bundle_sha256)
  {
    verify_digest(
      Path::new(source_bundle),
      source_bundle_sha256,
      "source bundle",
    )?;
  }
  verify_digest(Path::new(&request.patch), &request.patch_sha256, "patch")?;

  let state_dir = Path::new(&request.state_dir);
  fs::create_dir_all(state_dir).map_err(|source| ExpriError::IoContext {
    action: "create directory",
    path: state_dir.display().to_string(),
    source,
  })?;

  if !request.force && sync_is_current(state_dir, request)? {
    println!("node sync is current");
    return Ok(());
  }

  let git_dir = state_dir.join("git");
  if !git_dir.is_dir() {
    run_git(vec![
      "init".to_string(),
      "--bare".to_string(),
      git_dir_string(&git_dir),
    ])?;
  }
  if let Some(remote_url) = &request.remote_url {
    let fetched_remote = run_git_success(vec![
      "--git-dir".to_string(),
      git_dir_string(&git_dir),
      "fetch".to_string(),
      remote_url.clone(),
      "+refs/heads/*:refs/remotes/bootstrap/*".to_string(),
      "+HEAD:refs/remotes/bootstrap/HEAD".to_string(),
    ])?;
    if fetched_remote && head_available(&git_dir, &request.head)? {
      run_git(vec![
        "--git-dir".to_string(),
        git_dir_string(&git_dir),
        "update-ref".to_string(),
        "refs/heads/synced".to_string(),
        request.head.clone(),
      ])?;
    }
  }
  if !head_available(&git_dir, &request.head)? {
    let Some(source_bundle) = &request.source_bundle else {
      return Err(ExpriError::Message(format!(
        "remote URL did not provide {}, and no source bundle was uploaded",
        request.head
      )));
    };
    run_git(vec![
      "--git-dir".to_string(),
      git_dir_string(&git_dir),
      "fetch".to_string(),
      source_bundle.clone(),
      "+HEAD:refs/heads/synced".to_string(),
    ])?;
  }
  let stage_dir = create_stage_dir(state_dir)?;
  run_git(vec![
    "--git-dir".to_string(),
    git_dir_string(&git_dir),
    "--work-tree".to_string(),
    git_dir_string(&stage_dir),
    "checkout".to_string(),
    "-f".to_string(),
    request.head.clone(),
  ])?;

  apply_patch_zip(
    &stage_dir,
    Path::new(&request.patch),
    &request.remote_managed,
  )?;
  let checkout_manifest_sha256 = install_staged_checkout(
    state_dir,
    &git_dir,
    &stage_dir,
    &request.remote_managed,
    &request.patch_sha256,
  )?;
  fs::remove_dir_all(&stage_dir).map_err(|source| ExpriError::IoContext {
    action: "remove directory",
    path: stage_dir.display().to_string(),
    source,
  })?;
  write_state(state_dir, request, checkout_manifest_sha256)?;
  Ok(())
}

pub fn prepare_pull() -> Result<()> {
  let state_dir = Path::new(".expri");
  let out_dir = state_dir.join("out");
  fs::create_dir_all(&out_dir).map_err(|source| ExpriError::IoContext {
    action: "create directory",
    path: out_dir.display().to_string(),
    source,
  })?;
  let head = git_capture(["rev-parse", "HEAD"])?;
  let bundle_path = out_dir.join("pull-source.bundle");
  run_git(vec![
    "bundle".to_string(),
    "create".to_string(),
    bundle_path.to_string_lossy().to_string(),
    "HEAD".to_string(),
  ])?;
  let dirty = git::dirty_paths(Path::new("."), &crate::filter::SyncRules::defaults()?)?;
  let patch = crate::archive::build_patch_archive(Path::new("."), &dirty)?;
  let patch_path = out_dir.join("pull-patch.zip");
  fs::copy(&patch.path, &patch_path).map_err(|source| ExpriError::IoContext {
    action: "copy",
    path: patch_path.display().to_string(),
    source,
  })?;
  let (source_bundle_sha256, _) = sha256_file(&bundle_path)?;
  let (patch_sha256, _) = sha256_file(&patch_path)?;
  let artifacts = PullArtifacts {
    head,
    source_bundle: ".expri/out/pull-source.bundle".to_string(),
    source_bundle_sha256,
    patch: ".expri/out/pull-patch.zip".to_string(),
    patch_sha256,
    state_dir: ".expri".to_string(),
  };
  fs::write(
    out_dir.join("pull-artifacts.json"),
    serde_json::to_string_pretty(&artifacts)?,
  )
  .map_err(|source| ExpriError::IoContext {
    action: "write",
    path: out_dir.join("pull-artifacts.json").display().to_string(),
    source,
  })?;
  Ok(())
}

fn verify_digest(path: &Path, expected: &str, label: &str) -> Result<()> {
  let (actual, _) = sha256_file(path)?;
  if actual != expected {
    return Err(ExpriError::Message(format!(
      "{label} sha256 mismatch: expected {expected}, got {actual}"
    )));
  }
  Ok(())
}

fn sync_is_current(state_dir: &Path, request: &SyncApplyRequest) -> Result<bool> {
  let path = state_path(state_dir);
  if !path.is_file() {
    return Ok(false);
  }
  let raw = fs::read_to_string(&path).map_err(|source| ExpriError::IoContext {
    action: "read",
    path: path.display().to_string(),
    source,
  })?;
  let state = serde_json::from_str::<SyncState>(&raw)?;
  Ok(
    state.head == request.head
      && state.patch_sha256 == request.patch_sha256
      && state.checkout_manifest_sha256.is_some(),
  )
}

fn write_state(
  state_dir: &Path,
  request: &SyncApplyRequest,
  checkout_manifest_sha256: String,
) -> Result<()> {
  let state = SyncState {
    head: request.head.clone(),
    source_bundle_sha256: request.source_bundle_sha256.clone(),
    patch_sha256: request.patch_sha256.clone(),
    checkout_manifest_sha256: Some(checkout_manifest_sha256),
  };
  let path = state_path(state_dir);
  let raw = serde_json::to_string_pretty(&state)?;
  fs::write(&path, raw).map_err(|source| ExpriError::IoContext {
    action: "write",
    path: path.display().to_string(),
    source,
  })?;
  Ok(())
}

fn state_path(state_dir: &Path) -> PathBuf {
  state_dir.join("sync-state.json")
}

fn create_stage_dir(state_dir: &Path) -> Result<PathBuf> {
  let tmp_dir = state_dir.join("tmp");
  fs::create_dir_all(&tmp_dir).map_err(|source| ExpriError::IoContext {
    action: "create directory",
    path: tmp_dir.display().to_string(),
    source,
  })?;
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_nanos())
    .unwrap_or(0);
  let stage_dir = tmp_dir.join(format!("sync-{}-{nanos}", std::process::id()));
  fs::create_dir_all(&stage_dir).map_err(|source| ExpriError::IoContext {
    action: "create directory",
    path: stage_dir.display().to_string(),
    source,
  })?;
  Ok(stage_dir)
}

fn apply_patch_zip(
  worktree_root: &Path,
  patch_path: &Path,
  remote_managed: &[String],
) -> Result<()> {
  let file = File::open(patch_path).map_err(|source| ExpriError::IoContext {
    action: "open",
    path: patch_path.display().to_string(),
    source,
  })?;
  let mut archive = ZipArchive::new(file)?;
  let deleted = read_deleted_paths(&mut archive)?;
  for path in deleted {
    if is_remote_managed(&path, remote_managed) {
      continue;
    }
    remove_worktree_file(worktree_root, &path)?;
  }

  for index in 0..archive.len() {
    let mut entry = archive.by_index(index)?;
    let Some(relative_path) = entry.enclosed_name() else {
      return Err(ExpriError::Message(format!(
        "patch archive contains unsafe path: {}",
        entry.name()
      )));
    };
    if relative_path == Path::new(".deleted") || entry.is_dir() {
      continue;
    }
    validate_relative_path(&relative_path)?;
    if is_remote_managed(&relative_path, remote_managed) {
      continue;
    }
    let destination = worktree_root.join(&relative_path);
    if let Some(parent) = destination.parent() {
      fs::create_dir_all(parent).map_err(|source| ExpriError::IoContext {
        action: "create directory",
        path: parent.display().to_string(),
        source,
      })?;
    }
    let mut output = File::create(&destination).map_err(|source| ExpriError::IoContext {
      action: "create",
      path: destination.display().to_string(),
      source,
    })?;
    io::copy(&mut entry, &mut output)?;
  }

  Ok(())
}

fn is_remote_managed(path: &Path, remote_managed: &[String]) -> bool {
  remote_managed
    .iter()
    .any(|managed| path == Path::new(managed))
}

fn read_deleted_paths(archive: &mut ZipArchive<File>) -> Result<Vec<PathBuf>> {
  let mut deleted = Vec::new();
  let Ok(mut entry) = archive.by_name(".deleted") else {
    return Ok(deleted);
  };
  let mut raw = String::new();
  entry.read_to_string(&mut raw)?;
  for line in raw.lines() {
    if line.is_empty() {
      continue;
    }
    let path = PathBuf::from(line);
    validate_relative_path(&path)?;
    deleted.push(path);
  }
  Ok(deleted)
}

fn install_staged_checkout(
  state_dir: &Path,
  git_dir: &Path,
  stage_dir: &Path,
  remote_managed: &[String],
  patch_digest: &str,
) -> Result<String> {
  let staged_files = collect_staged_files(stage_dir, remote_managed)?;
  let previous_files = previous_installed_files(state_dir, git_dir)?;

  for path in &staged_files {
    install_staged_file(stage_dir, path)?;
  }
  for path in previous_files.difference(&staged_files) {
    if !is_remote_managed(path, remote_managed) {
      remove_worktree_file(Path::new("."), path)?;
    }
  }
  let digest = write_manifest(checkout_manifest_path(state_dir), &staged_files)?;
  let old_patch_manifest = manifest_path(state_dir);
  if old_patch_manifest.exists() {
    fs::remove_file(&old_patch_manifest).map_err(|source| ExpriError::IoContext {
      action: "remove",
      path: old_patch_manifest.display().to_string(),
      source,
    })?;
  }
  fs::write(state_dir.join("patch.sha256"), patch_digest).map_err(|source| {
    ExpriError::IoContext {
      action: "write",
      path: state_dir.join("patch.sha256").display().to_string(),
      source,
    }
  })?;
  Ok(digest)
}

fn previous_installed_files(state_dir: &Path, git_dir: &Path) -> Result<BTreeSet<PathBuf>> {
  let checkout_manifest = checkout_manifest_path(state_dir);
  let mut previous_files = read_manifest(checkout_manifest.clone())?
    .into_iter()
    .chain(read_manifest(manifest_path(state_dir))?)
    .collect::<BTreeSet<_>>();
  if checkout_manifest.is_file() {
    return Ok(previous_files);
  }

  let state = read_sync_state(state_dir)?;
  if let Some(state) = state {
    previous_files.extend(git_tree_files(git_dir, &state.head)?);
  }
  Ok(previous_files)
}

fn read_sync_state(state_dir: &Path) -> Result<Option<SyncState>> {
  let path = state_path(state_dir);
  if !path.is_file() {
    return Ok(None);
  }
  let raw = fs::read_to_string(&path).map_err(|source| ExpriError::IoContext {
    action: "read",
    path: path.display().to_string(),
    source,
  })?;
  Ok(serde_json::from_str::<SyncState>(&raw).ok())
}

fn git_tree_files(git_dir: &Path, revision: &str) -> Result<BTreeSet<PathBuf>> {
  let output = Command::new("git")
    .args([
      "--git-dir",
      &git_dir_string(git_dir),
      "ls-tree",
      "-r",
      "-z",
      "--name-only",
      revision,
    ])
    .output()?;
  if !output.status.success() {
    return Ok(BTreeSet::new());
  }
  let mut files = BTreeSet::new();
  for value in output.stdout.split(|byte| *byte == 0) {
    if value.is_empty() {
      continue;
    }
    let path = PathBuf::from(String::from_utf8_lossy(value).as_ref());
    validate_relative_path(&path)?;
    files.insert(path);
  }
  Ok(files)
}

fn collect_staged_files(stage_dir: &Path, remote_managed: &[String]) -> Result<BTreeSet<PathBuf>> {
  let mut files = BTreeSet::new();
  collect_staged_files_inner(stage_dir, Path::new(""), remote_managed, &mut files)?;
  Ok(files)
}

fn collect_staged_files_inner(
  stage_dir: &Path,
  relative_dir: &Path,
  remote_managed: &[String],
  files: &mut BTreeSet<PathBuf>,
) -> Result<()> {
  for entry in
    fs::read_dir(stage_dir.join(relative_dir)).map_err(|source| ExpriError::IoContext {
      action: "read directory",
      path: stage_dir.join(relative_dir).display().to_string(),
      source,
    })?
  {
    let entry = entry?;
    let relative_path = relative_dir.join(entry.file_name());
    validate_relative_path(&relative_path)?;
    let metadata = entry.metadata()?;
    if metadata.is_dir() {
      collect_staged_files_inner(stage_dir, &relative_path, remote_managed, files)?;
    } else if metadata.is_file() && !is_remote_managed(&relative_path, remote_managed) {
      files.insert(relative_path);
    }
  }
  Ok(())
}

fn install_staged_file(stage_dir: &Path, path: &Path) -> Result<()> {
  let source = stage_dir.join(path);
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).map_err(|source| ExpriError::IoContext {
      action: "create directory",
      path: parent.display().to_string(),
      source,
    })?;
  }
  remove_worktree_path_for_install(path)?;
  if let Err(link_error) = fs::hard_link(&source, path) {
    fs::copy(&source, path).map_err(|source_error| {
      ExpriError::Message(format!(
        "failed to install {}: hard link failed: {}; copy failed: {}",
        path.display(),
        link_error,
        source_error
      ))
    })?;
  }
  Ok(())
}

fn remove_worktree_file(root: &Path, path: &Path) -> Result<()> {
  validate_relative_path(path)?;
  let absolute_path = root.join(path);
  match fs::symlink_metadata(&absolute_path) {
    Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
      fs::remove_file(&absolute_path).map_err(|source| ExpriError::IoContext {
        action: "remove",
        path: absolute_path.display().to_string(),
        source,
      })?;
    }
    Ok(_) | Err(_) => {}
  }
  Ok(())
}

fn remove_worktree_path_for_install(path: &Path) -> Result<()> {
  validate_relative_path(path)?;
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_dir() => {
      fs::remove_dir_all(path).map_err(|source| ExpriError::IoContext {
        action: "remove directory",
        path: path.display().to_string(),
        source,
      })?;
    }
    Ok(_) => {
      fs::remove_file(path).map_err(|source| ExpriError::IoContext {
        action: "remove",
        path: path.display().to_string(),
        source,
      })?;
    }
    Err(_) => {}
  }
  Ok(())
}

fn read_manifest(path: PathBuf) -> Result<BTreeSet<PathBuf>> {
  if !path.is_file() {
    return Ok(BTreeSet::new());
  }
  let raw = fs::read_to_string(&path).map_err(|source| ExpriError::IoContext {
    action: "read",
    path: path.display().to_string(),
    source,
  })?;
  let mut paths = BTreeSet::new();
  for line in raw.lines() {
    if line.is_empty() {
      continue;
    }
    let path = PathBuf::from(line);
    validate_relative_path(&path)?;
    paths.insert(path);
  }
  Ok(paths)
}

fn write_manifest(path: PathBuf, manifest: &BTreeSet<PathBuf>) -> Result<String> {
  let mut raw = String::new();
  for path in manifest {
    raw.push_str(&path.to_string_lossy());
    raw.push('\n');
  }
  fs::write(&path, raw).map_err(|source| ExpriError::IoContext {
    action: "write",
    path: path.display().to_string(),
    source,
  })?;
  sha256_file(&path).map(|(digest, _)| digest)
}

fn manifest_path(state_dir: &Path) -> PathBuf {
  state_dir.join("patch.manifest")
}

fn checkout_manifest_path(state_dir: &Path) -> PathBuf {
  state_dir.join("checkout.manifest")
}

fn validate_relative_path(path: &Path) -> Result<()> {
  if !path.is_relative()
    || path
      .components()
      .any(|component| !matches!(component, Component::Normal(_)))
  {
    return Err(ExpriError::Message(format!(
      "unsafe relative path: {}",
      path.display()
    )));
  }
  Ok(())
}

fn run_git(args: Vec<String>) -> Result<()> {
  let status = Command::new("git").args(args).status()?;
  if !status.success() {
    return Err(ExpriError::CommandFailed {
      program: "git".to_string(),
      code: status.code(),
    });
  }
  Ok(())
}

fn git_capture<const N: usize>(args: [&str; N]) -> Result<String> {
  let output = Command::new("git").args(args).output()?;
  if !output.status.success() {
    return Err(ExpriError::CommandFailed {
      program: "git".to_string(),
      code: output.status.code(),
    });
  }
  Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_git_success(args: Vec<String>) -> Result<bool> {
  let status = Command::new("git")
    .args(args)
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()?;
  Ok(status.success())
}

fn head_available(git_dir: &Path, head: &str) -> Result<bool> {
  run_git_success(vec![
    "--git-dir".to_string(),
    git_dir_string(git_dir),
    "cat-file".to_string(),
    "-e".to_string(),
    format!("{head}^{{commit}}"),
  ])
}

fn git_dir_string(path: &Path) -> String {
  path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
  use std::env;
  use std::io::Write;
  use std::sync::{Mutex, OnceLock};

  use zip::write::SimpleFileOptions;

  use super::*;

  #[test]
  fn sync_apply_fetches_checkout_and_applies_patch() {
    let _guard = cwd_lock().lock().expect("cwd lock");
    let source = tempfile::tempdir().expect("source tempdir");
    run_command_in(source.path(), ["git", "init"]);
    fs::write(source.path().join("tracked.txt"), "tracked\n").expect("write tracked");
    run_command_in(source.path(), ["git", "add", "tracked.txt"]);
    run_command_in(
      source.path(),
      [
        "git",
        "-c",
        "user.name=Expri",
        "-c",
        "user.email=expri@example.com",
        "commit",
        "-m",
        "initial",
      ],
    );
    let head = command_output_in(source.path(), ["git", "rev-parse", "HEAD"]);
    let bundle = source.path().join("source.bundle");
    run_command_in(
      source.path(),
      [
        "git",
        "bundle",
        "create",
        bundle.to_str().expect("bundle path"),
        "HEAD",
      ],
    );

    let patch = source.path().join("patch.zip");
    write_test_patch(&patch);
    let (source_bundle_sha256, _) = sha256_file(&bundle).expect("bundle sha");
    let (patch_sha256, _) = sha256_file(&patch).expect("patch sha");

    let worktree = tempfile::tempdir().expect("worktree tempdir");
    let previous_cwd = env::current_dir().expect("cwd");
    env::set_current_dir(worktree.path()).expect("set cwd");
    let request = SyncApplyRequest {
      head,
      remote_url: None,
      source_bundle: Some(bundle.to_string_lossy().to_string()),
      source_bundle_sha256: Some(source_bundle_sha256),
      patch: patch.to_string_lossy().to_string(),
      patch_sha256,
      state_dir: ".expri".to_string(),
      remote_managed: Vec::new(),
      force: false,
    };
    let result = apply_request(&request);
    env::set_current_dir(previous_cwd).expect("restore cwd");
    result.expect("sync apply");

    assert_eq!(
      fs::read_to_string(worktree.path().join("tracked.txt")).expect("tracked"),
      "tracked\n"
    );
    assert_eq!(
      fs::read_to_string(worktree.path().join("dirty.txt")).expect("dirty"),
      "dirty\n"
    );
    assert!(worktree.path().join(".expri/sync-state.json").is_file());
  }

  #[test]
  fn sync_apply_preserves_remote_managed_files() {
    let _guard = cwd_lock().lock().expect("cwd lock");
    let source = tempfile::tempdir().expect("source tempdir");
    run_command_in(source.path(), ["git", "init"]);
    fs::write(source.path().join("tracked.txt"), "tracked\n").expect("write tracked");
    fs::write(source.path().join("uv.lock"), "from git\n").expect("write lock");
    run_command_in(source.path(), ["git", "add", "tracked.txt", "uv.lock"]);
    run_command_in(
      source.path(),
      [
        "git",
        "-c",
        "user.name=Expri",
        "-c",
        "user.email=expri@example.com",
        "commit",
        "-m",
        "initial",
      ],
    );
    let head = command_output_in(source.path(), ["git", "rev-parse", "HEAD"]);
    let bundle = source.path().join("source.bundle");
    run_command_in(
      source.path(),
      [
        "git",
        "bundle",
        "create",
        bundle.to_str().expect("bundle path"),
        "HEAD",
      ],
    );

    let patch = source.path().join("patch.zip");
    write_patch_with_entries(&patch, &[("uv.lock", "from patch\n")]);
    let (source_bundle_sha256, _) = sha256_file(&bundle).expect("bundle sha");
    let (patch_sha256, _) = sha256_file(&patch).expect("patch sha");

    let worktree = tempfile::tempdir().expect("worktree tempdir");
    fs::write(worktree.path().join("uv.lock"), "from remote\n").expect("remote lock");
    let previous_cwd = env::current_dir().expect("cwd");
    env::set_current_dir(worktree.path()).expect("set cwd");
    let request = SyncApplyRequest {
      head,
      remote_url: None,
      source_bundle: Some(bundle.to_string_lossy().to_string()),
      source_bundle_sha256: Some(source_bundle_sha256),
      patch: patch.to_string_lossy().to_string(),
      patch_sha256,
      state_dir: ".expri".to_string(),
      remote_managed: vec!["uv.lock".to_string()],
      force: false,
    };
    let result = apply_request(&request);
    env::set_current_dir(previous_cwd).expect("restore cwd");
    result.expect("sync apply");

    assert_eq!(
      fs::read_to_string(worktree.path().join("uv.lock")).expect("lock"),
      "from remote\n"
    );
  }

  #[test]
  fn sync_apply_removes_previous_tracked_files_without_checkout_manifest() {
    let _guard = cwd_lock().lock().expect("cwd lock");
    let source = tempfile::tempdir().expect("source tempdir");
    run_command_in(source.path(), ["git", "init"]);
    fs::write(source.path().join("kept.txt"), "kept\n").expect("write kept");
    fs::write(source.path().join("gone.txt"), "gone\n").expect("write gone");
    run_command_in(source.path(), ["git", "add", "kept.txt", "gone.txt"]);
    run_command_in(
      source.path(),
      [
        "git",
        "-c",
        "user.name=Expri",
        "-c",
        "user.email=expri@example.com",
        "commit",
        "-m",
        "initial",
      ],
    );
    let old_head = command_output_in(source.path(), ["git", "rev-parse", "HEAD"]);
    fs::remove_file(source.path().join("gone.txt")).expect("remove gone");
    run_command_in(source.path(), ["git", "add", "gone.txt"]);
    run_command_in(
      source.path(),
      [
        "git",
        "-c",
        "user.name=Expri",
        "-c",
        "user.email=expri@example.com",
        "commit",
        "-m",
        "remove gone",
      ],
    );
    let head = command_output_in(source.path(), ["git", "rev-parse", "HEAD"]);
    let bundle = source.path().join("source.bundle");
    run_command_in(
      source.path(),
      [
        "git",
        "bundle",
        "create",
        bundle.to_str().expect("bundle path"),
        "HEAD",
      ],
    );

    let patch = source.path().join("patch.zip");
    write_patch_with_entries(&patch, &[]);
    let (source_bundle_sha256, _) = sha256_file(&bundle).expect("bundle sha");
    let (patch_sha256, _) = sha256_file(&patch).expect("patch sha");

    let worktree = tempfile::tempdir().expect("worktree tempdir");
    fs::write(worktree.path().join("kept.txt"), "old kept\n").expect("old kept");
    fs::write(worktree.path().join("gone.txt"), "old gone\n").expect("old gone");
    fs::create_dir_all(worktree.path().join(".expri")).expect("expri dir");
    fs::write(
      worktree.path().join(".expri/sync-state.json"),
      format!(
        r#"{{
  "head": "{old_head}",
  "source_bundle_sha256": null,
  "patch_sha256": "old"
}}"#
      ),
    )
    .expect("old state");

    let previous_cwd = env::current_dir().expect("cwd");
    env::set_current_dir(worktree.path()).expect("set cwd");
    let request = SyncApplyRequest {
      head,
      remote_url: None,
      source_bundle: Some(bundle.to_string_lossy().to_string()),
      source_bundle_sha256: Some(source_bundle_sha256),
      patch: patch.to_string_lossy().to_string(),
      patch_sha256,
      state_dir: ".expri".to_string(),
      remote_managed: Vec::new(),
      force: false,
    };
    let result = apply_request(&request);
    env::set_current_dir(previous_cwd).expect("restore cwd");
    result.expect("sync apply");

    assert!(worktree.path().join("kept.txt").is_file());
    assert!(!worktree.path().join("gone.txt").exists());
  }

  #[test]
  fn sync_current_ignores_source_bundle_digest() {
    let state_dir = tempfile::tempdir().expect("state tempdir");
    fs::write(
      state_path(state_dir.path()),
      r#"{
  "head": "abc",
  "source_bundle_sha256": "old",
  "patch_sha256": "patch",
  "checkout_manifest_sha256": "manifest"
}"#,
    )
    .expect("write state");
    let request = SyncApplyRequest {
      head: "abc".to_string(),
      remote_url: None,
      source_bundle: Some("source.bundle".to_string()),
      source_bundle_sha256: Some("new".to_string()),
      patch: "patch.zip".to_string(),
      patch_sha256: "patch".to_string(),
      state_dir: ".expri".to_string(),
      remote_managed: Vec::new(),
      force: false,
    };

    assert!(sync_is_current(state_dir.path(), &request).expect("current"));
  }

  fn write_test_patch(path: &Path) {
    write_patch_with_entries(path, &[("dirty.txt", "dirty\n")]);
  }

  fn write_patch_with_entries(path: &Path, entries: &[(&str, &str)]) {
    let file = File::create(path).expect("patch file");
    let mut archive = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    archive.start_file(".deleted", options).expect("deleted");
    archive.write_all(b"").expect("deleted body");
    for (name, body) in entries {
      archive.start_file(*name, options).expect("entry");
      archive.write_all(body.as_bytes()).expect("entry body");
    }
    archive.finish().expect("finish patch");
  }

  fn run_command_in<const N: usize>(cwd: &Path, args: [&str; N]) {
    let (program, rest) = args.split_first().expect("program");
    let status = Command::new(program)
      .current_dir(cwd)
      .args(rest)
      .status()
      .expect("run command");
    assert!(status.success(), "{program} failed with {status}");
  }

  fn command_output_in<const N: usize>(cwd: &Path, args: [&str; N]) -> String {
    let (program, rest) = args.split_first().expect("program");
    let output = Command::new(program)
      .current_dir(cwd)
      .args(rest)
      .output()
      .expect("run command");
    assert!(output.status.success(), "{program} failed");
    String::from_utf8(output.stdout)
      .expect("utf8")
      .trim()
      .to_string()
  }

  fn cwd_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
  }
}
