//! Fetch a project's acknowledged metadata and selected completed objects.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::super::types::*;
use super::super::{FetchOptions, ServiceFetchOptions};
use super::download::{
  ObjectResult, fetch as fetch_run, fetch_objects, fetch_pinned, validate_record,
};
use super::{Api, METADATA, STREAMS, artifacts, fs};
use crate::error::{ExpriError, Result};

#[cfg(test)]
mod tests;

struct CachedRun {
  catalog: Value,
  destination: PathBuf,
  files: BTreeMap<String, std::fs::Metadata>,
  resolved: BTreeMap<String, Label>,
}

#[derive(Clone)]
struct Label {
  path: String,
  size: u64,
  sha256: String,
}

impl CachedRun {
  fn unchanged(&self, catalog: &Value) -> bool {
    self.catalog == *catalog
      && self.files.iter().all(|(path, saved)| {
        fs::optional_regular(&self.destination.join(path)).is_ok()
          && std::fs::symlink_metadata(self.destination.join(path))
            .is_ok_and(|current| fs::unchanged(saved, &current))
      })
  }

  fn capture(catalog: Value, report: &Value, resolved: BTreeMap<String, Label>) -> Result<Self> {
    let destination = PathBuf::from(
      report["destination"]
        .as_str()
        .ok_or_else(|| fs::message("fetch did not return its destination"))?,
    );
    let mut files = BTreeMap::new();
    for path in report["files"]
      .as_array()
      .into_iter()
      .flatten()
      .filter_map(Value::as_str)
      .chain([".pull-owner.json", "pull-state.json"])
    {
      files.insert(
        path.into(),
        std::fs::symlink_metadata(destination.join(path))?,
      );
    }
    Ok(Self {
      catalog,
      destination,
      files,
      resolved,
    })
  }
}

struct Watcher<'a> {
  options: &'a FetchOptions,
  api: Api,
  selected: BTreeSet<String>,
  origins: BTreeSet<String>,
  cache: BTreeMap<(String, String), CachedRun>,
  task: Option<ObjectTask>,
  verified: BTreeMap<(String, String, String), VerifiedObject>,
  candidates: Vec<(RunScope, BTreeMap<String, FileRecord>)>,
  last_scheduled: Option<(String, String)>,
}

struct ObjectTask {
  scope: RunScope,
  worker: std::thread::JoinHandle<Result<ObjectResult>>,
}

struct VerifiedObject {
  record: FileRecord,
  metadata: std::fs::Metadata,
  path: PathBuf,
}

fn permanent_rejection(error: &ExpriError) -> Option<&'static str> {
  match error {
    ExpriError::ServiceRejected {
      status: 401 | 403, ..
    } => Some("fetch authorization was rejected; check the service token and origin"),
    ExpriError::ServiceRejected { status: 410, .. } => {
      Some("this project was deleted; saved local files were retained")
    }
    _ => None,
  }
}

fn failed(error: &ExpriError, phase: &'static str) -> Result<Value> {
  if let Some(reason) = permanent_rejection(error) {
    return Err(fs::message(reason));
  }
  if matches!(error, ExpriError::DownloadChanged) {
    return Ok(json!({"status":"pending", "phase":phase,
      "message":"Selected checkpoint changed; waiting for its registered file record."}));
  }
  // Provider responses and signed URLs must never enter watcher output.
  let status = match error {
    ExpriError::ServiceRejected { status, .. } => Some(*status),
    _ => None,
  };
  Ok(
    json!({"status":"retrying", "phase":phase, "status_code":status,
    "message":"Saved local files and transfer progress were retained; fetch will retry."}),
  )
}

