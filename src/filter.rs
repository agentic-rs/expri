use std::path::{Component, Path};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::error::Result;

pub const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
  ".git",
  ".venv",
  "__pycache__",
  ".pytest_cache",
  ".mypy_cache",
  ".ruff_cache",
  ".pyre",
  ".hypothesis",
  "out",
  "target",
  "node_modules",
];

pub const DEFAULT_EXCLUDED_FILES: &[&str] =
  &[".env", "*.pyc", "*.pyo", "*.log", "*.tmp", ".DS_Store"];

#[derive(Debug)]
pub struct SyncRules {
  exclude_dirs: Vec<String>,
  exclude_files: GlobSet,
  include_ignored: Vec<String>,
  remote_managed: Vec<String>,
}

impl SyncRules {
  pub fn defaults() -> Result<Self> {
    Self::new(
      DEFAULT_EXCLUDED_DIRS
        .iter()
        .map(ToString::to_string)
        .collect(),
      DEFAULT_EXCLUDED_FILES
        .iter()
        .map(ToString::to_string)
        .collect(),
      Vec::new(),
      Vec::new(),
    )
  }

  pub fn new(
    exclude_dirs: Vec<String>,
    exclude_files: Vec<String>,
    include_ignored: Vec<String>,
    remote_managed: Vec<String>,
  ) -> Result<Self> {
    for path in &remote_managed {
      validate_remote_managed_path(Path::new(path))?;
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in exclude_files {
      builder.add(Glob::new(&pattern)?);
    }
    Ok(Self {
      exclude_dirs,
      exclude_files: builder.build()?,
      include_ignored,
      remote_managed,
    })
  }

  pub fn include_ignored(&self) -> &[String] {
    &self.include_ignored
  }

  pub fn remote_managed(&self) -> &[String] {
    &self.remote_managed
  }

  pub fn should_include(&self, relative_path: &Path) -> bool {
    if !relative_path.is_relative() {
      return false;
    }
    if self
      .remote_managed
      .iter()
      .any(|managed| relative_path == Path::new(managed))
    {
      return false;
    }
    if relative_path.components().any(|component| match component {
      Component::Normal(value) => self
        .exclude_dirs
        .iter()
        .any(|excluded| value == excluded.as_str()),
      _ => false,
    }) {
      return false;
    }
    match relative_path.file_name() {
      Some(name) => !self.exclude_files.is_match(Path::new(name)),
      None => true,
    }
  }
}

fn validate_remote_managed_path(path: &Path) -> Result<()> {
  if !path.is_relative()
    || path
      .components()
      .any(|component| !matches!(component, Component::Normal(_)))
  {
    return Err(crate::error::ExpriError::Message(format!(
      "sync remote_managed path must be relative and stay inside the repo: {}",
      path.display()
    )));
  }
  Ok(())
}
