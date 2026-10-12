//! Native owner-authorized worker for the shared resource queue.

mod journal;
pub mod source;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use expri_client::ResourceQueueClient;
use expri_core::config::RunServiceConfig;
use expri_core::global_config::QueueWorkerConfig;
use expri_core::resource_queue::{
  AttemptStatus, AttemptSummary, AttemptTerminalStatus, GpuVendor, QueueRequest, QueueResponse,
  ResourceAllocation,
};
use expri_core::storage::StoragePaths;

use crate::error::{ExpriError, Result};
use crate::protocol::RunRequest;
use journal::{Journal, Stage, private_directory, read_json, write_json};

pub struct DaemonOptions {
  pub storage: StoragePaths,
  pub worker: QueueWorkerConfig,
  pub client_config: PathBuf,
  pub poll_interval: Duration,
}

pub fn run_daemon(mut options: DaemonOptions) -> Result<()> {
  if !cfg!(any(target_os = "linux", target_os = "macos")) {
    return Err(message(
      "resource queue execution requires a native Linux or macOS worker",
    ));
  }
  options.worker.validate()?;
  if options.poll_interval.is_zero() || options.poll_interval > Duration::from_secs(20) {
    return Err(message(
      "queue daemon polling must be between zero and 20 seconds",
    ));
  }
  let client_config = fs::canonicalize(&options.client_config)?;
  // Read credentials through the existing global/legacy client loader. Registration
  // is owner-only; a publishing token cannot enable execution.
  let client = ResourceQueueClient::new(&client_config)?;
  options.storage.ensure_private()?;
  options.storage = StoragePaths::from_root(fs::canonicalize(options.storage.root())?)?;
  private_directory(&options.storage.queue_directory())?;
  let crate::lock::LockAttempt::Acquired(_instance) =
    crate::lock::try_lock_file(&options.storage.queue_directory().join("daemon.lock"), true)?
  else {
    return Err(message(
      "a queue daemon already owns this storage directory",
    ));
  };
  private_directory(&options.storage.queue_workers_directory())?;
  let directory = options
    .storage
    .queue_worker_directory(&options.worker.worker_id)?;
  private_directory(&directory)?;
  private_directory(&directory.join("attempts"))?;
  let session_path = directory.join("session.json");
  let session = if session_path.exists() {
    read_json::<String>(&session_path)?
  } else {
    new_id("session")
  };
  expri_core::identifiers::validate_component(&session)?;
  write_json(&session_path, &session)?;
  let session = Arc::new(Mutex::new(session));
  let (_, attempts) = register(&client, &options.worker, &session, &session_path)?;
  let _heartbeats = Heartbeats::start(
    client_config.clone(),
    options.worker.worker_id.clone(),
    directory.clone(),
    session.clone(),
  )?;
  eprintln!(
    "queue worker: {} (CPU and memory reservations are advisory)",
    options.worker.worker_id
  );
  let mut daemon = Daemon {
    options,
    client,
    client_config,
    directory,
    session,
    attempts,
  };
  loop {
    match daemon.tick() {
      Ok(()) => {}
      Err(error) => eprintln!("queue worker waiting: {error}"),
    }
    thread::sleep(daemon.options.poll_interval);
  }
}

struct Daemon {
  options: DaemonOptions,
  client: ResourceQueueClient,
  client_config: PathBuf,
  directory: PathBuf,
  session: Arc<Mutex<String>>,
  attempts: Vec<AttemptSummary>,
}

