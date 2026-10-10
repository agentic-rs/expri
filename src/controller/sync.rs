use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::Deserialize;

use crate::archive::{PatchArchive, build_patch_archive, sha256_file};
use crate::config::TargetConfig;
use crate::controller::protocol::{
  ProtocolPreference, apply_sync_with_preference, prepare_pull_with_preference,
};
use crate::controller::transport::Remote;
use crate::error::Result;
use crate::filter::SyncRules;
use crate::git::{self, RemoteCandidate, SourceBundle};
use crate::protocol::{PullArtifacts, SyncApplyRequest, SyncIdentity};
use crate::shell;

#[derive(Clone)]
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
  pub pull: bool,
  pub paths: Vec<PathBuf>,
  pub verbosity: u8,
  pub quiet: bool,
}

#[derive(Debug, Deserialize)]
struct RemoteSyncState {
  head: String,
  patch_sha256: String,
  checkout_manifest_sha256: Option<String>,
}

pub fn sync_target_with_receipt(options: SyncOptions) -> Result<Option<SyncIdentity>> {
  sync_target_with_output(options, false)
}

pub fn sync_target_with_diagnostic_receipt(options: SyncOptions) -> Result<Option<SyncIdentity>> {
  sync_target_with_output(options, true)
}

fn sync_target_with_output(
  options: SyncOptions,
  diagnostic_stdout: bool,
) -> Result<Option<SyncIdentity>> {
  let preference = ProtocolPreference::parse(options.target.protocol.as_deref())?;
  let node_bin = options
    .target
    .node_bin
    .clone()
    .unwrap_or_else(|| "expri".to_string());
  let remote = Remote::new(
    options.target.clone(),
    options.control_path.clone(),
    options.control_persist.clone(),
    options.dry_run,
    options.verbosity,
    options.quiet,
  )?
  .with_diagnostic_stdout(diagnostic_stdout);
  if !options.paths.is_empty() {
    return sync_paths(options, remote).map(|_| None);
  }
  if options.pull {
    return pull_target(options, remote, preference, &node_bin).map(|_| None);
  }
  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    eprintln!("sync target: {}", options.target_name);
    eprintln!("repo root: {}", options.repo_root.display());
  }

  remote.connect()?;
  let head = git::head(&options.repo_root)?;
  let remote_sync_state = if options.force {
    None
  } else {
    read_remote_sync_state(&remote)?
  };
  let remote_candidate = git::nearest_remote_url(&options.repo_root, &head)?;
  if options.verbosity > 0
    && !options.quiet
    && let Some(candidate) = &remote_candidate
  {
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
  let dirty = git::dirty_paths(&options.repo_root, &options.sync)?;
  let patch = build_patch_archive(&options.repo_root, &dirty)?;
  let identity = SyncIdentity {
    head: head.clone(),
    patch_sha256: patch.digest.clone(),
  };
  print_digest("patch zip", &patch.digest, patch.size, options.quiet);
  if options.verbosity > 0 && !options.quiet {
    eprintln!(
      "patch zip file count: {} deleted={}",
      patch.file_count, patch.deleted_count
    );
  }
  if remote_sync_is_current(remote_sync_state.as_ref(), &head, &patch) {
    if !options.quiet {
      eprintln!("sync skipped: target already has HEAD and patch");
    }
    return Ok(Some(identity));
  }

  let bundle = build_source_bundle_for_remote(
    &options.repo_root,
    remote_candidate.as_ref(),
    options.verbosity,
    options.quiet,
  )?;
  let apply_request = UploadApplyRequest {
    head: &head,
    bundle: bundle.as_ref(),
    patch: &patch,
    remote_managed: options.sync.remote_managed(),
    remote_url: remote_candidate
      .as_ref()
      .map(|candidate| candidate.url.as_str()),
    force: options.force,
    preference,
    node_bin: &node_bin,
    requires_environment: options.target.environment.is_some(),
  };
  upload_artifacts_and_apply(&remote, apply_request)?;
  Ok(Some(identity))
}

