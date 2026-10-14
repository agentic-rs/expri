//! Bounded, verified source bundles for queued jobs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use tempfile::TempDir;
use zip::write::SimpleFileOptions;

use crate::error::{ExpriError, Result};

pub const MAX_SOURCE_BYTES: u64 = expri_core::resource_queue::MAX_QUEUE_SOURCE_BYTES;
const MAX_SOURCE_ENTRIES: usize = 100_000;

pub struct SourceBundle {
  pub path: PathBuf,
  pub size: u64,
  pub sha256: String,
  _temporary: TempDir,
}

pub fn freeze_source(repo_root: &Path, remote_managed: &[String]) -> Result<SourceBundle> {
  let temporary = tempfile::Builder::new()
    .prefix("expri-queue-source-")
    .tempdir()?;
  let bundle_root = fs::canonicalize(temporary.path())?;
  let source = bundle_root.join("source");
  fs::create_dir(&source)?;
  crate::environment::snapshot::freeze_source_at(
    repo_root,
    remote_managed,
    &source,
    MAX_SOURCE_BYTES,
  )?;
  let path = bundle_root.join("source.zip");
  let mut archive = zip::ZipWriter::new(File::create(&path)?);
  let mut paths = Vec::new();
  source_paths(&source, &source, &mut paths)?;
  paths.sort();
  if paths.len() > MAX_SOURCE_ENTRIES {
    return Err(message("queued source has too many files"));
  }
  let mut links = BTreeMap::new();
  for relative in &paths {
    let absolute = source.join(relative);
    if fs::symlink_metadata(&absolute)?.file_type().is_symlink() {
      links.insert(relative.clone(), fs::read_link(absolute)?);
    }
  }
  validate_link_graph(&links)?;
  let mut total = 0_u64;
  for relative in paths {
    let name = archive_name(&relative)?;
    let absolute = source.join(&relative);
    let metadata = fs::symlink_metadata(&absolute)?;
    let options = SimpleFileOptions::default()
      .compression_method(zip::CompressionMethod::Deflated)
      .last_modified_time(zip::DateTime::default())
      .unix_permissions(safe_mode(&metadata));
    if metadata.file_type().is_symlink() {
      let target = fs::read_link(&absolute)?;
      validate_symlink(&relative, &target)?;
      let target = target
        .to_str()
        .ok_or_else(|| message("queued source symlink must be UTF-8"))?;
      total = bounded_total(total, target.len() as u64)?;
      archive
        .add_symlink(name, target, options)
        .map_err(zip_error)?;
    } else {
      total = bounded_total(total, metadata.len())?;
      archive.start_file(name, options).map_err(zip_error)?;
      let copied = io::copy(
        &mut File::open(&absolute)?.take(MAX_SOURCE_BYTES + 1),
        &mut archive,
      )?;
      if copied != metadata.len() {
        return Err(message("frozen source changed while archiving"));
      }
    }
  }
  archive.finish().map_err(zip_error)?.sync_all()?;
  let (sha256, size) = crate::archive::sha256_file(&path)?;
  if size > MAX_SOURCE_BYTES {
    return Err(message("queued source ZIP exceeds the 512 MiB limit"));
  }
  Ok(SourceBundle {
    path,
    size,
    sha256,
    _temporary: temporary,
  })
}

/// Extract only into a new empty directory. Symlinks are leaves and stay inside it.
pub fn unpack_source(archive: &Path, destination: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(archive)?;
  if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_SOURCE_BYTES {
    return Err(message(
      "queued source must be a regular ZIP at most 512 MiB",
    ));
  }
  if fs::symlink_metadata(destination).is_ok() {
    return Err(message("queued source destination must not already exist"));
  }
  let parent = destination
    .parent()
    .ok_or_else(|| message("source destination needs a parent"))?;
  let temporary = tempfile::Builder::new()
    .prefix("source-unpack-")
    .tempdir_in(parent)?;
  let mut archive = zip::ZipArchive::new(File::open(archive)?).map_err(zip_error)?;
  if archive.len() > MAX_SOURCE_ENTRIES {
    return Err(message("queued source has too many entries"));
  }
  let mut seen = BTreeSet::new();
  let mut files = BTreeSet::new();
  let mut symlinks = Vec::new();
  let mut total = 0_u64;
  for index in 0..archive.len() {
    let mut entry = archive.by_index(index).map_err(zip_error)?;
    let relative = safe_entry(entry.name())?;
    if !seen.insert(relative.clone()) {
      return Err(message("queued source ZIP contains duplicate paths"));
    }
    total = bounded_total(total, entry.size())?;
    let output = temporary.path().join(&relative);
    let mode = entry.unix_mode().unwrap_or(0o100644);
    let kind = mode & 0o170000;
    if !matches!(kind, 0 | 0o100000 | 0o040000 | 0o120000) {
      return Err(message(
        "queued source ZIP contains an unsupported file type",
      ));
    }
    if entry.is_dir() {
      fs::create_dir_all(&output)?;
      continue;
    }
    if kind == 0o040000 {
      return Err(message(
        "queued source directory mode does not match its entry",
      ));
    }
    fs::create_dir_all(output.parent().expect("relative file has parent"))?;
    if kind == 0o120000 {
      if entry.size() > 4096 {
        return Err(message("queued source symlink target is too long"));
      }
      let mut bytes = Vec::new();
      entry.by_ref().take(4097).read_to_end(&mut bytes)?;
      if bytes.len() as u64 != entry.size() {
        return Err(message("queued source symlink size is invalid"));
      }
      let target = PathBuf::from(
        String::from_utf8(bytes).map_err(|_| message("source symlink target must be UTF-8"))?,
      );
      validate_symlink(&relative, &target)?;
      symlinks.push((relative.clone(), target));
    } else {
      let mut options = OpenOptions::new();
      options.write(true).create_new(true);
      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
      }
      let mut file = options.open(&output)?;
      let expected = entry.size();
      let copied = io::copy(&mut entry.take(expected.saturating_add(1)), &mut file)?;
      if copied != expected {
        return Err(message("queued source entry size is invalid"));
      }
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600 | (mode & 0o111)))?;
      }
    }
    files.insert(relative);
  }
  let links = symlinks.iter().cloned().collect::<BTreeMap<_, _>>();
  validate_link_graph(&links)?;
  // Creating links last prevents ZIP entry order from redirecting extraction.
  for (relative, target) in symlinks {
    if seen
      .iter()
      .any(|other| other != &relative && other.starts_with(&relative))
    {
      return Err(message("queued source symlink cannot be an entry parent"));
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, temporary.path().join(relative))?;
    #[cfg(not(unix))]
    return Err(message("queued source symlinks require a Unix worker"));
  }
  let state = temporary.path().join(".expri");
  fs::create_dir(&state)?;
  let mut manifest = File::create(state.join("checkout.manifest"))?;
  for path in files {
    writeln!(manifest, "{}", archive_name(&path)?)?;
  }
  manifest.sync_all()?;
  fs::rename(temporary.path(), destination)?;
  Ok(())
}