impl Daemon {
  fn tick(&mut self) -> Result<()> {
    let heartbeat = self.client.request(&QueueRequest::Heartbeat {
      worker_id: self.options.worker.worker_id.clone(),
      session_id: self.session_id(),
    });
    let (mut quarantined, attempts) = match heartbeat {
      Ok(QueueResponse::Worker { worker, attempts }) => (worker.quarantined, attempts),
      Ok(_) => return Err(message("server returned an invalid queue heartbeat")),
      Err(error) if expired_session(&error) => {
        *self.session.lock().expect("queue session lock") = new_id("session");
        write_json(&self.directory.join("session.json"), &self.session_id())?;
        register(
          &self.client,
          &self.options.worker,
          &self.session,
          &self.directory.join("session.json"),
        )?
      }
      Err(error) => return Err(error),
    };
    self.attempts = attempts;
    let pending_claim = if self.directory.join("claim.json").exists() {
      Some(read_json::<String>(&self.directory.join("claim.json"))?)
    } else {
      None
    };
    for attempt in self.attempts.clone() {
      if !quarantined
        && attempt.status == AttemptStatus::Claimed
        && attempt.session_id == self.session_id()
        && pending_claim.as_deref() == Some(attempt.claim_id.as_str())
        && !self
          .directory
          .join("attempts")
          .join(&attempt.attempt_id)
          .exists()
      {
        // Retry this journaled claim after reconciling all other supervisors.
        // A lost response never starves existing cancellation or completion.
        continue;
      }
      if let Err(error) = self.reconcile(attempt.clone()) {
        quarantined = true;
        if attempt.session_id == self.session_id()
          && attempt.status != AttemptStatus::Unknown
          && !attempt.status.is_terminal()
        {
          self.client.request(&QueueRequest::ReportUncertain {
            attempt: attempt.identity(),
            message: queue_message(&error.to_string(), 512),
          })?;
        }
        eprintln!("queue worker quarantined: {error}");
      }
    }
    // A local launch journal that the server no longer considers outstanding
    // remains visible and blocks execution unless its supervisor is terminal.
    for entry in fs::read_dir(self.directory.join("attempts"))? {
      let path = entry?.path();
      let mut journal = Journal::load(&path)?;
      if !self
        .attempts
        .iter()
        .any(|attempt| attempt.attempt_id == journal.attempt.attempt_id)
        && journal.stage != Stage::Terminal
      {
        let confirmed = match self.client.request(&QueueRequest::GetJob {
          job_id: journal.attempt.job_id.clone(),
        })? {
          QueueResponse::Job { job } => job.attempt.filter(|attempt| {
            attempt.attempt_id == journal.attempt.attempt_id && attempt.status.is_terminal()
          }),
          _ => None,
        };
        if let Some(attempt) = confirmed
          && (local_terminal(&journal)?
            || attempt.status == AttemptStatus::Lost
              && local_status(&journal)?
                .as_ref()
                .is_none_or(|status| status["alive"] != true))
        {
          journal.attempt = attempt;
          journal.stage = Stage::Terminal;
          journal.save(&path)?;
          continue;
        }
        quarantined = true;
      }
    }
    if quarantined {
      return Ok(());
    }
    self.claim()
  }

  fn session_id(&self) -> String {
    self.session.lock().expect("queue session lock").clone()
  }