impl<'a> Watcher<'a> {
  fn new(options: &'a FetchOptions) -> Result<Self> {
    validate_component(&options.project_id)?;
    if options.origins.is_empty() || options.origins.len() > 64 {
      return Err(fs::message(
        "select between 1 and 64 explicit service origins",
      ));
    }
    for origin in &options.origins {
      validate_component(origin)?;
    }
    if options.labels.len() > 2
      || options
        .labels
        .iter()
        .any(|label| !matches!(label.as_str(), "best" | "latest"))
    {
      return Err(fs::message("checkpoint labels must be best or latest"));
    }
    if options.artifacts.len() + options.labels.len() > 64 {
      return Err(fs::message(
        "select at most 64 artifact paths and checkpoint labels",
      ));
    }
    Ok(Self {
      options,
      api: Api::new(&options.config)?,
      selected: artifacts(&options.artifacts)?,
      origins: options.origins.iter().cloned().collect(),
      cache: BTreeMap::new(),
      task: None,
      verified: BTreeMap::new(),
      candidates: Vec::new(),
      last_scheduled: None,
    })
  }

  fn catalog(&self, scope: &RunScope) -> Result<BTreeMap<String, FileRecord>> {
    let Response::Files { files } = self.api.request(&Request::ListFiles {
      scope: scope.clone(),
    })?
    else {
      return Err(fs::message("service did not return a file catalog"));
    };
    if files.len() > 1000 {
      return Err(fs::message("service file catalog exceeds its size limit"));
    }
    let mut records = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for record in files {
      validate_record(&record)?;
      let FileTarget::Run {
        scope: returned,
        path,
      } = &record.target
      else {
        return Err(fs::message(
          "run catalog unexpectedly contains private inputs",
        ));
      };
      if returned != scope || !seen.insert(path.clone()) {
        return Err(fs::message(
          "run catalog contains inconsistent file records",
        ));
      }
      records.insert(path.clone(), record);
    }
    Ok(records)
  }

  fn run(&mut self, scope: &RunScope) -> Result<Value> {
    if self.options.watch && !self.options.dry_run {
      self.run_live(scope)
    } else {
      self.run_sequential(scope)
    }
  }

  fn run_failed(
    &mut self,
    scope: &RunScope,
    error: &ExpriError,
    phase: &'static str,
  ) -> Result<Value> {
    if !matches!(error, ExpriError::ServiceRejected { status: 410, .. }) {
      return failed(error, phase);
    }
    // A run can expire between catalog selection and a download. Probe only
    // the scoped catalog so project deletion and auth failures remain fatal.
    let scopes = match self.api.request(&Request::ListRuns {
      project_id: scope.project_id.clone(),
      origin: scope.origin.clone(),
    }) {
      Ok(Response::Runs { runs }) if runs.len() <= 1000 => runs,
      Ok(_) => return Err(fs::message("service did not return a bounded run catalog")),
      Err(error) => return failed(&error, "run_catalog"),
    };
    let mut seen = BTreeSet::new();
    for returned in &scopes {
      validate_scope(returned)?;
      if returned.project_id != scope.project_id
        || returned.origin != scope.origin
        || !seen.insert(&returned.run_id)
      {
        return Err(fs::message(
          "service returned a run from another origin or a duplicate run",
        ));
      }
    }
    if scopes.iter().any(|returned| returned == scope) {
      return Ok(
        json!({"status":"retrying", "phase":phase, "status_code":410,
        "message":"Run availability changed; saved local files were retained. Fetch will check again."}),
      );
    }
    self
      .cache
      .remove(&(scope.origin.clone(), scope.run_id.clone()));
    self
      .verified
      .retain(|(origin, run_id, _), _| origin != &scope.origin || run_id != &scope.run_id);
    self.candidates.retain(|(candidate, _)| candidate != scope);
    Ok(json!({"status":"unavailable", "phase":phase,
      "message":"This run is no longer hosted; saved local files were retained."}))
  }

