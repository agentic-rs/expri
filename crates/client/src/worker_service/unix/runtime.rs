use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use expri_core::error::{ExpriError, Result};
use expri_core::lock::{LockAttempt, try_lock_file};
use expri_core::storage::StoragePaths;
use serde_json::{Value, json};

use super::super::{
  ComponentRunners, ServiceComponent, ServiceContext, error, files, preferences, registry,
};
use super::signals::{SIGNAL_STOP, SignalGuard};
use super::{TICK, socket, validate_socket};
use crate::publishing::PreparedJob;

const JOBS: usize = 4;
pub(in crate::worker_service) type Runner = Arc<
  dyn Fn(&PreparedJob, Arc<AtomicBool>, &mut dyn FnMut(Value) -> Result<()>) -> Result<bool>
    + Send
    + Sync,
>;

pub fn run_foreground(
  paths: &StoragePaths,
  collection_hook: impl FnOnce(ServiceContext) -> Result<()> + Send + 'static,
) -> Result<()> {
  let _signals = SignalGuard::install()?;
  run_runtime(
    paths,
    collection_hook,
    Arc::new(crate::publishing::run_job),
    true,
  )
}

pub fn run_foreground_components(paths: &StoragePaths, runners: ComponentRunners) -> Result<()> {
  let _signals = SignalGuard::install()?;
  run_components(paths, runners, Arc::new(crate::publishing::run_job), true)
}

pub(in crate::worker_service) fn run_runtime(
  paths: &StoragePaths,
  collection_hook: impl FnOnce(ServiceContext) -> Result<()> + Send + 'static,
  runner: Runner,
  signals: bool,
) -> Result<()> {
  let collection = std::sync::Mutex::new(Some(collection_hook));
  let mut runners = ComponentRunners::new();
  runners.insert(
    ServiceComponent::Collection,
    Arc::new(move |context| {
      let hook = collection
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
        .ok_or_else(|| error("legacy collection hook cannot be restarted"))?;
      hook(context)
    }),
  );
  run_components(paths, runners, runner, signals)
}