  fn reconcile(&mut self, mut attempt: AttemptSummary) -> Result<()> {
    let directory = self.directory.join("attempts").join(&attempt.attempt_id);
    if !directory.exists() {
      return Err(message(
        "server allocation has no local launch journal; confirm stopped before recovery",
      ));
    }
    let mut journal = Journal::load(&directory)?;
    if journal.attempt.worker_id != self.options.worker.worker_id {
      return Err(message("queue journal belongs to a different worker"));
    }
    if attempt.status.is_terminal() {
      if local_terminal(&journal)? {
        journal.attempt = attempt;
        journal.stage = Stage::Terminal;
        journal.save(&directory)?;
        return Ok(());
      }
      return Err(message(
        "server released an allocation whose local supervisor is not terminal",
      ));
    }
    // Refresh identities before any retry: an acknowledged adoption may have
    // rotated its fence even when its response was lost.
    journal.attempt = attempt.clone();
    journal.save(&directory)?;
    let status = local_status(&journal)?;
    if let Some(run_id) = &attempt.run_id
      && journal
        .run_dir
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        != Some(run_id.as_str())
    {
      return Err(message(
        "local run identity does not match the server allocation",
      ));
    }
    if attempt.status == AttemptStatus::Unknown || attempt.session_id != self.session_id() {
      let run_id = attempt.run_id.clone().ok_or_else(|| {
        message("uncertain launch has no server run identity; manual recovery required")
      })?;
      if !matches!(
        journal.stage,
        Stage::Active | Stage::LaunchPending | Stage::Quarantined
      ) {
        return Err(message(
          "uncertain prelaunch attempt requires manual recovery",
        ));
      }
      if status
        .as_ref()
        .is_none_or(|status| status["alive"] != true && !terminal_status(status))
      {
        journal.stage = Stage::Quarantined;
        journal.message =
          Some("supervisor lost; allocation remains held until stopped is confirmed".into());
        journal.save(&directory)?;
        return Err(message(
          "supervisor lost; allocation remains held until stopped is confirmed",
        ));
      }
      let mut identity = attempt.identity();
      identity.session_id = self.session_id();
      let QueueResponse::Attempt { attempt: adopted } =
        self.client.request(&QueueRequest::Start {
          attempt: identity,
          run_id,
        })?
      else {
        return Err(message("server did not acknowledge adoption"));
      };
      attempt = adopted;
      journal.attempt = attempt.clone();
      journal.save(&directory)?;
    }
    if let Some(status) = status {
      if terminal_status(&status) {
        let terminal = match status["status"].as_str() {
          Some("completed") => AttemptTerminalStatus::Completed,
          Some("cancelled") => AttemptTerminalStatus::Cancelled,
          _ => AttemptTerminalStatus::Failed,
        };
        let exit_code = status["state"]["exit_code"]
          .as_i64()
          .and_then(|code| i32::try_from(code).ok());
        let message = status["state"]["error"].as_str().map(str::to_owned);
        self.finish(&mut journal, &directory, terminal, exit_code, message)?;
        return Ok(());
      }
      if status["alive"] != true {
        journal.stage = Stage::Quarantined;
        journal.message = Some("supervisor lost; allocation remains held".into());
        journal.save(&directory)?;
        return Err(message("supervisor lost; confirm stopped before recovery"));
      }
      journal.stage = Stage::Active;
      journal.save(&directory)?;
      if attempt.cancel_requested {
        request_cancel(&journal)?;
      }
      return Ok(());
    }
    if attempt.cancel_requested && journal.stage == Stage::Claimed {
      self.finish(
        &mut journal,
        &directory,
        AttemptTerminalStatus::Cancelled,
        Some(130),
        Some("cancelled before launch".into()),
      )?;
      return Ok(());
    }
    Err(message(
      "prelaunch journal cannot be resumed; confirm stopped before recovery",
    ))
  }

  fn finish(
    &self,
    journal: &mut Journal,
    directory: &std::path::Path,
    status: AttemptTerminalStatus,
    exit_code: Option<i32>,
    completion_message: Option<String>,
  ) -> Result<()> {
    let QueueResponse::Attempt { attempt } = self.client.request(&QueueRequest::Finish {
      attempt: journal.attempt.identity(),
      status,
      exit_code,
      message: completion_message.map(|message| queue_message(&message, 2048)),
    })?
    else {
      return Err(message("server did not acknowledge attempt completion"));
    };
    journal.attempt = attempt;
    journal.stage = Stage::Terminal;
    journal.save(directory)
  }

