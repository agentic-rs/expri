//! A worker-local publisher outlives training and resumes the existing upload queue.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::PublishOptions;
use super::client::{self, Publisher, fs};
use super::types::validate_component;
use crate::config::RunServiceConfig;
use crate::error::{ExpriError, Result};
use crate::lock::{FileLock, LockAttempt, try_lock_file};

const REQUEST: &str = "publishing-request.json";
const STATE: &str = "publishing-state.json";
const LEASE: &str = ".publish.lock";
const RECORD_LIMIT: u64 = 32 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
  schema_version: u32,
  repo_root: PathBuf,
  run_dir: PathBuf,
  config: RunServiceConfig,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishingState {
  schema_version: u32,
  run_id: String,
  status: String,
  updated_at: String,
  last_success_at: Option<String>,
  last_error: Option<String>,
  retry_after_seconds: Option<u64>,
  dashboard_url: Option<String>,
  progress: Value,
}

impl PublishingState {
  fn new(intent: &Intent) -> Result<Self> {
    let run_id = run_id(&intent.run_dir)?;
    Ok(Self {
      schema_version: 1,
      dashboard_url: dashboard_url(&intent.config, &run_id),
      run_id,
      status: "pending".into(),
      updated_at: now(),
      last_success_at: None,
      last_error: None,
      retry_after_seconds: None,
      progress: json!({}),
    })
  }

  fn save(&mut self, directory: &Path, status: &str) -> Result<()> {
    self.status = status.into();
    self.updated_at = now();
    if serde_json::to_vec_pretty(self)?.len() as u64 + 1 > RECORD_LIMIT {
      return Err(error("publisher status exceeds its metadata limit"));
    }
    fs::atomic_json(&directory.join(STATE), self)
  }

  fn fail(&mut self, directory: &Path, detail: String) -> Result<()> {
    self.last_error = Some(detail);
    self.retry_after_seconds = None;
    self.save(directory, "error")
  }
}

fn error(detail: impl Into<String>) -> ExpriError {
  ExpriError::Message(detail.into())
}
fn now() -> String {
  chrono::Utc::now().to_rfc3339()
}

fn safe_error(failure: &ExpriError) -> String {
  let detail = match failure {
    ExpriError::Toml(_) => "Publisher client configuration is invalid".into(),
    ExpriError::Json(_) => "Publisher metadata is invalid".into(),
    ExpriError::Io(_) | ExpriError::IoContext { .. } => {
      "Publisher could not read or save local files".into()
    }
    _ => failure.to_string(),
  };
  if detail.contains("http://") || detail.contains("https://") {
    return "Service publishing failed; saved work can be retried".into();
  }
  detail
    .chars()
    .filter(|character| !character.is_control())
    .take(512)
    .collect()
}

fn run_id(directory: &Path) -> Result<String> {
  let id = directory
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| error("publisher run directory has no identity"))?;
  validate_component(id)?;
  Ok(id.to_string())
}

fn validate_intent(intent: &Intent, directory: &Path) -> Result<()> {
  intent.config.validate()?;
  if serde_json::to_vec_pretty(intent)?.len() as u64 + 1 > RECORD_LIMIT {
    return Err(error("publisher request exceeds its metadata limit"));
  }
  if intent.schema_version != 1
    || intent.run_dir != directory
    || !intent.repo_root.is_absolute()
    || intent.run_dir
      != intent
        .repo_root
        .join(".expri/runs")
        .join(run_id(directory)?)
  {
    return Err(error("publisher request does not belong to this run"));
  }
  fs::directory(&intent.repo_root)?;
  fs::directory(directory)
}

fn read_intent(directory: &Path) -> Result<Intent> {
  let intent: Intent =
    serde_json::from_slice(&fs::read_bounded(&directory.join(REQUEST), RECORD_LIMIT)?)?;
  validate_intent(&intent, directory)?;
  Ok(intent)
}