pub(in crate::worker_service) fn run_components(
  paths: &StoragePaths,
  mut runners: ComponentRunners,
  runner: Runner,
  signals: bool,
) -> Result<()> {
  let directory = files::ensure(paths)?;
  // Serialize foreground bootstrap with offline component choices before
  // taking the process lease; lazy launch may already hold start.lock.
  let bootstrap = preferences::Preferences::lock(&directory)?;
  let lease_path = directory.join("service.lock");
  files::private_file(&lease_path)?;
  let LockAttempt::Acquired(_lease) = try_lock_file(&lease_path, false)? else {
    return Err(error(
      "a worker service is already running for this user/storage root",
    ));
  };
  let socket_path = socket(&directory)?;
  if socket_path.try_exists()? {
    validate_socket(&socket_path)?;
    fs::remove_file(&socket_path)?;
  }
  files::directory(
    socket_path
      .parent()
      .ok_or_else(|| error("control socket needs a parent"))?,
  )?;
  let listener = UnixListener::bind(&socket_path)?;
  fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
  let _socket_guard = SocketGuard(socket_path);
  listener.set_nonblocking(true)?;
  let context = ServiceContext::with_preferences(&preferences::Preferences::load(paths.root())?);
  let publishing_directory = directory.clone();
  runners.add(
    ServiceComponent::Publishing,
    "run_publishing",
    Arc::new(move |context| run_publishing(&publishing_directory, context, runner.clone())),
  );
  let initial = snapshot(&context, 0, 0);
  files::write(&directory.join("status.json"), &initial)?;
  drop(bootstrap);
  let mut active = BTreeMap::<ServiceComponent, Vec<JoinHandle<Result<()>>>>::new();
  let mut saved_status = Value::Null;
  let mut failure = None;
  loop {
    if signals && SIGNAL_STOP.load(Ordering::Acquire) {
      context.request_stop();
    }
    for _ in 0..32 {
      match listener.accept() {
        Ok((mut stream, _)) => {
          let _ = super::handle_control(&mut stream, &context, &directory);
          if context.is_stopping() {
            break;
          }
        }
        Err(failure) if failure.kind() == std::io::ErrorKind::WouldBlock => break,
        Err(error) => {
          context.request_stop();
          failure = Some(error);
          break;
        }
      }
    }
    let finished: Vec<_> = active
      .iter()
      .filter(|(_, jobs)| jobs.iter().all(JoinHandle::is_finished))
      .map(|(component, _)| *component)
      .collect();
    for component in finished {
      if let Some(jobs) = active.remove(&component) {
        for job in jobs {
          let _ = job.join();
        }
        finish_component(&context, component);
      }
    }
    if context.is_stopping() {
      break;
    }
    for component in ServiceComponent::ALL {
      if active.contains_key(&component) {
        continue;
      }
      let mut components = context
        .shared
        .components
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
      let state = components.get_mut(&component).unwrap();
      if !state.desired_running || !state.launch_requested {
        continue;
      }
      state.launch_requested = false;
      let Some(parts) = runners.0.get(&component).cloned() else {
        state.phase = "unconfigured";
        state.detail = json!({"status": "unconfigured"});
        continue;
      };
      state.generation += 1;
      state.phase = "starting";
      state.detail = json!({"status": "starting", "last_error": null});
      state.stop = Some(Arc::new(AtomicBool::new(false)));
      state.parts = parts
        .keys()
        .map(|name| {
          (
            *name,
            super::super::PartState {
              phase: "starting",
              detail: json!({"status": "starting", "last_error": null}),
              stop: Arc::new(AtomicBool::new(false)),
            },
          )
        })
        .collect();
      let generation = state.generation;
      let contexts: Vec<_> = parts
        .into_iter()
        .map(|(name, runner)| {
          let context = ServiceContext {
            stop: state.parts[name].stop.clone(),
            shared: context.shared.clone(),
            component: Some(component),
            generation,
            part: Some(name),
          };
          (name, runner, context)
        })
        .collect();
      drop(components);
      if component == ServiceComponent::Collection {
        let mut collection = context
          .shared
          .collection
          .lock()
          .unwrap_or_else(|poison| poison.into_inner());
        collection.ready = false;
        collection.detail["status"] = json!("starting");
        collection.detail["last_error"] = Value::Null;
      }
      let mut tasks = Vec::new();
      for (name, runner, part_context) in contexts {
        let finishing = part_context.clone();
        match thread::Builder::new()
          .name(format!("{}-{name}", component.thread_name()))
          .spawn(move || {
            let result =
              std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runner(part_context)))
                .unwrap_or_else(|_| Err(error("service component panicked")));
            finish_part(&finishing, result.is_ok());
            result
          }) {
          Ok(task) => tasks.push(task),
          Err(_) => finish_part(
            &ServiceContext {
              stop: Arc::new(AtomicBool::new(false)),
              shared: context.shared.clone(),
              component: Some(component),
              generation,
              part: Some(name),
            },
            false,
          ),
        }
      }
      if tasks.is_empty() {
        finish_component(&context, component);
      } else {
        active.insert(component, tasks);
      }
    }
    let status = snapshot(
      &context,
      context.shared.active_jobs.load(Ordering::Acquire),
      context.shared.pending_jobs.load(Ordering::Acquire),
    );
    let mut comparison = status.clone();
    comparison.as_object_mut().unwrap().remove("updated_at_ms");
    if comparison != saved_status && files::write(&directory.join("status.json"), &status).is_ok() {
      saved_status = comparison;
    }
    context.wait(TICK);
  }
  // Acknowledgment is asynchronous, but the process retains its lease until
  // every component has joined, including its cancelled network transfers.
  let _ = files::write(
    &directory.join("status.json"),
    &snapshot(
      &context,
      context.shared.active_jobs.load(Ordering::Acquire),
      context.shared.pending_jobs.load(Ordering::Acquire),
    ),
  );
  for (component, tasks) in active {
    for task in tasks {
      let _ = task.join();
    }
    finish_component(&context, component);
  }
  let mut final_status = snapshot(
    &context,
    0,
    context.shared.pending_jobs.load(Ordering::Acquire),
  );
  final_status["running"] = json!(false);
  final_status["ready"] = json!(false);
  final_status["status"] = json!("stopped");
  files::write(&directory.join("status.json"), &final_status)?;
  if let Some(failure) = failure {
    return Err(failure.into());
  }
  Ok(())
}

fn finish_part(context: &ServiceContext, successful: bool) {
  let component = context.component.unwrap();
  let mut components = context
    .shared
    .components
    .lock()
    .unwrap_or_else(|poison| poison.into_inner());
  let state = components.get_mut(&component).unwrap();
  if state.generation != context.generation {
    return;
  }
  let part = state.parts.get_mut(context.part.unwrap()).unwrap();
  let stopped = context.is_stopping();
  let inactive = match part.detail["status"].as_str() {
    Some("disabled") => Some("disabled"),
    Some("unconfigured") => Some("unconfigured"),
    Some("paused") => Some("paused"),
    _ => None,
  };
  part.phase = if !successful {
    "failed"
  } else if stopped {
    "stopped"
  } else {
    inactive.unwrap_or("failed")
  };
  part.detail["status"] = json!(part.phase);
  part.detail["ready"] = json!(false);
  if part.phase == "failed" && part.detail["last_error"].as_str().is_none() {
    part.detail["last_error"] =
      json!("Component stopped unexpectedly; inspect the private service log");
  }
  let phase = part.phase;
  let last_error = part.detail["last_error"].clone();
  state.refresh_phase();
  if component == ServiceComponent::QueueExecution {
    let mut execution = context
      .shared
      .execution
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    execution["status"] = json!(phase);
    execution["online"] = json!(false);
  }
  if component == ServiceComponent::Collection && context.part == Some(component.as_str()) {
    let mut collection = context
      .shared
      .collection
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    collection.ready = false;
    collection.detail["status"] = json!(phase);
    if phase == "failed" && collection.detail["last_error"].as_str().is_none() {
      collection.detail["last_error"] = last_error;
    }
  }
}

