use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::PushCommand;
use crate::context::CommandContext;
use crate::controller::sync::{SyncOptions, sync_target_with_receipt};
use crate::error::Result;

pub fn run(command: PushCommand, target: Option<&str>, verbosity: u8, quiet: bool) -> Result<()> {
  let watch = command.watch;
  let context = CommandContext::load(command.config.clone(), command.repo.clone())?;
  let source = source_options(context, command, target, verbosity, quiet)?;
  if source.dry_run || !watch {
    sync_target_with_receipt(source)?;
  } else {
    watch_source(source, quiet);
  }
  Ok(())
}

fn source_options(
  context: CommandContext,
  command: PushCommand,
  target: Option<&str>,
  verbosity: u8,
  quiet: bool,
) -> Result<SyncOptions> {
  let context = context.into_target(target, command.control_path)?;
  Ok(SyncOptions {
    sync: context.config.push_rules()?,
    repo_root: context.repo_root,
    project_name: context.project_name,
    target_name: context.target_name,
    target: context.target,
    control_path: context.control_path,
    control_persist: command.control_persist,
    dry_run: command.dry_run,
    force: command.force,
    paths: command.paths,
    verbosity,
    quiet,
  })
}

fn watch_source(mut options: SyncOptions, quiet: bool) {
  let mut previous: Option<Value> = None;
  let mut failures = 0u32;
  let mut previous_source = None;
  let mut checked_at = None;
  options.quiet = true;
  loop {
    let observation = observe_source(&options);
    if let Ok(current) = &observation
      && previous_source.as_ref() == Some(current)
      && checked_at.is_some_and(|last: Instant| last.elapsed() < Duration::from_secs(30))
    {
      thread::sleep(Duration::from_secs(5));
      continue;
    }
    let result = observation.and_then(|current| {
      sync_target_with_receipt(options.clone()).map(|identity| (current, identity))
    });
    let report = match result {
      Ok((current, identity)) => {
        failures = 0;
        options.force = false;
        previous_source = Some(current);
        checked_at = Some(Instant::now());
        json!({"direction":"source_to_worker", "target":options.target_name,
          "status":"pushed", "identity":identity})
      }
      Err(_) => {
        failures = failures.saturating_add(1);
        json!({"direction":"source_to_worker", "target":options.target_name,
          "status":"retrying", "message":"Source push will retry; active run snapshots are unchanged."})
      }
    };
    if !quiet && previous.as_ref() != Some(&report) {
      eprintln!("{report}");
      previous = Some(report);
    }
    thread::sleep(Duration::from_secs(if failures == 0 {
      5
    } else {
      (5u64 << failures.min(4)).min(60)
    }));
  }
}

#[derive(PartialEq, Eq)]
struct SourceObservation {
  head: String,
  deleted: Vec<PathBuf>,
  files: Vec<(PathBuf, u64, Option<SystemTime>, i64, i64)>,
}

fn observe_source(options: &SyncOptions) -> Result<SourceObservation> {
  let dirty = crate::git::dirty_paths(&options.repo_root, &options.sync)?;
  let mut files = Vec::new();
  for path in dirty.files {
    let metadata = std::fs::symlink_metadata(options.repo_root.join(&path))?;
    #[cfg(unix)]
    let changed = {
      use std::os::unix::fs::MetadataExt;
      (metadata.ctime(), metadata.ctime_nsec())
    };
    #[cfg(not(unix))]
    let changed = (0, 0);
    files.push((
      path,
      metadata.len(),
      metadata.modified().ok(),
      changed.0,
      changed.1,
    ));
  }
  Ok(SourceObservation {
    head: crate::git::head(&options.repo_root)?,
    deleted: dirty.deleted,
    files,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn command() -> PushCommand {
    PushCommand {
      config: None,
      repo: None,
      control_path: None,
      control_persist: "30m".into(),
      dry_run: true,
      force: false,
      watch: true,
      paths: Vec::new(),
    }
  }

  fn context() -> CommandContext {
    CommandContext {
      config: toml::from_str(
        "[target.gpu]\nhost='gpu.example'\nremote_dir='/srv/project'\n[fetch]\nclient_config='owner.toml'\nproject_id='vision'\norigins=['other-worker']\n",
      )
      .unwrap(),
      repo_root: "/repo".into(),
      project_name: Some("vision".into()),
    }
  }

  #[test]
  fn push_selects_worker_independently_of_fetch_origins() {
    let options = source_options(context(), command(), None, 0, true).unwrap();
    assert_eq!(options.target_name, "gpu");
    assert_eq!(options.target.host, "gpu.example");
    assert_eq!(options.target.remote_dir, "/srv/project");
    assert_eq!(options.repo_root, PathBuf::from("/repo"));
  }

  #[test]
  fn push_still_requires_a_worker_when_only_fetch_is_configured() {
    let mut context = context();
    context.config.target.clear();
    let error = source_options(context, command(), None, 0, true)
      .err()
      .unwrap();
    assert!(error.to_string().contains("target is required"));
  }

  #[test]
  fn push_preserves_explicit_source_paths() {
    let mut command = command();
    command.watch = false;
    command.paths = vec!["code.py".into()];
    let options = source_options(context(), command, Some("gpu"), 0, true).unwrap();
    assert_eq!(options.paths, [PathBuf::from("code.py")]);
  }
}
