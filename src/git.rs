use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

pub fn head(repo_root: &Path) -> Result<String> {
  git_capture(repo_root, ["rev-parse", "HEAD"])
}

pub fn local_has_commit(repo_root: &Path, commit: &str) -> Result<bool> {
  let status = Command::new("git")
    .current_dir(repo_root)
    .args(["cat-file", "-e"])
    .arg(format!("{commit}^{{commit}}"))
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .map_err(|source| ExpriError::IoContext {
      action: "run git in",
      path: repo_root.display().to_string(),
      source,
    })?;
  Ok(status.success())
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