fn read_state(directory: &Path) -> Result<PublishingState> {
  let state: PublishingState =
    serde_json::from_slice(&fs::read_bounded(&directory.join(STATE), RECORD_LIMIT)?)?;
  if state.schema_version != 1
    || state.run_id != run_id(directory)?
    || !matches!(
      state.status.as_str(),
      "pending" | "publishing" | "retrying" | "error" | "published"
    )
  {
    return Err(error("publisher status does not belong to this run"));
  }
  Ok(state)
}

fn runtime_config(intent: &Intent) -> Result<PathBuf> {
  let path = std::fs::canonicalize(&intent.config.client_config)
    .map_err(|_| error("Publisher client configuration is missing or inaccessible"))?;
  if path.starts_with(&intent.repo_root) {
    return Err(error(
      "Publisher client configuration must stay outside the source checkout",
    ));
  }
  client::validate_config(&path).map_err(|failure| error(safe_error(&failure)))?;
  Ok(path)
}

pub(crate) fn dashboard_url(config: &RunServiceConfig, run_id: &str) -> Option<String> {
  if !config.publish {
    return None;
  }
  let mut url = reqwest::Url::parse(config.dashboard_url.as_ref()?).ok()?;
  url
    .query_pairs_mut()
    .append_pair("project_id", &config.project_id)
    .append_pair("origin", &config.origin)
    .append_pair("run_id", run_id);
  Some(url.into())
}

fn owned_lease(directory: &Path) -> Result<Option<FileLock>> {
  match try_lock_file(&directory.join(LEASE), true)? {
    LockAttempt::Acquired(lease) => Ok(Some(lease)),
    LockAttempt::Busy => Ok(None),
    LockAttempt::Missing => unreachable!("publisher lease is created"),
  }
}

fn worker_lease(directory: &Path) -> Result<Option<FileLock>> {
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    if let Some(lease) = owned_lease(directory)? {
      return Ok(Some(lease));
    }
    if Instant::now() >= deadline {
      return Ok(None);
    }
    // Status readers and launchers hold short leases; a running worker keeps it.
    thread::sleep(Duration::from_millis(20));
  }
}

#[cfg(unix)]
fn spawn_worker(intent: &Intent) -> Result<()> {
  use std::os::unix::process::CommandExt;
  let mut command = Command::new(std::env::current_exe()?);
  command
    .args(["service", "publish-worker", "--run-dir"])
    .arg(&intent.run_dir)
    .current_dir(&intent.repo_root)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
  // Re-exec closes the upload/run leases. The publisher has its own lifetime.
  unsafe {
    command.pre_exec(|| {
      if libc::setsid() < 0 {
        return Err(std::io::Error::last_os_error());
      }
      Ok(())
    });
  }
  let mut child = command.spawn()?;
  thread::spawn(move || {
    let _ = child.wait();
  });
  Ok(())
}

#[cfg(not(unix))]
fn spawn_worker(_: &Intent) -> Result<()> {
  Err(error(
    "Automatic service publishing requires Unix process supervision",
  ))
}

fn launch(intent: &Intent, state: &mut PublishingState, lease: FileLock) -> Result<()> {
  if let Err(failure) = runtime_config(intent) {
    let detail = safe_error(&failure);
    state.fail(&intent.run_dir, detail.clone())?;
    return Err(error(detail));
  }
  state.last_error = None;
  state.retry_after_seconds = None;
  state.save(&intent.run_dir, "pending")?;
  // Release before re-exec so a fast child can claim the publisher lease.
  // Concurrent candidates are harmless: only one worker obtains the lease.
  drop(lease);
  if let Err(failure) = spawn_worker(intent)
    && let Some(_lease) = owned_lease(&intent.run_dir)?
  {
    let current = read_state(&intent.run_dir)?;
    if current.updated_at != state.updated_at || current.status != "pending" {
      return Ok(());
    }
    let detail = safe_error(&failure);
    state.fail(&intent.run_dir, detail.clone())?;
    return Err(error(detail));
  }
  Ok(())
}

