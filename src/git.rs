use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use crate::archive::sha256_file;
use crate::error::{ExpriError, Result, command_exit_code};
use crate::filter::SyncRules;

#[derive(Debug)]
pub struct SourceBundle {
  pub _temp_dir: TempDir,
  pub path: PathBuf,
  pub digest: String,
  pub size: u64,
}

#[derive(Debug)]
pub struct DirtyPaths {
  pub files: Vec<PathBuf>,
  pub deleted: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct RemoteCandidate {
  pub name: String,
  pub url: String,
  pub base_commit: Option<String>,
  pub distance: Option<u64>,
}

pub fn head(repo_root: &Path) -> Result<String> {
  git_capture(repo_root, ["rev-parse", "HEAD"])
}

pub fn nearest_remote_url(repo_root: &Path, head: &str) -> Result<Option<RemoteCandidate>> {
  let remotes = git_capture(repo_root, ["remote"])?;
  let mut candidates = Vec::new();
  for remote in remotes.lines().filter(|remote| !remote.is_empty()) {
    let urls = git_capture_vec(repo_root, ["remote", "get-url", "--all", remote])?;
    let Some(url) = urls.first() else {
      continue;
    };
    let refs = remote_tracking_refs(repo_root, remote)?;
    let mut best_base = None;
    let mut best_distance = None;
    for ref_name in refs {
      let Ok(base) = git_capture(repo_root, ["merge-base", head, &ref_name]) else {
        continue;
      };
      if base.is_empty() {
        continue;
      }
      let distance = git_capture(
        repo_root,
        ["rev-list", "--count", &format!("{base}..{head}")],
      )?
      .parse::<u64>()
      .unwrap_or(u64::MAX);
      if best_distance.is_none_or(|current| distance < current) {
        best_base = Some(base);
        best_distance = Some(distance);
      }
    }
    candidates.push(RemoteCandidate {
      name: remote.to_string(),
      url: url.clone(),
      base_commit: best_base,
      distance: best_distance,
    });
  }
  candidates.sort_by(|left, right| {
    left
      .distance
      .unwrap_or(u64::MAX)
      .cmp(&right.distance.unwrap_or(u64::MAX))
      .then_with(|| remote_rank(&left.name).cmp(&remote_rank(&right.name)))
      .then_with(|| left.name.cmp(&right.name))
  });
  Ok(candidates.into_iter().next())
}

pub fn build_source_bundle(repo_root: &Path, base_commit: Option<&str>) -> Result<SourceBundle> {
  let temp_dir = tempfile::Builder::new().prefix("expri-source-").tempdir()?;
  let path = temp_dir.path().join("source.bundle");
  let refspec = match base_commit {
    Some(base_commit) => format!("{base_commit}..HEAD"),
    None => "HEAD".to_string(),
  };
  git_run(
    repo_root,
    [
      OsStr::new("bundle"),
      OsStr::new("create"),
      path.as_os_str(),
      OsStr::new(&refspec),
    ],
  )?;
  let (digest, size) = sha256_file(&path)?;
  Ok(SourceBundle {
    _temp_dir: temp_dir,
    path,
    digest,
    size,
  })
}

pub fn dirty_paths(repo_root: &Path, rules: &SyncRules) -> Result<DirtyPaths> {
  let mut relative_paths = BTreeSet::new();
  for value in git_capture_bytes(repo_root, ["diff", "--name-only", "-z", "HEAD", "--"])?
    .split(|byte| *byte == 0)
  {
    if !value.is_empty() {
      relative_paths.insert(PathBuf::from(String::from_utf8_lossy(value).as_ref()));
    }
  }
  for value in git_capture_bytes(
    repo_root,
    ["ls-files", "--others", "--exclude-standard", "-z"],
  )?
  .split(|byte| *byte == 0)
  {
    if !value.is_empty() {
      relative_paths.insert(PathBuf::from(String::from_utf8_lossy(value).as_ref()));
    }
  }
  for path in rules.include_ignored() {
    let relative_path = PathBuf::from(path);
    if repo_root.join(&relative_path).exists() {
      relative_paths.insert(relative_path);
    }
  }

  let mut files = Vec::new();
  let mut deleted = Vec::new();
  for relative_path in relative_paths {
    if !rules.should_include(&relative_path) || is_managed_asset(repo_root, &relative_path) {
      continue;
    }
    let absolute_path = repo_root.join(&relative_path);
    if absolute_path.is_file() {
      files.push(relative_path);
    } else {
      deleted.push(relative_path);
    }
  }
  Ok(DirtyPaths { files, deleted })
}

pub(crate) fn is_asset_sidecar(path: &Path) -> bool {
  path
    .file_name()
    .is_some_and(|name| name.to_string_lossy().ends_with(".expri.toml"))
}

pub(crate) fn is_managed_asset(repo_root: &Path, path: &Path) -> bool {
  let mut sidecar = path.as_os_str().to_os_string();
  sidecar.push(".expri.toml");
  std::fs::symlink_metadata(repo_root.join(PathBuf::from(sidecar))).is_ok()
}

pub(crate) fn is_worktree(repo_root: &Path) -> Result<bool> {
  let output = match Command::new("git")
    .current_dir(repo_root)
    .args(["rev-parse", "--is-inside-work-tree"])
    .output()
  {
    Ok(output) => output,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
    Err(source) => {
      return Err(ExpriError::IoContext {
        action: "run git in",
        path: repo_root.display().to_string(),
        source,
      });
    }
  };
  Ok(output.status.success() && output.stdout.as_slice() == b"true\n")
}

/// Paths that can become source code, including explicitly selected ignored files.
pub(crate) fn source_paths(repo_root: &Path, rules: &SyncRules) -> Result<Vec<PathBuf>> {
  // A --no-push controller may contain only configuration, without a Git checkout.
  if !is_worktree(repo_root)? {
    return Ok(Vec::new());
  }
  let mut paths = BTreeSet::new();
  for value in git_capture_bytes(
    repo_root,
    [
      "ls-files",
      "--cached",
      "--others",
      "--exclude-standard",
      "-z",
    ],
  )?
  .split(|byte| *byte == 0)
  .filter(|value| !value.is_empty())
  {
    paths.insert(path_from_git(value)?);
  }
  paths.extend(rules.include_ignored().iter().map(PathBuf::from));
  Ok(
    paths
      .into_iter()
      .filter(|path| {
        rules.should_include(path)
          && !is_managed_asset(repo_root, path)
          && std::fs::symlink_metadata(repo_root.join(path)).is_ok()
      })
      .collect(),
  )
}

/// A Git bundle includes tracked blobs before source exclusions are applied.
pub(crate) fn reject_tracked_assets(repo_root: &Path) -> Result<()> {
  for value in git_capture_bytes(repo_root, ["ls-files", "--cached", "-z"])?
    .split(|byte| *byte == 0)
    .filter(|value| !value.is_empty())
  {
    let path = path_from_git(value)?;
    if path.components().any(|component| {
      matches!(component, std::path::Component::Normal(name) if name.to_string_lossy().starts_with(crate::filter::ASSET_STAGING_PREFIX))
    }) {
      return Err(ExpriError::Message(format!(
        "temporary asset data is tracked by Git: {}; remove it from Git tracking before pushing source",
        path.display()
      )));
    }
    if is_managed_asset(repo_root, &path) {
      return Err(ExpriError::Message(format!(
        "asset data is tracked by Git: {}; remove it from Git tracking before pushing source (asset descriptors should be tracked)",
        path.display()
      )));
    }
  }
  Ok(())
}

#[cfg(unix)]
pub(crate) fn path_from_git(bytes: &[u8]) -> Result<PathBuf> {
  use std::os::unix::ffi::OsStringExt;
  Ok(std::ffi::OsString::from_vec(bytes.to_vec()).into())
}

#[cfg(not(unix))]
pub(crate) fn path_from_git(bytes: &[u8]) -> Result<PathBuf> {
  std::str::from_utf8(bytes)
    .map(PathBuf::from)
    .map_err(|error| ExpriError::Message(format!("Git source path is not UTF-8: {error}")))
}

pub fn ls_files(repo_root: &Path, paths: &[PathBuf]) -> Result<Vec<u8>> {
  let mut command = Command::new("git");
  command.current_dir(repo_root);
  command.args(["ls-files", "-z", "--"]);
  command.args(paths);
  let output = command.output().map_err(|source| ExpriError::IoContext {
    action: "run git in",
    path: repo_root.display().to_string(),
    source,
  })?;
  if !output.status.success() {
    return Err(ExpriError::CommandFailed {
      program: "git".to_string(),
      code: command_exit_code(&output.status),
    });
  }
  Ok(output.stdout)
}

fn git_capture<const N: usize>(repo_root: &Path, args: [&str; N]) -> Result<String> {
  let output = Command::new("git")
    .current_dir(repo_root)
    .args(args)
    .output()
    .map_err(|source| ExpriError::IoContext {
      action: "run git in",
      path: repo_root.display().to_string(),
      source,
    })?;
  if !output.status.success() {
    return Err(ExpriError::CommandFailed {
      program: "git".to_string(),
      code: command_exit_code(&output.status),
    });
  }
  Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_capture_vec<const N: usize>(repo_root: &Path, args: [&str; N]) -> Result<Vec<String>> {
  Ok(
    git_capture(repo_root, args)?
      .lines()
      .filter(|line| !line.is_empty())
      .map(ToString::to_string)
      .collect(),
  )
}

fn remote_tracking_refs(repo_root: &Path, remote: &str) -> Result<Vec<String>> {
  let prefix = format!("refs/remotes/{remote}");
  let output = git_capture(repo_root, ["for-each-ref", "--format=%(refname)", &prefix])?;
  Ok(
    output
      .lines()
      .filter(|ref_name| !ref_name.ends_with("/HEAD"))
      .map(ToString::to_string)
      .collect(),
  )
}

fn remote_rank(remote: &str) -> u8 {
  match remote {
    "origin" => 0,
    "upstream" => 1,
    _ => 2,
  }
}

fn git_capture_bytes<const N: usize>(repo_root: &Path, args: [&str; N]) -> Result<Vec<u8>> {
  let output = Command::new("git")
    .current_dir(repo_root)
    .args(args)
    .output()
    .map_err(|source| ExpriError::IoContext {
      action: "run git in",
      path: repo_root.display().to_string(),
      source,
    })?;
  if !output.status.success() {
    return Err(ExpriError::CommandFailed {
      program: "git".to_string(),
      code: command_exit_code(&output.status),
    });
  }
  Ok(output.stdout)
}

fn git_run<const N: usize>(repo_root: &Path, args: [&OsStr; N]) -> Result<()> {
  let status = Command::new("git")
    .current_dir(repo_root)
    .args(args)
    .status()
    .map_err(|source| ExpriError::IoContext {
      action: "run git in",
      path: repo_root.display().to_string(),
      source,
    })?;
  if !status.success() {
    return Err(ExpriError::CommandFailed {
      program: "git".to_string(),
      code: command_exit_code(&status),
    });
  }
  Ok(())
}

#[cfg(test)]
mod asset_tests {
  use super::*;
  use std::fs;

  fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git_capture(root.path(), ["init", "--quiet"]).unwrap();
    fs::write(root.path().join("train.py"), "train").unwrap();
    commit(root.path());
    root
  }

  fn commit(root: &Path) {
    git_capture(root, ["add", "."]).unwrap();
    git_capture(
      root,
      [
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.net",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--quiet",
        "-m",
        "test",
      ],
    )
    .unwrap();
  }

  #[test]
  fn source_and_patch_omit_managed_data_even_when_ignored_is_requested() {
    let root = repository();
    fs::write(
      root.path().join(".gitignore"),
      "data.bin\nignored.expri.toml\n",
    )
    .unwrap();
    fs::write(root.path().join("data.bin"), "heavy").unwrap();
    fs::write(root.path().join("data.bin.expri.toml"), "descriptor").unwrap();
    fs::write(root.path().join("ignored.expri.toml"), "ignored descriptor").unwrap();
    let rules =
      SyncRules::new(Vec::new(), Vec::new(), vec!["data.bin".into()], Vec::new()).unwrap();
    let selected = source_paths(root.path(), &rules).unwrap();
    assert!(!selected.contains(&PathBuf::from("data.bin")));
    assert!(selected.contains(&PathBuf::from("data.bin.expri.toml")));
    assert!(!selected.contains(&PathBuf::from("ignored.expri.toml")));
    let dirty = dirty_paths(root.path(), &rules).unwrap();
    assert!(!dirty.files.contains(&PathBuf::from("data.bin")));
    assert!(dirty.files.contains(&PathBuf::from("data.bin.expri.toml")));
    assert!(reject_tracked_assets(root.path()).is_ok());
  }

  #[test]
  fn tracked_asset_bytes_are_rejected_before_git_bundle_creation() {
    let root = repository();
    fs::write(root.path().join("data.bin"), "heavy").unwrap();
    fs::write(root.path().join("data.bin.expri.toml"), "descriptor").unwrap();
    commit(root.path());
    fs::write(root.path().join("data.bin"), "changed heavy").unwrap();
    let dirty = dirty_paths(root.path(), &SyncRules::defaults().unwrap()).unwrap();
    assert!(dirty.files.is_empty());
    assert!(
      reject_tracked_assets(root.path())
        .unwrap_err()
        .to_string()
        .contains("data.bin")
    );
    git_capture(root.path(), ["rm", "--cached", "data.bin"]).unwrap();
    assert!(reject_tracked_assets(root.path()).is_ok());
  }

  #[test]
  fn deleted_descriptors_are_source_deletions_and_do_not_require_assets() {
    let root = repository();
    fs::write(root.path().join("data.bin.expri.toml"), "descriptor").unwrap();
    commit(root.path());
    fs::remove_file(root.path().join("data.bin.expri.toml")).unwrap();
    let rules = SyncRules::defaults().unwrap();
    let dirty = dirty_paths(root.path(), &rules).unwrap();
    assert_eq!(dirty.deleted, [PathBuf::from("data.bin.expri.toml")]);
    assert!(
      !source_paths(root.path(), &rules)
        .unwrap()
        .iter()
        .any(|path| is_asset_sidecar(path))
    );
  }
}
