use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};

use super::message;
use super::preview::projection;
use crate::error::Result;

const CACHE_RECORD_LIMIT: u64 = 256 * 1024;

pub(crate) enum Download {
  Local {
    file: File,
    size: u64,
    filename: String,
  },
  Cloud {
    url: String,
    size: u64,
    filename: String,
  },
}

#[derive(Clone, Serialize)]
pub(crate) struct ArtifactRow {
  pub path: String,
  pub size: u64,
  pub local: Option<bool>,
  pub cloud: Option<bool>,
  pub worker: Option<bool>,
  pub download_url: Option<String>,
}

pub(crate) struct ReportedInventory {
  pub files: Vec<crate::run_artifacts::Artifact>,
  pub truncated: bool,
  pub recorded_at: Option<String>,
}

pub(crate) fn validate_output(path: &str) -> Result<()> {
  crate::run_artifacts::validate_path(path)
}

pub(crate) fn filename(path: &str) -> &str {
  path.rsplit('/').next().unwrap_or("download")
}

/// Both the ASCII fallback and the UTF-8 name are header-safe. The original
/// filename is never interpolated into a quoted HTTP header.
pub(crate) fn disposition(filename: &str) -> String {
  let fallback: String = filename
    .chars()
    .map(|character| {
      if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ' ') {
        character
      } else {
        '_'
      }
    })
    .collect();
  let encoded: String = filename
    .bytes()
    .map(|byte| {
      if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
        (byte as char).to_string()
      } else {
        format!("%{byte:02X}")
      }
    })
    .collect();
  format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

pub(crate) fn download_url(source: &str, run_id: &str, path: &str) -> String {
  let query = form_urlencoded::Serializer::new(String::new())
    .extend_pairs([("source", source), ("run_id", run_id), ("path", path)])
    .finish();
  format!("/api/artifact?{query}")
}

pub(crate) fn parse_inventory(value: &Value) -> Result<ReportedInventory> {
  if value["schema_version"] != 1 {
    return Err(message("unsupported artifact inventory schema"));
  }
  let files = parse_files(&value["files"])?;
  let truncated = value["truncated"]
    .as_bool()
    .ok_or_else(|| message("invalid artifact inventory truncation flag"))?;
  let recorded_at = value
    .get("recorded_at")
    .and_then(Value::as_str)
    .filter(|value| value.len() <= 128 && !value.chars().any(char::is_control))
    .map(str::to_string);
  Ok(ReportedInventory {
    files,
    truncated,
    recorded_at,
  })
}

fn parse_files(value: &Value) -> Result<Vec<crate::run_artifacts::Artifact>> {
  let files = value
    .as_array()
    .filter(|files| files.len() <= crate::run_artifacts::FILE_LIMIT)
    .ok_or_else(|| message("invalid or oversized artifact inventory"))?;
  let mut parsed = BTreeMap::<String, crate::run_artifacts::Artifact>::new();
  for file in files {
    let path = file["path"]
      .as_str()
      .ok_or_else(|| message("invalid artifact path"))?;
    validate_output(path)?;
    let size = file["size"]
      .as_u64()
      .ok_or_else(|| message("invalid artifact size"))?;
    if parsed
      .insert(
        path.into(),
        crate::run_artifacts::Artifact {
          path: path.into(),
          size,
        },
      )
      .is_some()
    {
      return Err(message("duplicate artifact inventory path"));
    }
  }
  Ok(parsed.into_values().collect())
}

pub(crate) fn bound_rows(
  rows: BTreeMap<String, ArtifactRow>,
  truncated: &mut bool,
) -> Result<Vec<ArtifactRow>> {
  let mut files = Vec::new();
  let mut bytes = 2;
  for row in rows.into_values() {
    let length = serde_json::to_vec(&row)?.len() + 1;
    if files.len() == crate::run_artifacts::FILE_LIMIT
      || bytes + length > crate::run_artifacts::INVENTORY_LIMIT
    {
      *truncated = true;
      continue;
    }
    bytes += length;
    files.push(row);
  }
  Ok(files)
}