pub(crate) fn start(repo_root: &Path, run_dir: &Path, config: &RunServiceConfig) -> Result<()> {
  if !config.publish {
    return Ok(());
  }
  let intent = Intent {
    schema_version: 1,
    repo_root: repo_root.canonicalize()?,
    run_dir: run_dir.canonicalize()?,
    config: config.clone(),
  };
  validate_intent(&intent, &intent.run_dir)?;
  let Some(lease) = owned_lease(&intent.run_dir)? else {
    return Ok(());
  };
  let request_path = intent.run_dir.join(REQUEST);
  fs::optional_regular(&request_path)?;
  if request_path.try_exists()? {
    let saved = read_intent(&intent.run_dir)?;
    if serde_json::to_value(&saved)? != serde_json::to_value(&intent)? {
      return Err(error(
        "publisher request belongs to different service settings",
      ));
    }
  } else {
    fs::atomic_json(&request_path, &intent)?;
  }
  let mut state = PublishingState::new(&intent)?;
  state.save(&intent.run_dir, "pending")?;
  launch(&intent, &mut state, lease)
}

pub(crate) fn resume(run_dir: &Path) -> Result<Value> {
  let directory = run_dir.canonicalize()?;
  fs::directory(&directory)?;
  let Some(lease) = owned_lease(&directory)? else {
    let mut report = status(&directory)?.ok_or_else(|| error("publisher status is missing"))?;
    report["already_running"] = json!(true);
    return Ok(report);
  };
  let intent = read_intent(&directory)?;
  let mut state = read_state(&directory)?;
  if state.status != "published" {
    launch(&intent, &mut state, lease)?;
  }
  let mut report = serde_json::to_value(&state)?;
  client::project_result_upload(&mut report["progress"]);
  report["started"] = json!(state.status != "published");
  Ok(report)
}

pub(crate) fn status(run_dir: &Path) -> Result<Option<Value>> {
  let directory = run_dir.canonicalize()?;
  fs::directory(&directory)?;
  let path = directory.join(STATE);
  fs::optional_regular(&path)?;
  if !path.try_exists()? {
    return Ok(None);
  }
  let mut report = serde_json::to_value(read_state(&directory)?)?;
  client::project_result_upload(&mut report["progress"]);
  report["worker_active"] = json!(matches!(
    try_lock_file(&directory.join(LEASE), false)?,
    LockAttempt::Busy
  ));
  Ok(Some(report))
}

fn mark_abandoned(directory: &Path) -> Result<()> {
  let LockAttempt::Acquired(_lease) = try_lock_file(&directory.join(".run.lock"), false)? else {
    return Ok(());
  };
  // Re-read after acquiring the run lease so normal terminal publication wins.
  let path = directory.join("run-state.json");
  let mut state: Value = serde_json::from_slice(&fs::read_bounded(&path, 16 * 1024 * 1024)?)?;
  if state.get("run_id").and_then(Value::as_str) != Some(run_id(directory)?.as_str()) {
    return Err(error("publisher run state has a different identity"));
  }
  if matches!(
    state.get("status").and_then(Value::as_str),
    Some("preparing" | "running")
  ) {
    state["status"] = json!("lost");
    state["finished_at"] = json!(now());
    state["error"] = json!("Run supervisor exited before publishing a terminal state");
    fs::atomic_json(&path, &state)?;
  }
  Ok(())
}

