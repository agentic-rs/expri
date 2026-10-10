//! Validated environment and publishing values shared by controllers and workers.

use std::collections::BTreeMap;
use std::path::{Component, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};
use crate::identifiers::validate_component;

/// Safe publishing intent. Credentials are resolved on the executing worker.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunServiceConfig {
  pub client_config: PathBuf,
  pub project_id: String,
  pub origin: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub dashboard_url: Option<String>,
  #[serde(default = "publish_enabled", skip_serializing_if = "is_enabled")]
  pub publish: bool,
}

fn publish_enabled() -> bool {
  true
}
fn is_enabled(value: &bool) -> bool {
  *value
}

impl RunServiceConfig {
  pub fn validate(&self) -> Result<()> {
    if !self.client_config.is_absolute()
      || self
        .client_config
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
      return Err(ExpriError::Message(
        "service.client_config must be an absolute path on the executing worker without '..'"
          .into(),
      ));
    }
    for (name, value) in [("project_id", &self.project_id), ("origin", &self.origin)] {
      validate_component(value)
        .map_err(|_| ExpriError::Message(format!("invalid service.{name}")))?;
    }
    if let Some(value) = &self.dashboard_url {
      let url = url::Url::parse(value)
        .map_err(|_| ExpriError::Message("invalid service.dashboard_url".into()))?;
      if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.path() != "/"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
      {
        return Err(ExpriError::Message(
          "service.dashboard_url must be an HTTP(S) root URL without credentials, query, or fragment"
            .into(),
        ));
      }
    }
    Ok(())
  }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
  pub base_python: Option<String>,
  #[serde(default)]
  pub reuse_packages: Vec<String>,
  #[serde(default)]
  pub require_cuda: bool,
  #[serde(default)]
  pub env: BTreeMap<String, String>,
}

impl EnvironmentConfig {
  pub fn validate(&self) -> Result<()> {
    if let Some(base_python) = &self.base_python
      && base_python.trim().is_empty()
    {
      return Err(ExpriError::Message(
        "environment.base_python must not be empty".to_string(),
      ));
    }
    for package in &self.reuse_packages {
      if !valid_package_name(package) {
        return Err(ExpriError::Message(format!(
          "invalid environment.reuse_packages name: {package:?}; use a package name without versions or extras"
        )));
      }
    }
    if !self.reuse_packages.is_empty() && self.base_python.is_none() {
      return Err(ExpriError::Message(
        "environment.reuse_packages requires environment.base_python".to_string(),
      ));
    }
    if self.require_cuda
      && !self
        .reuse_packages
        .iter()
        .any(|package| normalize_package_name(package) == "torch")
    {
      return Err(ExpriError::Message(
        "environment.require_cuda requires torch in environment.reuse_packages".to_string(),
      ));
    }
    for name in self.env.keys() {
      if !valid_environment_name(name) {
        return Err(ExpriError::Message(format!(
          "invalid environment.env variable name: {name:?}; use a POSIX variable name"
        )));
      }
      if name.starts_with("UV_") || matches!(name.as_str(), "PYTHONHOME" | "VIRTUAL_ENV") {
        return Err(ExpriError::Message(format!(
          "environment.env cannot override expri's Python/environment selection: {name}"
        )));
      }
    }
    Ok(())
  }
}

pub fn normalize_package_name(name: &str) -> String {
  let mut normalized = String::with_capacity(name.len());
  let mut previous_separator = false;
  for character in name.chars() {
    if matches!(character, '-' | '_' | '.') {
      if !previous_separator {
        normalized.push('-');
      }
      previous_separator = true;
    } else {
      normalized.push(character.to_ascii_lowercase());
      previous_separator = false;
    }
  }
  normalized
}

fn valid_package_name(name: &str) -> bool {
  let bytes = name.as_bytes();
  !bytes.is_empty()
    && bytes[0].is_ascii_alphanumeric()
    && bytes[bytes.len() - 1].is_ascii_alphanumeric()
    && bytes
      .iter()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_environment_name(name: &str) -> bool {
  let mut bytes = name.bytes();
  bytes
    .next()
    .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
    && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
