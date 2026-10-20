//! Opt-in worker-side requirements for a directly launched run.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};

pub const RUN_REQUIREMENTS_CAPABILITY: &str = "run-requirements-v1";
pub const MAX_DISK_REQUIREMENTS: usize = 16;
pub const MAX_REQUIREMENT_BYTES: u64 = 9_007_199_254_740_991;

pub fn default_recovery_seconds() -> u32 {
  30
}

pub fn default_disk_path() -> String {
  "outputs".into()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequirements {
  #[serde(default)]
  pub disk: Vec<DiskRequirement>,
  /// Continuous healthy time before automatically resuming; zero resumes immediately.
  #[serde(default = "default_recovery_seconds")]
  pub recovery_seconds: u32,
}

impl Default for ResourceRequirements {
  fn default() -> Self {
    Self {
      disk: Vec::new(),
      recovery_seconds: default_recovery_seconds(),
    }
  }
}

impl ResourceRequirements {
  pub fn is_empty(&self) -> bool {
    self.disk.is_empty()
  }

  pub fn validate(&self) -> Result<()> {
    if self.disk.len() > MAX_DISK_REQUIREMENTS {
      return Err(invalid("at most 16 disk requirements are allowed"));
    }
    if self.recovery_seconds > 3600 {
      return Err(invalid("recovery_seconds must be between 0 and 3600"));
    }
    let mut paths = BTreeSet::new();
    for disk in &self.disk {
      disk.validate()?;
      if !paths.insert(disk.path.as_str()) {
        return Err(invalid("disk requirement paths must be unique"));
      }
    }
    Ok(())
  }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskRequirement {
  /// "outputs" means this run's outputs directory; other paths are absolute on the worker.
  #[serde(default = "default_disk_path")]
  pub path: String,
  pub min_free_bytes: u64,
  pub resume_free_bytes: u64,
}

impl DiskRequirement {
  pub fn validate(&self) -> Result<()> {
    if self.path.is_empty()
      || self.path.len() > 4096
      || self.path.chars().any(char::is_control)
      || (self.path != "outputs" && !self.path.starts_with('/'))
      || self.path.split('/').any(|part| matches!(part, "." | ".."))
    {
      return Err(invalid(
        "disk path must be outputs or an absolute worker path without parent traversal or control characters",
      ));
    }
    if self.min_free_bytes == 0
      || self.min_free_bytes > MAX_REQUIREMENT_BYTES
      || self.resume_free_bytes > MAX_REQUIREMENT_BYTES
      || self.resume_free_bytes <= self.min_free_bytes
    {
      return Err(invalid(
        "disk sizes must be positive safe integers, with resume_free_bytes greater than min_free_bytes",
      ));
    }
    Ok(())
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePhase {
  Ready,
  Blocked,
  Recovering,
  Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskObservation {
  pub path: String,
  pub available_bytes: Option<u64>,
  pub min_free_bytes: u64,
  pub resume_free_bytes: u64,
  pub phase: ResourcePhase,
  pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceStatus {
  pub phase: ResourcePhase,
  pub blocked: bool,
  pub checked_at: String,
  pub recovery_started_at: Option<String>,
  pub disk: Vec<DiskObservation>,
}

impl ResourceStatus {
  /// Validate a published snapshot without guessing its hysteresis history.
  pub fn validate(&self, requirements: &ResourceRequirements) -> Result<()> {
    requirements.validate()?;
    if self.disk.len() > MAX_DISK_REQUIREMENTS
      || self.disk.len() != requirements.disk.len()
      || self.blocked != (self.phase != ResourcePhase::Ready)
    {
      return Err(invalid(
        "resource status does not match its requirements or phase",
      ));
    }
    let checked_at = parse_timestamp(&self.checked_at)?;
    if let Some(recovery_started_at) = &self.recovery_started_at
      && parse_timestamp(recovery_started_at)? > checked_at
    {
      return Err(invalid("resource recovery cannot start after its check"));
    }
    let mut paths = BTreeSet::new();
    for observation in &self.disk {
      let requirement = requirements
        .disk
        .iter()
        .find(|disk| disk.path == observation.path)
        .ok_or_else(|| invalid("resource status contains an unconfigured disk path"))?;
      if !paths.insert(observation.path.as_str())
        || observation.min_free_bytes != requirement.min_free_bytes
        || observation.resume_free_bytes != requirement.resume_free_bytes
        || observation
          .error
          .as_ref()
          .is_some_and(|error| error.len() > 512 || error.chars().any(char::is_control))
      {
        return Err(invalid(
          "resource disk observations must have unique configured paths, matching thresholds and bounded errors",
        ));
      }
    }
    Ok(())
  }
}

fn parse_timestamp(value: &str) -> Result<chrono::DateTime<chrono::FixedOffset>> {
  if value.len() > 64 {
    return Err(invalid(
      "resource timestamps must be bounded RFC 3339 timestamps",
    ));
  }
  chrono::DateTime::parse_from_rfc3339(value)
    .map_err(|_| invalid("resource timestamps must be bounded RFC 3339 timestamps"))
}

/// Parse integer byte quantities in decimal or binary units without float rounding.
pub fn parse_byte_size(value: &str) -> Result<u64> {
  let value = value.trim();
  let end = value.bytes().take_while(u8::is_ascii_digit).count();
  let number = value[..end].parse::<u64>().map_err(|_| {
    invalid("disk size must be a positive integer followed by B, KiB, MiB, GiB, or TiB")
  })?;
  let multiplier = match value[end..].trim().to_ascii_lowercase().as_str() {
    "" | "b" => 1,
    "kib" => 1024,
    "mib" => 1024_u64.pow(2),
    "gib" => 1024_u64.pow(3),
    "tib" => 1024_u64.pow(4),
    "kb" => 1000,
    "mb" => 1000_u64.pow(2),
    "gb" => 1000_u64.pow(3),
    "tb" => 1000_u64.pow(4),
    _ => {
      return Err(invalid(
        "unknown disk size unit; use B, KiB, MiB, GiB, or TiB",
      ));
    }
  };
  number
    .checked_mul(multiplier)
    .filter(|bytes| *bytes > 0 && *bytes <= MAX_REQUIREMENT_BYTES)
    .ok_or_else(|| invalid("disk size is zero or exceeds the safe integer limit"))
}

/// Default hysteresis is 20%, rounded upward, and always at least one byte.
pub fn default_resume_free_bytes(min_free_bytes: u64) -> Result<u64> {
  min_free_bytes
    .checked_add(min_free_bytes.div_ceil(5))
    .filter(|bytes| *bytes > min_free_bytes && *bytes <= MAX_REQUIREMENT_BYTES)
    .ok_or_else(|| {
      invalid("default resume disk size exceeds its limit; specify resume_free explicitly")
    })
}

fn invalid(message: &str) -> ExpriError {
  ExpriError::Message(format!("invalid resource requirements: {message}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn policy() -> ResourceRequirements {
    ResourceRequirements {
      disk: vec![DiskRequirement {
        path: "outputs".into(),
        min_free_bytes: 100,
        resume_free_bytes: 120,
      }],
      ..Default::default()
    }
  }

  #[test]
  fn requirements_are_opt_in_with_bounded_hysteresis() {
    let empty: ResourceRequirements = serde_json::from_str("{}").unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.recovery_seconds, 30);
    assert!(empty.validate().is_ok());
    let mut requirements = policy();
    assert!(requirements.validate().is_ok());
    requirements.recovery_seconds = 0;
    assert!(requirements.validate().is_ok());
    requirements.recovery_seconds = 3601;
    assert!(requirements.validate().is_err());
    for (minimum, resume) in [(0, 10), (10, 10), (20, 10), (1, u64::MAX)] {
      let mut requirements = policy();
      requirements.disk[0].min_free_bytes = minimum;
      requirements.disk[0].resume_free_bytes = resume;
      assert!(requirements.validate().is_err());
    }
  }

  #[test]
  fn paths_are_worker_absolute_or_run_outputs_and_unique() {
    for path in ["outputs", "/", "/mnt/training data"] {
      let mut requirements = policy();
      requirements.disk[0].path = path.into();
      assert!(requirements.validate().is_ok(), "{path}");
    }
    for path in [
      "",
      "../disk",
      "outputs/subdir",
      "/mnt/../disk",
      "/mnt/./disk",
      "/tmp\nother",
    ] {
      let mut requirements = policy();
      requirements.disk[0].path = path.into();
      assert!(requirements.validate().is_err(), "{path:?}");
    }
    let mut requirements = policy();
    requirements.disk.push(requirements.disk[0].clone());
    assert!(requirements.validate().is_err());
    requirements.disk = (0..17)
      .map(|index| DiskRequirement {
        path: format!("/disk/{index}"),
        min_free_bytes: 100,
        resume_free_bytes: 120,
      })
      .collect();
    assert!(requirements.validate().is_err());
  }

  #[test]
  fn disk_sizes_are_checked_without_rounding_or_overflow() {
    assert_eq!(parse_byte_size("20GiB").unwrap(), 20 * 1024_u64.pow(3));
    assert_eq!(parse_byte_size("20 GB").unwrap(), 20_000_000_000);
    assert_eq!(default_resume_free_bytes(1).unwrap(), 2);
    assert_eq!(default_resume_free_bytes(101).unwrap(), 122);
    for value in [
      "",
      "0",
      "-1GiB",
      "1.5GiB",
      "1PB",
      "9007199254740992B",
      "18446744073709551615TiB",
    ] {
      assert!(parse_byte_size(value).is_err(), "{value}");
    }
    assert!(default_resume_free_bytes(MAX_REQUIREMENT_BYTES).is_err());
  }

  #[test]
  fn status_is_bounded_and_matches_the_configured_policy() {
    let requirements = policy();
    let status = ResourceStatus {
      phase: ResourcePhase::Recovering,
      blocked: true,
      checked_at: "2026-10-20T05:00:00Z".into(),
      recovery_started_at: Some("2026-10-20T04:59:50Z".into()),
      disk: vec![DiskObservation {
        path: "outputs".into(),
        available_bytes: Some(u64::MAX),
        min_free_bytes: 100,
        resume_free_bytes: 120,
        phase: ResourcePhase::Recovering,
        error: None,
      }],
    };
    assert!(status.validate(&requirements).is_ok());
    let mut invalid = status.clone();
    invalid.disk[0].min_free_bytes = 99;
    assert!(invalid.validate(&requirements).is_err());
    invalid = status.clone();
    invalid.blocked = false;
    assert!(invalid.validate(&requirements).is_err());
    invalid = status.clone();
    invalid.recovery_started_at = Some("2026-10-20T05:00:01Z".into());
    assert!(invalid.validate(&requirements).is_err());
    invalid = status.clone();
    invalid.disk[0].error = Some("bad\npath".into());
    assert!(invalid.validate(&requirements).is_err());
    invalid = status;
    invalid.checked_at = "yesterday".into();
    assert!(invalid.validate(&requirements).is_err());
  }
}