pub(super) fn worker(run_dir: &Path) -> Result<()> {
  let directory = run_dir.canonicalize()?;
  fs::directory(&directory)?;
  let Some(_lease) = worker_lease(&directory)? else {
    return Ok(());
  };
  let mut state = read_state(&directory)?;
  let initialized = (|| {
    let intent = read_intent(&directory)?;
    let config = runtime_config(&intent)?;
    Publisher::new(&PublishOptions {
      config,
      run_dir: directory.clone(),
      project_id: intent.config.project_id,
      origin: intent.config.origin,
      artifacts: Vec::new(),
      watch: true,
      queue_dir: intent.repo_root.join(".expri/publish"),
    })
  })();
  let mut publisher = match initialized {
    Ok(publisher) => publisher,
    Err(failure) => {
      state.fail(&directory, safe_error(&failure))?;
      return Err(error("Publisher initialization failed; inspect run status"));
    }
  };
  let mut failures = 0u32;
  loop {
    if let Err(failure) = mark_abandoned(&directory) {
      state.fail(&directory, safe_error(&failure))?;
      return Err(error("Publisher cannot inspect the run lease"));
    }
    state.retry_after_seconds = None;
    state.save(&directory, "publishing")?;
    match publisher.cycle(&mut |progress| {
      state.progress = progress;
      state.save(&directory, "publishing")
    }) {
      Ok(done) => {
        failures = 0;
        state.last_success_at = Some(now());
        state.last_error = None;
        state.progress = publisher.progress(done);
        state.save(&directory, if done { "published" } else { "publishing" })?;
        if done {
          return Ok(());
        }
        thread::sleep(Duration::from_secs(5));
      }
      Err(failure) => {
        if let Some(reason) = Publisher::permanent_rejection(&failure) {
          state.fail(&directory, reason.into())?;
          return Err(error(reason));
        }
        let delay = (5u64 << failures.min(4)).min(60);
        failures = failures.saturating_add(1);
        state.last_error = Some(safe_error(&error(publisher.error_text(&failure))));
        state.retry_after_seconds = Some(delay);
        state.save(&directory, "retrying")?;
        thread::sleep(Duration::from_secs(delay));
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, RunServiceConfig) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let repo = root.join("source");
    let run = repo.join(".expri/runs/run-fixture");
    std::fs::create_dir_all(&run).unwrap();
    fs::atomic_json(
      &run.join("run-state.json"),
      &json!({"run_id": "run-fixture", "status": "preparing"}),
    )
    .unwrap();
    let config = RunServiceConfig {
      client_config: root.join("private-client.toml"),
      project_id: "project".into(),
      origin: "worker".into(),
      dashboard_url: Some("https://example.invalid/".into()),
      inputs: Vec::new(),
      publish: true,
    };
    (temporary, repo, run, config)
  }

  #[test]
  fn missing_client_config_keeps_resumable_intent_and_does_not_change_run_result() {
    let (_temporary, repo, run, config) = fixture();
    let run_before = std::fs::read(run.join("run-state.json")).unwrap();
    assert!(start(&repo, &run, &config).is_err());
    let intent = read_intent(&run).unwrap();
    assert_eq!(intent.config, config);
    let recorded = status(&run).unwrap().unwrap();
    assert_eq!(recorded["status"], "error");
    assert_eq!(recorded["worker_active"], false);
    assert!(recorded["last_error"].as_str().unwrap().contains("missing"));
    assert!(resume(&run).is_err());
    assert_eq!(
      std::fs::read(run.join("run-state.json")).unwrap(),
      run_before
    );
    assert!(!repo.join(".expri/publish").exists());
  }

  #[test]
  fn runtime_configuration_errors_never_echo_client_contents() {
    let (_temporary, repo, run, config) = fixture();
    let secret = "fixture-secret-never-in-publishing-status";
    std::fs::write(
      &config.client_config,
      format!("url = 'https://example.invalid/'\ntoken_env = '{secret}'\nbroken = [\n"),
    )
    .unwrap();
    let failure = start(&repo, &run, &config).unwrap_err().to_string();
    assert!(!failure.contains(secret));
    assert!(
      !std::fs::read_to_string(run.join(STATE))
        .unwrap()
        .contains(secret)
    );
    assert!(
      !std::fs::read_to_string(run.join(REQUEST))
        .unwrap()
        .contains(secret)
    );
    std::fs::write(
      &config.client_config,
      "url='https://example.invalid/'\ntoken_env='EXPRI_PUBLISHING_TEST_ABSENT_A90407'\n",
    )
    .unwrap();
    assert!(resume(&run).is_err());
    assert_eq!(
      status(&run).unwrap().unwrap()["last_error"],
      "service token environment variable is missing"
    );
  }

  #[test]
  fn configuration_inside_checkout_is_a_durable_error() {
    let (_temporary, repo, run, mut config) = fixture();
    config.client_config = repo.join("worker.toml");
    std::fs::write(
      &config.client_config,
      "url='https://example.invalid/'\ntoken_env='PATH'\n",
    )
    .unwrap();
    assert!(start(&repo, &run, &config).is_err());
    assert_eq!(
      read_intent(&run).unwrap().config.client_config,
      config.client_config
    );
    assert!(
      status(&run).unwrap().unwrap()["last_error"]
        .as_str()
        .unwrap()
        .contains("outside")
    );
  }

  #[test]
  fn deleted_project_stops_publisher_without_changing_active_training_or_local_data() {
    let (_temporary, repo, run, config) = fixture();
    let (url, task) = client::tests::reject_publishing(410);
    std::fs::write(
      &config.client_config,
      format!("url={url:?}\ntoken_env='PATH'\n"),
    )
    .unwrap();
    fs::atomic_json(
      &run.join("run-state.json"),
      &json!({"run_id": "run-fixture", "status": "running"}),
    )
    .unwrap();
    let run_before = std::fs::read(run.join("run-state.json")).unwrap();
    let snapshot = br#"{"run_id":"run-fixture"}"#;
    let metrics = b"{\"step\":1,\"loss\":0.5}\n";
    std::fs::write(run.join("snapshot.json"), snapshot).unwrap();
    std::fs::create_dir(run.join("outputs")).unwrap();
    std::fs::write(run.join("outputs/metrics.jsonl"), metrics).unwrap();
    let intent = Intent {
      schema_version: 1,
      repo_root: repo.clone(),
      run_dir: run.clone(),
      config,
    };
    fs::atomic_json(&run.join(REQUEST), &intent).unwrap();
    PublishingState::new(&intent)
      .unwrap()
      .save(&run, "pending")
      .unwrap();
    let _training_lease = crate::lock::run_lock(&run).unwrap();
    let directory = run.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let publisher = thread::spawn(move || send.send(worker(&directory)).unwrap());
    let failure = receive
      .recv_timeout(Duration::from_secs(4))
      .expect("a deleted project must stop its publisher before the retry")
      .unwrap_err();
    publisher.join().unwrap();
    task.join().unwrap();
    assert!(failure.to_string().contains("new project identifier"));
    let recorded = status(&run).unwrap().unwrap();
    assert_eq!(recorded["status"], "error");
    assert_eq!(recorded["retry_after_seconds"], Value::Null);
    assert_eq!(recorded["worker_active"], false);
    assert!(recorded["last_error"].as_str().unwrap().contains("deleted"));
    assert_eq!(
      std::fs::read(run.join("run-state.json")).unwrap(),
      run_before
    );
    assert_eq!(
      std::fs::read(run.join("outputs/metrics.jsonl")).unwrap(),
      metrics
    );
    assert!(matches!(
      try_lock_file(&run.join(".run.lock"), false).unwrap(),
      LockAttempt::Busy
    ));
    let queue_dir = repo.join(".expri/publish/runs/project/worker/run-fixture");
    let queue: Value =
      serde_json::from_slice(&std::fs::read(queue_dir.join("queue.json")).unwrap()).unwrap();
    let saved = &queue["files"]["snapshot.json"];
    assert_eq!(saved["upload"]["complete"], false);
    assert_eq!(
      std::fs::read(queue_dir.join(saved["snapshot"].as_str().unwrap())).unwrap(),
      snapshot,
    );
    assert!(matches!(
      try_lock_file(&queue_dir.join(".sync.lock"), false).unwrap(),
      LockAttempt::Acquired(_)
    ));
  }

  #[test]
  fn released_run_lease_marks_only_abandoned_active_state_lost() {
    let (_temporary, _repo, run, _config) = fixture();
    let lease = crate::lock::run_lock(&run).unwrap();
    mark_abandoned(&run).unwrap();
    let state = || {
      serde_json::from_slice::<Value>(&std::fs::read(run.join("run-state.json")).unwrap()).unwrap()
    };
    assert_eq!(state()["status"], "preparing");
    drop(lease);
    mark_abandoned(&run).unwrap();
    assert_eq!(state()["status"], "lost");
    assert!(state()["finished_at"].is_string());
    let terminal = json!({"run_id": "run-fixture", "status": "completed", "exit_code": 0});
    fs::atomic_json(&run.join("run-state.json"), &terminal).unwrap();
    mark_abandoned(&run).unwrap();
    assert_eq!(state(), terminal);
  }

  #[test]
  fn status_is_read_only_and_rejects_foreign_or_corrupt_records() {
    let (_temporary, repo, run, config) = fixture();
    assert!(status(&run).unwrap().is_none());
    assert!(!run.join(LEASE).exists());
    let intent = Intent {
      schema_version: 1,
      repo_root: repo,
      run_dir: run.clone(),
      config,
    };
    let mut state = PublishingState::new(&intent).unwrap();
    state.run_id = "another-run".into();
    state.save(&run, "retrying").unwrap();
    assert!(status(&run).is_err());
    std::fs::write(run.join(STATE), b"{").unwrap();
    assert!(status(&run).is_err());
  }

  #[test]
  fn status_exposes_result_upload_without_rewriting_saved_progress() {
    let (_temporary, repo, run, config) = fixture();
    let intent = Intent {
      schema_version: 1,
      repo_root: repo,
      run_dir: run.clone(),
      config,
    };
    let mut state = PublishingState::new(&intent).unwrap();
    state.progress = json!({"archive":{"status":"archived","incomplete":false,
      "file":null,"last_error":null}});
    state.save(&run, "published").unwrap();
    let saved = std::fs::read(run.join(STATE)).unwrap();
    let report = status(&run).unwrap().unwrap();
    assert_eq!(report["status"], "published");
    assert!(report["progress"].get("archive").is_none());
    assert_eq!(report["progress"]["result_upload"]["status"], "uploaded");
    assert_eq!(std::fs::read(run.join(STATE)).unwrap(), saved);
  }

  #[test]
  fn status_limit_measures_the_pretty_record_that_will_be_read() {
    let (_temporary, repo, run, config) = fixture();
    let intent = Intent {
      schema_version: 1,
      repo_root: repo,
      run_dir: run.clone(),
      config,
    };
    let mut state = PublishingState::new(&intent).unwrap();
    state.progress = json!({"records": vec!["x"; 3_000]});
    assert!(serde_json::to_vec(&state).unwrap().len() < RECORD_LIMIT as usize);
    assert!(serde_json::to_vec_pretty(&state).unwrap().len() > RECORD_LIMIT as usize);
    assert!(state.save(&run, "publishing").is_err());
    assert!(
      !run.join(STATE).exists(),
      "an unreadable oversized status was published"
    );
  }

  #[test]
  fn dashboard_link_contains_only_public_scope_and_run_identity() {
    let (_temporary, _repo, _run, mut config) = fixture();
    let url = reqwest::Url::parse(&dashboard_url(&config, "run-fixture").unwrap()).unwrap();
    let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
      query,
      std::collections::BTreeMap::from([
        ("project_id".into(), "project".into()),
        ("origin".into(), "worker".into()),
        ("run_id".into(), "run-fixture".into()),
      ])
    );
    config.dashboard_url = None;
    assert!(dashboard_url(&config, "run-fixture").is_none());
    config.dashboard_url = Some("https://example.invalid/".into());
    config.publish = false;
    assert!(dashboard_url(&config, "run-fixture").is_none());
  }
}
