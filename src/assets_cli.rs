//! Workspace-facing commands for small asset sidecars and cached data.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::assets::{self, Asset, Descriptor, Source};
use crate::error::{ExpriError, Result};
use crate::output::print_report;

mod context;
use context::{client_config, parse_output, parse_source, project_context};

#[derive(Debug, Args)]
pub struct AssetsCommand {
  #[command(subcommand)]
  command: AssetsSubcommand,
}

#[derive(Debug, Subcommand)]
enum AssetsSubcommand {
  /// Track a local file, or add a URL and its destination path.
  Add {
    source: String,
    path: Option<PathBuf>,
    /// Record a pinned source without downloading its file bytes.
    #[arg(long)]
    no_download: bool,
    /// Known content digest for a source without trustworthy checksum metadata.
    #[arg(long, requires = "size", requires = "no_download")]
    sha256: Option<String>,
    /// Exact byte size accompanying --sha256.
    #[arg(long, requires = "sha256")]
    size: Option<u64>,
    #[command(flatten)]
    options: Options,
  },
  /// Make a hosted run output a persistent asset without downloading it.
  Import {
    source: String,
    path: PathBuf,
    /// Explicit private input ID; otherwise the server allocates one idempotently.
    #[arg(long)]
    input_id: Option<String>,
    #[command(flatten)]
    options: Options,
  },
  /// Upload exact tracked bytes and record their private availability.
  Upload {
    path: Option<PathBuf>,
    /// Explicit private input ID; requires selecting one asset.
    #[arg(long, requires = "path")]
    input_id: Option<String>,
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
  /// Print the complete report as JSON instead of a human-readable table.
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
    AssetsSubcommand::Add {
      source,
      path,
      no_download,
      sha256,
      size,
      options,
    } => {
      let root = repo_root(options.repo.as_deref())?;
      let config = client_config(&root, options.client_config.as_deref())?;
      let report = if let Some(path) = path {
        let source = parse_source(&source)?;
        let identity = sha256
          .zip(size)
          .map(|(sha256, size)| assets::Identity { size, sha256 });
        add_source(
          &root,
          &source,
          &path,
          config.as_deref(),
          no_download,
          identity.as_ref(),
        )?
      } else {
        if no_download || sha256.is_some() || size.is_some() || source.contains("://") {
          return Err(message(
            "adding a URL requires a destination path; --no-download applies to URL sources",
          ));
        }
        add_local(&root, Path::new(&source))?
      };
      (report, options.json)
    }
    AssetsSubcommand::Import {
      source,
      path,
      input_id,
      options,
    } => {
      let root = repo_root(options.repo.as_deref())?;
      let config = client_config(&root, options.client_config.as_deref())?;
      (
        import_output(
          &root,
          &source,
          &path,
          input_id.as_deref(),
          config.as_deref(),
        )?,
        options.json,
      )
    }
    AssetsSubcommand::Upload {
      path,
      input_id,
      options,
    } => {
      let root = repo_root(options.repo.as_deref())?;
      let config = client_config(&root, options.client_config.as_deref())?;
      let mut reports = Vec::new();
      for asset in selection(&root, path.as_deref())? {
        reports.push(upload(
          &root,
          &asset,
          input_id.as_deref(),
          config.as_deref(),
        )?);
      }
      (json!({"assets": reports}), options.json)
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
      if matches!(asset.descriptor.source, Source::Local) {
        let descriptor = assets::download::adopt(&root, &root.join(&path), &mut || Ok(false))?;
        let _lease = checkout_lease(&root)?;
        ensure_descriptor_current(&root, &asset)?;
        if !matches_file(&root.join(&path), &descriptor)? {
          return Err(message(
            "local asset changed while updating; retry after writing finishes",
          ));
        }
        let mut descriptor = descriptor;
        if descriptor.sha256 == asset.descriptor.sha256 && descriptor.size == asset.descriptor.size
        {
          descriptor.availability = asset.descriptor.availability.clone();
          descriptor.producer = asset.descriptor.producer.clone();
        }
        publish_descriptor(&root, &path, &descriptor, Some(&asset.descriptor))?;
        let report = asset_report(&path, &descriptor);
        (
          json!({"path": report["path"], "sidecar": report["sidecar"], "changed": descriptor != asset.descriptor, "size": descriptor.size, "sha256": descriptor.sha256}),
          options.json,
        )
      } else {
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
        let mut descriptor =
          assets::download::resolve(&root, &source, config.as_deref(), &mut || Ok(false))?;
        if descriptor.sha256 == asset.descriptor.sha256 && descriptor.size == asset.descriptor.size
        {
          descriptor.availability = asset.descriptor.availability.clone();
          descriptor.producer = asset.descriptor.producer.clone();
        }
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
        let availability = if matches!(asset.descriptor.source, Source::Local) && asset.descriptor.availability.is_none() { "local_only" } else { "downloadable" };
        Ok(json!({"path": asset.path, "sidecar": asset.sidecar, "status": status, "availability": availability, "size": asset.descriptor.size, "sha256": asset.descriptor.sha256, "producer": asset.descriptor.producer}))
      }).collect::<Result<Vec<_>>>()?;
      (json!({"assets": reports}), options.json)
    }
  };
  if json_output || !quiet {
    print_report(&report, json_output)?;
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

fn selection(root: &Path, path: Option<&Path>) -> Result<Vec<Asset>> {
  let assets = assets::discover(root)?;
  if let Some(path) = path {
    assets::validate_asset_path(path)?;
    let asset = assets
      .into_iter()
      .find(|asset| asset.path == path)
      .ok_or_else(|| message("asset sidecar is missing; add the asset first"))?;
    Ok(vec![asset])
  } else {
    Ok(assets)
  }
}

fn checkout_lease(root: &Path) -> Result<crate::lock::FileLock> {
  assets::directories(&root.join(".expri"))?;
  crate::lock::worktree_lock(&root.join(".expri"))
}

fn ensure_new_asset(root: &Path, path: &Path, local: bool) -> Result<()> {
  assets::validate_asset_path(path)?;
  assets::ensure_parent(root, path)?;
  let sidecar = assets::sidecar_path(path);
  assets::optional_regular(&root.join(&sidecar))?;
  if (!local && assets::inspect(&root.join(path))?.is_some())
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
      "asset bytes are tracked by Git; remove the path from the Git index before adding it",
    ));
  }
  check_import_assets(root, path)?;
  Ok(())
}

