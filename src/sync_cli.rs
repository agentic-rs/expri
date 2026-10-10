use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::SyncCommand;
use crate::context::CommandContext;
use crate::controller::sync::{SyncOptions, sync_target_with_receipt};
use crate::error::Result;
use crate::service::{FileSyncOptions, sync_files};

pub fn run(command: SyncCommand, target: Option<&str>, verbosity: u8, quiet: bool) -> Result<()> {
  let context = CommandContext::load(command.config, command.repo)?;
  let files = context
    .config
    .file_sync
    .as_ref()
    .map(|config| FileSyncOptions {
      config: if config.client_config.is_absolute() {
        config.client_config.clone()
      } else {
        context.repo_root.join(&config.client_config)
      },
      project_id: config.project_id.clone(),
      origins: config.origins.clone(),
      repo: context.repo_root.clone(),
      results_dir: context.config.download_results_dir().into(),
      artifacts: config.artifacts.clone(),
      labels: config.labels.clone(),
      watch: command.watch,
      dry_run: command.dry_run,
      quiet,
    });
  // With file_sync configured, an explicit -T chooses the source destination;
  // without -T the same command is useful on laptops with several workers.
  let source = if target.is_some() || files.is_none() || command.pull || !command.paths.is_empty() {
    let context = context.into_target(target, command.control_path)?;
    Some(SyncOptions {
      sync: context.config.sync_rules()?,
      repo_root: context.repo_root,
      project_name: context.project_name,
      target_name: context.target_name,
      target: context.target,
      control_path: context.control_path,
      control_persist: command.control_persist,
      dry_run: command.dry_run,
      force: command.force,
      pull: command.pull,
      paths: command.paths,
      verbosity,
      quiet,
    })
  } else {
    None
  };
  // Path transfer and --pull retain their established source-only behavior.
  let files =
    files.filter(|_| !command.pull && source.as_ref().is_none_or(|source| source.paths.is_empty()));
  if command.watch && !command.dry_run {
    match (source, files) {
      (Some(source), Some(files)) => {
        thread::Builder::new()
          .name("expri-source-sync".into())
          .spawn(move || watch_source(source, quiet))?;
        sync_files(files)?;
      }
      (Some(source), None) => watch_source(source, quiet),
      (None, Some(files)) => {
        sync_files(files)?;
      }
      (None, None) => unreachable!("source or file sync is selected"),
    }
    return Ok(());
  }
  // Independent directions should both make progress even when one is offline.
  let source_result = source.map(sync_target_with_receipt).transpose();
  let file_result = files.map(sync_files).transpose();
  if let Some(report) = file_result?
    && !quiet
  {
    println!("{}", serde_json::to_string(&report)?);
  }
  source_result?;
  Ok(())
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
          "status":"synchronized", "identity":identity})
      }
      Err(_) => {
        failures = failures.saturating_add(1);
        json!({"direction":"source_to_worker", "target":options.target_name,
          "status":"retrying", "message":"Source sync will retry; active run snapshots are unchanged."})
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
