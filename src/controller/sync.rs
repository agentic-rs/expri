use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::archive::{PatchArchive, build_patch_archive, sha256_file};
use crate::config::TargetConfig;
use crate::controller::protocol::{ProtocolPreference, apply_sync_with_preference};
use crate::controller::transport::Remote;
use crate::error::Result;
use crate::filter::SyncRules;
use crate::git::{self, RemoteCandidate, SourceBundle};
use crate::protocol::{PullArtifacts, SyncApplyRequest};
use crate::shell;

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

pub fn sync_target(options: SyncOptions) -> Result<()> {
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
  );
  if !options.paths.is_empty() {
    return sync_paths(options, remote);
  }
  if options.pull {
    return pull_target(options, remote, preference, &node_bin);
  }
  if options.verbosity > 0 && !options.quiet {
    if let Some(project_name) = &options.project_name {
      eprintln!("project: {project_name}");
    }
    eprintln!("sync target: {}", options.target_name);
    eprintln!("repo root: {}", options.repo_root.display());
  }

  let _opened_master = remote.open_master()?;
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
    return Ok(());
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
  };
  upload_artifacts_and_apply(&remote, apply_request)?;
  Ok(())
}

fn read_remote_sync_state(remote: &Remote) -> Result<Option<RemoteSyncState>> {
  let raw = remote.ssh_capture_bytes(&format!(
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
  let _opened_master = remote.open_master()?;
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
  remote.ssh_capture_bytes(&command)
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
  let _opened_master = remote.open_master()?;
  prepare_remote_pull(&remote, preference, node_bin)?;

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

fn prepare_remote_pull(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  match preference {
    ProtocolPreference::ExpriNode | ProtocolPreference::Auto => remote.ssh(&format!(
      "cd {} && {} node pull-prepare",
      remote.quoted_remote_dir(),
      shell::quote(node_bin)
    )),
    ProtocolPreference::Ssh => remote.ssh(&format!(
      "cd {} && python3 - <<'PY'\n{}\nPY",
      remote.quoted_remote_dir(),
      pull_prepare_script()
    )),
  }
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

fn pull_prepare_script() -> String {
  r#"import hashlib, json, pathlib, subprocess, zipfile

def sha256(path):
  h = hashlib.sha256()
  with open(path, "rb") as f:
    for chunk in iter(lambda: f.read(1024 * 1024), b""):
      h.update(chunk)
  return h.hexdigest()

out = pathlib.Path(".expri/out")
out.mkdir(parents=True, exist_ok=True)
head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
bundle = out / "pull-source.bundle"
patch = out / "pull-patch.zip"
subprocess.run(["git", "bundle", "create", str(bundle), "HEAD"], check=True)
changed = subprocess.check_output(["git", "diff", "--name-only", "-z", "HEAD", "--"])
untracked = subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard", "-z"])
paths = sorted({p for p in (changed + untracked).decode().split("\0") if p})
with zipfile.ZipFile(patch, "w", compression=zipfile.ZIP_DEFLATED) as archive:
  deleted = []
  for path in paths:
    p = pathlib.Path(path)
    if p.is_file():
      archive.write(p, path)
    else:
      deleted.append(path)
  archive.writestr(".deleted", "".join(f"{path}\n" for path in deleted))
artifacts = {
  "head": head,
  "source_bundle": ".expri/out/pull-source.bundle",
  "source_bundle_sha256": sha256(bundle),
  "patch": ".expri/out/pull-patch.zip",
  "patch_sha256": sha256(patch),
  "state_dir": ".expri",
}
(out / "pull-artifacts.json").write_text(json.dumps(artifacts, indent=2, sort_keys=True))
"#
  .to_string()
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
}

fn upload_artifacts_and_apply(remote: &Remote, apply: UploadApplyRequest<'_>) -> Result<()> {
  let request_id = request_id(apply.head, &apply.patch.digest);
  let remote_request_dir = format!("{}/inbox/{request_id}", remote.meta_dir());
  remote.ssh(&format!("mkdir -p {remote_request_dir}"))?;

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
  )
}

fn request_id(head: &str, patch_digest: &str) -> String {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_nanos())
    .unwrap_or(0);
  let head_prefix = head.get(..12).unwrap_or(head);
  let patch_prefix = patch_digest.get(..12).unwrap_or(patch_digest);
  format!(
    "req-{head_prefix}-{patch_prefix}-{}-{nanos}",
    std::process::id()
  )
}

fn print_digest(label: &str, digest: &str, size: u64, quiet: bool) {
  if !quiet {
    eprintln!("{label} sha256={digest} size={size} bytes");
  }
}
