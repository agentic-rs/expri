//! Source adapters prepare one verified, immutable file in the repository cache.
//! Redirect and signed URLs exist only in memory, never in descriptors or receipts.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
  Descriptor, Source, cache_file, check_cancelled, directories, inspect, message, open,
  optional_regular, sync_directory, unchanged,
};
use crate::error::{ExpriError, Result};
use crate::lock::{FileLock, LockAttempt};

mod http;
#[cfg(test)]
mod tests;

/// Resolve a moving source for import/update, downloading and hashing its bytes.
pub fn resolve(
  repo: &Path,
  source: &Source,
  client_config: Option<&Path>,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<Descriptor> {
  source.validate()?;
  check_cancelled(cancelled)?;
  match source {
    Source::Expri { .. } => private_download(repo, source, None, client_config, cancelled),
    _ => http::resolve(repo, source, cancelled),
  }
}

/// Reproduce the recorded version. A verified cache is usable without networking.
pub fn ensure(
  repo: &Path,
  descriptor: &Descriptor,
  client_config: Option<&Path>,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<PathBuf> {
  descriptor.validate()?;
  check_cancelled(cancelled)?;
  let destination = cache_file(repo, &descriptor.sha256);
  if verified(&destination, descriptor.size, &descriptor.sha256, cancelled)? {
    return Ok(destination);
  }
  match &descriptor.source {
    Source::Expri { .. } => {
      private_download(
        repo,
        &descriptor.source,
        Some(descriptor),
        client_config,
        cancelled,
      )?;
    }
    _ => {
      http::ensure(repo, descriptor, cancelled)?;
    }
  }
  Ok(destination)
}

#[derive(Deserialize)]
struct ClientEndpoint {
  url: String,
}

fn matching_client(source: &Source, client_config: Option<&Path>) -> Result<PathBuf> {
  let Source::Expri { url, .. } = source else {
    return Err(message("asset source is not an expri input"));
  };
  let path = client_config.ok_or_else(|| {
    message("private assets require --client-config or the worker's service client config")
  })?;
  // A user-selected read-only configuration can live behind a system alias
  // such as macOS /tmp. Managed transfer/cache paths remain symlink-free.
  let path = std::fs::canonicalize(path)?;
  let mut bytes = Vec::new();
  open(&path)?.take(32 * 1024 + 1).read_to_end(&mut bytes)?;
  if bytes.len() > 32 * 1024 {
    return Err(message("service client config exceeds its size limit"));
  }
  let text =
    std::str::from_utf8(&bytes).map_err(|_| message("service client config must be UTF-8"))?;
  // Do not include parser errors: configuration may contain credential material.
  let config: ClientEndpoint =
    toml::from_str(text).map_err(|_| message("cannot read service client endpoint"))?;
  let actual =
    reqwest::Url::parse(&config.url).map_err(|_| message("invalid service client endpoint"))?;
  let expected = reqwest::Url::parse(url).map_err(|_| message("invalid expri asset endpoint"))?;
  if actual != expected
    || !actual.username().is_empty()
    || actual.password().is_some()
    || actual.query().is_some()
    || actual.fragment().is_some()
  {
    return Err(message(
      "service client endpoint does not match the private asset server",
    ));
  }
  Ok(path)
}

fn private_download(
  repo: &Path,
  source: &Source,
  expected: Option<&Descriptor>,
  client_config: Option<&Path>,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<Descriptor> {
  let config = matching_client(source, client_config)?;
  let Source::Expri {
    project_id,
    input_id,
    ..
  } = source
  else {
    return Err(message("asset source is not an expri input"));
  };
  let staging = transfer_directory(repo, source, expected)?;
  directories(&staging)?;
  let _lease = lease(&staging.join(".asset.lock"), cancelled)?;
  if let Some(expected) = expected {
    let cached = cache_file(repo, &expected.sha256);
    if verified(&cached, expected.size, &expected.sha256, cancelled)? {
      return Ok(expected.clone());
    }
  }
  let mut result = None;
  crate::service::input_download_prepared(
    crate::service::InputDownloadOptions {
      config,
      project_id: project_id.clone(),
      input_id: input_id.clone(),
      destination: staging.join("file"),
    },
    cancelled,
    &mut |path, report, cancelled| {
      let descriptor = Descriptor {
        version: 1,
        source: source.clone(),
        size: report["size"]
          .as_u64()
          .ok_or_else(|| message("private asset record has no size"))?,
        sha256: report["sha256"]
          .as_str()
          .ok_or_else(|| message("private asset record has no SHA256 digest"))?
          .to_string(),
      };
      descriptor.validate()?;
      if let Some(expected) = expected
        && (descriptor.size != expected.size || descriptor.sha256 != expected.sha256)
      {
        return Err(message(
          "private asset changed; use assets update to select its new version",
        ));
      }
      publish(repo, path, &descriptor, cancelled)?;
      result = Some(descriptor);
      Ok(())
    },
  )
  .map_err(|error| match error {
    ExpriError::ServiceRejected { status, .. } => {
      message(format!("private asset server returned HTTP {status}"))
    }
    ExpriError::ServiceUnavailable { .. } => {
      message("private asset service is unavailable; retry to resume saved progress")
    }
    ExpriError::Json(_) => message("private asset response or transfer receipt is invalid"),
    ExpriError::Toml(_) => message("private asset service client configuration is invalid"),
    error => error,
  })?;
  // The verified content cache owns the bytes after preparation; retain only
  // small transfer receipts, rather than a second full private-input copy.
  let temporary = staging.join("file");
  optional_regular(&temporary)?;
  if inspect(&temporary)?.is_some() {
    std::fs::remove_file(temporary)?;
    sync_directory(&staging)?;
  }
  result.ok_or_else(|| message("private asset download did not prepare a file"))
}

/// Internal staging identity only; the descriptor has a single content SHA256.
fn transfer_directory(
  repo: &Path,
  source: &Source,
  expected: Option<&Descriptor>,
) -> Result<PathBuf> {
  let identity = serde_json::to_vec(&(source, expected.map(|value| (&value.sha256, value.size))))?;
  let key = hex(&Sha256::digest(identity));
  Ok(repo.join(".expri/assets/.transfers").join(key))
}

fn lease(path: &Path, cancelled: &mut dyn FnMut() -> Result<bool>) -> Result<FileLock> {
  loop {
    check_cancelled(cancelled)?;
    match crate::lock::try_lock_file(path, true)? {
      LockAttempt::Acquired(lease) => return Ok(lease),
      LockAttempt::Busy => thread::sleep(Duration::from_millis(200)),
      LockAttempt::Missing => return Err(message("asset transfer lease disappeared")),
    }
  }
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest(path: &Path, cancelled: &mut dyn FnMut() -> Result<bool>) -> Result<(u64, String)> {
  check_cancelled(cancelled)?;
  let mut file = open(path)?;
  let initial = file.metadata()?;
  let mut count = 0u64;
  let mut hash = Sha256::new();
  let mut buffer = [0u8; 64 * 1024];
  loop {
    check_cancelled(cancelled)?;
    let length = file.read(&mut buffer)?;
    if length == 0 {
      break;
    }
    hash.update(&buffer[..length]);
    count = count
      .checked_add(length as u64)
      .ok_or_else(|| message("asset size exceeds its supported range"))?;
  }
  if count != initial.len()
    || !unchanged(&initial, &file.metadata()?)
    || !unchanged(&initial, &open(path)?.metadata()?)
  {
    return Err(message("asset changed while verifying its bytes"));
  }
  Ok((count, hex(&hash.finalize())))
}

fn verified(
  path: &Path,
  size: u64,
  sha256: &str,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<bool> {
  optional_regular(path)?;
  if inspect(path)?.is_none() {
    return Ok(false);
  }
  let (actual_size, actual_digest) = digest(path, cancelled)?;
  if actual_size != size || actual_digest != sha256 {
    return Err(message(
      "cached asset is corrupt; remove its cache file before downloading again",
    ));
  }
  protect(&open(path)?)?;
  Ok(true)
}

fn protect(file: &File) -> Result<()> {
  let mut permissions = file.metadata()?.permissions();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o400);
  }
  #[cfg(not(unix))]
  permissions.set_readonly(true);
  file.set_permissions(permissions)?;
  file.sync_all()?;
  Ok(())
}

/// The cache never replaces existing files, including files hard-linked into runs.
fn publish(
  repo: &Path,
  source: &Path,
  descriptor: &Descriptor,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<PathBuf> {
  let destination = cache_file(repo, &descriptor.sha256);
  let directory = destination.parent().expect("asset cache has a parent");
  directories(directory)?;
  let _lease = lease(&directory.join(".asset.lock"), cancelled)?;
  if verified(&destination, descriptor.size, &descriptor.sha256, cancelled)? {
    return Ok(destination);
  }
  let mut input = open(source)?;
  let initial = input.metadata()?;
  if initial.len() != descriptor.size {
    return Err(message(
      "downloaded asset size does not match its descriptor",
    ));
  }
  input.seek(SeekFrom::Start(0))?;
  let mut output = tempfile::NamedTempFile::new_in(directory)?;
  let mut hash = Sha256::new();
  let mut remaining = descriptor.size;
  let mut buffer = [0u8; 64 * 1024];
  while remaining > 0 {
    check_cancelled(cancelled)?;
    let length = remaining.min(buffer.len() as u64) as usize;
    input.read_exact(&mut buffer[..length])?;
    output.write_all(&buffer[..length])?;
    hash.update(&buffer[..length]);
    remaining -= length as u64;
  }
  if input.read(&mut [0u8; 1])? != 0
    || hex(&hash.finalize()) != descriptor.sha256
    || !unchanged(&initial, &input.metadata()?)
    || !unchanged(&initial, &open(source)?.metadata()?)
  {
    return Err(message(
      "downloaded asset does not match its SHA256 descriptor",
    ));
  }
  check_cancelled(cancelled)?;
  protect(output.as_file())?;
  optional_regular(&destination)?;
  output
    .persist_noclobber(&destination)
    .map_err(|error| error.error)?;
  sync_directory(directory)?;
  Ok(destination)
}