  fn run_sequential(&mut self, scope: &RunScope) -> Result<Value> {
    let records = match self.catalog(scope) {
      Ok(records) => records,
      Err(error) => return self.run_failed(scope, &error, "file_catalog"),
    };
    let key = (scope.origin.clone(), scope.run_id.clone());
    let resolved = self
      .cache
      .get(&key)
      .map(|cache| cache.resolved.clone())
      .unwrap_or_default();
    let catalog = self.fingerprint(&records, &resolved)?;
    let (artifacts, pending) = self.selection(&records, &resolved);
    if !records.contains_key("run-state.json") {
      return Ok(json!({"status":"pending", "pending_files":pending,
        "message":"Run metadata has not been published yet."}));
    }
    if self.options.dry_run {
      return Ok(json!({"status":"planned", "available_files":artifacts,
        "pending_files":pending.iter().filter(|path|!path.starts_with("label:")).collect::<Vec<_>>(),
        "requested_labels":self.options.labels,"must_read_inventory":!self.options.labels.is_empty()}));
    }
    if let Some(cached) = self.cache.get(&key)
      && cached.unchanged(&catalog)
    {
      return Ok(
        json!({"status":if pending.is_empty() {"downloaded"} else {"pending"},
        "destination":cached.destination, "pending_files":pending, "downloaded_bytes":0}),
      );
    }
    let first = match self.fetch_selected(scope, artifacts.clone(), &records) {
      Ok(report) => report,
      Err(error) => return self.run_failed(scope, &error, "download"),
    };
    let resolved = if self.options.labels.is_empty() {
      BTreeMap::new()
    } else {
      match self.resolve_labels(&PathBuf::from(first["destination"].as_str().unwrap())) {
        Ok(resolved) => resolved,
        Err(error) => return self.run_failed(scope, &error, "checkpoint_labels"),
      }
    };
    let (selected, pending) = self.selection(&records, &resolved);
    let mut report = if selected == artifacts {
      first.clone()
    } else {
      match self.fetch_selected(scope, selected, &records) {
        Ok(report) => report,
        Err(error) => return self.run_failed(scope, &error, "download"),
      }
    };
    if report != first {
      for field in ["downloaded_bytes", "resumed_bytes", "reused_bytes"] {
        report[field] = json!(
          report[field]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(first[field].as_u64().unwrap_or(0))
        );
      }
    }
    let catalog = self.fingerprint(&records, &resolved)?;
    self
      .cache
      .insert(key, CachedRun::capture(catalog, &report, resolved)?);
    if self.cache.len() > 1000 {
      self.cache.pop_first();
    }
    Ok(
      json!({"status":if pending.is_empty() {"downloaded"} else {"pending"},
      "destination":report["destination"], "pending_files":pending,
      "downloaded_bytes":report["downloaded_bytes"], "resumed_bytes":report["resumed_bytes"],
      "reused_bytes":report["reused_bytes"]}),
    )
  }

  fn run_live(&mut self, scope: &RunScope) -> Result<Value> {
    let records = match self.catalog(scope) {
      Ok(records) => records,
      Err(error) => return self.run_failed(scope, &error, "file_catalog"),
    };
    if !records.contains_key("run-state.json") {
      return Ok(json!({"status":"pending","message":"Run metadata has not been published yet."}));
    }
    let key = (scope.origin.clone(), scope.run_id.clone());
    let metadata = serde_json::to_value(
      records
        .iter()
        .filter(|(path, _)| METADATA.contains(&path.as_str()) || STREAMS.contains(&path.as_str()))
        .collect::<BTreeMap<_, _>>(),
    )?;
    let mut downloaded_bytes = 0;
    if !self
      .cache
      .get(&key)
      .is_some_and(|cache| cache.unchanged(&metadata))
    {
      let report = match self.fetch(scope, Vec::new()) {
        Ok(report) => report,
        Err(error) => return self.run_failed(scope, &error, "metadata_download"),
      };
      downloaded_bytes = report["downloaded_bytes"].as_u64().unwrap_or(0);
      let resolved =
        match self.resolve_labels(&PathBuf::from(report["destination"].as_str().unwrap())) {
          Ok(resolved) => resolved,
          Err(error) => return self.run_failed(scope, &error, "checkpoint_labels"),
        };
      let mut cached = CachedRun::capture(metadata, &report, resolved)?;
      // Checkpoint publication merges this receipt independently of metadata.
      cached.files.remove("pull-state.json");
      self.cache.insert(key.clone(), cached);
      if self.cache.len() > 1000 {
        self.cache.pop_first();
      }
    }
    let Some(cached) = self.cache.get(&key) else {
      return Ok(json!({"status":"pending"}));
    };
    let destination = cached.destination.clone();
    let (selected, pending) = self.selection(&records, &cached.resolved);
    let mut queued = Vec::new();
    for path in selected {
      let record = &records[&path];
      let verified = self
        .verified
        .get(&(scope.origin.clone(), scope.run_id.clone(), path.clone()));
      let unchanged = verified.is_some_and(|verified| {
        verified.record.target == record.target
          && verified.record.size == record.size
          && verified.record.sha256 == record.sha256
          && fs::optional_regular(&verified.path).is_ok()
          && std::fs::symlink_metadata(&verified.path)
            .is_ok_and(|metadata| fs::unchanged(&metadata, &verified.metadata))
      });
      if !unchanged {
        queued.push(path);
      }
    }
    if !queued.is_empty() {
      self.candidates.push((
        scope.clone(),
        queued
          .iter()
          .map(|path| (path.clone(), records[path].clone()))
          .collect(),
      ));
    }
    let active = self.task.as_ref().is_some_and(|task| task.scope == *scope);
    Ok(
      json!({"status":if active {"downloading"} else if !queued.is_empty() {"queued"}
      else if !pending.is_empty() {"pending"} else {"downloaded"},
      "destination":destination,"pending_files":pending,"queued_files":queued,
      "downloaded_bytes":downloaded_bytes}),
    )
  }

