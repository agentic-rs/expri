//! Paths for new machine-scoped data. Resolving paths never creates files.

use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};

use crate::error::{ExpriError, Result};

#[derive(Clone, Debug)]
pub struct StoragePaths {
  root: PathBuf,
  boundary: PathBuf,
}

impl StoragePaths {
  pub fn resolve(explicit: Option<&Path>) -> Result<Self> {
    let override_path = std::env::var_os("EXPRI_STORAGE_DIR").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Self::resolve_with(
      explicit,
      override_path.as_deref(),
      home.as_deref(),
      &std::env::current_dir()?,
    )
  }

  /// Pure resolution for callers with an explicit environment and for tests.
  pub fn resolve_with(
    explicit: Option<&Path>,
    override_path: Option<&Path>,
    home: Option<&Path>,
    cwd: &Path,
  ) -> Result<Self> {
    if let Some(path) = explicit.or(override_path) {
      return Self::from_root(absolute(path, cwd)?);
    }
    let home = home
      .filter(|path| !path.as_os_str().is_empty())
      .ok_or_else(|| invalid("HOME is unavailable; set EXPRI_STORAGE_DIR or pass --storage-dir"))?;
    let home = absolute(home, cwd)?;
    Ok(Self {
      root: home.join(".tokn/expri"),
      boundary: home.join(".tokn"),
    })
  }

  pub fn from_root(root: PathBuf) -> Result<Self> {
    if !root.is_absolute()
      || root
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
      return Err(invalid(
        "system storage root must be an absolute path without '..'",
      ));
    }
    Ok(Self {
      boundary: root.clone(),
      root,
    })
  }

  pub fn root(&self) -> &Path {
    &self.root
  }
  pub fn config_file(&self) -> PathBuf {
    self.root.join("config.toml")
  }
  pub fn profile_database(&self) -> PathBuf {
    self.root.join("profiles.sqlite3")
  }

  /// Inspect owned boundaries without creating directories or changing modes.
  /// Aliases above the boundary (such as a linked HOME) remain supported.
  pub fn validate_existing(&self) -> Result<()> {
    let suffix = self
      .root
      .strip_prefix(&self.boundary)
      .map_err(|_| invalid("invalid storage boundary"))?;
    let mut path = self.boundary.clone();
    if !existing_owned_directory(&path)? {
      return Ok(());
    }
    for component in suffix.components() {
      if let Component::Normal(name) = component {
        path.push(name);
      }
      if !existing_owned_directory(&path)? {
        return Ok(());
      }
    }
    Ok(())
  }

  /// Resolve aliases above the owned boundary, then reject aliases below it.
  /// Existing directories outside expri's root are never chmodded.
  pub fn ensure_private(&self) -> Result<()> {
    self.validate_existing()?;
    let parent = self
      .boundary
      .parent()
      .ok_or_else(|| invalid("system storage root needs a parent"))?;
    let parent = ensure_external_parent(parent)?;
    let boundary = parent.join(
      self
        .boundary
        .file_name()
        .ok_or_else(|| invalid("invalid system storage root"))?,
    );
    create_owned_directory(&boundary)?;
    let suffix = self
      .root
      .strip_prefix(&self.boundary)
      .map_err(|_| invalid("invalid storage boundary"))?;
    let mut path = boundary;
    for component in suffix.components() {
      if let Component::Normal(name) = component {
        path.push(name);
        create_owned_directory(&path)?;
      }
    }
    private_directory(&path)
  }
}

fn absolute(path: &Path, cwd: &Path) -> Result<PathBuf> {
  if path.as_os_str().is_empty()
    || path
      .components()
      .any(|part| matches!(part, Component::ParentDir))
  {
    return Err(invalid(
      "system storage path must be nonempty and contain no '..'",
    ));
  }
  Ok(if path.is_absolute() {
    path.to_path_buf()
  } else {
    cwd.join(path)
  })
}

fn existing_owned_directory(path: &Path) -> Result<bool> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
    Ok(_) => Err(invalid(
      "system storage directory must not be a symlink or file",
    )),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(error.into()),
  }
}

fn ensure_external_parent(path: &Path) -> Result<PathBuf> {
  if path.exists() {
    return Ok(fs::canonicalize(path)?);
  }
  let parent = path
    .parent()
    .ok_or_else(|| invalid("storage parent is unavailable"))?;
  let parent = ensure_external_parent(parent)?;
  let path = parent.join(
    path
      .file_name()
      .ok_or_else(|| invalid("invalid storage parent"))?,
  );
  create_owned_directory(&path)?;
  Ok(path)
}