fn finish_component(context: &ServiceContext, component: ServiceComponent) {
  let mut components = context
    .shared
    .components
    .lock()
    .unwrap_or_else(|poison| poison.into_inner());
  let state = components.get_mut(&component).unwrap();
  let stopped = state
    .stop
    .take()
    .is_some_and(|stop| stop.load(Ordering::Acquire));
  state.phase = if stopped {
    "stopped"
  } else if state.parts.values().any(|part| part.phase == "failed") {
    "failed"
  } else if state.parts.values().any(|part| part.phase == "paused") {
    "paused"
  } else if state.parts.values().any(|part| part.phase == "disabled") {
    "disabled"
  } else {
    "unconfigured"
  };
  state.detail["status"] = json!(state.phase);
  state.detail["ready"] = json!(false);
}

struct PublishingJobs {
  tasks: BTreeMap<PathBuf, JoinHandle<Result<()>>>,
  stop: Arc<AtomicBool>,
}

impl Drop for PublishingJobs {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    for (_, task) in std::mem::take(&mut self.tasks) {
      let _ = task.join();
    }
    let _ = crate::publishing::drain_jobs(&self.stop);
  }
}

fn run_publishing(directory: &Path, context: ServiceContext, runner: Runner) -> Result<()> {
  let mut jobs = PublishingJobs {
    tasks: BTreeMap::new(),
    stop: context.stop_flag(),
  };
  let active = &mut jobs.tasks;
  let mut cooldowns = BTreeMap::<PathBuf, Instant>::new();
  let mut next_scan = Instant::now();
  let mut pending_count = 0;
  context.component_status(json!({"status": "running", "job_concurrency": JOBS}));
  while !context.is_stopping() {
    let finished: Vec<_> = active
      .iter()
      .filter(|(_, task)| task.is_finished())
      .map(|(path, _)| path.clone())
      .collect();
    let completed_job = !finished.is_empty();
    for path in finished {
      if let Some(job) = active.remove(&path) {
        match job.join() {
          Ok(Ok(())) => {
            cooldowns.remove(&path);
          }
          _ => {
            cooldowns.insert(path.clone(), Instant::now() + Duration::from_secs(5));
            record_local_failure(&path);
          }
        }
      }
    }
    if completed_job
      || context.shared.rescan.swap(false, Ordering::AcqRel)
      || Instant::now() >= next_scan
    {
      next_scan = Instant::now() + Duration::from_secs(1);
      match registry::pending(directory) {
        Ok(pending) => {
          pending_count = pending.len();
          for path in pending {
            if context.is_stopping() {
              break;
            }
            if active.len() >= JOBS {
              break;
            }
            if active.contains_key(&path)
              || cooldowns
                .get(&path)
                .is_some_and(|deadline| *deadline > Instant::now())
            {
              continue;
            }
            if let Ok(state) = registry::read_state(&path) {
              let now = registry::now();
              if state.retry_at_ms > now {
                // A useful bounded slice can be due before the ordinary idle
                // scan. Keep its deadline without changing the fair job order.
                let delay =
                  Duration::from_millis(state.retry_at_ms.saturating_sub(now).min(1_000) as u64);
                next_scan = next_scan.min(Instant::now() + delay);
                continue;
              }
            }
            let stop = context.stop_flag();
            let runner = runner.clone();
            let owned_path = path.clone();
            match thread::Builder::new()
              .name("expri-publishing".into())
              .spawn(move || run_slice(&owned_path, stop, &runner))
            {
              Ok(task) => {
                active.insert(path, task);
              }
              Err(_) => warning(
                &context,
                "A publishing worker could not start; saved jobs remain queued",
              ),
            }
          }
        }
        Err(_) => warning(
          &context,
          "Publishing registry could not be inspected; saved jobs remain intact",
        ),
      }
    }
    context
      .shared
      .active_jobs
      .store(active.len(), Ordering::Release);
    context
      .shared
      .pending_jobs
      .store(pending_count, Ordering::Release);
    context.component_status(json!({"active_jobs": active.len(), "pending_jobs": pending_count}));
    context.wait(TICK);
  }
  for (_, job) in std::mem::take(active) {
    let _ = job.join();
  }
  let result = crate::publishing::drain_jobs(&context.stop_flag());
  context.shared.active_jobs.store(0, Ordering::Release);
  context.component_status(json!({"active_jobs": 0}));
  result
}