  fn claim(&mut self) -> Result<()> {
    let claim_path = self.directory.join("claim.json");
    // A claim ID is durable before the request, so lost replies never create a
    // second allocation on the next attempt.
    let claim_id = if claim_path.exists() {
      read_json::<String>(&claim_path)?
    } else {
      let id = new_id("claim");
      write_json(&claim_path, &id)?;
      id
    };
    let response = self.client.request(&QueueRequest::Claim {
      worker_id: self.options.worker.worker_id.clone(),
      session_id: self.session_id(),
      claim_id,
    })?;
    let QueueResponse::Claimed { assignment } = response else {
      return Err(message("server did not acknowledge queue claim"));
    };
    let Some(assignment) = assignment else {
      fs::remove_file(claim_path)?;
      return Ok(());
    };
    let directory = self
      .directory
      .join("attempts")
      .join(&assignment.attempt.attempt_id);
    if directory.exists() {
      // Only an already-journaled idempotent claim may be returned again. Recovery
      // handles it without launching another supervisor.
      Journal::load(&directory)?;
      fs::remove_file(claim_path)?;
      return Ok(());
    }
    private_directory(&directory)?;
    let mut journal = Journal::claimed(*assignment);
    if journal.attempt.status.is_terminal() {
      journal.stage = Stage::Terminal;
      journal.save(&directory)?;
      fs::remove_file(claim_path)?;
      return Ok(());
    }
    journal.save(&directory)?;
    fs::remove_file(claim_path)?;
    if !self
      .options
      .worker
      .project_ids
      .contains(&journal.assignment.submission.project_id)
      || journal.attempt.worker_id != self.options.worker.worker_id
    {
      return Err(message(
        "server assigned a job outside this worker's project allowlist",
      ));
    }
    validate_allocation(
      &journal.attempt.allocation,
      &self.options.worker,
      &journal.assignment.submission.resources,
    )?;
    if journal.attempt.cancel_requested {
      self.finish(
        &mut journal,
        &directory,
        AttemptTerminalStatus::Cancelled,
        Some(130),
        Some("cancelled before launch".into()),
      )?;
      return Ok(());
    }
    let dispatch = self.dispatch(&mut journal, &directory);
    if let Err(error) = dispatch {
      // A failed download/prepare has not launched any worker. Once launch intent
      // is persisted, a failure can be ambiguous and must retain its allocation.
      if matches!(journal.stage, Stage::Claimed | Stage::Prepared) {
        self.finish(
          &mut journal,
          &directory,
          AttemptTerminalStatus::Failed,
          Some(error.exit_code()),
          Some(error.to_string()),
        )?;
      } else {
        journal.stage = Stage::Quarantined;
        journal.message = Some(error.to_string());
        journal.save(&directory)?;
      }
      return Err(error);
    }
    Ok(())
  }

  fn dispatch(&mut self, journal: &mut Journal, directory: &std::path::Path) -> Result<()> {
    let archive = directory.join("source.zip");
    let expected = &journal.assignment.submission.source;
    self
      .client
      .download_source(&journal.attempt.identity(), expected, &archive)?;
    let (digest, size) = crate::archive::sha256_file(&archive)?;
    if digest != expected.sha256 || size != expected.size {
      return Err(message(
        "downloaded queued source does not match the submitted snapshot",
      ));
    }
    let repo_root = directory.join("source");
    source::unpack_source(&archive, &repo_root)?;
    private_directory(&repo_root)?;
    journal.repo_root = Some(repo_root.clone());
    journal.save(directory)?;
    let QueueResponse::Worker { attempts, .. } = self.client.request(&QueueRequest::Heartbeat {
      worker_id: self.options.worker.worker_id.clone(),
      session_id: self.session_id(),
    })?
    else {
      return Err(message("invalid queue heartbeat before launch"));
    };
    let current = attempts
      .into_iter()
      .find(|attempt| attempt.attempt_id == journal.attempt.attempt_id)
      .ok_or_else(|| message("attempt allocation disappeared before launch"))?;
    if current.status == AttemptStatus::Unknown || current.session_id != self.session_id() {
      return Err(message(
        "attempt lease expired before launch; allocation remains held",
      ));
    }
    journal.attempt = current;
    if journal.attempt.cancel_requested {
      self.finish(
        journal,
        directory,
        AttemptTerminalStatus::Cancelled,
        Some(130),
        Some("cancelled before launch".into()),
      )?;
      return Ok(());
    }
    let request = queued_run_request(journal, &self.options.worker.worker_id, &self.client_config);
    let mut prepared = crate::node::run::prepare_request_at(&request, &repo_root)?;
    crate::node::run::enable_queue_cleanup(&mut prepared)?;
    journal.run_dir = Some(prepared.snapshot.run_dir.clone());
    journal.stage = Stage::Prepared;
    journal.save(directory)?;
    journal.stage = Stage::LaunchPending;
    journal.save(directory)?;
    let QueueResponse::Attempt { attempt } = self.client.request(&QueueRequest::Start {
      attempt: journal.attempt.identity(),
      run_id: prepared.snapshot.run_id.clone(),
    })?
    else {
      return Err(message("server did not acknowledge launch identity"));
    };
    journal.attempt = attempt;
    journal.save(directory)?;
    if journal.attempt.cancel_requested {
      // No subprocess has launched yet; a definitive cancellation can release.
      crate::node::run::abort_prepared(prepared, true, "cancelled before launch")?;
      self.finish(
        journal,
        directory,
        AttemptTerminalStatus::Cancelled,
        Some(130),
        Some("cancelled before launch".into()),
      )?;
      return Ok(());
    }
    crate::node::run::start_prepared(&request, prepared)?;
    journal.stage = Stage::Active;
    journal.save(directory)
  }
}