fn read_remote_sync_state(remote: &Remote) -> Result<Option<RemoteSyncState>> {
  let raw = remote.capture_bytes(&format!(
    "cat {}/sync-state.json 2>/dev/null || true",
    remote.meta_dir()
  ))?;
  if raw.is_empty() {
    return Ok(None);
  }
  Ok(serde_json::from_slice::<RemoteSyncState>(&raw).ok())
}

fn remote_sync_is_current(
  state: Option<&RemoteSyncState>,
  head: &str,
  patch: &PatchArchive,
) -> bool {
  state.is_some_and(|state| {
    state.head == head
      && state.patch_sha256 == patch.digest
      && state.checkout_manifest_sha256.is_some()
  })
}

fn sync_paths(options: SyncOptions, remote: Remote) -> Result<()> {
  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    if options.pull {
      eprintln!("pull paths target: {}", options.target_name);
    } else {
      eprintln!("sync paths target: {}", options.target_name);
    }
    for path in &options.paths {
      eprintln!("path: {}", path.display());
    }
  }
  validate_sync_paths(&options.paths)?;
  remote.connect()?;
  let list = if options.pull {
    remote_git_ls_files(&remote, &options.paths)?
  } else {
    git::ls_files(&options.repo_root, &options.paths)?
  };
  if list.is_empty()
    && options.verbosity > 0
    && !options.quiet
    && !(options.pull && options.dry_run)
  {
    eprintln!("no tracked files matched");
  }
  let list_dir = tempfile::Builder::new().prefix("expri-files-").tempdir()?;
  let list_path = list_dir.path().join("files-from");
  fs::write(&list_path, &list)?;
  if options.pull {
    remote.download_files_from(&remote.remote_dir, &options.repo_root, &list_path)
  } else {
    remote.upload_files_from(&options.repo_root, &remote.remote_dir, &list_path)
  }
}

fn remote_git_ls_files(remote: &Remote, paths: &[PathBuf]) -> Result<Vec<u8>> {
  let mut command = format!("cd {} && git ls-files -z --", remote.quoted_remote_dir());
  for path in paths {
    command.push(' ');
    command.push_str(&shell::quote(path.to_string_lossy()));
  }
  remote.capture_bytes(&command)
}

fn validate_sync_paths(paths: &[PathBuf]) -> Result<()> {
  for path in paths {
    if path.is_absolute()
      || path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
      return Err(crate::error::ExpriError::Message(format!(
        "sync path must be relative and stay inside the repo: {}",
        path.display()
      )));
    }
  }
  Ok(())
}

fn pull_target(
  options: SyncOptions,
  remote: Remote,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    eprintln!("pull target: {}", options.target_name);
    eprintln!("repo root: {}", options.repo_root.display());
  }
  remote.connect()?;
  prepare_pull_with_preference(&remote, preference, node_bin)?;

  let local_dir = options
    .repo_root
    .join(".git")
    .join("expri")
    .join(&options.target_name);
  fs::create_dir_all(&local_dir)?;
  let artifacts_path = local_dir.join("pull-artifacts.json");
  let bundle_path = local_dir.join("source.bundle");
  let patch_path = local_dir.join("patch.zip");
  remote.download_file(
    &format!("{}/out/pull-artifacts.json", remote.meta_dir()),
    &artifacts_path,
  )?;
  remote.download_file(
    &format!("{}/out/pull-source.bundle", remote.meta_dir()),
    &bundle_path,
  )?;
  remote.download_file(
    &format!("{}/out/pull-patch.zip", remote.meta_dir()),
    &patch_path,
  )?;
  if options.dry_run {
    eprintln!(
      "+ git -C {} fetch {} +HEAD:refs/remotes/expri/{}/synced",
      options.repo_root.display(),
      bundle_path.display(),
      options.target_name
    );
    return Ok(());
  }

  let artifacts: PullArtifacts = serde_json::from_str(&fs::read_to_string(&artifacts_path)?)?;
  verify_download(
    &bundle_path,
    &artifacts.source_bundle_sha256,
    "source bundle",
  )?;
  verify_download(&patch_path, &artifacts.patch_sha256, "patch")?;
  let ref_name = format!("refs/remotes/expri/{}/synced", options.target_name);
  git::fetch_bundle_to_ref(&options.repo_root, &bundle_path, &ref_name)?;
  if !options.quiet {
    eprintln!(
      "updated refs/remotes/expri/{}/synced to {}",
      options.target_name, artifacts.head
    );
    eprintln!("stored remote patch at {}", patch_path.display());
  }
  Ok(())
}