fn warning(context: &ServiceContext, detail: &str) {
  context.component_status(json!({"warnings": [detail]}));
}

fn record_local_failure(directory: &Path) {
  if let Ok(mut state) = registry::read_state(directory) {
    state.status = "retrying".into();
    state.last_error = Some("Saved publishing registration or local files could not be read or updated; restore access to the run and retry".into());
    state.retry_at_ms = registry::now() + 5_000;
    state.failures = state.failures.saturating_add(1);
    let _ = state.save_if_current(directory);
  }
}

fn run_slice(directory: &Path, stop: Arc<AtomicBool>, runner: &Runner) -> Result<()> {
  let job = registry::read_job(directory)?;
  let mut state = registry::read_state(directory)?;
  state.status = "publishing".into();
  if !state.save_if_current(directory)? {
    return Ok(());
  }
  let mut backlog_remaining = false;
  let result = runner(&job, stop.clone(), &mut |progress| {
    // Only a fresh successful slice may request prompt continuation. Persisted
    // progress can be stale when a legacy publisher currently owns the lease.
    backlog_remaining = progress["backlog_remaining"].as_bool() == Some(true);
    state.progress = redact_value(progress, &job);
    if state.save_if_current(directory)? {
      Ok(())
    } else {
      Err(error("publication epoch changed during publishing slice"))
    }
  });
  match result {
    Ok(true) => {
      state.status = "published".into();
      state.last_error = None;
      state.failures = 0;
      state.retry_at_ms = 0;
    }
    Ok(false) => {
      state.status = "queued".into();
      state.last_error = None;
      state.failures = 0;
      state.retry_at_ms = if stop.load(Ordering::Acquire) {
        0
      } else if backlog_remaining {
        registry::now() + 100
      } else {
        registry::now() + 5_000
      };
    }
    Err(failure) => {
      let permanent = matches!(
        failure,
        ExpriError::ServiceRejected {
          status: 401 | 403 | 410,
          ..
        }
      );
      state.status = if permanent { "error" } else { "retrying" }.into();
      let detail = job.redact(&failure.to_string());
      state.last_error = Some(
        detail
          .chars()
          .filter(|character| !character.is_control())
          .take(512)
          .collect(),
      );
      state.retry_at_ms =
        registry::now() + ((5u64 << state.failures.min(4)).min(60) * 1_000) as i64;
      state.failures = state.failures.saturating_add(1);
    }
  }
  state.save_if_current(directory).map(|_| ())
}

fn redact_value(value: Value, job: &PreparedJob) -> Value {
  match value {
    Value::String(value) => json!(job.redact(&value)),
    Value::Array(values) => Value::Array(
      values
        .into_iter()
        .map(|value| redact_value(value, job))
        .collect(),
    ),
    Value::Object(values) => Value::Object(
      values
        .into_iter()
        .map(|(key, value)| (key, redact_value(value, job)))
        .collect(),
    ),
    value => value,
  }
}

pub(in crate::worker_service) fn snapshot(
  context: &ServiceContext,
  active: usize,
  pending: usize,
) -> Value {
  let state = context
    .shared
    .collection
    .lock()
    .unwrap_or_else(|poison| poison.into_inner())
    .clone();
  let status = if context.is_stopping() {
    "stopping"
  } else {
    "running"
  };
  let components: BTreeMap<_, _> = context
    .shared
    .components
    .lock()
    .unwrap_or_else(|poison| poison.into_inner())
    .iter()
    .map(|(component, state)| (component.as_str(), state.snapshot()))
    .collect();
  json!({"schema_version": 1, "pid": std::process::id(), "running": true,
    "capabilities": [expri_core::experiment_assignment::EXPERIMENT_ASSIGNMENT_CAPABILITY],
    "ready": !context.is_stopping(), "collection_ready": state.ready && !context.is_stopping(), "status": status,
    "collection": state.detail, "active_jobs": active, "pending_jobs": pending,
    "execution": context.shared.execution.lock().unwrap_or_else(|poison| poison.into_inner()).clone(),
    "components": components,
    "job_concurrency": JOBS, "updated_at_ms": registry::now()})
}

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
  fn drop(&mut self) {
    let _ = fs::remove_file(&self.0);
  }
}
