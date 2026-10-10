//! Small, Git-tracked asset descriptors with immutable local content bindings.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::filter::DEFAULT_EXCLUDED_DIRS;

mod bind;
pub mod download;

pub use bind::bind_verified;
pub(crate) use bind::{
  check_cancelled, directories, directory, inspect, message, open, optional_regular,
  sync_directory, unchanged,
};

pub const SIDECAR_SUFFIX: &str = ".expri.toml";
const MAX_DESCRIPTOR_BYTES: u64 = 64 * 1024;
pub(crate) const MAX_ASSETS: usize = 64;
const MAX_DISCOVERY_ENTRIES: usize = 100_000;
const MAX_DISCOVERY_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
  pub version: u32,
  pub source: Source,
  pub size: u64,
  pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
  Url {
    url: String,
  },
  HuggingFace {
    repo_id: String,
    repo_type: String,
    filename: String,
    revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested_revision: Option<String>,
  },
  Expri {
    url: String,
    project_id: String,
    input_id: String,
  },
}

impl Source {
  pub fn validate(&self) -> Result<()> {
    match self {
      Self::Url { url } => {
        let parsed = parse_url(url)?;
        for (key, _) in parsed.query_pairs() {
          let key = key.to_ascii_lowercase();
          if matches!(
            key.as_str(),
            "token"
              | "access_token"
              | "auth"
              | "authorization"
              | "signature"
              | "sig"
              | "password"
              | "secret"
              | "key"
              | "api_key"
              | "apikey"
              | "credential"
              | "credentials"
              | "awsaccesskeyid"
              | "key-pair-id"
          ) || key.starts_with("x-amz-")
            || key.starts_with("x-goog-")
          {
            return Err(message(
              "asset URL must not contain credentials or signed download parameters; use a private expri input reference",
            ));
          }
        }
      }
      Self::HuggingFace {
        repo_id,
        repo_type,
        filename,
        revision,
        requested_revision,
      } => {
        let parts = repo_id.split('/').collect::<Vec<_>>();
        if parts.is_empty() || parts.len() > 2 || parts.iter().any(|part| !valid_hf_name(part)) {
          return Err(message("invalid Hugging Face repository identifier"));
        }
        if !matches!(repo_type.as_str(), "model" | "dataset") {
          return Err(message(
            "Hugging Face repository type must be model or dataset",
          ));
        }
        validate_relative(filename)?;
        validate_hf_revision(revision)?;
        if let Some(requested) = requested_revision {
          validate_hf_revision(requested)?;
        }
      }
      Self::Expri {
        url,
        project_id,
        input_id,
      } => {
        let parsed = parse_url(url)?;
        if parsed.path() != "/" || parsed.query().is_some() {
          return Err(message("private asset service URL must be a base origin"));
        }
        crate::service::validate_component(project_id)?;
        crate::service::validate_component(input_id)?;
      }
    }
    Ok(())
  }
}

