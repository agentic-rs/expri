//! Workspace-facing commands for small asset sidecars and cached data.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::assets::{self, Asset, Descriptor, Source};
use crate::error::{ExpriError, Result};

#[derive(Debug, Args)]
pub struct AssetsCommand {
  #[command(subcommand)]
  command: AssetsSubcommand,
}

#[derive(Debug, Subcommand)]
enum AssetsSubcommand {
  /// Download a source and create a Git-tracked .expri.toml sidecar.
  Import {
    source: String,
    path: PathBuf,
    #[command(flatten)]
    options: Options,
  },
  /// Prepare the exact versions recorded in asset sidecars.
  Download {
    path: Option<PathBuf>,
    /// Explicitly replace local bytes that differ from the sidecar.
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    options: Options,
  },
  /// Resolve the source again and update the sidecar to its current version.
  Update {
    path: PathBuf,
    #[command(flatten)]
    options: Options,
  },
  /// Verify workspace files against their sidecars without contacting sources.
  Status {
    path: Option<PathBuf>,
    #[command(flatten)]
    options: Options,
  },
}

#[derive(Debug, Args)]
struct Options {
  /// Project checkout; defaults to the current Git root or directory.
  #[arg(long)]
  repo: Option<PathBuf>,
  /// Local expri client configuration for private inputs; never stored in sidecars.
  #[arg(long)]
  client_config: Option<PathBuf>,
  #[arg(long)]
  json: bool,
}

pub fn run(command: AssetsCommand, target: Option<&str>, quiet: bool) -> Result<()> {
  if target.is_some() {
    return Err(message(
      "assets commands run in a local checkout; run them on the worker to manage its workspace",
    ));
  }
  let (report, json_output) = match command.command {
    AssetsSubcommand::Import {
      source,
      path,
      options,
    } => {
      let root = repo_root(options.repo.as_deref())?;
      let config = client_config(&root, options.client_config.as_deref())?;
      let source = parse_source(&source, config.as_deref())?;
      (
        import(&root, &source, &path, config.as_deref())?,
        options.json,
      )
    }
    AssetsSubcommand::Download {
      path,
      force,
      options,
    } => {
      let root = repo_root(options.repo.as_deref())?;
      let config = client_config(&root, options.client_config.as_deref())?;
      let mut reports = Vec::new();
      for asset in selection(&root, path.as_deref())? {
        let source =
          assets::download::ensure(&root, &asset.descriptor, config.as_deref(), &mut || {
            Ok(false)
          })?;
        let _lease = checkout_lease(&root)?;
        ensure_descriptor_current(&root, &asset)?;
        let reused = matches_file(&root.join(&asset.path), &asset.descriptor)?;
        if !reused {
          if assets::inspect(&root.join(&asset.path))?.is_some() && !force {
            return Err(message(
              "local asset differs from its sidecar; preserve your edits elsewhere, or use assets download --force to replace it",
            ));
          }
          bind_workspace(&root, &asset.path, &source, &asset.descriptor)?;
        }
        reports.push(json!({"path": asset.path, "size": asset.descriptor.size, "sha256": asset.descriptor.sha256, "reused": reused}));
      }
      (json!({"assets": reports}), options.json)
    }
    AssetsSubcommand::Update { path, options } => {
      let root = repo_root(options.repo.as_deref())?;
      let config = client_config(&root, options.client_config.as_deref())?;
      let asset = selection(&root, Some(&path))?.remove(0);
      let mut source = asset.descriptor.source.clone();
      if let Source::HuggingFace {
        revision,
        requested_revision,
        ..
      } = &mut source
      {
        *revision = requested_revision
          .clone()
          .unwrap_or_else(|| revision.clone());
      }
      let descriptor =
        assets::download::resolve(&root, &source, config.as_deref(), &mut || Ok(false))?;
      let _lease = checkout_lease(&root)?;
      ensure_descriptor_current(&root, &asset)?;
      if assets::inspect(&root.join(&path))?.is_some()
        && !matches_file(&root.join(&path), &asset.descriptor)?
      {
        return Err(message(
          "local asset has changed; preserve your edits elsewhere before updating its source",
        ));
      }
      publish_workspace_asset(
        &root,
        &path,
        &assets::cache_file(&root, &descriptor.sha256),
        &descriptor,
        Some(&asset.descriptor),
        &mut || Ok(()),
      )?;
      (
        json!({"path": path, "sidecar": asset.sidecar, "changed": descriptor != asset.descriptor, "size": descriptor.size, "sha256": descriptor.sha256}),
        options.json,
      )
    }
    AssetsSubcommand::Status { path, options } => {
      let root = repo_root(options.repo.as_deref())?;
      let reports = selection(&root, path.as_deref())?.into_iter().map(|asset| {
        let status = if assets::inspect(&root.join(&asset.path))?.is_none() {
          "missing"
        } else if matches_file(&root.join(&asset.path), &asset.descriptor)? {
          "ready"
        } else {
          "modified"
        };
        Ok(json!({"path": asset.path, "sidecar": asset.sidecar, "status": status, "size": asset.descriptor.size, "sha256": asset.descriptor.sha256}))
      }).collect::<Result<Vec<_>>>()?;
      (json!({"assets": reports}), options.json)
    }
  };
  if json_output || !quiet {
    println!("{}", serde_json::to_string_pretty(&report)?);
  }
  Ok(())
}

