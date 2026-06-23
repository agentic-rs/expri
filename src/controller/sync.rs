use std::path::{Path, PathBuf};

use crate::archive::{PatchArchive, build_patch_archive};
use crate::config::TargetConfig;
use crate::controller::transport::Remote;
use crate::error::Result;
use crate::filter::SyncRules;
use crate::git::{self, SourceBundle};
use crate::node;
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
  pub verbosity: u8,
  pub quiet: bool,
}

pub fn sync_target(options: SyncOptions) -> Result<()> {
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
  let dirty = git::dirty_paths(&options.repo_root, &options.sync)?;
  let patch = build_patch_archive(&options.repo_root, &dirty)?;
  print_digest("patch zip", &patch.digest, patch.size, options.quiet);
  if options.verbosity > 0 && !options.quiet {
    eprintln!(
      "patch zip file count: {} deleted={}",
      patch.file_count, patch.deleted_count
    );
  }

  if !options.force
    && remote_synced_head(&remote)? == head
    && remote_patch_digest(&remote)? == patch.digest
  {
    if options.verbosity > 0 && !options.quiet {
      eprintln!("remote workspace is current");
    }
    return Ok(());
  }

  sync_bundle(
    &remote,
    &options.repo_root,
    &head,
    options.verbosity,
    options.quiet,
  )?;
  checkout_synced_commit(&remote, &head)?;
  apply_patch_archive(&remote, &patch)?;
  Ok(())
}

fn sync_bundle(
  remote: &Remote,
  repo_root: &Path,
  head: &str,
  verbosity: u8,
  quiet: bool,
) -> Result<()> {
  let meta = remote.meta_dir();
  let git_dir = remote.git_dir();
  remote.ssh(&format!("mkdir -p {meta}"))?;
  remote.ssh(&format!("test -d {git_dir} || git init --bare {git_dir}"))?;

  if remote_has_commit(remote, head)? {
    if verbosity > 0 && !quiet {
      eprintln!("remote already has commit: {head}");
    }
    remote.ssh(&format!(
      "git --git-dir {git_dir} update-ref refs/heads/synced {}",
      shell::quote(head)
    ))?;
    return Ok(());
  }

  let base_commit = match remote_synced_head(remote)? {
    commit if !commit.is_empty() && git::local_has_commit(repo_root, &commit)? => Some(commit),
    _ => None,
  };
  if verbosity > 0 && !quiet {
    match &base_commit {
      Some(commit) => eprintln!("source bundle refspec: {commit}..HEAD"),
      None => eprintln!("source bundle refspec: HEAD"),
    }
  }

  let bundle = git::build_source_bundle(repo_root, base_commit.as_deref())?;
  print_digest("source bundle", &bundle.digest, bundle.size, quiet);
  upload_bundle(remote, &bundle)?;
  Ok(())
}

fn upload_bundle(remote: &Remote, bundle: &SourceBundle) -> Result<()> {
  let meta = remote.meta_dir();
  remote.upload_file(&bundle.path, &format!("{meta}/source.bundle"))?;
  remote.ssh(&format!(
    "cd {} && git --git-dir .expri/git fetch .expri/source.bundle +HEAD:refs/heads/synced && printf %s {} > .expri/source.bundle.sha256",
    remote.quoted_remote_dir(),
    shell::quote(&bundle.digest)
  ))
}

fn checkout_synced_commit(remote: &Remote, head: &str) -> Result<()> {
  remote.ssh(&format!(
    "cd {} && git --git-dir .expri/git --work-tree . checkout -f {}",
    remote.quoted_remote_dir(),
    shell::quote(head)
  ))
}

fn apply_patch_archive(remote: &Remote, patch: &PatchArchive) -> Result<()> {
  let meta = remote.meta_dir();
  remote.upload_file(&patch.path, &format!("{meta}/patch.zip"))?;
  let script = node::sync::patch_apply_script(&patch.digest);
  remote.ssh(&format!(
    "cd {} && python3 - <<'PY'\n{script}\nPY",
    remote.quoted_remote_dir(),
  ))
}

fn remote_has_commit(remote: &Remote, commit: &str) -> Result<bool> {
  Ok(
    remote.ssh_capture(&format!(
      "git --git-dir {} cat-file -e {}^{{commit}} 2>/dev/null && echo yes || true",
      remote.git_dir(),
      shell::quote(commit)
    ))?
      == "yes",
  )
}

fn remote_synced_head(remote: &Remote) -> Result<String> {
  remote.ssh_capture(&format!(
    "git --git-dir {} rev-parse refs/heads/synced 2>/dev/null || true",
    remote.git_dir()
  ))
}

fn remote_patch_digest(remote: &Remote) -> Result<String> {
  remote.ssh_capture(&format!(
    "cat {}/patch.sha256 2>/dev/null || true",
    remote.meta_dir()
  ))
}

fn print_digest(label: &str, digest: &str, size: u64, quiet: bool) {
  if !quiet {
    eprintln!("{label} sha256={digest} size={size} bytes");
  }
}
