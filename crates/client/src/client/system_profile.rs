//! Typed machine-profile operations share the artifact client's bounded transport.

use std::path::Path;

use expri_core::system_profile::{
  MachineProfile, ProfileAcknowledgement, ProfileBatch, ProfileHistory, ProfileHistoryQuery,
  ProfileResolution, SYSTEM_PROFILE_CAPABILITY, validate_hour, validate_inventory,
  validate_profile_batch, validate_sample,
};

use super::{Api, fs};
use crate::error::Result;
use crate::types::{MAX_SYSTEM_MACHINES, Request, Response, validate_component};

pub struct SystemProfileClient {
  api: Api,
}

impl SystemProfileClient {
  pub fn new(config: &Path) -> Result<Self> {
    Self::from_api(Api::new(config)?)
  }

  fn from_api(api: Api) -> Result<Self> {
    let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
      return Err(fs::message("service did not return its capabilities"));
    };
    if !features
      .iter()
      .any(|feature| feature == SYSTEM_PROFILE_CAPABILITY)
    {
      return Err(fs::message("service does not support system-profile-v1"));
    }
    Ok(Self { api })
  }

  /// Normalized authenticated endpoint identity; credentials never enter local state.
  pub fn endpoint_key(&self) -> &str {
    &self.api.endpoint
  }

  pub fn publish(&self, batch: &ProfileBatch) -> Result<ProfileAcknowledgement> {
    validate_profile_batch(batch)?;
    let Response::SystemProfileAcknowledged { acknowledgement } =
      self.api.request(&Request::PutSystemProfile {
        batch: batch.clone(),
      })?
    else {
      return Err(fs::message(
        "service did not acknowledge the system profile",
      ));
    };
    if acknowledgement.machine_id != batch.machine_id
      || acknowledgement.sample_sequence > i64::MAX as u64
      || acknowledgement.inventory_revision > i64::MAX as u64
      || acknowledgement.sample_sequence
        != batch
          .samples
          .iter()
          .map(|sample| sample.sequence)
          .max()
          .unwrap_or(0)
      || acknowledgement.inventory_revision
        != batch
          .inventories
          .iter()
          .map(|inventory| inventory.revision)
          .max()
          .unwrap_or(0)
      || acknowledgement.hour_revisions
        != batch
          .hours
          .iter()
          .map(|hour| (hour.hour_start_ms, hour.revision))
          .collect()
    {
      return Err(fs::message(
        "service returned an inconsistent system profile acknowledgement",
      ));
    }
    Ok(acknowledgement)
  }

  pub fn machines(
    &self,
    limit: usize,
    cursor: Option<&str>,
  ) -> Result<(Vec<MachineProfile>, Option<String>)> {
    if !(1..=MAX_SYSTEM_MACHINES).contains(&limit) {
      return Err(fs::message(
        "system machine limit must be between 1 and 100",
      ));
    }
    if let Some(cursor) = cursor {
      validate_component(cursor)?;
    }
    let Response::SystemMachines {
      machines,
      next_cursor,
    } = self.api.request(&Request::ListSystemMachines {
      limit,
      cursor: cursor.map(str::to_owned),
    })?
    else {
      return Err(fs::message("service did not return a system machine page"));
    };
    if machines.len() > limit
      || machines
        .windows(2)
        .any(|pair| pair[0].machine_id >= pair[1].machine_id)
      || machines
        .first()
        .is_some_and(|machine| cursor.is_some_and(|cursor| machine.machine_id.as_str() <= cursor))
      || next_cursor.as_deref().is_some_and(|next| {
        machines
          .last()
          .is_none_or(|machine| machine.machine_id != next)
      })
    {
      return Err(fs::message(
        "service returned an inconsistent system machine page",
      ));
    }
    for machine in &machines {
      validate_machine(machine)?;
    }
    Ok((machines, next_cursor))
  }

  pub fn machine(
    &self,
    machine_id: &str,
    inventory_revision: Option<u64>,
  ) -> Result<MachineProfile> {
    validate_component(machine_id)?;
    if inventory_revision.is_some_and(|revision| revision == 0 || revision > i64::MAX as u64) {
      return Err(fs::message("invalid system inventory revision"));
    }
    let Response::SystemMachine { machine } = self.api.request(&Request::GetSystemMachine {
      machine_id: machine_id.into(),
      inventory_revision,
    })?
    else {
      return Err(fs::message("service did not return the system machine"));
    };
    validate_machine(&machine)?;
    if machine.machine_id != machine_id
      || inventory_revision.is_some_and(|revision| revision != machine.inventory_revision)
    {
      return Err(fs::message(
        "service returned another system machine or inventory revision",
      ));
    }
    Ok(machine)
  }

  pub fn history(&self, query: &ProfileHistoryQuery) -> Result<ProfileHistory> {
    query.validate()?;
    let Response::SystemHistory { history } = self.api.request(&Request::SystemHistory {
      query: query.clone(),
    })?
    else {
      return Err(fs::message("service did not return system history"));
    };
    if history.machine_id != query.machine_id
      || history.resolution != query.resolution
      || history.samples.len().max(history.hours.len()) > query.limit
    {
      return Err(fs::message("service returned inconsistent system history"));
    }
    let last_cursor = match query.resolution {
      ProfileResolution::Detail => {
        if !history.hours.is_empty()
          || history
            .samples
            .windows(2)
            .any(|pair| pair[0].sequence >= pair[1].sequence)
        {
          return Err(fs::message("service returned inconsistent detail history"));
        }
        for sample in &history.samples {
          validate_sample(sample)?;
          if sample.observed_at_ms < query.start_ms
            || sample.observed_at_ms >= query.end_ms
            || query
              .cursor
              .is_some_and(|cursor| sample.sequence as i64 <= cursor)
          {
            return Err(fs::message(
              "service returned detail outside the requested history",
            ));
          }
        }
        history.samples.last().map(|sample| sample.sequence as i64)
      }
      ProfileResolution::Hourly => {
        if !history.samples.is_empty()
          || history
            .hours
            .windows(2)
            .any(|pair| pair[0].hour_start_ms >= pair[1].hour_start_ms)
        {
          return Err(fs::message("service returned inconsistent hourly history"));
        }
        for hour in &history.hours {
          validate_hour(hour)?;
          if hour.hour_start_ms < query.start_ms
            || hour.hour_start_ms >= query.end_ms
            || query
              .cursor
              .is_some_and(|cursor| hour.hour_start_ms <= cursor)
          {
            return Err(fs::message(
              "service returned an hour outside the requested history",
            ));
          }
        }
        history.hours.last().map(|hour| hour.hour_start_ms)
      }
    };
    if history
      .next_cursor
      .is_some_and(|cursor| Some(cursor) != last_cursor)
    {
      return Err(fs::message(
        "service returned an inconsistent system history cursor",
      ));
    }
    Ok(history)
  }
}

fn validate_machine(machine: &MachineProfile) -> Result<()> {
  validate_component(&machine.machine_id)?;
  validate_inventory(&machine.inventory)?;
  if machine.inventory_revision == 0
    || machine.inventory_revision > i64::MAX as u64
    || machine.last_sample_at_ms.is_some_and(|at| at < 0)
    || machine.last_received_at_ms.is_some_and(|at| at < 0)
  {
    return Err(fs::message(
      "service returned an invalid system machine profile",
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests;
