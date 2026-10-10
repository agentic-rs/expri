//! Checkpoint transfers have their own lane so slow object uploads never block metrics.

use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};

use serde_json::json;

use super::{Api, Publisher, Queue, fs, run_target};
use crate::error::Result;
use crate::service::registrations::{self, Registration};
use crate::service::types::RunScope;

#[derive(Default)]
pub(super) struct CheckpointLane {
  task: Option<JoinHandle<Result<()>>>,
}

impl CheckpointLane {
  pub(super) fn poll(
    &mut self,
    config: &Path,
    run_dir: &Path,
    scope: &RunScope,
    queue_dir: &Path,
    watch: bool,
  ) -> Result<bool> {
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
            config,
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
      let config = config.to_path_buf();
      let run_dir = run_dir.to_path_buf();
      let scope = scope.clone();
      let directory = queue_dir.join("checkpoints");
      let registration = registration.clone();
      self.task = Some(thread::spawn(move || {
        transfer(&config, &run_dir, &scope, directory, &registration)
      }));
    }
    Ok(self.task.is_none() && registrations.iter().all(|file| file.sync_status == "cloud"))
  }
}

fn transfer(
  config: &Path,
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
    let api = Api::new(config)?;
    let mut queue = Queue::new(directory, json!({"endpoint":api.endpoint,"scope":scope}))?;
    super::upload::sync_registered(
      &api,
      &mut queue,
      &registration.path,
      run_target(scope, &registration.path),
      &run_dir.join(&registration.path),
      registration,
    )?;
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
      let detail = if Publisher::permanent_rejection(error).is_some() {
        "Checkpoint publication was rejected; inspect publisher status"
      } else {
        "Checkpoint upload will retry; completed multipart parts are saved"
      };
      registrations::update(run_dir, registration, "registered", Some(detail))?;
    }
  }
  result
}
