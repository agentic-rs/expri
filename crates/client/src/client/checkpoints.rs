//! Checkpoint transfers have their own lane so slow object uploads never block metrics.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use serde_json::json;

use super::{Api, Publisher, Queue, fs, run_target};
use crate::error::Result;
use crate::registrations::{self, Registration};
use crate::types::RunScope;

#[derive(Default)]
pub(super) struct CheckpointLane {
  task: Option<JoinHandle<Result<()>>>,
}

const SHARED_TRANSFERS: usize = 2;
const FINISHED_RECEIPTS: usize = 128;
enum TransferOutcome {
  Registered,
  Artifact(Box<super::SavedFile>),
}
struct SharedTask {
  api: Api,
  task: JoinHandle<Result<TransferOutcome>>,
}
type SharedTransfers = BTreeMap<String, SharedTask>;
static SHARED: OnceLock<Mutex<SharedTransfers>> = OnceLock::new();

impl CheckpointLane {
  pub(super) fn poll(
    &mut self,
    api: &Api,
    run_dir: &Path,
    scope: &RunScope,
    queue_dir: &Path,
    watch: bool,
  ) -> Result<bool> {
    if api.cooperative() {
      return shared_poll(api, run_dir, scope, queue_dir);
    }
    if self.task.as_ref().is_some_and(JoinHandle::is_finished) {
      let result = self
        .task
        .take()
        .unwrap()
        .join()
        .map_err(|_| fs::message("checkpoint publisher exited unexpectedly"))?;
      if let Err(error) = result
        && Publisher::permanent_rejection(&error).is_some()
      {
        return Err(error);
      }
    }
    let registrations = registrations::records(run_dir)?;
    if !watch {
      for registration in &registrations {
        if registration.sync_status == "needs_attention" {
          return Err(fs::message(
            "registered checkpoint requires attention; inspect artifact status",
          ));
        }
        if registration.sync_status != "cloud" {
          transfer(
            api,
            run_dir,
            scope,
            queue_dir.join("checkpoints"),
            registration,
          )?;
        }
      }
      return Ok(
        registrations::records(run_dir)?
          .iter()
          .all(|file| file.sync_status == "cloud"),
      );
    }
    if self.task.is_none()
      && let Some(registration) = registrations
        .iter()
        .find(|file| matches!(file.sync_status.as_str(), "registered" | "uploading"))
    {
      let api = api.clone();
      let run_dir = run_dir.to_path_buf();
      let scope = scope.clone();
      let directory = queue_dir.join("checkpoints");
      let registration = registration.clone();
      self.task = Some(thread::spawn(move || {
        transfer(&api, &run_dir, &scope, directory, &registration)
      }));
    }
    Ok(self.task.is_none() && registrations.iter().all(|file| file.sync_status == "cloud"))
  }
}

fn shared_poll(api: &Api, run_dir: &Path, scope: &RunScope, queue_dir: &Path) -> Result<bool> {
  api.check_cancelled()?;
  let key =
    json!({"binding":api.binding_id(), "run_dir":run_dir, "scope":scope, "queue_dir":queue_dir})
      .to_string();
  let mut tasks = SHARED
    .get_or_init(|| Mutex::new(BTreeMap::new()))
    .lock()
    .map_err(|_| fs::message("shared checkpoint scheduling is unavailable"))?;
  if tasks.get(&key).is_some_and(|task| task.task.is_finished()) {
    let result = tasks
      .remove(&key)
      .unwrap()
      .task
      .join()
      .map_err(|_| fs::message("checkpoint publisher exited unexpectedly"))?;
    if let Err(error) = result
      && Publisher::permanent_rejection(&error).is_some()
    {
      return Err(error);
    }
  }
  let registrations = registrations::records(run_dir)?;
  if tasks.contains_key(&key) {
    return Ok(false);
  }
  let active = tasks
    .values()
    .filter(|task| !task.task.is_finished())
    .count();
  if active >= SHARED_TRANSFERS {
    return Ok(registrations.iter().all(|file| file.sync_status == "cloud"));
  }
  // Finished unobserved tasks hold no leases. Bound retained handles/clients too;
  // durable registration state retains completion or needs-attention data.
  if tasks.len() >= FINISHED_RECEIPTS {
    let finished = tasks
      .iter()
      .find_map(|(key, task)| task.task.is_finished().then(|| key.clone()));
    if let Some(finished) = finished {
      let _ = tasks.remove(&finished).unwrap().task.join();
    }
  }
  if let Some(registration) = registrations
    .iter()
    .find(|file| matches!(file.sync_status.as_str(), "registered" | "uploading"))
  {
    api.check_cancelled()?;
    let api = api.clone();
    let run_dir = run_dir.to_path_buf();
    let scope = scope.clone();
    let directory = queue_dir.join("checkpoints");
    let registration = registration.clone();
    let binding = api.clone();
    let task = thread::Builder::new()
      .name("expri-checkpoint-upload".into())
      .spawn(move || {
        transfer(&api, &run_dir, &scope, directory, &registration)
          .map(|_| TransferOutcome::Registered)
      })?;
    tasks.insert(key, SharedTask { api: binding, task });
    return Ok(false);
  }
  Ok(registrations.iter().all(|file| file.sync_status == "cloud"))
}

