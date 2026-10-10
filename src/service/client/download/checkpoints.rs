//! Independent object lane: no run-cache lease is held during network transfers.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
  Api, FileRecord, FileStorage, FileTarget, LockAttempt, PullOptions, Request, Response, Result,
  artifacts, cache_owner, downloaded_files, fs, message, publish, staged_download, staging,
  validate_record, validate_scope, verify_expected_objects,
};

pub(in crate::service::client) struct ObjectResult {
  pub(in crate::service::client) report: Value,
  pub(in crate::service::client) files: Vec<(FileRecord, std::fs::Metadata)>,
}

pub(in crate::service::client) fn pull_objects(
  options: PullOptions,
  expected: BTreeMap<String, FileRecord>,
) -> Result<ObjectResult> {
  let api = Api::new(&options.config)?;
  let scope = super::RunScope {
    project_id: options.project_id,
    origin: options.origin,
    run_id: options.run_id,
  };
  validate_scope(&scope)?;
  let artifacts = artifacts(&options.artifacts)?;
  let Response::Files { files } = api.request(&Request::ListFiles {
    scope: scope.clone(),
  })?
  else {
    return Err(message("service did not return a file catalog"));
  };
  if files.len() > 1000 {
    return Err(message("service file catalog exceeds its size limit"));
  }
  let mut selected = BTreeMap::new();
  let mut seen = std::collections::BTreeSet::new();
  for file in files {
    validate_record(&file)?;
    let FileTarget::Run {
      scope: returned,
      path,
    } = &file.target
    else {
      return Err(message("run catalog unexpectedly contains private inputs"));
    };
    if returned != &scope || !seen.insert(path.clone()) {
      return Err(message("run catalog contains inconsistent file records"));
    }
    if artifacts.contains(path) && matches!(file.storage, FileStorage::Object) {
      selected.insert(path.clone(), file);
    }
  }
  verify_expected_objects(&selected, &artifacts, &expected)?;
  let source = options
    .source
    .unwrap_or_else(|| format!("service-{}-{}", scope.project_id, scope.origin));
  let results_dir = options
    .results_dir
    .to_str()
    .ok_or_else(|| message("results directory must be UTF-8"))?;
  let runs_dir = crate::controller::run_pull::cached_runs_dir(&options.repo, results_dir, &source)?;
  let destination = runs_dir.join(&scope.run_id);
  let owner =
    json!({"schema_version":1,"service_endpoint":api.endpoint,"source":source,"scope":scope});
  // The metadata lane must create and validate the run cache first.
  cache_owner(&destination, &owner)?;
  let mut staging = staging::Staging::open(
    runs_dir
      .parent()
      .unwrap()
      .join(".service-pull/checkpoints")
      .join(&scope.run_id),
    &owner,
  )?;
  staging.select(&selected)?;
  let mut resumed_bytes = 0u64;
  let mut downloaded_bytes = 0u64;
  let mut reused_bytes = 0u64;
  for (path, record) in &selected {
    fs::optional_regular(&destination.join(path))?;
    if staging.reuse(path, record, &destination.join(path))? {
      reused_bytes = reused_bytes.saturating_add(record.size);
    } else {
      let (resumed, downloaded) = staged_download(&api, &mut staging, path, record)?;
      resumed_bytes = resumed_bytes.saturating_add(resumed);
      downloaded_bytes = downloaded_bytes.saturating_add(downloaded);
    }
    let Response::File { file: current } = api.request(&Request::GetFile {
      target: record.target.clone(),
    })?
    else {
      return Err(message("service did not confirm its checkpoint record"));
    };
    validate_record(&current)?;
    if current.target != record.target
      || current.size != record.size
      || current.sha256 != record.sha256
      || !matches!(current.storage, FileStorage::Object)
    {
      staging.reset(path)?;
      return Err(crate::error::ExpriError::DownloadChanged);
    }
  }
  let LockAttempt::Acquired(_publication) =
    crate::lock::try_lock_file(&destination.join(".pull.lock"), true)?
  else {
    return Err(message(
      "metadata is publishing this run cache; checkpoint progress was retained",
    ));
  };
  cache_owner(&destination, &owner)?;
  let receipt_path = destination.join("pull-state.json");
  let mut receipt: Value = serde_json::from_slice(&fs::read_bounded(&receipt_path, 256 * 1024)?)?;
  let mut catalog = BTreeMap::new();
  for available in receipt["available_files"]
    .as_array()
    .into_iter()
    .flatten()
    .take(200)
  {
    let Some(path) = available["path"].as_str() else {
      continue;
    };
    if crate::run_artifacts::validate_path(path).is_err() {
      continue;
    }
    let (Some(size), Some(digest)) = (available["size"].as_u64(), available["sha256"].as_str())
    else {
      continue;
    };
    if super::validate_digest(digest).is_err() {
      continue;
    }
    catalog.insert(
      path.to_string(),
      FileRecord {
        target: FileTarget::Run {
          scope: scope.clone(),
          path: path.into(),
        },
        size,
        sha256: Some(digest.into()),
        storage: FileStorage::Object,
      },
    );
  }
  for (path, record) in &selected {
    staging.ensure_verified(path)?;
    fs::directories(destination.join(path).parent().unwrap())?;
    publish(&staging.path(path), &destination.join(path))?;
    catalog.insert(path.clone(), record.clone());
  }
  receipt["downloaded_files"] = json!(downloaded_files(&destination, &catalog, &selected)?);
  // Merge only the newly verified object records into the latest metadata receipt.
  let mut available = Vec::new();
  for (path, record) in &selected {
    available.push(json!({"path":path,"size":record.size,"sha256":record.sha256}));
    if serde_json::to_vec(&available)?.len() > 64 * 1024 {
      available.pop();
      receipt["available_files_truncated"] = json!(true);
      break;
    }
  }
  for file in receipt["available_files"].as_array().into_iter().flatten() {
    if file["path"]
      .as_str()
      .is_some_and(|path| selected.contains_key(path))
    {
      continue;
    }
    available.push(file.clone());
    if available.len() > 200 || serde_json::to_vec(&available)?.len() > 64 * 1024 {
      available.pop();
      receipt["available_files_truncated"] = json!(true);
      break;
    }
  }
  receipt["available_files"] = json!(available);
  receipt["checkpoints_pulled_at"] = json!(chrono::Utc::now().to_rfc3339());
  fs::atomic_json(&receipt_path, &receipt)?;
  let mut files = Vec::new();
  for (path, record) in &selected {
    staging.ensure_verified(path)?;
    files.push((
      record.clone(),
      std::fs::symlink_metadata(destination.join(path))?,
    ));
  }
  staging.clear()?;
  Ok(ObjectResult {
    report: json!({"scope":scope,"destination":destination,"files":selected.keys().collect::<Vec<_>>(),
    "records":selected.values().collect::<Vec<_>>(),"downloaded_bytes":downloaded_bytes,
    "resumed_bytes":resumed_bytes,"reused_bytes":reused_bytes}),
    files,
  })
}
