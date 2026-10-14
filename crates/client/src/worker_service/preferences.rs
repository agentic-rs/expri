//! Durable manual component choices. Automatic startup never changes these choices.
use std::collections::BTreeMap;
use std::path::Path;

use expri_core::error::Result;
use expri_core::lock::{FileLock, LockAttempt, try_lock_file};
use serde::{Deserialize, Serialize};

use super::{ServiceComponent, error, files};

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Preferences {
  #[serde(default)]
  components: BTreeMap<ServiceComponent, ComponentChoice>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ComponentChoice {
  Enable,
  Disable,
}

impl Preferences {
  pub(super) fn enabled(&self, component: ServiceComponent) -> bool {
    !matches!(
      self.components.get(&component),
      Some(ComponentChoice::Disable)
    )
  }

  pub(super) fn load(root: &Path) -> Result<Self> {
    let path = root.join("service.json");
    let metadata = match std::fs::symlink_metadata(&path) {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
      Err(error) => return Err(error.into()),
    };
    #[cfg(unix)]
    {
      use std::os::unix::fs::MetadataExt;
      if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
        return Err(error(
          "service settings must be owned by the current user without hardlinks",
        ));
      }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    let Some(text) = expri_core::global_config::read_config_text(&path)? else {
      return Err(error("service settings changed while reading"));
    };
    serde_json::from_str(&text).map_err(|_| error("saved service component choices are invalid"))
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
    let root = directory
      .parent()
      .ok_or_else(|| error("service storage needs a parent"))?;
    let mut preferences = Self::load(root)?;
    preferences.components.insert(
      component,
      if enabled {
        ComponentChoice::Enable
      } else {
        ComponentChoice::Disable
      },
    );
    let path = root.join("service.json");
    // Manual nonsecret settings may be 0644. Secure an existing file without
    // truncation before the private replacement, but never create an empty file.
    if path.try_exists()? {
      let _ = files::private_file(&path)?;
    }
    files::write(&path, &preferences)
  }
}