impl Descriptor {
  pub fn validate(&self) -> Result<()> {
    if self.version != 1 {
      return Err(message(
        "unsupported asset descriptor version; expected version 1",
      ));
    }
    self.source.validate()?;
    if !valid_hex(&self.sha256, 64) {
      return Err(message(
        "asset SHA256 must contain 64 lowercase hexadecimal digits",
      ));
    }
    if let Source::HuggingFace { revision, .. } = &self.source
      && !valid_hex(revision, 40)
    {
      return Err(message(
        "asset Hugging Face revision must be a resolved 40-digit commit",
      ));
    }
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
  pub path: PathBuf,
  pub sidecar: PathBuf,
  pub descriptor: Descriptor,
}

pub fn sidecar_path(path: &Path) -> PathBuf {
  let mut name = path.as_os_str().to_os_string();
  name.push(SIDECAR_SUFFIX);
  PathBuf::from(name)
}

pub fn asset_path(sidecar: &Path) -> Result<PathBuf> {
  let text = sidecar
    .to_str()
    .ok_or_else(|| message("asset descriptor paths must be UTF-8"))?;
  let path = PathBuf::from(
    text
      .strip_suffix(SIDECAR_SUFFIX)
      .ok_or_else(|| message("asset descriptor filename must end in .expri.toml"))?,
  );
  validate_asset_path(&path)?;
  Ok(path)
}

pub fn load(path: &Path) -> Result<Descriptor> {
  let mut bytes = Vec::new();
  open(path)?
    .take(MAX_DESCRIPTOR_BYTES + 1)
    .read_to_end(&mut bytes)?;
  if bytes.len() as u64 > MAX_DESCRIPTOR_BYTES {
    return Err(message("asset descriptor exceeds its 64 KiB size limit"));
  }
  let text =
    std::str::from_utf8(&bytes).map_err(|_| message("asset descriptor must be UTF-8 TOML"))?;
  let descriptor: Descriptor = toml::from_str(text)?;
  descriptor.validate()?;
  Ok(descriptor)
}

pub fn save(path: &Path, descriptor: &Descriptor) -> Result<()> {
  descriptor.validate()?;
  let bytes = toml::to_string_pretty(descriptor)
    .map_err(|error| message(format!("cannot encode asset descriptor: {error}")))?;
  if bytes.len() as u64 > MAX_DESCRIPTOR_BYTES {
    return Err(message("asset descriptor exceeds its 64 KiB size limit"));
  }
  optional_regular(path)?;
  let parent = path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .unwrap_or(Path::new("."));
  directories(parent)?;
  let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
  temporary.write_all(bytes.as_bytes())?;
  temporary.as_file().sync_all()?;
  optional_regular(path)?;
  temporary.persist(path).map_err(|error| error.error)?;
  sync_directory(parent)
}

/// Discover descriptors without following links or entering generated/private directories.
pub fn discover(root: &Path) -> Result<Vec<Asset>> {
  directory(root)?;
  let mut assets = Vec::new();
  let mut visited = 0;
  discover_directory(root, Path::new(""), 0, &mut visited, &mut assets)?;
  assets.sort_by(|first, second| first.path.cmp(&second.path));
  let sidecars = assets
    .iter()
    .map(|asset| &asset.sidecar)
    .collect::<BTreeSet<_>>();
  for (index, asset) in assets.iter().enumerate() {
    if sidecars
      .iter()
      .any(|sidecar| asset.path.starts_with(sidecar) || sidecar.starts_with(&asset.path))
    {
      return Err(message(
        "asset destinations must not overlap descriptor paths",
      ));
    }
    for other in &assets[index + 1..] {
      if asset.path.starts_with(&other.path) || other.path.starts_with(&asset.path) {
        return Err(message("asset destinations must not overlap"));
      }
    }
    optional_regular(&root.join(&asset.path))?;
  }
  Ok(assets)
}

fn discover_directory(
  root: &Path,
  relative: &Path,
  depth: usize,
  visited: &mut usize,
  assets: &mut Vec<Asset>,
) -> Result<()> {
  if depth > MAX_DISCOVERY_DEPTH {
    return Err(message("asset discovery exceeds its directory depth limit"));
  }
  let directory_path = root.join(relative);
  directory(&directory_path)?;
  for entry in fs::read_dir(&directory_path)? {
    let entry = entry?;
    *visited += 1;
    if *visited > MAX_DISCOVERY_ENTRIES {
      return Err(message("asset discovery exceeds its directory entry limit"));
    }
    let name = entry.file_name();
    if crate::filter::is_private_source_path(Path::new(&name))
      || DEFAULT_EXCLUDED_DIRS
        .iter()
        .any(|excluded| name == *excluded)
    {
      continue;
    }
    let relative_path = relative.join(&name);
    let metadata = fs::symlink_metadata(entry.path())?;
    if name.to_string_lossy().ends_with(SIDECAR_SUFFIX) {
      if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(message(
          "asset descriptors must be regular files without symlinks",
        ));
      }
      let path = asset_path(&relative_path)?;
      if assets.len() == MAX_ASSETS {
        return Err(message("a project supports at most 64 asset descriptors"));
      }
      assets.push(Asset {
        path,
        sidecar: relative_path,
        descriptor: load(&entry.path())?,
      });
    } else if metadata.is_dir() && !metadata.file_type().is_symlink() {
      discover_directory(root, &relative_path, depth + 1, visited, assets)?;
    }
  }
  Ok(())
}