fn add_source(
  root: &Path,
  source: &Source,
  path: &Path,
  config: Option<&Path>,
  no_download: bool,
  identity: Option<&assets::Identity>,
) -> Result<Value> {
  ensure_new_asset(root, path, false)?;
  let descriptor = if no_download {
    assets::download::resolve_metadata(root, source, identity, config, &mut || Ok(false))?
  } else {
    assets::download::resolve(root, source, config, &mut || Ok(false))?
  };
  let _lease = checkout_lease(root)?;
  ensure_new_asset(root, path, false)?;
  // Ignore the data before publishing a descriptor or workspace binding.
  ignore(root, path)?;
  if no_download {
    publish_descriptor(root, path, &descriptor, None)?;
  } else {
    publish_workspace_asset(
      root,
      path,
      &assets::cache_file(root, &descriptor.sha256),
      &descriptor,
      None,
      &mut || Ok(()),
    )?;
  }
  Ok(asset_report(path, &descriptor))
}

fn add_local(root: &Path, path: &Path) -> Result<Value> {
  ensure_new_asset(root, path, true)?;
  let descriptor = assets::download::adopt(root, &root.join(path), &mut || Ok(false))?;
  let _lease = checkout_lease(root)?;
  ensure_new_asset(root, path, true)?;
  if !matches_file(&root.join(path), &descriptor)? {
    return Err(message(
      "local asset changed while adding; retry after writing finishes",
    ));
  }
  ignore(root, path)?;
  publish_descriptor(root, path, &descriptor, None)?;
  Ok(asset_report(path, &descriptor))
}

