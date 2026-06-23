use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use crate::archive::sha256_file;
use crate::error::{ExpriError, Result};
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
    if !rules.should_include(&relative_path) {
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
      code: output.status.code(),
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
      code: output.status.code(),
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
      code: status.code(),
    });
  }
  Ok(())
}
