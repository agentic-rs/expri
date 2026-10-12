//! Durable local collection and idempotent receiving of authoritative hour snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use expri_core::error::{ExpriError, Result};
use expri_core::identifiers::validate_component;
use expri_core::system_profile::*;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;

const MAX_PAGE_BYTES: usize = 900 * 1024;
const PRUNE_BATCH: usize = 1024;
const CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

fn sql<T>(result: rusqlite::Result<T>) -> Result<T> {
  result.map_err(|_| rejection(500, "system profile database operation failed"))
}
fn rejection(status: u16, detail: &str) -> ExpriError {
  ExpriError::ServiceRejected {
    status,
    detail: detail.into(),
  }
}
fn invalid(error: ExpriError) -> ExpriError {
  match error {
    error @ ExpriError::ServiceRejected { .. } => error,
    error => rejection(400, &error.to_string()),
  }
}
fn conflict() -> ExpriError {
  rejection(409, "system profile revision has conflicting content")
}
fn integer(value: u64) -> Result<i64> {
  i64::try_from(value).map_err(|_| rejection(400, "system profile integer exceeds its range"))
}
fn json<T: Serialize>(value: &T) -> Result<String> {
  Ok(serde_json::to_string(value)?)
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
  serde_json::from_str(value).map_err(|_| rejection(500, "stored system profile is invalid"))
}

pub fn initialize(connection: &Connection) -> Result<()> {
  sql(connection.busy_timeout(Duration::from_secs(5)))?;
  sql(connection.execute_batch(
    "PRAGMA foreign_keys = ON;
     CREATE TABLE IF NOT EXISTS system_machines (
       machine_id TEXT PRIMARY KEY,
       inventory_revision INTEGER NOT NULL DEFAULT 0,
       sample_sequence INTEGER NOT NULL DEFAULT 0,
       last_sample_at_ms INTEGER,
       last_received_at_ms INTEGER
     );
     CREATE TABLE IF NOT EXISTS system_inventories (
       machine_id TEXT NOT NULL REFERENCES system_machines(machine_id),
       revision INTEGER NOT NULL,
       payload TEXT NOT NULL,
       PRIMARY KEY (machine_id, revision)
     );
     CREATE TABLE IF NOT EXISTS system_samples (
       machine_id TEXT NOT NULL REFERENCES system_machines(machine_id),
       sequence INTEGER NOT NULL,
       observed_at_ms INTEGER NOT NULL,
       inventory_revision INTEGER NOT NULL,
       payload TEXT NOT NULL,
       PRIMARY KEY (machine_id, sequence)
     );
     CREATE INDEX IF NOT EXISTS system_samples_time ON system_samples(observed_at_ms);
     CREATE INDEX IF NOT EXISTS system_samples_machine_time ON system_samples(machine_id, observed_at_ms, sequence);
     CREATE TABLE IF NOT EXISTS system_hours (
       machine_id TEXT NOT NULL REFERENCES system_machines(machine_id),
       hour_start_ms INTEGER NOT NULL,
       revision INTEGER NOT NULL,
       payload TEXT NOT NULL,
       PRIMARY KEY (machine_id, hour_start_ms)
     );
     CREATE TABLE IF NOT EXISTS system_local (
       singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
       machine_id TEXT NOT NULL,
       sequence INTEGER NOT NULL DEFAULT 0,
       inventory_revision INTEGER NOT NULL DEFAULT 0
     );
     CREATE TABLE IF NOT EXISTS system_endpoint_ack (
       endpoint_key TEXT PRIMARY KEY,
       sample_sequence INTEGER NOT NULL DEFAULT 0,
       inventory_revision INTEGER NOT NULL DEFAULT 0
     );
     CREATE TABLE IF NOT EXISTS system_endpoint_hour_ack (
       endpoint_key TEXT NOT NULL,
       hour_start_ms INTEGER NOT NULL,
       revision INTEGER NOT NULL,
       PRIMARY KEY (endpoint_key, hour_start_ms)
     );
     CREATE TABLE IF NOT EXISTS system_local_quarantine (
       machine_id TEXT NOT NULL,
       kind TEXT NOT NULL,
       record_key INTEGER NOT NULL,
       revision INTEGER NOT NULL,
       reason TEXT NOT NULL,
       quarantined_at_ms INTEGER NOT NULL,
       payload TEXT,
       PRIMARY KEY(machine_id, kind, record_key, revision)
     );"
  ))
}