fn optional_json(run_dir: &Path, path: &str, limit: u64) -> Result<Option<Value>> {
  let Some(file) = open_fixed(run_dir, path)? else {
    return Ok(None);
  };
  let mut bytes = Vec::new();
  file.take(limit + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > limit {
    return Err(message("artifact metadata exceeds its size limit"));
  }
  Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(super) fn local_catalog(run_dir: &Path, source: &super::Source, run_id: &str) -> Result<Value> {
  let mut rows = BTreeMap::new();
  let mut warnings = Vec::new();
  let mut truncated = false;
  let mut recorded_at = None;
  match optional_json(run_dir, crate::run_artifacts::INVENTORY_PATH, crate::run_artifacts::INVENTORY_LIMIT as u64)
    .and_then(|value| value.as_ref().map(parse_inventory).transpose()) {
    Ok(Some(inventory)) => {
      truncated |= inventory.truncated;
      recorded_at = inventory.recorded_at;
      for file in inventory.files {
        rows.insert(file.path.clone(), ArtifactRow { path: file.path, size: file.size,
          local: Some(false), cloud: None, worker: Some(true), download_url: None });
      }
    }
    Ok(None) => {}
    Err(_) => warnings.push(json!({"message": "Worker artifact inventory is unavailable or invalid; reported availability is unknown."})),
  }
  let mut pull_scope = Value::Null;
  if source.kind == "cached" {
    match optional_json(run_dir, "pull-state.json", CACHE_RECORD_LIMIT) {
      Ok(Some(receipt)) => {
        if let Ok(scope) = serde_json::from_value::<crate::service::types::RunScope>(receipt["scope"].clone())
          && scope.run_id == run_id && crate::service::types::validate_scope(&scope).is_ok() {
          pull_scope = serde_json::to_value(scope)?;
        }
        if receipt.get("available_files").is_some() {
          match parse_files(&receipt["available_files"]) {
            Ok(files) => {
              truncated |= receipt["available_files_truncated"].as_bool().unwrap_or(false);
              let records = receipt["available_files"].as_array().expect("validated file array");
              for file in files {
                let object = records.iter().find(|record| record["path"] == file.path)
                  .and_then(|record| record["sha256"].as_str())
                  .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
                let row = rows.entry(file.path.clone()).or_insert_with(|| ArtifactRow { path: file.path,
                  size: file.size, local: Some(false), cloud: None, worker: None, download_url: None });
                row.cloud = Some(object);
                if object { row.size = file.size; }
              }
              warnings.push(json!({"message": "Cloud availability reflects the last service pull; refresh with the CLI for current availability."}));
            }
            Err(_) => warnings.push(json!({"message": "Cached cloud artifact catalog is invalid; cloud availability is unknown."})),
          }
        }
      }
      Ok(None) => {}
      Err(_) => warnings.push(json!({"message": "Cached pull receipt is unavailable or invalid; cloud availability is unknown."})),
    }
  }
  let inventory = crate::run_artifacts::scan(run_dir)?;
  truncated |= inventory.truncated;
  for file in inventory.files {
    let row = rows
      .entry(file.path.clone())
      .or_insert_with(|| ArtifactRow {
        path: file.path.clone(),
        size: file.size,
        local: None,
        cloud: None,
        worker: None,
        download_url: None,
      });
    row.size = file.size;
    row.local = Some(true);
    row.download_url = Some(download_url(&source.source_id, run_id, &file.path));
  }
  let files = bound_rows(rows, &mut truncated)?;
  if truncated {
    warnings.push(json!({"message": "Artifact listing is limited to 200 files and bounded metadata; some files are omitted."}));
  }
  Ok(
    json!({"source": source, "run_id": run_id, "files": files, "truncated": truncated,
    "warnings": super::preview::bounded_warnings(&warnings), "pull_scope": pull_scope,
    "inventory_recorded_at": recorded_at}),
  )
}

/// Walk directory descriptors rather than pathname prefixes, so replacing a
/// parent with a symlink cannot redirect a download outside the repository.
#[cfg(unix)]
pub(crate) fn open_beneath(root: &Path, relative: &Path) -> Result<Option<File>> {
  use std::os::fd::{AsRawFd, FromRawFd};
  use std::os::unix::ffi::OsStrExt;
  use std::os::unix::fs::OpenOptionsExt;
  let components: Vec<_> = relative.components().collect();
  if components.is_empty()
    || components
      .iter()
      .any(|component| !matches!(component, std::path::Component::Normal(_)))
  {
    return Err(message("invalid artifact path"));
  }
  let mut directory = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
    .open(root)?;
  for (index, component) in components.iter().enumerate() {
    let name = std::ffi::CString::new(component.as_os_str().as_bytes())
      .map_err(|_| message("invalid artifact path"))?;
    let last = index + 1 == components.len();
    let flags = libc::O_RDONLY
      | libc::O_NOFOLLOW
      | libc::O_CLOEXEC
      | if last {
        libc::O_NONBLOCK
      } else {
        libc::O_DIRECTORY
      };
    // The directory descriptor remains owned until openat has returned.
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor == -1 {
      let error = std::io::Error::last_os_error();
      if error.kind() == std::io::ErrorKind::NotFound {
        return Ok(None);
      }
      return Err(message(
        "artifact must be a regular file inside real directories",
      ));
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if last {
      if !file.metadata()?.is_file() {
        return Err(message("artifact must be a regular file"));
      }
      return Ok(Some(file));
    }
    directory = file;
  }
  unreachable!("nonempty artifact path")
}

#[cfg(not(unix))]
pub(crate) fn open_beneath(root: &Path, relative: &Path) -> Result<Option<File>> {
  let path = relative
    .to_str()
    .ok_or_else(|| message("invalid artifact path"))?;
  open_fixed(root, path)
}

pub(super) fn optional_metadata(path: &Path) -> std::io::Result<Option<fs::Metadata>> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error),
  }
}

