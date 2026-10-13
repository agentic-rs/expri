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

use super::super::{ServiceContext, error, files, registry};
use super::signals::{SIGNAL_STOP, SignalGuard};
use super::{TICK, handle, socket, validate_socket};
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

pub(in crate::worker_service) fn run_runtime(
  paths: &StoragePaths,
  collection_hook: impl FnOnce(ServiceContext) -> Result<()> + Send + 'static,
  runner: Runner,
  signals: bool,
) -> Result<()> {
  let directory = files::ensure(paths)?;
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
  let context = ServiceContext::new();
  let initial = snapshot(&context, 0, 0);
  files::write(&directory.join("status.json"), &initial)?;
  let collector_context = context.clone();
  let collector = thread::Builder::new()
    .name("expri-collection".into())
    .spawn(move || {
      let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        collection_hook(collector_context.clone())
      }))
      .unwrap_or_else(|_| Err(error("local collection worker panicked")));
      if result.is_err() || !collector_context.is_stopping() {
        let mut state = collector_context
          .state
          .lock()
          .unwrap_or_else(|poison| poison.into_inner());
        state.ready = false;
        state.detail["status"] = json!("failed");
        if state.detail["last_error"].as_str().is_none() {
          state.detail["last_error"] =
            json!("Local collection stopped unexpectedly; inspect the private service log");
        }
      }
      result
    })?;
  let mut active = BTreeMap::<PathBuf, JoinHandle<Result<()>>>::new();
  let mut cooldowns = BTreeMap::<PathBuf, Instant>::new();
  let mut saved_status = Value::Null;
  let mut next_scan = Instant::now();
  let mut pending_count = 0;
  loop {
    if signals && SIGNAL_STOP.load(Ordering::Acquire) {
      context.request_stop();
    }
    for _ in 0..32 {
      match listener.accept() {
        Ok((mut stream, _)) => {
          let _ = handle(&mut stream, &context, active.len(), pending_count);
          if context.is_stopping() {
            break;
          }
        }
        Err(failure) if failure.kind() == std::io::ErrorKind::WouldBlock => break,
        Err(failure) => {
          context.request_stop();
          for (_, job) in active {
            let _ = job.join();
          }
          let _ = collector.join();
          let _ = crate::publishing::drain_jobs(&context.stop_flag());
          return Err(failure.into());
        }
      }
    }
    let finished: Vec<_> = active
      .iter()
      .filter(|(_, job)| job.is_finished())
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
    if context.is_stopping() {
      break;
    }
    if completed_job || context.rescan.swap(false, Ordering::AcqRel) || Instant::now() >= next_scan
    {
      next_scan = Instant::now() + Duration::from_secs(1);
      match registry::pending(&directory) {
        Ok(pending) => {
          pending_count = pending.len();
          for path in pending {
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
    let status = snapshot(&context, active.len(), pending_count);
    let mut comparison = status.clone();
    comparison.as_object_mut().unwrap().remove("updated_at_ms");
    if comparison != saved_status && files::write(&directory.join("status.json"), &status).is_ok() {
      saved_status = comparison;
    }
    context.wait(TICK);
  }
  // IPC stop is already acknowledged. Remaining requests are refused while local
  // collection finishes its final fence and HTTP lanes checkpoint cancellations.
  let _ = files::write(
    &directory.join("status.json"),
    &snapshot(&context, active.len(), pending_count),
  );
  for (_, job) in active {
    let _ = job.join();
  }
  let collection_result = collector
    .join()
    .unwrap_or_else(|_| Err(error("local collection worker panicked")));
  let upload_result = crate::publishing::drain_jobs(&context.stop_flag());
  let mut final_status = snapshot(&context, 0, pending_count);
  final_status["running"] = json!(false);
  final_status["ready"] = json!(false);
  final_status["status"] = json!(if final_status["collection"]["status"] == "failed" {
    "collection_failed"
  } else {
    "stopped"
  });
  files::write(&directory.join("status.json"), &final_status)?;
  collection_result.map_err(|_| error("local collection failed; inspect service status"))?;
  upload_result
}

fn warning(context: &ServiceContext, detail: &str) {
  context.collection_status(json!({"warnings": [detail]}));
}

fn record_local_failure(directory: &Path) {
  if let Ok(mut state) = registry::read_state(directory) {
    state.status = "retrying".into();
    state.last_error = Some("Saved publishing registration or local files could not be read or updated; restore access to the run and retry".into());
    state.retry_at_ms = registry::now() + 5_000;
    state.failures = state.failures.saturating_add(1);
    let _ = state.save(directory);
  }
}

fn run_slice(directory: &Path, stop: Arc<AtomicBool>, runner: &Runner) -> Result<()> {
  let job = registry::read_job(directory)?;
  let mut state = registry::read_state(directory)?;
  state.status = "publishing".into();
  state.save(directory)?;
  let mut backlog_remaining = false;
  let result = runner(&job, stop.clone(), &mut |progress| {
    // Only a fresh successful slice may request prompt continuation. Persisted
    // progress can be stale when a legacy publisher currently owns the lease.
    backlog_remaining = progress["backlog_remaining"].as_bool() == Some(true);
    state.progress = redact_value(progress, &job);
    state.save(directory)
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
  state.save(directory)
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
    .state
    .lock()
    .unwrap_or_else(|poison| poison.into_inner());
  let status = if context.is_stopping() {
    "stopping"
  } else if state.ready {
    "running"
  } else if state.detail["status"] == "failed" {
    "collection_failed"
  } else {
    "starting"
  };
  json!({"schema_version": 1, "pid": std::process::id(), "running": true,
    "ready": state.ready && !context.is_stopping(), "status": status,
    "collection": state.detail, "active_jobs": active, "pending_jobs": pending,
    "execution": context.execution.lock().unwrap_or_else(|poison| poison.into_inner()).clone(),
    "job_concurrency": JOBS, "updated_at_ms": registry::now()})
}

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
  fn drop(&mut self) {
    let _ = fs::remove_file(&self.0);
  }
}
