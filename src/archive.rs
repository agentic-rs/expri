use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

use crate::error::{ExpriError, Result};
use crate::git::DirtyPaths;

#[derive(Debug)]
pub struct PatchArchive {
  pub _temp_dir: TempDir,
  pub path: PathBuf,
  pub digest: String,
  pub size: u64,
  pub file_count: usize,
  pub deleted_count: usize,
}

pub fn build_patch_archive(repo_root: &Path, dirty: &DirtyPaths) -> Result<PatchArchive> {
  let temp_dir = tempfile::Builder::new().prefix("expri-patch-").tempdir()?;
  let path = temp_dir.path().join("patch.zip");
  let file = File::create(&path).map_err(|source| ExpriError::IoContext {
    action: "create",
    path: path.display().to_string(),
    source,
  })?;
  let mut archive = zip::ZipWriter::new(file);
  let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

  let mut deleted = String::new();
  for path in &dirty.deleted {
    deleted.push_str(&path.to_string_lossy());
    deleted.push('\n');
  }
  archive.start_file(".deleted", options)?;
  archive.write_all(deleted.as_bytes())?;

  for relative_path in &dirty.files {
    archive.start_file(relative_path.to_string_lossy(), options)?;
    let absolute_path = repo_root.join(relative_path);
    let mut file = File::open(&absolute_path).map_err(|source| ExpriError::IoContext {
      action: "open",
      path: absolute_path.display().to_string(),
      source,
    })?;
    io::copy(&mut file, &mut archive)?;
  }
  archive.finish()?;

  let (digest, size) = sha256_file(&path)?;
  Ok(PatchArchive {
    _temp_dir: temp_dir,
    path,
    digest,
    size,
    file_count: dirty.files.len(),
    deleted_count: dirty.deleted.len(),
  })
}

pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
  let mut file = File::open(path).map_err(|source| ExpriError::IoContext {
    action: "open",
    path: path.display().to_string(),
    source,
  })?;
  let mut hasher = Sha256::new();
  let mut size = 0;
  let mut buffer = [0; 64 * 1024];
  loop {
    let read = file.read(&mut buffer)?;
    if read == 0 {
      break;
    }
    size += read as u64;
    hasher.update(&buffer[..read]);
  }
  Ok((format!("{:x}", hasher.finalize()), size))
}