fn put_inventory(
  transaction: &Transaction<'_>,
  machine_id: &str,
  revision: &InventoryRevision,
) -> Result<()> {
  let payload = json(revision)?;
  let existing: Option<String> = sql(
    transaction
      .query_row(
        "SELECT payload FROM system_inventories WHERE machine_id=?1 AND revision=?2",
        params![machine_id, integer(revision.revision)?],
        |row| row.get(0),
      )
      .optional(),
  )?;
  if let Some(existing) = existing {
    if decode::<InventoryRevision>(&existing)? != *revision {
      return Err(conflict());
    }
  } else {
    sql(transaction.execute(
      "INSERT INTO system_inventories(machine_id,revision,payload) VALUES(?1,?2,?3)",
      params![machine_id, integer(revision.revision)?, payload],
    ))?;
  }
  sql(transaction.execute(
    "UPDATE system_machines SET inventory_revision=MAX(inventory_revision,?2) WHERE machine_id=?1",
    params![machine_id, integer(revision.revision)?],
  ))?;
  Ok(())
}

fn inventory_exists(connection: &Connection, machine_id: &str, revision: u64) -> Result<bool> {
  sql(connection.query_row(
    "SELECT EXISTS(SELECT 1 FROM system_inventories WHERE machine_id=?1 AND revision=?2)",
    params![machine_id, integer(revision)?],
    |row| row.get(0),
  ))
}

fn put_sample(
  transaction: &Transaction<'_>,
  machine_id: &str,
  sample: &SystemSample,
  now_ms: i64,
) -> Result<()> {
  let existing: Option<String> = sql(
    transaction
      .query_row(
        "SELECT payload FROM system_samples WHERE machine_id=?1 AND sequence=?2",
        params![machine_id, integer(sample.sequence)?],
        |row| row.get(0),
      )
      .optional(),
  )?;
  if let Some(existing) = existing {
    if decode::<SystemSample>(&existing)? != *sample {
      return Err(conflict());
    }
  } else if sample.observed_at_ms >= now_ms.saturating_sub(DETAIL_RETENTION_MS) {
    sql(transaction.execute(
      "INSERT INTO system_samples(machine_id,sequence,observed_at_ms,inventory_revision,payload) VALUES(?1,?2,?3,?4,?5)",
      params![machine_id, integer(sample.sequence)?, sample.observed_at_ms, integer(sample.inventory_revision)?, json(sample)?],
    ))?;
  }
  sql(transaction.execute(
    "UPDATE system_machines SET sample_sequence=MAX(sample_sequence,?2),
       last_sample_at_ms=MAX(COALESCE(last_sample_at_ms,?3),?3) WHERE machine_id=?1",
    params![machine_id, integer(sample.sequence)?, sample.observed_at_ms],
  ))?;
  Ok(())
}

