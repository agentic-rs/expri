use clap::Args;
use expri_core::resource_requirements::{
  DiskRequirement, ResourceRequirements, default_disk_path, default_resume_free_bytes,
  parse_byte_size,
};

use crate::error::{ExpriError, Result};

#[derive(Debug, Default, Args)]
pub(crate) struct RunRequirements {
  /// Pause below this free disk size, for example 20GiB. Repeat for multiple paths.
  #[arg(long = "disk-free", value_name = "SIZE")]
  pub disk_free: Vec<String>,
  /// Worker path for each --disk-free, in the same order; defaults to this run's outputs.
  #[arg(long = "disk-path", value_name = "PATH", requires = "disk_free")]
  pub disk_path: Vec<String>,
  /// Resume above this free size; defaults to 20% above each --disk-free.
  #[arg(long = "disk-resume-free", value_name = "SIZE", requires = "disk_free")]
  pub disk_resume_free: Vec<String>,
  /// Require continuous healthy disk space for this many seconds before resuming.
  #[arg(long, value_parser = clap::value_parser!(u32).range(0..=3600))]
  pub disk_recovery_seconds: Option<u32>,
}

impl RunRequirements {
  /// Explicit disk flags replace the configured disk list; otherwise retain every task path.
  pub fn resolve(&self, defaults: &ResourceRequirements) -> Result<ResourceRequirements> {
    let mut requirements = defaults.clone();
    if !self.disk_free.is_empty() {
      for (name, count) in [
        ("--disk-path", self.disk_path.len()),
        ("--disk-resume-free", self.disk_resume_free.len()),
      ] {
        if count != 0 && count != self.disk_free.len() {
          return Err(invalid(&format!("provide one {name} for each --disk-free")));
        }
      }
      requirements.disk = self
        .disk_free
        .iter()
        .enumerate()
        .map(|(index, value)| {
          let min_free_bytes = parse_byte_size(value)?;
          Ok(DiskRequirement {
            path: self
              .disk_path
              .get(index)
              .cloned()
              .unwrap_or_else(default_disk_path),
            min_free_bytes,
            resume_free_bytes: match self.disk_resume_free.get(index) {
              Some(value) => parse_byte_size(value)?,
              None => default_resume_free_bytes(min_free_bytes)?,
            },
          })
        })
        .collect::<Result<Vec<_>>>()?;
    }
    if let Some(seconds) = self.disk_recovery_seconds {
      if requirements.is_empty() {
        return Err(invalid(
          "--disk-recovery-seconds requires --disk-free or configured disk requirements",
        ));
      }
      requirements.recovery_seconds = seconds;
    }
    requirements.validate()?;
    Ok(requirements)
  }
}

fn invalid(message: &str) -> ExpriError {
  ExpriError::Message(message.into())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn configured() -> ResourceRequirements {
    ResourceRequirements {
      disk: ["/mnt/data", "outputs"]
        .into_iter()
        .map(|path| DiskRequirement {
          path: path.into(),
          min_free_bytes: 100,
          resume_free_bytes: 120,
        })
        .collect(),
      recovery_seconds: 45,
    }
  }

  #[test]
  fn task_paths_survive_without_cli_override() {
    assert_eq!(
      RunRequirements::default().resolve(&configured()).unwrap(),
      configured()
    );
    let options = RunRequirements {
      disk_recovery_seconds: Some(0),
      ..Default::default()
    };
    let resolved = options.resolve(&configured()).unwrap();
    assert_eq!(resolved.disk, configured().disk);
    assert_eq!(resolved.recovery_seconds, 0);
  }

  #[test]
  fn explicit_disk_list_replaces_defaults_and_pairs_sizes_with_paths() {
    let options = RunRequirements {
      disk_free: vec!["10GiB".into(), "20GB".into()],
      disk_path: vec!["outputs".into(), "/mnt/checkpoints".into()],
      ..Default::default()
    };
    let resolved = options.resolve(&configured()).unwrap();
    assert_eq!(resolved.disk.len(), 2);
    assert_eq!(resolved.disk[0].resume_free_bytes, 12 * 1024_u64.pow(3));
    assert_eq!(resolved.disk[1].min_free_bytes, 20_000_000_000);
    assert_eq!(resolved.recovery_seconds, 45);
    let invalid = RunRequirements {
      disk_path: vec!["outputs".into()],
      ..options
    };
    assert!(invalid.resolve(&configured()).is_err());
  }

  #[test]
  fn recovery_without_disk_does_not_silently_enable_a_policy() {
    let options = RunRequirements {
      disk_recovery_seconds: Some(20),
      ..Default::default()
    };
    assert!(options.resolve(&ResourceRequirements::default()).is_err());
    assert!(
      RunRequirements::default()
        .resolve(&ResourceRequirements::default())
        .unwrap()
        .is_empty()
    );
  }
}
