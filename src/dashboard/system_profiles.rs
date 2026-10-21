//! Local dashboard reads never create a collector, profile database, or machine identity.

use expri_core::identifiers::validate_component;
use expri_core::service::MAX_SYSTEM_MACHINES;
use expri_core::system_profile::{MachineProfile, ProfileHistory, ProfileHistoryQuery};
use expri_dashboard::dashboard::SystemMachines;
use expri_system::store::LocalStore;

use super::{Dashboard, ExpriError, Result};

fn missing() -> ExpriError {
  ExpriError::ServiceRejected {
    status: 404,
    detail: "system machine or inventory revision is missing".into(),
  }
}

struct SystemDeadlineReader<R> {
  reader: R,
  deadline: std::time::Instant,
}

impl<R: std::io::Read> std::io::Read for SystemDeadlineReader<R> {
  fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
    if std::time::Instant::now() >= self.deadline {
      return Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "run system read deadline exceeded",
      ));
    }
    self.reader.read(output)
  }
}

impl Dashboard {
  pub(super) fn run_system_metrics(
    &self,
    source: &str,
    run_id: &str,
    cursor: Option<u64>,
    limit: usize,
  ) -> Result<expri_runs::system_metrics::RunSystemMetrics> {
    use expri_runs::system_metrics::{RunSystemMetrics, RunSystemWindow};
    let (_, runs_dir) = self.source(source)?;
    let summary = expri_runs::runs::summary_directory(&runs_dir, run_id)?;
    let run_dir = runs_dir.join(run_id);
    let state_relative = run_dir
      .strip_prefix(&self.repo_root)
      .map_err(|_| ExpriError::Message("run metadata is outside the repository".into()))?
      .join("run-state.json");
    use std::io::Read;
    let state =
      if let Some(file) = super::artifacts::open_beneath(&self.repo_root, &state_relative)? {
        let mut bytes = Vec::new();
        file
          .take((expri_core::run_state::MAX_STATE_BYTES + 1) as u64)
          .read_to_end(&mut bytes)?;
        if bytes.len() > expri_core::run_state::MAX_STATE_BYTES {
          serde_json::Value::Null
        } else {
          serde_json::from_slice(&bytes).unwrap_or_default()
        }
      } else {
        serde_json::Value::Null
      };
    let Some(window) = RunSystemWindow::from_state(run_id, &state) else {
      return Ok(RunSystemMetrics::unavailable_for_run(
        run_id,
        &summary["run"],
        "Validated run capture identity and period are unavailable.",
      ));
    };
    let relative = run_dir
      .strip_prefix(&self.repo_root)
      .map_err(|_| ExpriError::Message("run system capture is outside the repository".into()))?
      .join(expri_core::run_system::RUN_SYSTEM_PATH);
    let Some(file) = super::artifacts::open_beneath(&self.repo_root, &relative)? else {
      return Ok(RunSystemMetrics::unavailable_for_run(
        run_id,
        &summary["run"],
        "This run has no captured system observations.",
      ));
    };
    let size = file.metadata()?.len();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    expri_runs::system_metrics::read_minutes(
      SystemDeadlineReader {
        reader: file.take(size),
        deadline,
      },
      || {
        super::artifacts::open_beneath(&self.repo_root, &relative)?
          .map(|file| SystemDeadlineReader {
            reader: file.take(size),
            deadline,
          })
          .ok_or_else(|| ExpriError::Message("run system capture changed during review".into()))
      },
      run_id,
      &window,
      cursor,
      limit,
    )
  }

  fn local_system_store(&self) -> Result<Option<LocalStore>> {
    self
      .system_profiles_path
      .as_deref()
      .map(LocalStore::open_readonly)
      .transpose()
      .map(Option::flatten)
  }

  pub(super) fn system_machines_page(
    &self,
    limit: usize,
    cursor: Option<&str>,
  ) -> Result<Option<SystemMachines>> {
    if !(1..=MAX_SYSTEM_MACHINES).contains(&limit) {
      return Err(ExpriError::Message(
        "system machine limit must be between 1 and 100".into(),
      ));
    }
    if let Some(cursor) = cursor {
      validate_component(cursor)?;
    }
    let Some(store) = self.local_system_store()? else {
      return Ok(None);
    };
    let machines = if cursor.is_some_and(|cursor| store.machine_id() <= cursor) {
      Vec::new()
    } else {
      store.profile()?.into_iter().collect()
    };
    Ok(Some(SystemMachines {
      machines,
      next_cursor: None,
    }))
  }

  pub(super) fn system_machine_profile(
    &self,
    machine_id: &str,
    inventory_revision: Option<u64>,
  ) -> Result<Option<MachineProfile>> {
    validate_component(machine_id)?;
    if inventory_revision.is_some_and(|revision| revision == 0 || revision > i64::MAX as u64) {
      return Err(ExpriError::Message(
        "invalid system inventory revision".into(),
      ));
    }
    let Some(store) = self.local_system_store()? else {
      return Ok(None);
    };
    if machine_id != store.machine_id() {
      return Err(missing());
    }
    store
      .inventory(inventory_revision)?
      .map(Some)
      .ok_or_else(missing)
  }

  pub(super) fn system_machine_history(
    &self,
    query: &ProfileHistoryQuery,
  ) -> Result<Option<ProfileHistory>> {
    query.validate()?;
    let Some(store) = self.local_system_store()? else {
      return Ok(None);
    };
    if query.machine_id != store.machine_id() || store.profile()?.is_none() {
      return Err(missing());
    }
    store.history(query).map(Some)
  }
}

#[cfg(test)]
mod tests;