  fn accept_objects(&mut self, scope: &RunScope, result: ObjectResult) -> Result<u64> {
    let destination = PathBuf::from(
      result.report["destination"]
        .as_str()
        .ok_or_else(|| fs::message("checkpoint download has no destination"))?,
    );
    for (record, metadata) in result.files {
      let FileTarget::Run {
        scope: returned,
        path,
      } = &record.target
      else {
        return Err(fs::message("checkpoint result contains an input"));
      };
      if returned != scope {
        return Err(fs::message("checkpoint result belongs to another run"));
      }
      let cached_path = destination.join(path);
      self.verified.insert(
        (scope.origin.clone(), scope.run_id.clone(), path.clone()),
        VerifiedObject {
          record,
          metadata,
          path: cached_path,
        },
      );
    }
    Ok(result.report["downloaded_bytes"].as_u64().unwrap_or(0))
  }

  fn schedule_objects(&mut self) {
    if self.task.is_some() || self.candidates.is_empty() {
      return;
    }
    self.candidates.sort_by(|first, second| {
      (&first.0.origin, &first.0.run_id).cmp(&(&second.0.origin, &second.0.run_id))
    });
    let next = self
      .candidates
      .iter()
      .position(|(scope, _)| {
        self
          .last_scheduled
          .as_ref()
          .is_none_or(|last| (&scope.origin, &scope.run_id) > (&last.0, &last.1))
      })
      .unwrap_or(0);
    let (scope, expected) = self.candidates.swap_remove(next);
    let options = ServiceFetchOptions {
      config: self.options.config.clone(),
      project_id: scope.project_id.clone(),
      origin: scope.origin.clone(),
      run_id: scope.run_id.clone(),
      repo: self.options.repo.clone(),
      results_dir: self.options.results_dir.clone(),
      source: None,
      artifacts: expected.keys().cloned().collect(),
    };
    self.last_scheduled = Some((scope.origin.clone(), scope.run_id.clone()));
    self.task = Some(ObjectTask {
      scope,
      worker: thread::spawn(move || fetch_objects(options, expected)),
    });
  }

  fn fetch(&self, scope: &RunScope, artifacts: Vec<String>) -> Result<Value> {
    fetch_run(self.fetch_options(scope, artifacts))
  }

  fn fetch_selected(
    &self,
    scope: &RunScope,
    artifacts: Vec<String>,
    records: &BTreeMap<String, FileRecord>,
  ) -> Result<Value> {
    let expected = artifacts
      .iter()
      .map(|path| (path.clone(), records[path].clone()))
      .collect();
    fetch_pinned(self.fetch_options(scope, artifacts), &expected)
  }

  fn fetch_options(&self, scope: &RunScope, artifacts: Vec<String>) -> ServiceFetchOptions {
    ServiceFetchOptions {
      config: self.options.config.clone(),
      project_id: scope.project_id.clone(),
      origin: scope.origin.clone(),
      run_id: scope.run_id.clone(),
      repo: self.options.repo.clone(),
      results_dir: self.options.results_dir.clone(),
      source: None,
      artifacts,
    }
  }

