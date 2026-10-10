use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use crate::error::{ExpriError, Result};

#[derive(Debug, Args)]
pub struct ArtifactCommand {
  #[command(subcommand)]
  command: ArtifactSubcommand,
}

#[derive(Debug, Subcommand)]
enum ArtifactSubcommand {
  /// Register a finalized output for background upload, without reading its bytes.
  Register {
    path: PathBuf,
    /// Defaults to EXPRI_RUN_DIR inside an expri task.
    #[arg(long)]
    run_dir: Option<PathBuf>,
    /// Move a best/latest alias to this file, without copying the object.
    #[arg(long = "label", value_parser = ["best", "latest"])]
    labels: Vec<String>,
  },
  /// Inspect the local registration outbox and transfer status.
  List {
    #[arg(long)]
    run_dir: Option<PathBuf>,
  },
  /// Discard a failed local handoff without deleting file or cloud bytes.
  Unregister {
    path: PathBuf,
    #[arg(long)]
    run_dir: Option<PathBuf>,
  },
}

pub fn run(command: ArtifactCommand, target: Option<&str>) -> Result<()> {
  if target.is_some() {
    return Err(ExpriError::Message(
      "artifact registration runs on the training machine; omit -T".into(),
    ));
  }
  let report = match command.command {
    ArtifactSubcommand::Register {
      path,
      run_dir,
      labels,
    } => {
      let run_dir = resolve_run_dir(run_dir)?;
      let path = relative_output(&run_dir, &path)?;
      if labels.is_empty() {
        crate::service::registrations::register(&run_dir, &path)?
      } else {
        crate::service::registrations::register_with_labels(&run_dir, &path, &labels)?
      }
    }
    ArtifactSubcommand::List { run_dir } => {
      crate::service::registrations::list(&resolve_run_dir(run_dir)?)?
    }
    ArtifactSubcommand::Unregister { path, run_dir } => {
      let run_dir = resolve_run_dir(run_dir)?;
      let path = relative_output(&run_dir, &path)?;
      crate::service::registrations::unregister(&run_dir, &path)?
    }
  };
  println!("{}", serde_json::to_string(&report)?);
  Ok(())
}

fn resolve_run_dir(path: Option<PathBuf>) -> Result<PathBuf> {
  let path = path
    .or_else(|| std::env::var_os("EXPRI_RUN_DIR").map(PathBuf::from))
    .ok_or_else(|| {
      ExpriError::Message("specify --run-dir or run inside an expri task with EXPRI_RUN_DIR".into())
    })?;
  Ok(if path.is_absolute() {
    path
  } else {
    std::env::current_dir()?.join(path)
  })
}

fn relative_output(run_dir: &Path, path: &Path) -> Result<String> {
  let relative = if path.is_absolute() {
    path.strip_prefix(run_dir).map_err(|_| {
      ExpriError::Message("registered output must be inside this run's outputs directory".into())
    })?
  } else {
    path
  };
  let value = relative
    .to_str()
    .filter(|value| crate::run_artifacts::validate_path(value).is_ok())
    .ok_or_else(|| {
      ExpriError::Message(
        "register a safe outputs/ path relative to the run, or its absolute path".into(),
      )
    })?;
  Ok(value.into())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn registration_paths_stay_inside_run_outputs() {
    let root = Path::new("/worker/.expri/runs/one");
    assert_eq!(
      relative_output(root, &root.join("outputs/checkpoint.pt")).unwrap(),
      "outputs/checkpoint.pt"
    );
    assert_eq!(
      relative_output(root, Path::new("outputs/checkpoint.pt")).unwrap(),
      "outputs/checkpoint.pt"
    );
    for path in [
      "/other/outputs/checkpoint.pt",
      "outputs/../code/x",
      "outputs/.secret",
      "checkpoint.pt",
    ] {
      assert!(relative_output(root, Path::new(path)).is_err(), "{path}");
    }
  }
}