fn put_hour(transaction: &Transaction<'_>, machine_id: &str, hour: &SystemHour) -> Result<()> {
  let existing: Option<(i64, String)> = sql(
    transaction
      .query_row(
        "SELECT revision,payload FROM system_hours WHERE machine_id=?1 AND hour_start_ms=?2",
        params![machine_id, hour.hour_start_ms],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .optional(),
  )?;
  if let Some((revision, payload)) = existing {
    if revision > integer(hour.revision)? {
      return Ok(());
    }
    if revision == integer(hour.revision)? {
      return if decode::<SystemHour>(&payload)? == *hour {
        Ok(())
      } else {
        Err(conflict())
      };
    }
  }
  sql(transaction.execute(
    "INSERT INTO system_hours(machine_id,hour_start_ms,revision,payload) VALUES(?1,?2,?3,?4)
     ON CONFLICT(machine_id,hour_start_ms) DO UPDATE SET revision=excluded.revision,payload=excluded.payload",
    params![machine_id, hour.hour_start_ms, integer(hour.revision)?, json(hour)?],
  ))?;
  sql(transaction.execute(
    "UPDATE system_machines SET last_sample_at_ms=MAX(COALESCE(last_sample_at_ms,?2),?2) WHERE machine_id=?1",
    params![machine_id, hour.last_sample_at_ms],
  ))?;
  Ok(())
}

/// Receivers accept absolute local summaries; raw samples never contribute again.
pub fn ingest(
  connection: &mut Connection,
  batch: &ProfileBatch,
  now_ms: i64,
) -> Result<ProfileAcknowledgement> {
  validate_profile_batch(batch).map_err(invalid)?;
  if now_ms < 0 {
    return Err(rejection(
      400,
      "system profile batch exceeds its byte or time limit",
    ));
  }
  let latest_allowed = now_ms.saturating_add(CLOCK_SKEW_MS);
  if batch
    .inventories
    .iter()
    .any(|revision| revision.inventory.observed_at_ms > latest_allowed)
    || batch
      .samples
      .iter()
      .any(|sample| sample.observed_at_ms > latest_allowed)
    || batch.hours.iter().any(|hour| {
      hour.hour_start_ms > latest_allowed / HOUR_MS * HOUR_MS
        || hour.last_sample_at_ms > latest_allowed
        || hour
          .metrics
          .values()
          .any(|metric| metric.last_at_ms > latest_allowed)
    })
  {
    return Err(rejection(
      400,
      "system profile observation is too far in the future",
    ));
  }
  let transaction = sql(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
  sql(transaction.execute(
    "INSERT OR IGNORE INTO system_machines(machine_id) VALUES(?1)",
    [&batch.machine_id],
  ))?;
  for revision in &batch.inventories {
    put_inventory(&transaction, &batch.machine_id, revision)?;
  }
  let revisions = batch
    .samples
    .iter()
    .map(|sample| sample.inventory_revision)
    .chain(
      batch
        .hours
        .iter()
        .flat_map(|hour| hour.inventory_revisions.iter().copied()),
    )
    .collect::<BTreeSet<_>>();
  for revision in revisions {
    if !inventory_exists(&transaction, &batch.machine_id, revision)? {
      return Err(rejection(
        400,
        "system profile references an unknown inventory revision",
      ));
    }
  }
  let has_inventory: bool = sql(transaction.query_row(
    "SELECT inventory_revision > 0 FROM system_machines WHERE machine_id=?1",
    [&batch.machine_id],
    |row| row.get(0),
  ))?;
  if !has_inventory {
    return Err(rejection(400, "machine profile requires an inventory"));
  }
  for sample in &batch.samples {
    put_sample(&transaction, &batch.machine_id, sample, now_ms)?;
  }
  for hour in &batch.hours {
    put_hour(&transaction, &batch.machine_id, hour)?;
  }
  sql(transaction.execute(
    "UPDATE system_machines SET last_received_at_ms=?2 WHERE machine_id=?1",
    params![batch.machine_id, now_ms],
  ))?;
  prune_transaction(&transaction, now_ms, PRUNE_BATCH)?;
  // Acknowledge the exact request, not a newer or unrelated server watermark.
  let acknowledgement = ProfileAcknowledgement {
    machine_id: batch.machine_id.clone(),
    sample_sequence: batch
      .samples
      .iter()
      .map(|sample| sample.sequence)
      .max()
      .unwrap_or(0),
    inventory_revision: batch
      .inventories
      .iter()
      .map(|inventory| inventory.revision)
      .max()
      .unwrap_or(0),
    hour_revisions: batch
      .hours
      .iter()
      .map(|hour| (hour.hour_start_ms, hour.revision))
      .collect(),
  };
  sql(transaction.commit())?;
  Ok(acknowledgement)
}

pub fn machine_profile(
  connection: &Connection,
  machine_id: &str,
  inventory_revision: Option<u64>,
) -> Result<Option<MachineProfile>> {
  validate_component(machine_id).map_err(invalid)?;
  let machine: Option<(u64, Option<i64>, Option<i64>)> = sql(connection.query_row(
    "SELECT inventory_revision,last_sample_at_ms,last_received_at_ms FROM system_machines WHERE machine_id=?1",
    [machine_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
  ).optional())?;
  let Some((current_revision, last_sample_at_ms, last_received_at_ms)) = machine else {
    return Ok(None);
  };
  let revision = inventory_revision.unwrap_or(current_revision);
  let payload: Option<String> = sql(
    connection
      .query_row(
    "SELECT i.payload FROM system_inventories i WHERE i.machine_id=?1 AND i.revision=?2 AND NOT EXISTS(SELECT 1 FROM system_local_quarantine q WHERE q.machine_id=i.machine_id AND q.kind='inventory' AND q.record_key=i.revision)",
        params![machine_id, integer(revision)?],
        |row| row.get(0),
      )
      .optional(),
  )?;
  let Some(payload) = payload else {
    return Ok(None);
  };
  let inventory: InventoryRevision = decode(&payload)?;
  Ok(Some(MachineProfile {
    machine_id: machine_id.into(),
    inventory_revision: revision,
    inventory: inventory.inventory,
    last_sample_at_ms,
    last_received_at_ms,
  }))
}

pub fn list_machines(
  connection: &Connection,
  limit: usize,
  cursor: Option<&str>,
) -> Result<(Vec<MachineProfile>, Option<String>)> {
  if !(1..=MAX_HISTORY_ROWS).contains(&limit) {
    return Err(rejection(
      400,
      "machine list limit must be between 1 and 1024",
    ));
  }
  if let Some(cursor) = cursor {
    validate_component(cursor).map_err(invalid)?;
  }
  let mut statement = sql(connection.prepare("SELECT machine_id FROM system_machines WHERE machine_id > ?1 AND inventory_revision > 0 ORDER BY machine_id LIMIT ?2"))?;
  let mut rows = sql(statement.query(params![cursor.unwrap_or(""), limit as i64 + 1]))?;
  let mut profiles = Vec::new();
  let mut bytes = 0;
  let mut next_cursor = None;
  while let Some(row) = sql(rows.next())? {
    let id: String = sql(row.get(0))?;
    if profiles.len() == limit {
      next_cursor = profiles
        .last()
        .map(|profile: &MachineProfile| profile.machine_id.clone());
      break;
    }
    if let Some(profile) = machine_profile(connection, &id, None)? {
      let size = serde_json::to_vec(&profile)?.len() + 64;
      if bytes + size > MAX_PAGE_BYTES {
        if profiles.is_empty() {
          return Err(rejection(
            413,
            "one system inventory exceeds the history page size limit",
          ));
        }
        next_cursor = profiles
          .last()
          .map(|profile: &MachineProfile| profile.machine_id.clone());
        break;
      }
      bytes += size;
      profiles.push(profile);
    }
  }
  Ok((profiles, next_cursor))
}

/// Detail cursors are durable sample sequences; hourly cursors are UTC hour starts.
pub fn history(connection: &Connection, query: &ProfileHistoryQuery) -> Result<ProfileHistory> {
  query.validate().map_err(invalid)?;
  let mut result = ProfileHistory {
    machine_id: query.machine_id.clone(),
    resolution: query.resolution,
    samples: Vec::new(),
    hours: Vec::new(),
    next_cursor: None,
  };
  let (table, time_column, cursor_column) = match query.resolution {
    ProfileResolution::Detail => ("system_samples", "observed_at_ms", "sequence"),
    ProfileResolution::Hourly => ("system_hours", "hour_start_ms", "hour_start_ms"),
  };
  let kind = if query.resolution == ProfileResolution::Detail {
    "sample"
  } else {
    "hour"
  };
  let revision_column = if query.resolution == ProfileResolution::Detail {
    "sequence"
  } else {
    "revision"
  };
  let statement = format!(
    "SELECT {cursor_column},payload FROM {table} r WHERE machine_id=?1 AND {time_column}>=?2 AND {time_column}<?3 AND {cursor_column}>?4 AND NOT EXISTS(SELECT 1 FROM system_local_quarantine q WHERE q.machine_id=r.machine_id AND q.kind='{kind}' AND q.record_key=r.{cursor_column} AND q.revision>=r.{revision_column}) ORDER BY {cursor_column} LIMIT ?5"
  );
  let mut statement = sql(connection.prepare(&statement))?;
  let mut rows = sql(statement.query(params![
    query.machine_id,
    query.start_ms,
    query.end_ms,
    query.cursor.unwrap_or(-1),
    query.limit as i64 + 1
  ]))?;
  let mut bytes = 0;
  let mut count = 0;
  let mut last_cursor = None;
  while let Some(row) = sql(rows.next())? {
    let cursor: i64 = sql(row.get(0))?;
    let payload: String = sql(row.get(1))?;
    if count == query.limit || bytes + payload.len() + 64 > MAX_PAGE_BYTES {
      if count == 0 {
        return Err(rejection(
          413,
          "one system history record exceeds the page size limit",
        ));
      }
      result.next_cursor = last_cursor;
      break;
    }
    bytes += payload.len() + 64;
    count += 1;
    last_cursor = Some(cursor);
    match query.resolution {
      ProfileResolution::Detail => result.samples.push(decode(&payload)?),
      ProfileResolution::Hourly => result.hours.push(decode(&payload)?),
    }
  }
  Ok(result)
}

fn prune_transaction(transaction: &Transaction<'_>, now_ms: i64, limit: usize) -> Result<usize> {
  sql(transaction.execute(
    "DELETE FROM system_samples WHERE rowid IN (SELECT rowid FROM system_samples WHERE observed_at_ms < ?1 ORDER BY observed_at_ms LIMIT ?2)",
    params![now_ms.saturating_sub(DETAIL_RETENTION_MS),limit.min(10_000) as i64],
  ))
}

pub fn prune(connection: &mut Connection, now_ms: i64, limit: usize) -> Result<usize> {
  if now_ms < 0 || limit == 0 {
    return Err(rejection(
      400,
      "invalid profile retention clock or batch size",
    ));
  }
  let transaction = sql(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
  let removed = prune_transaction(&transaction, now_ms, limit)?;
  sql(transaction.commit())?;
  Ok(removed)
}

pub fn compact(connection: &mut Connection, now_ms: i64) -> Result<usize> {
  prune(connection, now_ms, PRUNE_BATCH)
}

mod local;
pub use local::{LocalStatus, LocalStore};
mod quarantine;

#[cfg(test)]
mod tests;