  fn fingerprint(
    &self,
    records: &BTreeMap<String, FileRecord>,
    resolved: &BTreeMap<String, Label>,
  ) -> Result<Value> {
    let files = records
      .iter()
      .filter(|(path, _)| {
        self.selected.contains(*path)
          || resolved.values().any(|selected| selected.path == **path)
          || METADATA.contains(&path.as_str())
          || STREAMS.contains(&path.as_str())
      })
      .collect::<BTreeMap<_, _>>();
    serde_json::to_value(files).map_err(Into::into)
  }

  fn selection(
    &self,
    records: &BTreeMap<String, FileRecord>,
    resolved: &BTreeMap<String, Label>,
  ) -> (Vec<String>, Vec<String>) {
    let available = |path: &String| {
      records
        .get(path)
        .is_some_and(|record| matches!(record.storage, FileStorage::Object))
    };
    let mut selected = self
      .selected
      .iter()
      .filter(|path| available(path))
      .cloned()
      .collect::<BTreeSet<_>>();
    let mut pending = self
      .selected
      .iter()
      .filter(|path| !available(path))
      .cloned()
      .collect::<Vec<_>>();
    for label in &self.options.labels {
      if let Some(checkpoint) = resolved.get(label).filter(|checkpoint| {
        records.get(&checkpoint.path).is_some_and(|record| {
          matches!(record.storage, FileStorage::Object)
            && record.size == checkpoint.size
            && record.sha256.as_deref() == Some(checkpoint.sha256.as_str())
        })
      }) {
        selected.insert(checkpoint.path.clone());
      } else {
        pending.push(format!("label:{label}"));
      }
    }
    (selected.into_iter().collect(), pending)
  }

  fn resolve_labels(&self, destination: &std::path::Path) -> Result<BTreeMap<String, Label>> {
    let path = destination.join(crate::run_artifacts::INVENTORY_PATH);
    if fs::inspect(&path)?.is_none() {
      return Ok(BTreeMap::new());
    }
    let inventory: crate::run_artifacts::Inventory = serde_json::from_slice(&fs::read_bounded(
      &path,
      crate::run_artifacts::INVENTORY_LIMIT as u64,
    )?)?;
    if inventory.files.len() > crate::run_artifacts::FILE_LIMIT {
      return Err(fs::message(
        "checkpoint label inventory exceeds its size limit",
      ));
    }
    let mut resolved = BTreeMap::new();
    for artifact in inventory.files {
      crate::run_artifacts::validate_path(&artifact.path)?;
      if artifact.sync_status.as_deref() != Some("cloud") {
        continue;
      }
      let Some(digest) = &artifact.sha256 else {
        continue;
      };
      super::validate_digest(digest)?;
      for label in artifact
        .labels
        .iter()
        .filter(|label| self.options.labels.contains(label))
      {
        if resolved
          .insert(
            label.clone(),
            Label {
              path: artifact.path.clone(),
              size: artifact.size,
              sha256: digest.clone(),
            },
          )
          .is_some()
        {
          return Err(fs::message(
            "checkpoint label identifies more than one file",
          ));
        }
      }
    }
    Ok(resolved)
  }