fn import_output(
  root: &Path,
  source: &str,
  path: &Path,
  input_id: Option<&str>,
  config: Option<&Path>,
) -> Result<Value> {
  let producer = parse_output(source)?;
  assets::validate_asset_path(path)?;
  assets::ensure_parent(root, path)?;
  let sidecar = root.join(assets::sidecar_path(path));
  assets::optional_regular(&sidecar)?;
  let previous = if assets::inspect(&sidecar)?.is_some() {
    let previous = assets::load(&sidecar)?;
    if previous.producer.as_ref() != Some(&producer) {
      return Err(message("asset sidecar already exists for another source"));
    }
    Some(previous)
  } else {
    ensure_new_asset(root, path, false)?;
    None
  };
  let (config, project_id) = project_context(config)?;
  if let Some(previous) = &previous {
    let existing_id = private_input_id(previous)
      .ok_or_else(|| message("existing asset has no persistent private input"))?;
    if input_id.is_some_and(|requested| requested != existing_id) {
      return Err(message(
        "existing asset has a different private input ID; use a new destination path",
      ));
    }
    let target = expri_core::service::FileTarget::Input {
      project_id,
      input_id: existing_id.into(),
    };
    let record = expri_client::file_metadata(config, &target)?;
    verify_remote_identity(&record, previous)?;
    if record
      .provenance
      .as_ref()
      .map(|provenance| &provenance.producer)
      != previous.producer.as_ref()
    {
      return Err(message(
        "persistent asset has different producing-run provenance",
      ));
    }
    let _lease = checkout_lease(root)?;
    if assets::load(&sidecar)? != *previous {
      return Err(message(
        "asset sidecar changed during import; retry using its current version",
      ));
    }
    let mut report = asset_report(path, previous);
    report["reused"] = json!(true);
    return Ok(report);
  }
  let scope = expri_core::service::RunScope {
    project_id,
    origin: producer.origin.clone(),
    run_id: producer.run_id.clone(),
  };
  let target = expri_core::service::FileTarget::Run {
    scope: scope.clone(),
    path: producer.path.clone(),
  };
  let source = expri_client::file_metadata(config, &target)?;
  if !matches!(source.storage, expri_core::service::FileStorage::Object) {
    return Err(message("import requires a finalized, uploaded run output"));
  }
  let sha256 = source
    .sha256
    .as_deref()
    .ok_or_else(|| message("run output has no content digest"))?;
  let record = expri_client::import_output(
    config,
    &scope,
    &producer.path,
    input_id,
    source.size,
    sha256,
  )?;
  let expri_core::service::FileTarget::Input { input_id, .. } = record.target else {
    return Err(message("server returned an invalid imported asset"));
  };
  let descriptor = Descriptor {
    version: assets::DESCRIPTOR_VERSION,
    source: Source::Expri { input_id },
    size: record.size,
    sha256: record
      .sha256
      .ok_or_else(|| message("imported asset has no content digest"))?,
    availability: None,
    producer: record.provenance.map(|provenance| provenance.producer),
  };
  descriptor.validate()?;
  let _lease = checkout_lease(root)?;
  ensure_new_asset(root, path, false)?;
  ignore(root, path)?;
  publish_descriptor(root, path, &descriptor, None)?;
  let mut report = asset_report(path, &descriptor);
  report["reused"] = json!(false);
  Ok(report)
}