fn create_owned_directory(path: &Path) -> Result<()> {
  let mut builder = fs::DirBuilder::new();
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
  }
  match builder.create(path) {
    Ok(()) => {
      #[cfg(unix)]
      if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
      }
      Ok(())
    }
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
      let metadata = fs::symlink_metadata(path)?;
      if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
      } else {
        Err(invalid(
          "system storage directory must not be a symlink or file",
        ))
      }
    }
    Err(error) => Err(error.into()),
  }
}

fn private_directory(path: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(invalid("system storage directory must be a real directory"));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
  }
  Ok(())
}

/// Create a managed regular file privately, rejecting an existing symlink.
/// Canonical aliases in the trusted external parent are supported.
pub fn ensure_private_file(path: &Path) -> Result<File> {
  let parent = path
    .parent()
    .ok_or_else(|| invalid("managed file needs a parent"))?;
  let parent = fs::canonicalize(parent)?;
  let path = parent.join(
    path
      .file_name()
      .ok_or_else(|| invalid("managed file needs a filename"))?,
  );
  let mut options = OpenOptions::new();
  options.read(true).write(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  let file = match options.create_new(true).open(&path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
      let before = fs::symlink_metadata(&path)?;
      if !before.is_file() || before.file_type().is_symlink() {
        return Err(invalid(
          "managed system file must be a regular file without symlinks",
        ));
      }
      let file = OpenOptions::new().read(true).write(true).open(&path)?;
      let after = fs::symlink_metadata(&path)?;
      if !after.is_file() || after.file_type().is_symlink() {
        return Err(invalid("managed system file changed while opening"));
      }
      #[cfg(unix)]
      {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        if before.dev() != opened.dev()
          || before.ino() != opened.ino()
          || after.dev() != opened.dev()
          || after.ino() != opened.ino()
        {
          return Err(invalid("managed system file changed while opening"));
        }
      }
      file
    }
    Err(error) => return Err(error.into()),
  };
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
  }
  file.sync_all()?;
  #[cfg(unix)]
  File::open(parent)?.sync_all()?;
  Ok(file)
}

fn invalid(message: &str) -> ExpriError {
  ExpriError::Message(message.into())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resolution_is_pure_and_respects_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let storage = StoragePaths::resolve_with(None, None, Some(&root), &root).unwrap();
    assert_eq!(storage.root(), root.join(".tokn/expri"));
    storage.validate_existing().unwrap();
    assert!(!root.join(".tokn").exists());
    let explicit = StoragePaths::resolve_with(
      Some(Path::new("explicit")),
      Some(Path::new("env")),
      None,
      &root,
    )
    .unwrap();
    assert_eq!(explicit.root(), root.join("explicit"));
    assert!(StoragePaths::resolve_with(None, None, None, &root).is_err());
    assert!(StoragePaths::resolve_with(Some(Path::new("")), None, Some(&root), &root).is_err());
    assert!(StoragePaths::from_root(root.join("../bad")).is_err());
  }

  #[test]
  fn writes_create_private_owned_data_only() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let storage = StoragePaths::resolve_with(None, None, Some(&root), &root).unwrap();
    storage.ensure_private().unwrap();
    let file = ensure_private_file(&storage.config_file()).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(
        fs::metadata(storage.root()).unwrap().permissions().mode() & 0o777,
        0o700
      );
      assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert!(!root.join(".expri").exists());
  }

  #[cfg(unix)]
  #[test]
  fn external_aliases_work_owned_symlinks_do_not() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let actual = root.join("actual");
    fs::create_dir(&actual).unwrap();
    let alias = root.join("alias");
    symlink(&actual, &alias).unwrap();
    let storage = StoragePaths::resolve_with(None, None, Some(&alias), &root).unwrap();
    storage.ensure_private().unwrap();
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, storage.config_file()).unwrap();
    assert!(ensure_private_file(&storage.config_file()).is_err());
    let other_home = root.join("other-home");
    fs::create_dir(&other_home).unwrap();
    symlink(&outside, other_home.join(".tokn")).unwrap();
    assert!(
      StoragePaths::resolve_with(None, None, Some(&other_home), &root)
        .unwrap()
        .validate_existing()
        .is_err()
    );
    assert!(
      StoragePaths::resolve_with(None, None, Some(&other_home), &root)
        .unwrap()
        .ensure_private()
        .is_err()
    );
    assert!(!outside.join("expri").exists());
  }
}