fn local_status(journal: &Journal) -> Result<Option<serde_json::Value>> {
  let (Some(repo), Some(run)) = (&journal.repo_root, &journal.run_dir) else {
    return Ok(None);
  };
  let run_id = run
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| message("invalid queue run identity"))?;
  crate::jobs::status_at(repo, run_id).map(Some)
}

fn queued_run_request(
  journal: &Journal,
  worker_id: &str,
  client_config: &std::path::Path,
) -> RunRequest {
  let submission = &journal.assignment.submission;
  let mut environment = submission.environment.clone();
  environment
    .env
    .extend(allocation_env(&journal.attempt.allocation));
  environment
    .env
    .insert("EXPRI_QUEUE_JOB_ID".into(), journal.attempt.job_id.clone());
  environment.env.insert(
    "EXPRI_QUEUE_ATTEMPT_ID".into(),
    journal.attempt.attempt_id.clone(),
  );
  RunRequest {
    name: submission.task.clone(),
    command: submission.command.clone(),
    environment,
    // Downloading private assets still needs the executing owner's credentials
    // when automatic result publishing is disabled.
    service: Some(RunServiceConfig {
      client_config: client_config.to_path_buf(),
      project_id: submission.project_id.clone(),
      origin: worker_id.into(),
      dashboard_url: None,
      publish: submission.publish,
    }),
    detach: true,
    smoke: submission.smoke,
    remote_managed: submission.remote_managed.clone(),
    extras: submission.extras.clone(),
    sync_args: submission.sync_args.clone(),
    expected_sync: None,
  }
}
fn terminal_status(status: &serde_json::Value) -> bool {
  matches!(
    status["status"].as_str(),
    Some("completed" | "failed" | "cancelled")
  ) && status["alive"] != true
    && status["state"]
      .get("queue_cleanup_uncertain")
      .is_none_or(|value| value == false)
}
fn local_terminal(journal: &Journal) -> Result<bool> {
  Ok(local_status(journal)?.as_ref().is_some_and(terminal_status))
}
fn request_cancel(journal: &Journal) -> Result<()> {
  let (Some(repo), Some(run)) = (&journal.repo_root, &journal.run_dir) else {
    return Ok(());
  };
  let run_id = run
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| message("invalid queue run identity"))?;
  crate::jobs::query_at(
    &expri_core::protocol::JobRequest::Cancel {
      run_id: run_id.into(),
    },
    repo,
  )?;
  Ok(())
}