fn source_paths(root: &Path, directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
  for entry in fs::read_dir(directory)? {
    let entry = entry?;
    let metadata = fs::symlink_metadata(entry.path())?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
      source_paths(root, &entry.path(), paths)?;
    } else if metadata.is_file() || metadata.file_type().is_symlink() {
      paths.push(
        entry
          .path()
          .strip_prefix(root)
          .expect("source child")
          .to_path_buf(),
      );
    } else {
      return Err(message("queued source contains an unsupported file type"));
    }
  }
  Ok(())
}

fn archive_name(path: &Path) -> Result<String> {
  let name = path
    .to_str()
    .ok_or_else(|| message("queued source paths must be UTF-8"))?;
  safe_entry(name)?;
  Ok(name.to_owned())
}

fn safe_entry(name: &str) -> Result<PathBuf> {
  let trimmed = name.trim_end_matches('/');
  let path = PathBuf::from(trimmed);
  if name.is_empty()
    || name.contains(['\\', '\0', '\n', '\r'])
    || name.starts_with('/')
    || name.contains(':')
    || trimmed
      .split('/')
      .any(|part| part == "." || part == ".." || part.is_empty())
    || path
      .components()
      .any(|part| !matches!(part, Component::Normal(_)))
    || path
      .components()
      .next()
      .is_some_and(|part| part.as_os_str() == ".expri" || part.as_os_str() == ".git")
  {
    return Err(message("queued source ZIP contains an unsafe path"));
  }
  Ok(path)
}

fn validate_symlink(relative: &Path, target: &Path) -> Result<()> {
  let raw = target
    .to_str()
    .ok_or_else(|| message("source symlink target must be UTF-8"))?;
  if raw.is_empty() || raw.contains(['\\', '\0', ':']) || target.is_absolute() {
    return Err(message(
      "queued source symlink escapes its source directory",
    ));
  }
  let mut depth = relative
    .parent()
    .map_or(0, |parent| parent.components().count());
  for part in target.components() {
    match part {
      Component::Normal(_) => depth += 1,
      Component::CurDir => {}
      Component::ParentDir if depth > 0 => depth -= 1,
      _ => {
        return Err(message(
          "queued source symlink escapes its source directory",
        ));
      }
    }
  }
  Ok(())
}

fn validate_link_graph(links: &BTreeMap<PathBuf, PathBuf>) -> Result<()> {
  for (relative, target) in links {
    resolve_link(relative, target, links, &mut BTreeSet::new())?;
  }
  Ok(())
}

fn resolve_link(
  relative: &Path,
  target: &Path,
  links: &BTreeMap<PathBuf, PathBuf>,
  followed: &mut BTreeSet<PathBuf>,
) -> Result<PathBuf> {
  validate_symlink(relative, target)?;
  if !followed.insert(relative.to_path_buf()) || followed.len() > 64 {
    return Err(message("queued source symlink chain is cyclic or too deep"));
  }
  let mut resolved = relative.parent().unwrap_or(Path::new("")).to_path_buf();
  for component in target.components() {
    match component {
      Component::Normal(name) => {
        resolved.push(name);
        if let Some(next_target) = links.get(&resolved) {
          resolved = resolve_link(&resolved, next_target, links, followed)?;
        }
      }
      Component::CurDir => {}
      Component::ParentDir if resolved.pop() => {}
      _ => {
        return Err(message(
          "queued source symlink escapes its source directory",
        ));
      }
    }
  }
  followed.remove(relative);
  Ok(resolved)
}

fn bounded_total(total: u64, size: u64) -> Result<u64> {
  total
    .checked_add(size)
    .filter(|total| *total <= MAX_SOURCE_BYTES)
    .ok_or_else(|| message("queued source exceeds the 512 MiB uncompressed limit"))
}

#[cfg(unix)]
fn safe_mode(metadata: &fs::Metadata) -> u32 {
  use std::os::unix::fs::PermissionsExt;
  0o600 | (metadata.permissions().mode() & 0o111)
}
#[cfg(not(unix))]
fn safe_mode(_: &fs::Metadata) -> u32 {
  0o600
}

fn zip_error(error: zip::result::ZipError) -> ExpriError {
  message(format!("invalid queued source ZIP: {error}"))
}
fn message(value: impl Into<String>) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests;
