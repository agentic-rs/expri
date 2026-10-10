//! Provision immutable project inputs before environment preparation and training.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::InputDownloadOptions;
use super::client::fs;
use crate::config::RunServiceConfig;
use crate::error::Result;

pub(crate) fn prepare(
  repo: &Path,
  run_dir: &Path,
  service: &RunServiceConfig,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<Option<(PathBuf, Vec<Value>)>> {
  service.validate()?;
  if service.inputs.is_empty() {
    return Ok(None);
  }
  fs::directory(run_dir)?;
  let cache = repo.join(".expri/inputs").join(&service.project_id);
  let directory = run_dir.join("inputs");
  fs::directories(&directory)?;
  let mut inputs = Vec::new();
  for input in &service.inputs {
    if cancelled()? {
      return Err(fs::message("Run cancelled while preparing private inputs"));
    }
    let binding = directory.join(&input.destination);
    let parent = binding.parent().expect("input binding has a parent");
    fs::directories(parent)?;
    fs::optional_regular(&binding)?;
    if fs::inspect(&binding)?.is_some() {
      return Err(fs::message(
        "run private input binding already exists; use a new run directory",
      ));
    }
    let report = super::client::input_download_prepared(
      InputDownloadOptions {
        config: service.client_config.clone(),
        project_id: service.project_id.clone(),
        input_id: input.input_id.clone(),
        destination: cache.join(&input.input_id).join("file"),
      },
      cancelled,
      &mut |source, report, cancelled| pin_verified(source, &binding, report, cancelled),
    )?;
    // Persist reproduction facts, never credentials or signed URLs.
    inputs.push(
      json!({"input_id":input.input_id, "destination":input.destination,
      "size":report["size"], "sha256":report["sha256"], "reused":report["reused"],
      "offline":report["offline"]}),
    );
  }
  Ok(Some((directory, inputs)))
}

/// Called while the shared input transfer lease still owns the verified cache.
fn pin_verified(
  source: &Path,
  binding: &Path,
  report: &Value,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  check_cancelled(cancelled)?;
  let mut file = fs::open(source)?;
  let initial = file.metadata()?;
  let size = report["size"]
    .as_u64()
    .ok_or_else(|| fs::message("private input receipt has no size"))?;
  let digest = report["sha256"]
    .as_str()
    .filter(|digest| {
      digest.len() == 64
        && digest
          .bytes()
          .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
    .ok_or_else(|| fs::message("private input receipt has no valid SHA256 digest"))?;
  if initial.len() != size {
    return Err(fs::message("verified private input changed before binding"));
  }
  fs::optional_regular(binding)?;
  if fs::inspect(binding)?.is_some() {
    return Err(fs::message(
      "run private input binding already exists; use a new run directory",
    ));
  }
  protect_read_only(&file)?;
  let protected = file.metadata()?;
  if !fs::unchanged(&initial, &protected)
    || !fs::unchanged(&protected, &fs::open(source)?.metadata()?)
  {
    return Err(fs::message("verified private input changed before binding"));
  }
  // Changing permissions updates ctime; subsequent checks use the protected inode.
  let initial = protected;
  check_cancelled(cancelled)?;
  match std::fs::hard_link(source, binding) {
    Ok(()) => {
      let bound = fs::open(binding)?;
      if !fs::unchanged(&initial, &bound.metadata()?)
        || !fs::unchanged(&initial, &file.metadata()?)
        || !fs::unchanged(&initial, &fs::open(source)?.metadata()?)
      {
        return Err(fs::message("verified private input changed while binding"));
      }
    }
    Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
      copy_verified(&mut file, binding, size, digest, cancelled)?;
      if !fs::unchanged(&initial, &file.metadata()?)
        || !fs::unchanged(&initial, &fs::open(source)?.metadata()?)
      {
        return Err(fs::message("verified private input changed while binding"));
      }
    }
    Err(error) => return Err(error.into()),
  }
  fs::sync_directory(binding.parent().expect("binding has a parent"))
}

fn check_cancelled(cancelled: &mut dyn FnMut() -> Result<bool>) -> Result<()> {
  if cancelled()? {
    return Err(fs::message("Run cancelled while binding private inputs"));
  }
  Ok(())
}

fn protect_read_only(file: &File) -> Result<()> {
  let mut permissions = file.metadata()?.permissions();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    if permissions.mode() & 0o7777 == 0o400 {
      return Ok(());
    }
    permissions.set_mode(0o400);
  }
  #[cfg(not(unix))]
  {
    if permissions.readonly() {
      return Ok(());
    }
    permissions.set_readonly(true);
  }
  // Update the verified descriptor rather than reopening a replaceable cache path.
  file.set_permissions(permissions)?;
  file.sync_all()?;
  Ok(())
}

/// Cross-filesystem fallback verifies the existing digest during the required copy.
fn copy_verified(
  source: &mut File,
  binding: &Path,
  size: u64,
  expected: &str,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  check_cancelled(cancelled)?;
  let parent = binding.parent().expect("input binding has a parent");
  fs::directory(parent)?;
  let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
  let mut remaining = size;
  let mut digest = Sha256::new();
  let mut buffer = [0u8; 64 * 1024];
  while remaining > 0 {
    check_cancelled(cancelled)?;
    let length = remaining.min(buffer.len() as u64) as usize;
    source.read_exact(&mut buffer[..length])?;
    temporary.write_all(&buffer[..length])?;
    digest.update(&buffer[..length]);
    remaining -= length as u64;
  }
  let actual = digest
    .finalize()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect::<String>();
  if actual != expected || source.read(&mut [0u8; 1])? != 0 {
    return Err(fs::message(
      "private input copy did not match its verified SHA256 digest",
    ));
  }
  check_cancelled(cancelled)?;
  protect_read_only(temporary.as_file())?;
  temporary.as_file().sync_all()?;
  fs::optional_regular(binding)?;
  temporary
    .persist_noclobber(binding)
    .map_err(|error| error.error)?;
  fs::sync_directory(parent)
}

#[cfg(test)]
mod tests;