fn repo_root(explicit: Option<&Path>) -> Result<PathBuf> {
  let path = if let Some(path) = explicit {
    path.to_path_buf()
  } else {
    let git = Command::new("git")
      .args(["rev-parse", "--show-toplevel"])
      .output()?;
    if git.status.success() {
      PathBuf::from(
        String::from_utf8(git.stdout)
          .map_err(|_| message("checkout path must be UTF-8"))?
          .trim(),
      )
    } else {
      std::env::current_dir()?
    }
  };
  let root = fs::canonicalize(path)?;
  assets::directory(&root)?;
  Ok(root)
}

fn client_config(root: &Path, explicit: Option<&Path>) -> Result<Option<PathBuf>> {
  if let Some(path) = explicit {
    return Ok(Some(std::path::absolute(path)?));
  }
  let config = root.join("expri.toml");
  if config.exists() {
    return Ok(
      crate::config::Config::load(&config)?
        .local_service()?
        .map(|service| service.client_config),
    );
  }
  Ok(None)
}

fn parse_source(value: &str, client_config: Option<&Path>) -> Result<Source> {
  let source = if let Some(value) = value.strip_prefix("hf://") {
    let (repo_type, value) = if let Some(value) = value.strip_prefix("datasets/") {
      ("dataset", value)
    } else {
      ("model", value.strip_prefix("models/").unwrap_or(value))
    };
    let mut parts = value.splitn(3, '/');
    let namespace = parts.next().unwrap_or("");
    let repository = parts.next().unwrap_or("");
    let filename = parts.next().unwrap_or("");
    let (repository, revision) = repository.split_once('@').unwrap_or((repository, "main"));
    Source::HuggingFace {
      repo_id: format!("{namespace}/{repository}"),
      repo_type: repo_type.into(),
      filename: filename.into(),
      revision: revision.into(),
      requested_revision: Some(revision.into()),
    }
  } else if value.starts_with("expri://") {
    let parsed =
      reqwest::Url::parse(value).map_err(|_| message("invalid private asset reference"))?;
    if !parsed.username().is_empty()
      || parsed.password().is_some()
      || parsed.query().is_some()
      || parsed.fragment().is_some()
    {
      return Err(message(
        "private asset references must not contain credentials, query parameters, or fragments",
      ));
    }
    let host = parsed
      .host_str()
      .ok_or_else(|| message("private asset reference requires a server"))?;
    let authority = host.to_owned();
    let authority = parsed
      .port()
      .map_or_else(|| authority.clone(), |port| format!("{authority}:{port}"));
    let parts = parsed
      .path()
      .trim_start_matches('/')
      .split('/')
      .collect::<Vec<_>>();
    if parts.len() != 3 || parts[1] != "inputs" {
      return Err(message(
        "private asset reference must be expri://server/project/inputs/input-id",
      ));
    }
    let mut url = format!("https://{authority}/");
    if let Some(config) = client_config {
      let config = fs::canonicalize(config)?;
      let mut bytes = Vec::new();
      assets::open(&config)?
        .take(32 * 1024 + 1)
        .read_to_end(&mut bytes)?;
      if bytes.len() > 32 * 1024 {
        return Err(message("service client config exceeds its size limit"));
      }
      let text =
        std::str::from_utf8(&bytes).map_err(|_| message("service client config must be UTF-8"))?;
      let config: crate::service::types::ClientConfig =
        toml::from_str(text).map_err(|_| message("service client configuration is invalid"))?;
      let configured =
        reqwest::Url::parse(&config.url).map_err(|_| message("invalid client service URL"))?;
      if configured.host_str() != Some(host) || configured.port() != parsed.port() {
        return Err(message(
          "client configuration points at a different asset server",
        ));
      }
      url = configured.to_string();
    }
    Source::Expri {
      url,
      project_id: parts[0].into(),
      input_id: parts[2].into(),
    }
  } else {
    Source::Url { url: value.into() }
  };
  source.validate()?;
  Ok(source)
}

