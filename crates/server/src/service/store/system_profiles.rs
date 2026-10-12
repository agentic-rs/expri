//! Machine telemetry is independent of project/run lifecycle and object storage.

use expri_core::system_profile::{
  DETAIL_RETENTION_MS, MachineProfile, ProfileAcknowledgement, ProfileBatch, ProfileHistory,
  ProfileHistoryQuery, ProfileResolution,
};
use expri_system::store as profiles;

use super::*;

const RETENTION_BATCH: usize = 1024;

fn profile_error(error: crate::error::ExpriError) -> ApiError {
  match error {
    crate::error::ExpriError::ServiceRejected { status, detail } => ApiError::new(status, detail),
    crate::error::ExpriError::Message(detail) => ApiError::new(400, detail),
    _ => ApiError::new(500, "system profile operation failed"),
  }
}

fn now_ms() -> i64 {
  chrono::Utc::now().timestamp_millis().max(0)
}

impl<S: ObjectStorage> Store<S> {
  pub(super) fn put_system_profile(
    &self,
    batch: &ProfileBatch,
  ) -> ApiResult<ProfileAcknowledgement> {
    expri_core::system_profile::validate_profile_batch(batch).map_err(profile_error)?;
    profiles::ingest(&mut *self.db()?, batch, now_ms()).map_err(profile_error)
  }

  pub(super) fn list_system_machines(
    &self,
    limit: usize,
    cursor: Option<&str>,
  ) -> ApiResult<(Vec<MachineProfile>, Option<String>)> {
    if !(1..=MAX_SYSTEM_MACHINES).contains(&limit) {
      return Err(ApiError::new(
        400,
        "system machine limit must be between 1 and 100",
      ));
    }
    if let Some(cursor) = cursor {
      validate_component(cursor).map_err(bad)?;
    }
    profiles::list_machines(&*self.db()?, limit, cursor).map_err(profile_error)
  }

  pub(super) fn system_machine(
    &self,
    machine_id: &str,
    inventory_revision: Option<u64>,
  ) -> ApiResult<MachineProfile> {
    validate_component(machine_id).map_err(bad)?;
    if inventory_revision.is_some_and(|revision| revision == 0 || revision > i64::MAX as u64) {
      return Err(ApiError::new(400, "invalid system inventory revision"));
    }
    profiles::machine_profile(&*self.db()?, machine_id, inventory_revision)
      .map_err(profile_error)?
      .ok_or_else(|| ApiError::new(404, "system machine or inventory revision is missing"))
  }

  pub(super) fn system_history(&self, query: &ProfileHistoryQuery) -> ApiResult<ProfileHistory> {
    self.system_history_at(query, now_ms())
  }

  fn system_history_at(
    &self,
    query: &ProfileHistoryQuery,
    at_ms: i64,
  ) -> ApiResult<ProfileHistory> {
    query.validate().map_err(bad)?;
    self.system_machine(&query.machine_id, None)?;
    let mut selection = query.clone();
    if selection.resolution == ProfileResolution::Detail {
      selection.start_ms = selection
        .start_ms
        .max(at_ms.saturating_sub(DETAIL_RETENTION_MS));
      if selection.end_ms <= selection.start_ms {
        return Ok(ProfileHistory {
          machine_id: selection.machine_id,
          resolution: selection.resolution,
          samples: Vec::new(),
          hours: Vec::new(),
          next_cursor: None,
        });
      }
    }
    profiles::history(&*self.db()?, &selection).map_err(profile_error)
  }

  pub(in crate::service) fn system_profile_retention_cycle(&self) -> ApiResult<usize> {
    self.system_profile_retention_cycle_at(now_ms())
  }

  fn system_profile_retention_cycle_at(&self, at_ms: i64) -> ApiResult<usize> {
    profiles::prune(&mut *self.db()?, at_ms, RETENTION_BATCH).map_err(profile_error)
  }
}

#[cfg(test)]
mod tests;