  fn cycle(&mut self) -> Result<Value> {
    self.candidates.clear();
    let mut runs = Vec::new();
    let mut seen = BTreeSet::new();
    let mut successful_origins = BTreeSet::new();
    let mut pending = 0;
    let mut retrying = 0;
    let mut count = 0;
    let mut downloaded_bytes = 0u64;
    if self
      .task
      .as_ref()
      .is_some_and(|task| task.worker.is_finished())
    {
      let task = self.task.take().unwrap();
      match task.worker.join() {
        Ok(Ok(result)) => {
          downloaded_bytes = self.accept_objects(&task.scope, result)?;
        }
        Ok(Err(error)) => {
          let mut report = self.run_failed(&task.scope, &error, "checkpoint_download")?;
          report["scope"] = json!(task.scope);
          retrying += usize::from(report["status"] == "retrying");
          pending += usize::from(report["status"] == "pending");
          runs.push(report);
        }
        Err(_) => {
          runs.push(json!({"scope":task.scope,"status":"retrying",
          "message":"Checkpoint transfer stopped; saved progress was retained."}));
          retrying += 1;
        }
      }
    }
    let mut truncated = false;
    for origin in self.origins.clone() {
      let scopes = match self.api.request(&Request::ListRuns {
        project_id: self.options.project_id.clone(),
        origin: origin.clone(),
      }) {
        Ok(Response::Runs { runs }) if runs.len() <= 1000 => runs,
        Ok(_) => return Err(fs::message("service did not return a bounded run catalog")),
        Err(error) => {
          let mut error = failed(&error, "run_catalog")?;
          error["origin"] = json!(origin);
          runs.push(error);
          retrying += 1;
          continue;
        }
      };
      successful_origins.insert(origin.clone());
      for scope in scopes {
        validate_scope(&scope)?;
        let key = (scope.origin.clone(), scope.run_id.clone());
        if scope.project_id != self.options.project_id
          || scope.origin != origin
          || !seen.insert(key)
        {
          return Err(fs::message(
            "service returned a run from another origin or a duplicate run",
          ));
        }
        let mut report = self.run(&scope)?;
        downloaded_bytes =
          downloaded_bytes.saturating_add(report["downloaded_bytes"].as_u64().unwrap_or(0));
        pending += usize::from(matches!(
          report["status"].as_str(),
          Some("pending" | "queued" | "downloading")
        ));
        retrying += usize::from(report["status"] == "retrying");
        count += usize::from(report["status"] != "unavailable");
        report["scope"] = json!(scope);
        if runs.len() < 200 {
          runs.push(report);
          if serde_json::to_vec(&runs)?.len() > 64 * 1024 {
            runs.pop();
            truncated = true;
          }
        } else {
          truncated = true;
        }
      }
    }
    self
      .cache
      .retain(|key, _| !successful_origins.contains(&key.0) || seen.contains(key));
    if self.options.watch && !self.options.dry_run {
      self.schedule_objects();
    }
    if let Some(task) = &self.task {
      for report in &mut runs {
        if report["scope"] == json!(task.scope) {
          report["status"] = json!("downloading");
        }
      }
    }
    Ok(
      json!({"project_id":self.options.project_id, "dry_run":self.options.dry_run,
      "status":if retrying > 0 {"retrying"} else if pending > 0 {"pending"} else {"fetched"},
      "run_count":count, "pending_runs":pending, "retrying_runs":retrying, "downloaded_bytes":downloaded_bytes,
      "active_transfers":usize::from(self.task.is_some()),
      "runs":runs, "runs_truncated":truncated}),
    )
  }
}

pub fn fetch(options: FetchOptions) -> Result<Value> {
  let mut watcher = Watcher::new(&options)?;
  let mut last_report = None;
  let mut failures = 0;
  loop {
    let report = watcher.cycle()?;
    if !options.watch || options.dry_run {
      if report["retrying_runs"].as_u64().unwrap_or(0) > 0 {
        return Err(fs::message(
          "fetch could not finish; saved local files and progress were retained. Rerun the command or use --watch to retry automatically.",
        ));
      }
      return Ok(report);
    }
    failures = if report["status"] == "retrying" {
      failures + 1
    } else {
      0
    };
    let summary = json!({"project_id":report["project_id"], "status":report["status"],
      "run_count":report["run_count"], "pending_runs":report["pending_runs"],
      "retrying_runs":report["retrying_runs"], "downloaded_bytes":report["downloaded_bytes"],
      "active_transfers":report["active_transfers"]});
    if !options.quiet && last_report.as_ref() != Some(&summary) {
      println!("{}", serde_json::to_string(&summary)?);
      last_report = Some(summary);
    }
    thread::sleep(Duration::from_secs(poll_delay(failures)));
  }
}

fn poll_delay(failures: u32) -> u64 {
  (5 * (1u64 << failures.min(4))).min(60)
}
