use std::fs;
use std::path::PathBuf;

use crate::archive::{PatchArchive, build_patch_archive};
use crate::config::TargetConfig;
use crate::controller::protocol::{ProtocolPreference, apply_sync_with_preference};
use crate::controller::transport::Remote;
use crate::error::Result;
use crate::filter::SyncRules;
use crate::git::{self, RemoteCandidate, SourceBundle};
use crate::protocol::SyncApplyRequest;

pub struct SyncOptions {
  pub repo_root: PathBuf,
  pub project_name: Option<String>,
  pub target_name: String,
  pub target: TargetConfig,
  pub sync: SyncRules,
  pub control_path: String,
  pub control_persist: String,
  pub dry_run: bool,
  pub force: bool,
  pub verbosity: u8,
  pub quiet: bool,
}

pub fn sync_target(options: SyncOptions) -> Result<()> {
  let preference = ProtocolPreference::parse(options.target.protocol.as_deref())?;
  let node_bin = options
    .target
    .node_bin
    .clone()
    .unwrap_or_else(|| "expri".to_string());
  let remote = Remote::new(
    options.target,
    options.control_path,
    options.control_persist,
    options.dry_run,
    options.verbosity,
    options.quiet,
  );
  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    eprintln!("sync target: {}", options.target_name);
    eprintln!("repo root: {}", options.repo_root.display());
  }

  let _opened_master = remote.open_master()?;
  let head = git::head(&options.repo_root)?;
  let remote_candidate = git::nearest_remote_url(&options.repo_root, &head)?;
  if options.verbosity > 0 && !options.quiet {
    if let Some(candidate) = &remote_candidate {
      match candidate.distance {
        Some(distance) => eprintln!(
          "nearest git remote: {} ({}, base={}, distance={distance})",
          candidate.name,
          candidate.url,
          candidate.base_commit.as_deref().unwrap_or("unknown")
        ),
        None => eprintln!("nearest git remote: {} ({})", candidate.name, candidate.url),
      }
    }
  }
  let dirty = git::dirty_paths(&options.repo_root, &options.sync)?;
  let patch = build_patch_archive(&options.repo_root, &dirty)?;
  print_digest("patch zip", &patch.digest, patch.size, options.quiet);
  if options.verbosity > 0 && !options.quiet {
    eprintln!(
      "patch zip file count: {} deleted={}",
      patch.file_count, patch.deleted_count
    );
  }

  let bundle = build_source_bundle_for_remote(
    &options.repo_root,
    remote_candidate.as_ref(),
    options.verbosity,
    options.quiet,
  )?;
  upload_artifacts_and_apply(
    &remote,
    &head,
    bundle.as_ref(),
    &patch,
    remote_candidate
      .as_ref()
      .map(|candidate| candidate.url.as_str()),
    options.force,
    preference,
    &node_bin,
  )?;
  Ok(())
}

fn build_source_bundle_for_remote(
  repo_root: &PathBuf,
  remote_candidate: Option<&RemoteCandidate>,
  verbosity: u8,
  quiet: bool,
) -> Result<Option<SourceBundle>> {
  let base_commit = remote_candidate.and_then(|candidate| candidate.base_commit.as_deref());
  let distance = remote_candidate.and_then(|candidate| candidate.distance);
  if matches!(distance, Some(0)) {
    if verbosity > 0 && !quiet {
      eprintln!("source bundle: skipped; nearest remote already has HEAD");
    }
    return Ok(None);
  }
  let bundle = git::build_source_bundle(repo_root, base_commit)?;
  match base_commit {
    Some(base_commit) if verbosity > 0 && !quiet => {
      eprintln!("source bundle refspec: {base_commit}..HEAD")
    }
    None if verbosity > 0 && !quiet => eprintln!("source bundle refspec: HEAD"),
    _ => {}
  }
  print_digest("source bundle", &bundle.digest, bundle.size, quiet);
  Ok(Some(bundle))
}

fn upload_artifacts_and_apply(
  remote: &Remote,
  head: &str,
  bundle: Option<&SourceBundle>,
  patch: &PatchArchive,
  remote_url: Option<&str>,
  force: bool,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  let inbox = format!("{}/inbox", remote.meta_dir());
  remote.ssh(&format!("mkdir -p {inbox}"))?;
  if let Some(bundle) = bundle {
    remote.upload_file(&bundle.path, &format!("{inbox}/source.bundle"))?;
  }
  remote.upload_file(&patch.path, &format!("{inbox}/patch.zip"))?;

  let request = SyncApplyRequest {
    head: head.to_string(),
    remote_url: remote_url.map(ToString::to_string),
    source_bundle: bundle.map(|_| ".expri/inbox/source.bundle".to_string()),
    source_bundle_sha256: bundle.map(|bundle| bundle.digest.clone()),
    patch: ".expri/inbox/patch.zip".to_string(),
    patch_sha256: patch.digest.clone(),
    state_dir: ".expri".to_string(),
    force,
  };
  let request_dir = tempfile::Builder::new()
    .prefix("expri-request-")
    .tempdir()?;
  let request_path = request_dir.path().join("sync-request.json");
  fs::write(&request_path, serde_json::to_string_pretty(&request)?)?;
  remote.upload_file(&request_path, &format!("{inbox}/sync-request.json"))?;
  apply_sync_with_preference(
    remote,
    ".expri/inbox/sync-request.json",
    preference,
    node_bin,
  )
}

fn print_digest(label: &str, digest: &str, size: u64, quiet: bool) {
  if !quiet {
    eprintln!("{label} sha256={digest} size={size} bytes");
  }
}