fn verify_download(path: &Path, expected: &str, label: &str) -> Result<()> {
  let (actual, _) = sha256_file(path)?;
  if actual != expected {
    return Err(crate::error::ExpriError::Message(format!(
      "{label} sha256 mismatch: expected {expected}, got {actual}"
    )));
  }
  Ok(())
}

fn build_source_bundle_for_remote(
  repo_root: &Path,
  remote_candidate: Option<&RemoteCandidate>,
  verbosity: u8,
  quiet: bool,
) -> Result<Option<SourceBundle>> {
  let distance = remote_candidate.and_then(|candidate| candidate.distance);
  if matches!(distance, Some(0)) {
    if verbosity > 0 && !quiet {
      eprintln!("source bundle: skipped; nearest remote already has HEAD");
    }
    return Ok(None);
  }
  let base_commit = remote_candidate.and_then(|candidate| candidate.base_commit.as_deref());
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

struct UploadApplyRequest<'a> {
  head: &'a str,
  bundle: Option<&'a SourceBundle>,
  patch: &'a PatchArchive,
  remote_managed: &'a [String],
  remote_url: Option<&'a str>,
  force: bool,
  preference: ProtocolPreference,
  node_bin: &'a str,
  requires_environment: bool,
}

fn upload_artifacts_and_apply(remote: &Remote, apply: UploadApplyRequest<'_>) -> Result<()> {
  let request_id = request_id(apply.head, &apply.patch.digest);
  let remote_request_dir = format!("{}/inbox/{request_id}", remote.meta_dir());
  remote.execute(&format!("mkdir -p {remote_request_dir}"))?;

  let request_dir = tempfile::Builder::new()
    .prefix("expri-request-")
    .tempdir()?;
  let patch_path = request_dir.path().join("patch.zip");
  fs::copy(&apply.patch.path, &patch_path)?;
  let source_bundle_path = if let Some(bundle) = apply.bundle {
    let path = request_dir.path().join("source.bundle");
    fs::copy(&bundle.path, &path)?;
    Some(path)
  } else {
    None
  };

  let request = SyncApplyRequest {
    head: apply.head.to_string(),
    remote_url: apply.remote_url.map(ToString::to_string),
    source_bundle: source_bundle_path
      .as_ref()
      .map(|_| format!(".expri/inbox/{request_id}/source.bundle")),
    source_bundle_sha256: apply.bundle.map(|bundle| bundle.digest.clone()),
    patch: format!(".expri/inbox/{request_id}/patch.zip"),
    patch_sha256: apply.patch.digest.clone(),
    state_dir: ".expri".to_string(),
    remote_managed: apply.remote_managed.to_vec(),
    force: apply.force,
  };
  let request_path = request_dir.path().join("request.json");
  fs::write(&request_path, serde_json::to_string_pretty(&request)?)?;
  remote.upload_dir(request_dir.path(), &remote_request_dir)?;
  apply_sync_with_preference(
    remote,
    &format!(".expri/inbox/{request_id}/request.json"),
    apply.preference,
    apply.node_bin,
    apply.requires_environment,
  )
}

fn request_id(head: &str, patch_digest: &str) -> String {
  let head_prefix = head.get(..12).unwrap_or(head);
  let patch_prefix = patch_digest.get(..12).unwrap_or(patch_digest);
  let timestamp = utc_timestamp();
  format!("sync-{head_prefix}-{patch_prefix}-{timestamp}")
}

fn utc_timestamp() -> String {
  Utc::now().format("%Y%m%dT%H%M%S%fZ").to_string()
}

fn print_digest(label: &str, digest: &str, size: u64, quiet: bool) {
  if !quiet {
    eprintln!("{label} sha256={digest} size={size} bytes");
  }
}