/// Apply already-authorized cancellation while staging blocks the dispatcher.
/// Only the native supervisor owns signals and writes terminal run state.
fn cancel_known_attempt(
  worker_directory: &std::path::Path,
  attempt: &AttemptSummary,
) -> Result<()> {
  if !attempt.cancel_requested || attempt.status.is_terminal() {
    return Ok(());
  }
  let directory = worker_directory.join("attempts").join(&attempt.attempt_id);
  let journal = Journal::load(&directory)?;
  if journal.attempt.worker_id != attempt.worker_id
    || journal.attempt.job_id != attempt.job_id
    || journal
      .run_dir
      .as_ref()
      .and_then(|path| path.file_name())
      .and_then(|name| name.to_str())
      != attempt.run_id.as_deref()
    || attempt.run_id.is_none()
  {
    return Err(message(
      "queue heartbeat cancellation does not match an owned run",
    ));
  }
  if local_status(&journal)?
    .as_ref()
    .is_some_and(|status| status["alive"] == true)
  {
    request_cancel(&journal)?;
  }
  Ok(())
}

fn acknowledge_known_terminal(
  client: &ResourceQueueClient,
  worker_directory: &std::path::Path,
  attempt: &AttemptSummary,
) -> Result<()> {
  if attempt.status.is_terminal()
    || attempt.status == AttemptStatus::Unknown
    || attempt.run_id.is_none()
  {
    return Ok(());
  }
  let journal = Journal::load(&worker_directory.join("attempts").join(&attempt.attempt_id))?;
  if journal.attempt.worker_id != attempt.worker_id
    || journal.attempt.job_id != attempt.job_id
    || journal
      .run_dir
      .as_ref()
      .and_then(|path| path.file_name())
      .and_then(|name| name.to_str())
      != attempt.run_id.as_deref()
  {
    return Err(message(
      "queue terminal acknowledgement does not match an owned run",
    ));
  }
  let Some(status) = local_status(&journal)? else {
    return Ok(());
  };
  if !terminal_status(&status) {
    return Ok(());
  }
  let terminal = match status["status"].as_str() {
    Some("completed") => AttemptTerminalStatus::Completed,
    Some("cancelled") => AttemptTerminalStatus::Cancelled,
    _ => AttemptTerminalStatus::Failed,
  };
  client.request(&QueueRequest::Finish {
    attempt: attempt.identity(),
    status: terminal,
    exit_code: status["state"]["exit_code"]
      .as_i64()
      .and_then(|code| i32::try_from(code).ok()),
    message: status["state"]["error"]
      .as_str()
      .map(|error| queue_message(error, 2048)),
  })?;
  Ok(())
}

fn allocation_env(allocation: &ResourceAllocation) -> BTreeMap<String, String> {
  let mut env = BTreeMap::new();
  for key in [
    "OMP_NUM_THREADS",
    "MKL_NUM_THREADS",
    "OPENBLAS_NUM_THREADS",
    "NUMEXPR_NUM_THREADS",
    "VECLIB_MAXIMUM_THREADS",
    "EXPRI_ALLOCATED_CPUS",
  ] {
    env.insert(key.into(), allocation.cpus.to_string());
  }
  env.insert(
    "EXPRI_ALLOCATED_MEMORY_BYTES".into(),
    allocation.memory_bytes.to_string(),
  );
  let selectors = |vendor| {
    allocation
      .gpus
      .iter()
      .filter(|gpu| gpu.vendor == vendor)
      .filter_map(|gpu| gpu.selector.as_deref())
      .collect::<Vec<_>>()
      .join(",")
  };
  env.insert("CUDA_VISIBLE_DEVICES".into(), selectors(GpuVendor::Nvidia));
  let amd = selectors(GpuVendor::Amd);
  env.insert("HIP_VISIBLE_DEVICES".into(), amd.clone());
  env.insert("ROCR_VISIBLE_DEVICES".into(), amd);
  env
}

