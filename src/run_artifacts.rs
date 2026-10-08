//! Bounded output discovery shared by publishing and local review.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};

pub(crate) const FILE_LIMIT: usize = 200;
pub(crate) const INVENTORY_LIMIT: usize = 64 * 1024;
pub(crate) const INVENTORY_PATH: &str = "outputs/.expri-artifacts.json";
const ENTRY_LIMIT: usize = 2000;
const DEPTH_LIMIT: usize = 16;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Artifact {
  pub path: String,
  pub size: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Inventory {
  pub files: Vec<Artifact>,
  pub truncated: bool,
}

pub(crate) fn validate_path(path: &str) -> Result<()> {
  if !path.starts_with("outputs/")
    || path.len() > 1024
    || path.contains('\\')
    || path.chars().any(char::is_control)
    || path.split('/').any(|part| {
      part.is_empty()
        || part.starts_with('.')
        || matches!(part, "cache" | "__pycache__" | "node_modules")
    })
  {
    return Err(ExpriError::Message("invalid output artifact path".into()));
  }
  Ok(())
}

pub(crate) fn scan(run_dir: &Path) -> Result<Inventory> {
  let metadata = fs::symlink_metadata(run_dir)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(ExpriError::Message(
      "artifact run directory must be a real directory".into(),
    ));
  }
  let mut files = BTreeMap::new();
  let mut pending = vec![(run_dir.join("outputs"), 0)];
  let mut entries = 0;
  let mut truncated = false;
  let mut encoded_bytes = 0;
  while let Some((directory, depth)) = pending.pop() {
    if depth > DEPTH_LIMIT {
      truncated = true;
      continue;
    }
    // Check each prefix again so a linked output directory is never traversed.
    let relative = directory.strip_prefix(run_dir).expect("output path");
    let mut prefix = run_dir.to_path_buf();
    let mut readable = true;
    for component in relative.components() {
      prefix.push(component);
      match fs::symlink_metadata(&prefix) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
          readable = false;
          break;
        }
        _ => {
          truncated = true;
          readable = false;
          break;
        }
      }
    }
    if !readable {
      continue;
    }
    let listing = match fs::read_dir(&directory) {
      Ok(listing) => listing,
      Err(_) => {
        truncated = true;
        continue;
      }
    };
    for entry in listing {
      entries += 1;
      if entries > ENTRY_LIMIT {
        truncated = true;
        pending.clear();
        break;
      }
      let Ok(entry) = entry else {
        truncated = true;
        continue;
      };
      let path = entry.path();
      let Some(relative) = path.strip_prefix(run_dir).ok().and_then(Path::to_str) else {
        truncated = true;
        continue;
      };
      if validate_path(relative).is_err() {
        continue;
      }
      let Ok(metadata) = fs::symlink_metadata(&path) else {
        truncated = true;
        continue;
      };
      if metadata.file_type().is_symlink() {
        continue;
      }
      if metadata.is_dir() {
        pending.push((path, depth + 1));
      } else if metadata.is_file() {
        let record = Artifact {
          path: relative.into(),
          size: metadata.len(),
        };
        // Leave space for indentation in the atomic pretty-printed manifest.
        let length = serde_json::to_vec_pretty(&record)?.len() + 24;
        if files.len() == FILE_LIMIT || encoded_bytes + length > INVENTORY_LIMIT - 1024 {
          truncated = true;
          continue;
        }
        encoded_bytes += length;
        files.insert(record.path.clone(), record);
      }
    }
    if entries > ENTRY_LIMIT {
      break;
    }
  }
  Ok(Inventory {
    files: files.into_values().collect(),
    truncated,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lists_regular_outputs_without_reading_checkpoint_bytes() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("outputs/nested")).unwrap();
    let checkpoint = fs::File::create(root.path().join("outputs/model one.pt")).unwrap();
    checkpoint.set_len(16 * 1024 * 1024 * 1024).unwrap();
    fs::write(root.path().join("outputs/nested/summary.json"), "{}").unwrap();
    fs::write(root.path().join("outputs/.env"), "private").unwrap();
    fs::write(root.path().join("train.py"), "private code").unwrap();
    let inventory = scan(root.path()).unwrap();
    assert_eq!(inventory.files.len(), 2);
    assert_eq!(
      inventory.files[0],
      Artifact {
        path: "outputs/model one.pt".into(),
        size: 16 * 1024 * 1024 * 1024
      }
    );
    assert!(!inventory.truncated);
  }

  #[test]
  fn bounds_large_directory_and_serialized_inventory() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("outputs")).unwrap();
    for index in 0..FILE_LIMIT + 10 {
      fs::write(
        root
          .path()
          .join(format!("outputs/checkpoint-{index:03}.pt")),
        [],
      )
      .unwrap();
    }
    let inventory = scan(root.path()).unwrap();
    assert_eq!(inventory.files.len(), FILE_LIMIT);
    assert!(inventory.truncated);
    assert!(serde_json::to_vec(&inventory).unwrap().len() < INVENTORY_LIMIT);
  }

  #[test]
  fn validates_scope_and_preserves_ordinary_shell_metacharacters() {
    assert!(validate_path("outputs/model's $best.pt").is_ok());
    for path in [
      "outputs/../secret",
      "code/model.pt",
      "outputs/.env",
      "outputs/cache/.venv/model.pt",
      "outputs/a\\b",
      "outputs/a\nb",
    ] {
      assert!(validate_path(path).is_err(), "{path}");
    }
  }

  #[cfg(unix)]
  #[test]
  fn never_lists_linked_files_directories_or_special_files() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("outputs")).unwrap();
    fs::write(outside.path().join("secret.pt"), "private").unwrap();
    symlink(outside.path(), root.path().join("outputs/linked")).unwrap();
    symlink(
      outside.path().join("secret.pt"),
      root.path().join("outputs/linked.pt"),
    )
    .unwrap();
    let socket =
      std::os::unix::net::UnixListener::bind(root.path().join("outputs/socket")).unwrap();
    assert!(scan(root.path()).unwrap().files.is_empty());
    drop(socket);
  }
}
