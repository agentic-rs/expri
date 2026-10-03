mod download;
mod fs;
mod http;
mod queue;
#[cfg(test)]
mod tests;
mod upload;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::types::*;
use super::{InputPutOptions, PushOptions};
use crate::error::Result;
use fs::message;
use http::Api;
use queue::{Queue, SavedFile};
use upload::{sync_file, sync_stream};

pub use download::{input_get, pull};

const RECORD_LIMIT: u64 = 16 * 1024 * 1024;
const OBJECT_BATCH: u64 = 8 * 1024 * 1024;
const METADATA: [&str; 4] = [
  "snapshot.json",
  "environment/environment-state.json",
  "outputs/params.json",
  "run-state.json",
];
const STREAMS: [&str; 3] = [
  "outputs/metrics.jsonl",
  "logs/stdout.log",
  "logs/stderr.log",
];

fn terminal(state: &Value) -> bool {
  matches!(
    state.get("status").and_then(Value::as_str),
    Some("completed" | "failed" | "cancelled")
  )
}

pub fn push(options: PushOptions) -> Result<Value> {
  let api = Api::new(&options.config)?;
  let run_dir = std::path::absolute(&options.run_dir)?;
  fs::directory(&run_dir)?;
  let state: Value = serde_json::from_slice(&fs::read_bounded(
    &run_dir.join("run-state.json"),
    RECORD_LIMIT,
  )?)?;
  let run_id = state
    .get("run_id")
    .and_then(Value::as_str)
    .ok_or_else(|| message("run state has no run_id"))?
    .to_string();
  let scope = RunScope {
    project_id: options.project_id,
    origin: options.origin,
    run_id,
  };
  validate_scope(&scope)?;
  let artifacts = artifacts(&options.artifacts)?;
  let queue_dir = std::path::absolute(options.queue_dir)?
    .join("runs")
    .join(&scope.project_id)
    .join(&scope.origin)
    .join(&scope.run_id);
  let owner = json!({"endpoint": api.endpoint, "scope": scope});
  let mut queue = Queue::new(queue_dir, owner)?;
  let mut last_error = String::new();
  let mut failures = 0u64;
  loop {
    match push_cycle(
      &api,
      &mut queue,
      &scope,
      &run_dir,
      &artifacts,
      options.watch,
    ) {
      Ok(done) if !options.watch || done => {
        return Ok(json!({
          "scope": scope, "terminal": done, "files": queue.state.files.keys().collect::<Vec<_>>(),
          "stream_offsets": queue.state.streams, "queue_dir": queue.directory,
        }));
      }
      Ok(_) => {
        last_error.clear();
        failures = 0;
      }
      Err(error) if !options.watch => return Err(error),
      Err(error) => {
        let detail: String = error
          .to_string()
          .chars()
          .filter(|character| !character.is_control())
          .take(512)
          .collect();
        if last_error != detail || failures.is_multiple_of(15) {
          eprintln!("Service sync pending; saved work will retry: {detail}");
          last_error = detail;
        }
        failures += 1;
      }
    }
    thread::sleep(Duration::from_secs(2));
  }
}

fn artifacts(values: &[String]) -> Result<BTreeSet<String>> {
  if values.len() > 64 {
    return Err(message("select at most 64 explicit artifacts"));
  }
  values
    .iter()
    .map(|path| {
      validate_run_path(path)?;
      if !path.starts_with("outputs/") {
        return Err(message(
          "explicit service artifacts must be files under outputs/",
        ));
      }
      Ok(path.clone())
    })
    .collect()
}

fn push_cycle(
  api: &Api,
  queue: &mut Queue,
  scope: &RunScope,
  run_dir: &Path,
  artifacts: &BTreeSet<String>,
  watch: bool,
) -> Result<bool> {
  fs::directory(run_dir)?;
  let state: Value = serde_json::from_slice(&fs::read_bounded(
    &run_dir.join("run-state.json"),
    RECORD_LIMIT,
  )?)?;
  if state.get("run_id").and_then(Value::as_str) != Some(scope.run_id.as_str()) {
    return Err(message("run identity changed while synchronizing"));
  }
  let done = terminal(&state);
  if !done && !artifacts.is_empty() && !watch {
    return Err(message(
      "explicit output artifacts require a completed, failed or cancelled run",
    ));
  }
  if fs::inspect(&run_dir.join("snapshot.json"))?.is_none() {
    return Err(message("run snapshot metadata is missing"));
  }
  for path in METADATA.iter().filter(|path| **path != "run-state.json") {
    let source = run_dir.join(path);
    if fs::inspect(&source)?.is_some() {
      sync_file(api, queue, path, run_target(scope, path), &source, true)?;
    }
  }
  for path in STREAMS {
    let source = run_dir.join(path);
    if fs::inspect(&source)?.is_some() {
      if !queue
        .state
        .files
        .get(path)
        .is_some_and(|saved| saved.upload.complete)
      {
        sync_stream(api, queue, scope, path, &source, done)?;
      }
      if done {
        sync_file(api, queue, path, run_target(scope, path), &source, false)?;
      }
    }
  }
  for path in artifacts.iter().filter(|_| done) {
    sync_file(
      api,
      queue,
      path,
      run_target(scope, path),
      &run_dir.join(path),
      false,
    )?;
  }
  sync_file(
    api,
    queue,
    "run-state.json",
    run_target(scope, "run-state.json"),
    &run_dir.join("run-state.json"),
    true,
  )?;
  Ok(done)
}

pub fn input_put(options: InputPutOptions) -> Result<Value> {
  let api = Api::new(&options.config)?;
  let target = FileTarget::Input {
    project_id: options.project_id,
    input_id: options.input_id,
  };
  validate_target(&target)?;
  let FileTarget::Input {
    project_id,
    input_id,
  } = &target
  else {
    unreachable!()
  };
  let directory = std::path::absolute(options.queue_dir)?
    .join("inputs")
    .join(project_id)
    .join(input_id);
  let mut queue = Queue::new(
    directory,
    json!({"endpoint": api.endpoint, "target": target}),
  )?;
  sync_file(
    &api,
    &mut queue,
    "input",
    target.clone(),
    &std::path::absolute(options.file)?,
    false,
  )?;
  let file = &queue.state.files["input"];
  Ok(
    json!({"target": target, "size": file.size, "sha256": file.sha256, "queue_dir": queue.directory}),
  )
}

pub fn list(config: PathBuf, project_id: String, origin: String) -> Result<Value> {
  validate_component(&project_id)?;
  validate_component(&origin)?;
  let api = Api::new(&config)?;
  let Response::Runs { runs } = api.request(&Request::ListRuns { project_id, origin })? else {
    return Err(message("service did not return a run catalog"));
  };
  Ok(json!({"runs": runs}))
}

fn validate_digest(value: &str) -> Result<()> {
  if value.len() != 64
    || !value
      .bytes()
      .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
  {
    return Err(message("service file has an invalid SHA256 digest"));
  }
  Ok(())
}

fn run_target(scope: &RunScope, path: &str) -> FileTarget {
  FileTarget::Run {
    scope: scope.clone(),
    path: path.to_string(),
  }
}