fn upload(
  root: &Path,
  asset: &Asset,
  input_id: Option<&str>,
  config: Option<&Path>,
) -> Result<Value> {
  let (config, project_id) = project_context(config)?;
  if assets::inspect(&root.join(&asset.path))?.is_some()
    && !matches_file(&root.join(&asset.path), &asset.descriptor)?
  {
    return Err(message(
      "local asset differs from its descriptor; update it explicitly before uploading",
    ));
  }
  let existing_id = private_input_id(&asset.descriptor);
  let input_id = input_id
    .or(existing_id)
    .map(str::to_owned)
    .unwrap_or_else(|| format!("file-{}", asset.descriptor.sha256));
  expri_core::service::validate_component(&input_id)?;
  let reused = if let Some(existing_id) = existing_id {
    let source = expri_core::service::FileTarget::Input {
      project_id: project_id.clone(),
      input_id: existing_id.into(),
    };
    let record = expri_client::file_metadata(config, &source)?;
    verify_remote_identity(&record, &asset.descriptor)?;
    if existing_id != input_id {
      let target = expri_core::service::FileTarget::Input {
        project_id: project_id.clone(),
        input_id: input_id.clone(),
      };
      expri_client::client::reference(expri_client::ReferenceOptions {
        config: config.to_path_buf(),
        source: serde_json::to_string(&source)?,
        target: serde_json::to_string(&target)?,
        size: asset.descriptor.size,
        sha256: asset.descriptor.sha256.clone(),
      })?;
      let record = expri_client::file_metadata(config, &target)?;
      verify_remote_identity(&record, &asset.descriptor)?;
    }
    true
  } else {
    let cached =
      assets::download::ensure(root, &asset.descriptor, Some(config), &mut || Ok(false))?;
    let report = expri_client::client::input_upload(expri_client::InputUploadOptions {
      config: config.to_path_buf(),
      project_id,
      input_id: input_id.clone(),
      file: cached,
      queue_dir: root.join(".expri/publish"),
    })?;
    if report["size"].as_u64() != Some(asset.descriptor.size)
      || report["sha256"].as_str() != Some(asset.descriptor.sha256.as_str())
    {
      return Err(message(
        "uploaded bytes do not match the asset descriptor; local metadata was preserved",
      ));
    }
    false
  };
  let mut descriptor = asset.descriptor.clone();
  descriptor.availability = if matches!(&descriptor.source, Source::Expri { input_id: source_id } if source_id == &input_id)
  {
    None
  } else {
    Some(assets::ExpriAvailability {
      input_id: input_id.clone(),
    })
  };
  let _lease = checkout_lease(root)?;
  ensure_descriptor_current(root, asset)?;
  publish_descriptor(root, &asset.path, &descriptor, Some(&asset.descriptor))?;
  let mut report = asset_report(&asset.path, &descriptor);
  report["input_id"] = json!(input_id);
  report["reused"] = json!(reused);
  Ok(report)
}

fn private_input_id(descriptor: &Descriptor) -> Option<&str> {
  descriptor
    .availability
    .as_ref()
    .map(|availability| availability.input_id.as_str())
    .or(match &descriptor.source {
      Source::Expri { input_id } => Some(input_id.as_str()),
      _ => None,
    })
}

fn verify_remote_identity(
  record: &expri_core::service::FileRecord,
  descriptor: &Descriptor,
) -> Result<()> {
  if record.size != descriptor.size
    || record.sha256.as_deref() != Some(descriptor.sha256.as_str())
    || !matches!(record.storage, expri_core::service::FileStorage::Object)
  {
    return Err(message("persistent asset no longer matches its descriptor"));
  }
  if descriptor.producer.as_ref().is_some_and(|expected| {
    record
      .provenance
      .as_ref()
      .map(|provenance| &provenance.producer)
      != Some(expected)
  }) {
    return Err(message(
      "persistent asset has different producing-run provenance",
    ));
  }
  Ok(())
}

fn asset_report(path: &Path, descriptor: &Descriptor) -> Value {
  json!({"path": path, "sidecar": assets::sidecar_path(path), "source": descriptor.source, "size": descriptor.size, "sha256": descriptor.sha256, "availability": descriptor.availability, "producer": descriptor.producer})
}

fn publish_descriptor(
  root: &Path,
  path: &Path,
  descriptor: &Descriptor,
  previous: Option<&Descriptor>,
) -> Result<()> {
  assets::ensure_parent(root, path)?;
  let sidecar = root.join(assets::sidecar_path(path));
  assets::optional_regular(&sidecar)?;
  let parent = sidecar.parent().expect("sidecar has a parent");
  let temporary = tempfile::Builder::new()
    .prefix(crate::filter::ASSET_STAGING_PREFIX)
    .tempdir_in(parent)?;
  let staged = temporary.path().join("descriptor");
  assets::save(&staged, descriptor)?;
  if let Some(previous) = previous {
    if assets::load(&sidecar)? != *previous {
      return Err(message(
        "asset sidecar changed during publication; retry using its current version",
      ));
    }
    fs::rename(&staged, &sidecar)?;
  } else {
    fs::hard_link(&staged, &sidecar)?;
  }
  assets::sync_directory(parent)
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