fn selection(root: &Path, path: Option<&Path>) -> Result<Vec<Asset>> {
  let assets = assets::discover(root)?;
  if let Some(path) = path {
    assets::validate_asset_path(path)?;
    let asset = assets
      .into_iter()
      .find(|asset| asset.path == path)
      .ok_or_else(|| message("asset sidecar is missing; import the asset first"))?;
    Ok(vec![asset])
  } else {
    Ok(assets)
  }
}

fn checkout_lease(root: &Path) -> Result<crate::lock::FileLock> {
  assets::directories(&root.join(".expri"))?;
  crate::lock::worktree_lock(&root.join(".expri"))
}

fn import(root: &Path, source: &Source, path: &Path, config: Option<&Path>) -> Result<Value> {
  assets::validate_asset_path(path)?;
  assets::ensure_parent(root, path)?;
  let sidecar = assets::sidecar_path(path);
  assets::optional_regular(&root.join(&sidecar))?;
  if assets::inspect(&root.join(path))?.is_some()
    || assets::inspect(&root.join(&sidecar))?.is_some()
  {
    return Err(message(
      "asset path or sidecar already exists; use assets update for an existing asset",
    ));
  }
  let tracked = Command::new("git")
    .current_dir(root)
    .args(["--literal-pathspecs", "ls-files", "--error-unmatch", "--"])
    .arg(path)
    .output()?;
  if tracked.status.success() {
    return Err(message(
      "asset bytes are tracked by Git; remove the path from the Git index before importing it",
    ));
  }
  check_import_assets(root, path)?;
  let descriptor = assets::download::resolve(root, source, config, &mut || Ok(false))?;
  let _lease = checkout_lease(root)?;
  assets::ensure_parent(root, path)?;
  assets::optional_regular(&root.join(&sidecar))?;
  if assets::inspect(&root.join(path))?.is_some()
    || assets::inspect(&root.join(&sidecar))?.is_some()
  {
    return Err(message(
      "asset path changed during import; existing files were preserved",
    ));
  }
  check_import_assets(root, path)?;
  // Ignore the data before publishing a descriptor or workspace binding.
  ignore(root, path)?;
  publish_workspace_asset(
    root,
    path,
    &assets::cache_file(root, &descriptor.sha256),
    &descriptor,
    None,
    &mut || Ok(()),
  )?;
  Ok(
    json!({"path": path, "sidecar": sidecar, "source": descriptor.source, "size": descriptor.size, "sha256": descriptor.sha256}),
  )
}

fn check_import_assets(root: &Path, path: &Path) -> Result<()> {
  let existing = assets::discover(root)?;
  if existing.len() >= assets::MAX_ASSETS {
    return Err(message("a project supports at most 64 asset descriptors"));
  }
  let sidecar = assets::sidecar_path(path);
  for asset in existing {
    if asset.path.starts_with(path)
      || path.starts_with(&asset.path)
      || asset.sidecar.starts_with(path)
      || sidecar.starts_with(&asset.path)
    {
      return Err(message(
        "asset path overlaps another managed asset or descriptor",
      ));
    }
  }
  Ok(())
}