fn validate_allocation(
  allocation: &ResourceAllocation,
  worker: &QueueWorkerConfig,
  request: &expri_core::resource_queue::ResourceRequest,
) -> Result<()> {
  if allocation.cpus != request.cpus
    || allocation.memory_bytes != request.memory_bytes
    || allocation.cpus > worker.capacity.cpus
    || allocation.memory_bytes > worker.capacity.memory_bytes
    || allocation.gpus.len() != request.gpus as usize
    || allocation
      .gpus
      .iter()
      .any(|gpu| !worker.capacity.gpus.contains(gpu))
  {
    return Err(message(
      "server allocation does not match this worker's declared capacity and job request",
    ));
  }
  let mut devices = std::collections::BTreeSet::new();
  if allocation
    .gpus
    .iter()
    .any(|gpu| !devices.insert(&gpu.device_id))
  {
    return Err(message("server allocation contains duplicate GPU devices"));
  }
  Ok(())
}

fn register(
  client: &ResourceQueueClient,
  worker: &QueueWorkerConfig,
  session: &Arc<Mutex<String>>,
  path: &std::path::Path,
) -> Result<(bool, Vec<AttemptSummary>)> {
  let register_request = || QueueRequest::RegisterWorker {
    worker_id: worker.worker_id.clone(),
    session_id: session.lock().expect("queue session lock").clone(),
    project_ids: worker.project_ids.clone(),
    capacity: worker.capacity.clone(),
  };
  let response = match client.request(&register_request()) {
    Err(error) if expired_session(&error) => {
      *session.lock().expect("queue session lock") = new_id("session");
      write_json(path, &session.lock().expect("queue session lock").clone())?;
      client.request(&register_request())?
    }
    result => result?,
  };
  let QueueResponse::Worker { worker, attempts } = response else {
    return Err(message(
      "server did not acknowledge queue worker registration",
    ));
  };
  Ok((worker.quarantined, attempts))
}

struct Heartbeats {
  stopped: Arc<AtomicBool>,
  thread: Option<thread::JoinHandle<()>>,
}
impl Heartbeats {
  fn start(
    config: PathBuf,
    worker_id: String,
    directory: PathBuf,
    session: Arc<Mutex<String>>,
  ) -> Result<Self> {
    let client = ResourceQueueClient::new(&config)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let thread = thread::Builder::new()
      .name("queue-heartbeat".into())
      .spawn(move || {
        while !stop.load(Ordering::Acquire) {
          let session_id = session.lock().expect("queue session lock").clone();
          let response = client.request(&QueueRequest::Heartbeat {
            worker_id: worker_id.clone(),
            session_id,
          });
          if let Ok(QueueResponse::Worker { attempts, .. }) = response {
            for attempt in attempts {
              let _ = cancel_known_attempt(&directory, &attempt);
              let _ = acknowledge_known_terminal(&client, &directory, &attempt);
            }
          }
          for _ in 0..10 {
            if stop.load(Ordering::Acquire) {
              return;
            }
            thread::sleep(Duration::from_secs(1));
          }
        }
      })?;
    Ok(Self {
      stopped,
      thread: Some(thread),
    })
  }
}
impl Drop for Heartbeats {
  fn drop(&mut self) {
    self.stopped.store(true, Ordering::Release);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}
fn expired_session(error: &ExpriError) -> bool {
  matches!(error, ExpriError::ServiceRejected { status: 409, detail } if detail.contains("worker session expired"))
}
fn new_id(prefix: &str) -> String {
  format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}
fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}
fn queue_message(value: &str, max_bytes: usize) -> String {
  let mut output = String::new();
  for character in value.chars() {
    let character = if character.is_control() {
      ' '
    } else {
      character
    };
    if output.len() + character.len_utf8() > max_bytes {
      break;
    }
    output.push(character);
  }
  output
}

#[cfg(test)]
mod tests;