pub fn cache_file(repo: &Path, sha256: &str) -> PathBuf {
  repo.join(".expri/assets").join(sha256).join("file")
}

pub(crate) fn validate_asset_path(path: &Path) -> Result<()> {
  let text = path
    .to_str()
    .ok_or_else(|| message("asset paths must be UTF-8"))?;
  validate_relative(text)?;
  for component in path.components() {
    let Component::Normal(name) = component else {
      return Err(message(
        "asset destinations must be ordinary relative paths",
      ));
    };
    let name = name.to_string_lossy();
    if name.starts_with('.')
      || name.ends_with(SIDECAR_SUFFIX)
      || DEFAULT_EXCLUDED_DIRS.contains(&name.as_ref())
      || reserved_name(&name)
    {
      return Err(message(
        "asset destination uses a reserved code, configuration, or descriptor path",
      ));
    }
  }
  Ok(())
}

pub(crate) fn ensure_parent(root: &Path, relative: &Path) -> Result<()> {
  validate_asset_path(relative)?;
  directory(root)?;
  if let Some(parent) = relative
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
  {
    directories(&root.join(parent))?;
  }
  optional_regular(&root.join(relative))
}

fn reserved_name(name: &str) -> bool {
  matches!(
    name,
    "expri.toml"
      | "pyproject.toml"
      | "uv.lock"
      | "Cargo.toml"
      | "Cargo.lock"
      | "package.json"
      | "package-lock.json"
      | "pnpm-lock.yaml"
      | "yarn.lock"
      | "bun.lock"
      | "bun.lockb"
  ) || (name.starts_with("expri.") && name.ends_with(".toml"))
}

fn validate_relative(value: &str) -> Result<()> {
  if value.is_empty()
    || value.len() > 1024
    || value.contains('\\')
    || value.chars().any(char::is_control)
    || value
      .split('/')
      .any(|part| part.is_empty() || part == "." || part == "..")
    || Path::new(value)
      .components()
      .any(|part| !matches!(part, Component::Normal(_)))
  {
    return Err(message(
      "asset paths must be ordinary relative paths without '..'",
    ));
  }
  Ok(())
}

fn parse_url(value: &str) -> Result<reqwest::Url> {
  if value.is_empty()
    || value.len() > 8192
    || value.contains('\\')
    || value.chars().any(char::is_whitespace)
    || value.chars().any(char::is_control)
  {
    return Err(message("invalid asset source URL"));
  }
  let url = reqwest::Url::parse(value).map_err(|_| message("invalid asset source URL"))?;
  if !matches!(url.scheme(), "http" | "https")
    || url.host_str().is_none()
    || !url.username().is_empty()
    || url.password().is_some()
    || url.fragment().is_some()
  {
    return Err(message(
      "asset source URL must be HTTP or HTTPS without credentials or a fragment",
    ));
  }
  Ok(url)
}

fn valid_hf_name(value: &str) -> bool {
  !value.is_empty()
    && value.len() <= 96
    && !value.starts_with(['.', '-'])
    && !value.ends_with(['.', '-'])
    && !value.contains("..")
    && !value.contains("--")
    && value
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn validate_hf_revision(value: &str) -> Result<()> {
  if value.is_empty()
    || value.len() > 256
    || value.contains('\\')
    || value.chars().any(char::is_control)
    || value.chars().any(char::is_whitespace)
    || value
      .split('/')
      .any(|part| part.is_empty() || part == "." || part == "..")
    || value.contains(['?', '#'])
  {
    return Err(message("invalid Hugging Face revision"));
  }
  Ok(())
}

pub(crate) fn valid_hex(value: &str, length: usize) -> bool {
  value.len() == length
    && value
      .bytes()
      .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests;