pub(super) fn real_prefix(root: &Path, path: &Path) -> Result<()> {
  let relative = path
    .strip_prefix(root)
    .map_err(|_| message("dashboard path is outside the repository"))?;
  let mut current = root.to_path_buf();
  for component in relative.components() {
    current.push(component);
    let Some(metadata) = optional_metadata(&current)? else {
      break;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
      return Err(message(format!(
        "dashboard directory must be a real directory: {}",
        current.display()
      )));
    }
  }
  Ok(())
}

/// Fixed log/cache files are opened without following links or blocking on FIFOs.
pub(super) fn open_fixed(run_dir: &Path, relative: &str) -> Result<Option<File>> {
  let path = run_dir.join(relative);
  real_prefix(run_dir, path.parent().unwrap())?;
  let Some(initial) = optional_metadata(&path)? else {
    return Ok(None);
  };
  if !initial.is_file() || initial.file_type().is_symlink() {
    return Err(message(format!("{relative} must be a regular file")));
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options.open(&path)?;
  let opened = file.metadata()?;
  if !opened.is_file() {
    return Err(message(format!("{relative} must be a regular file")));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if initial.dev() != opened.dev() || initial.ino() != opened.ino() {
      return Err(message(format!("{relative} changed while opening")));
    }
  }
  real_prefix(run_dir, path.parent().unwrap())?;
  Ok(Some(file))
}

pub(super) fn cache_record(run_dir: &Path, truncated: &mut bool) -> Result<Value> {
  let Some(file) = open_fixed(run_dir, "pull-state.json")? else {
    return Ok(Value::Null);
  };
  let mut bytes = Vec::new();
  file.take(CACHE_RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > CACHE_RECORD_LIMIT {
    return Err(message("pull-state.json exceeds the 256 KiB size limit"));
  }
  let record: Value = serde_json::from_slice(&bytes)?;
  if !record.is_object() {
    return Err(message("pull-state.json must be an object"));
  }
  Ok(projection(
    &record,
    &[
      "pulled_at",
      "selected_files",
      "target_name",
      "remote_run_dir",
    ],
    truncated,
  ))
}
