//! Durable manual component choices. Automatic startup never changes these choices.
use std::collections::BTreeMap;
use std::path::Path;

use expri_core::error::Result;
use expri_core::lock::{FileLock, LockAttempt, try_lock_file};
use serde::{Deserialize, Serialize};

use super::{ServiceComponent, error, files};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Preferences {
  schema_version: u32,
  desired_running: BTreeMap<ServiceComponent, bool>,
}

impl Default for Preferences {
  fn default() -> Self {
    Self {
      schema_version: 1,
      desired_running: BTreeMap::new(),
    }
  }
}

impl Preferences {
  pub(super) fn enabled(&self, component: ServiceComponent) -> bool {
    self
      .desired_running
      .get(&component)
      .copied()
      .unwrap_or(true)
  }

  pub(super) fn load(directory: &Path) -> Result<Self> {
    let path = directory.join("components.json");
    if !path.try_exists()? {
      return Ok(Self::default());
    }
    let value: Self = serde_json::from_slice(&files::read(&path)?)
      .map_err(|_| error("saved component choices are invalid"))?;
    if value.schema_version != 1 {
      return Err(error("saved component choices have an unsupported version"));
    }
    Ok(value)
  }

  pub(super) fn save_choice(
    directory: &Path,
    component: ServiceComponent,
    enabled: bool,
  ) -> Result<()> {
    let _lease = Self::lock(directory)?;
    Self::save_choice_locked(directory, component, enabled)
  }

  pub(super) fn lock(directory: &Path) -> Result<FileLock> {
    let path = directory.join("components.lock");
    files::private_file(&path)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
      if let LockAttempt::Acquired(lease) = try_lock_file(&path, false)? {
        return Ok(lease);
      }
      if std::time::Instant::now() >= deadline {
        return Err(error("component choices are busy; retry shortly"));
      }
      std::thread::sleep(std::time::Duration::from_millis(10));
    }
  }

  pub(super) fn save_choice_locked(
    directory: &Path,
    component: ServiceComponent,
    enabled: bool,
  ) -> Result<()> {
    let mut preferences = Self::load(directory)?;
    preferences.desired_running.insert(component, enabled);
    files::write(&directory.join("components.json"), &preferences)
  }
}