pub(super) fn artifact(
  api: &Api,
  queue: &mut Queue,
  scope: &RunScope,
  run_dir: &Path,
  path: &str,
) -> Result<bool> {
  api.check_cancelled()?;
  if queue
    .state
    .files
    .get(path)
    .is_some_and(|saved| saved.upload.complete)
  {
    return Ok(true);
  }
  let key = json!({"binding":api.binding_id(), "run_dir":run_dir, "scope":scope, "queue_dir":queue.directory, "artifact":path}).to_string();
  let mut tasks = SHARED
    .get_or_init(|| Mutex::new(BTreeMap::new()))
    .lock()
    .map_err(|_| fs::message("shared artifact scheduling is unavailable"))?;
  if tasks.get(&key).is_some_and(|task| task.task.is_finished()) {
    match tasks
      .remove(&key)
      .unwrap()
      .task
      .join()
      .map_err(|_| fs::message("artifact publisher exited unexpectedly"))??
    {
      TransferOutcome::Artifact(saved) => {
        queue.state.files.insert(path.into(), *saved);
        queue.save()?;
        return Ok(true);
      }
      TransferOutcome::Registered => {
        return Err(fs::message("artifact publisher returned another transfer"));
      }
    }
  }
  if tasks.contains_key(&key)
    || tasks
      .values()
      .filter(|task| !task.task.is_finished())
      .count()
      >= SHARED_TRANSFERS
  {
    return Ok(false);
  }
  if tasks.len() >= FINISHED_RECEIPTS
    && let Some(finished) = tasks
      .iter()
      .find_map(|(key, task)| task.task.is_finished().then(|| key.clone()))
  {
    let _ = tasks.remove(&finished).unwrap().task.join();
  }
  api.check_cancelled()?;
  let api = api.clone();
  let binding = api.clone();
  let scope = scope.clone();
  use sha2::{Digest, Sha256};
  let artifact_id = Sha256::digest(path.as_bytes())
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect::<String>();
  let directory = queue.directory.join("artifacts").join(artifact_id);
  let source = run_dir.join(path);
  let path = path.to_owned();
  let task = thread::Builder::new()
    .name("expri-artifact-upload".into())
    .spawn(move || {
      let mut queue = Queue::new(directory, json!({"endpoint":api.endpoint, "scope":scope}))?;
      super::upload::sync_file(
        &api,
        &mut queue,
        &path,
        run_target(&scope, &path),
        &source,
        false,
      )?;
      Ok(TransferOutcome::Artifact(Box::new(
        queue.state.files[&path].clone(),
      )))
    })?;
  tasks.insert(key, SharedTask { api: binding, task });
  Ok(false)
}

pub(crate) fn drain(stop: &std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<()> {
  stop.store(true, std::sync::atomic::Ordering::Release);
  let Some(shared) = SHARED.get() else {
    return Ok(());
  };
  let tasks = {
    let mut shared = shared
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    let keys = shared
      .iter()
      .filter(|(_, task)| task.api.belongs_to(stop))
      .map(|(key, _)| key.clone())
      .collect::<Vec<_>>();
    keys
      .into_iter()
      .map(|key| {
        let task = shared.remove(&key).unwrap();
        task.api.cancel();
        task
      })
      .collect::<Vec<_>>()
  };
  let mut panicked = false;
  for task in tasks {
    panicked |= task.task.join().is_err();
  }
  if panicked {
    return Err(fs::message(
      "an upload worker exited unexpectedly during stop",
    ));
  }
  Ok(())
}

fn transfer(
  api: &Api,
  run_dir: &Path,
  scope: &RunScope,
  directory: PathBuf,
  registration: &Registration,
) -> Result<()> {
  if registrations::verify(run_dir, registration).is_err() {
    registrations::update(
      run_dir,
      registration,
      "needs_attention",
      Some(
        "Registered checkpoint changed or is unavailable; unregister it and finalize the corrected checkpoint under a new path",
      ),
    )?;
    return Ok(());
  }
  registrations::update(run_dir, registration, "uploading", None)?;
  let result = (|| {
    let mut queue = Queue::new(directory, json!({"endpoint":api.endpoint,"scope":scope}))?;
    super::upload::sync_registered(
      api,
      &mut queue,
      &registration.path,
      run_target(scope, &registration.path),
      &run_dir.join(&registration.path),
      registration,
    )?;
    if !queue.state.files[&registration.path].upload.complete {
      return Ok(());
    }
    registrations::verify(run_dir, registration)?;
    registrations::complete(
      run_dir,
      registration,
      &queue.state.files[&registration.path].sha256,
    )
  })();
  if let Err(error) = &result {
    if registrations::verify(run_dir, registration).is_err() {
      registrations::update(
        run_dir,
        registration,
        "needs_attention",
        Some(
          "Registered checkpoint changed or is unavailable; unregister it and finalize the corrected checkpoint under a new path",
        ),
      )?;
    } else {
      let rejected = Publisher::permanent_rejection(error).is_some();
      let detail = if rejected {
        "Checkpoint publication was rejected; inspect publisher status"
      } else {
        "Checkpoint upload will retry; completed multipart parts are saved"
      };
      registrations::update(
        run_dir,
        registration,
        if rejected {
          "needs_attention"
        } else {
          "registered"
        },
        Some(detail),
      )?;
    }
  }
  result
}