fn ensure_descriptor_current(root: &Path, asset: &Asset) -> Result<()> {
  if assets::load(&root.join(&asset.sidecar))? != asset.descriptor {
    return Err(message(
      "asset sidecar changed during download; retry using its current version",
    ));
  }
  Ok(())
}

fn matches_file(path: &Path, descriptor: &Descriptor) -> Result<bool> {
  assets::optional_regular(path)?;
  let Some(metadata) = assets::inspect(path)? else {
    return Ok(false);
  };
  if metadata.len() != descriptor.size {
    return Ok(false);
  }
  let (digest, size) = crate::archive::sha256_file(path)?;
  Ok(size == descriptor.size && digest == descriptor.sha256)
}

fn bind_workspace(root: &Path, path: &Path, source: &Path, descriptor: &Descriptor) -> Result<()> {
  assets::ensure_parent(root, path)?;
  let destination = root.join(path);
  let parent = destination.parent().expect("asset has a parent");
  let temporary = tempfile::Builder::new()
    .prefix(crate::filter::ASSET_STAGING_PREFIX)
    .tempdir_in(parent)?;
  let binding = temporary.path().join("file");
  assets::bind_verified(source, &binding, descriptor, &mut || Ok(false))?;
  assets::optional_regular(&destination)?;
  fs::rename(&binding, &destination)?;
  assets::sync_directory(parent)
}

/// Prepare both files first, retaining old inodes until the new sidecar commits.
fn publish_workspace_asset(
  root: &Path,
  path: &Path,
  source: &Path,
  descriptor: &Descriptor,
  previous: Option<&Descriptor>,
  before_commit: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
  assets::ensure_parent(root, path)?;
  let destination = root.join(path);
  let sidecar = root.join(assets::sidecar_path(path));
  let parent = destination.parent().expect("asset has a parent");
  assets::optional_regular(&sidecar)?;
  let temporary = tempfile::Builder::new()
    .prefix(crate::filter::ASSET_STAGING_PREFIX)
    .tempdir_in(parent)?;
  let staged_descriptor = temporary.path().join("descriptor");
  let staged_binding = temporary.path().join("binding");
  // A descriptor write or binding failure cannot change the workspace.
  assets::save(&staged_descriptor, descriptor)?;
  assets::bind_verified(source, &staged_binding, descriptor, &mut || Ok(false))?;
  let descriptor_metadata = assets::open(&staged_descriptor)?.metadata()?;
  let binding_metadata = assets::open(&staged_binding)?.metadata()?;
  let old_binding = temporary.path().join("previous-binding");
  let old_sidecar = temporary.path().join("previous-descriptor");
  let had_binding = assets::inspect(&destination)?.is_some();
  if let Some(previous) = previous {
    if assets::load(&sidecar)? != *previous {
      return Err(message(
        "asset sidecar changed during update; existing files were preserved",
      ));
    }
    if had_binding && !matches_file(&destination, previous)? {
      return Err(message(
        "local asset changed during update; existing files were preserved",
      ));
    }
    fs::hard_link(&sidecar, &old_sidecar)?;
  } else if had_binding || assets::inspect(&sidecar)?.is_some() {
    return Err(message(
      "asset path changed during import; existing files were preserved",
    ));
  }
  if had_binding {
    fs::hard_link(&destination, &old_binding)?;
  }
  fs::rename(&staged_binding, &destination)?;
  let mut descriptor_installed = false;
  let commit = (|| {
    before_commit()?;
    assets::optional_regular(&sidecar)?;
    if let Some(previous) = previous {
      if assets::load(&sidecar)? != *previous {
        return Err(message("asset sidecar changed during update"));
      }
      fs::rename(&staged_descriptor, &sidecar)?;
    } else {
      // Import must never replace a descriptor created by another writer.
      fs::hard_link(&staged_descriptor, &sidecar)?;
    }
    descriptor_installed = true;
    assets::sync_directory(parent)
  })();
  if let Err(error) = commit {
    let rollback = (|| {
      restore_owned_path(
        &destination,
        &binding_metadata,
        had_binding.then_some(old_binding.as_path()),
      )?;
      if descriptor_installed {
        restore_owned_path(
          &sidecar,
          &descriptor_metadata,
          previous.map(|_| old_sidecar.as_path()),
        )?;
      }
      assets::sync_directory(parent)
    })();
    if let Err(rollback) = rollback {
      return Err(message(format!(
        "{error}; asset rollback failed: {rollback}"
      )));
    }
    return Err(error);
  }
  Ok(())
}

