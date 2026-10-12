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

impl Dashboard {
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
