use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::archive::sha256_file;
use crate::error::{ExpriError, Result};
use crate::protocol::SyncApplyRequest;

#[derive(Debug, Deserialize, Serialize)]
struct SyncState {
  head: String,
  source_bundle_sha256: Option<String>,
  patch_sha256: String,
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
  run_git(vec![
    "--git-dir".to_string(),
    git_dir_string(&git_dir),
    "--work-tree".to_string(),
    ".".to_string(),
    "checkout".to_string(),
    "-f".to_string(),
    request.head.clone(),
  ])?;

  apply_patch_zip(state_dir, Path::new(&request.patch), &request.patch_sha256)?;
  write_state(state_dir, request)?;
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
      && state.source_bundle_sha256 == request.source_bundle_sha256
      && state.patch_sha256 == request.patch_sha256,
  )
}

fn write_state(state_dir: &Path, request: &SyncApplyRequest) -> Result<()> {
  let state = SyncState {
    head: request.head.clone(),
    source_bundle_sha256: request.source_bundle_sha256.clone(),
    patch_sha256: request.patch_sha256.clone(),
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

fn apply_patch_zip(state_dir: &Path, patch_path: &Path, patch_digest: &str) -> Result<()> {
  remove_previous_overlay(state_dir)?;
  let patch_dir = state_dir.join("patch");
  if patch_dir.exists() {
    fs::remove_dir_all(&patch_dir).map_err(|source| ExpriError::IoContext {
      action: "remove directory",
      path: patch_dir.display().to_string(),
      source,
    })?;
  }
  fs::create_dir_all(&patch_dir).map_err(|source| ExpriError::IoContext {
    action: "create directory",
    path: patch_dir.display().to_string(),
    source,
  })?;

  let file = File::open(patch_path).map_err(|source| ExpriError::IoContext {
    action: "open",
    path: patch_path.display().to_string(),
    source,
  })?;
  let mut archive = ZipArchive::new(file)?;
  let deleted = read_deleted_paths(&mut archive)?;
  for path in deleted {
    remove_worktree_file(&path)?;
  }

  let mut manifest = Vec::new();
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
    let destination = PathBuf::from(&relative_path);
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
    manifest.push(destination);
  }

  write_manifest(state_dir, &manifest)?;
  fs::write(state_dir.join("patch.sha256"), patch_digest).map_err(|source| {
    ExpriError::IoContext {
      action: "write",
      path: state_dir.join("patch.sha256").display().to_string(),
      source,
    }
  })?;
  Ok(())
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

fn remove_previous_overlay(state_dir: &Path) -> Result<()> {
  let manifest_path = manifest_path(state_dir);
  if !manifest_path.is_file() {
    return Ok(());
  }
  let raw = fs::read_to_string(&manifest_path).map_err(|source| ExpriError::IoContext {
    action: "read",
    path: manifest_path.display().to_string(),
    source,
  })?;
  for line in raw.lines() {
    if line.is_empty() {
      continue;
    }
    let path = PathBuf::from(line);
    validate_relative_path(&path)?;
    remove_worktree_file(&path)?;
  }
  Ok(())
}

fn remove_worktree_file(path: &Path) -> Result<()> {
  validate_relative_path(path)?;
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
      fs::remove_file(path).map_err(|source| ExpriError::IoContext {
        action: "remove",
        path: path.display().to_string(),
        source,
      })?;
    }
    Ok(_) | Err(_) => {}
  }
  Ok(())
}

fn write_manifest(state_dir: &Path, manifest: &[PathBuf]) -> Result<()> {
  let mut raw = String::new();
  for path in manifest {
    raw.push_str(&path.to_string_lossy());
    raw.push('\n');
  }
  let path = manifest_path(state_dir);
  fs::write(&path, raw).map_err(|source| ExpriError::IoContext {
    action: "write",
    path: path.display().to_string(),
    source,
  })?;
  Ok(())
}

fn manifest_path(state_dir: &Path) -> PathBuf {
  state_dir.join("patch.manifest")
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

  use zip::write::SimpleFileOptions;

  use super::*;

  #[test]
  fn sync_apply_fetches_checkout_and_applies_patch() {
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

  fn write_test_patch(path: &Path) {
    let file = File::create(path).expect("patch file");
    let mut archive = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    archive.start_file(".deleted", options).expect("deleted");
    archive.write_all(b"").expect("deleted body");
    archive.start_file("dirty.txt", options).expect("dirty");
    archive.write_all(b"dirty\n").expect("dirty body");
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
}