fn restore_owned_path(
  path: &Path,
  installed: &fs::Metadata,
  previous: Option<&Path>,
) -> Result<()> {
  assets::optional_regular(path)?;
  if !assets::inspect(path)?.is_some_and(|current| assets::unchanged(&current, installed)) {
    return Err(message(
      "asset path changed after publication; existing files were preserved",
    ));
  }
  if let Some(previous) = previous {
    fs::rename(previous, path)?;
  } else {
    fs::remove_file(path)?;
  }
  Ok(())
}

fn ignore(root: &Path, path: &Path) -> Result<()> {
  let ignore_path = root.join(".gitignore");
  assets::optional_regular(&ignore_path)?;
  let mut contents = if ignore_path.exists() {
    fs::read_to_string(&ignore_path)?
  } else {
    String::new()
  };
  let entry = format!("/{}", ignore_pattern(path)?);
  if !contents.is_empty() && !contents.ends_with('\n') {
    contents.push('\n');
  }
  if !contents
    .lines()
    .any(|line| matches!(line, ".expri/" | "/.expri/"))
  {
    contents.push_str("/.expri/\n");
  }
  let staging_entry = format!("{}*/", crate::filter::ASSET_STAGING_PREFIX);
  if !contents.lines().any(|line| line == staging_entry) {
    contents.push_str(&staging_entry);
    contents.push('\n');
  }
  let sidecar = assets::sidecar_path(path);
  // Reopen only the required directory chain, preserving exclusion of its
  // siblings. Git cannot see a negated file beneath an ignored directory.
  let mut parent = PathBuf::new();
  if let Some(directory) = path.parent() {
    for component in directory.components() {
      parent.push(component);
      if git_ignored(root, &parent)? {
        let pattern = ignore_pattern(&parent)?;
        contents.push_str(&format!("!/{pattern}/\n/{pattern}/*\n"));
      }
    }
  }
  if !contents.lines().any(|line| line == entry) {
    contents.push_str(&entry);
    contents.push('\n');
  }
  let descriptor_entry = format!("!/{}", ignore_pattern(&sidecar)?);
  if !contents.lines().any(|line| line == descriptor_entry) {
    contents.push_str(&descriptor_entry);
    contents.push('\n');
  }
  let mut temporary = tempfile::NamedTempFile::new_in(root)?;
  temporary.write_all(contents.as_bytes())?;
  temporary.as_file().sync_all()?;
  assets::optional_regular(&ignore_path)?;
  temporary
    .persist(ignore_path)
    .map_err(|error| error.error)?;
  assets::sync_directory(root)?;
  if git_ignored(root, &sidecar)? {
    return Err(message(
      "asset sidecar remains ignored by nested Git rules; allow its .expri.toml path before retrying import",
    ));
  }
  Ok(())
}

fn ignore_pattern(path: &Path) -> Result<String> {
  let raw = path
    .to_str()
    .ok_or_else(|| message("asset path must be UTF-8"))?;
  Ok(
    raw
      .chars()
      .flat_map(|character| {
        if matches!(character, '*' | '?' | '[' | ']' | '\\' | ' ') {
          vec!['\\', character]
        } else {
          vec![character]
        }
      })
      .collect(),
  )
}

fn git_ignored(root: &Path, path: &Path) -> Result<bool> {
  let output = Command::new("git")
    .current_dir(root)
    .args(["check-ignore", "--no-index", "--quiet", "--"])
    .arg(path)
    .output()?;
  match output.status.code() {
    Some(0) => Ok(true),
    Some(1) => Ok(false),
    Some(128) if String::from_utf8_lossy(&output.stderr).contains("not a git repository") => {
      Ok(false)
    }
    _ => Err(message("cannot check Git asset ignore rules")),
  }
}

fn message(value: impl Into<String>) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests;
