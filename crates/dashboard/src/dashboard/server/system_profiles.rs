//! Strict machine-history routes with byte-aware pagination, independent of run sources.

use expri_core::identifiers::validate_component;
use expri_core::service::MAX_SYSTEM_MACHINES;
use expri_core::system_profile::{
  MAX_HISTORY_ROWS, ProfileHistory, ProfileHistoryQuery, ProfileResolution,
};
use serde::Serialize;

use super::{DashboardView, JSON_LIMIT, Query, Reply, service_error};

fn unavailable() -> Reply {
  Reply::error(404, "system monitoring is unavailable")
}

fn identifier<'a>(query: &'a Query, field: &str) -> Result<&'a str, Reply> {
  let value = query.required(field)?;
  validate_component(value).map_err(service_error)?;
  Ok(value)
}

fn integer(value: &str, field: &str, minimum: i64) -> Result<i64, Reply> {
  if !value.is_empty()
    && value.bytes().all(|byte| byte.is_ascii_digit())
    && let Ok(value) = value.parse::<i64>()
    && value >= minimum
  {
    return Ok(value);
  }
  Err(Reply::error(
    400,
    format!("{field} must be an integer from {minimum} to {}", i64::MAX),
  ))
}

fn optional_integer(query: &Query, field: &str, minimum: i64) -> Result<Option<i64>, Reply> {
  query
    .optional(field)
    .map(|value| integer(value, field, minimum))
    .transpose()
}

fn limit(query: &Query, maximum: usize) -> Result<usize, Reply> {
  let limit = optional_integer(query, "limit", 1)?.unwrap_or(100);
  usize::try_from(limit)
    .ok()
    .filter(|limit| *limit <= maximum)
    .ok_or_else(|| Reply::error(400, format!("limit must be between 1 and {maximum}")))
}

pub(super) fn machines(dashboard: &impl DashboardView, query: &Query) -> Result<Reply, Reply> {
  query.allow(&["limit", "cursor"])?;
  let limit = limit(query, MAX_SYSTEM_MACHINES)?;
  let cursor = query.optional("cursor");
  if let Some(cursor) = cursor {
    validate_component(cursor).map_err(service_error)?;
  }
  let page = dashboard
    .system_machines(limit, cursor)
    .map_err(service_error)?
    .ok_or_else(unavailable)?;
  machine_reply(page, limit)
}

fn machine_reply(mut page: crate::dashboard::SystemMachines, limit: usize) -> Result<Reply, Reply> {
  let count = page_count(&page.machines, limit)?;
  if count < page.machines.len() {
    page.next_cursor = page
      .machines
      .get(count - 1)
      .map(|row| row.machine_id.clone());
    page.machines.truncate(count);
  }
  Ok(Reply::serialized(&page))
}

pub(super) fn profile(dashboard: &impl DashboardView, query: &Query) -> Result<Reply, Reply> {
  query.allow(&["machine_id", "inventory_revision"])?;
  let machine_id = identifier(query, "machine_id")?;
  let revision = optional_integer(query, "inventory_revision", 1)?.map(|value| value as u64);
  let profile = dashboard
    .system_profile(machine_id, revision)
    .map_err(service_error)?
    .ok_or_else(unavailable)?;
  Ok(Reply::serialized(&profile))
}

pub(super) fn history(dashboard: &impl DashboardView, query: &Query) -> Result<Reply, Reply> {
  query.allow(&[
    "machine_id",
    "start_ms",
    "end_ms",
    "resolution",
    "cursor",
    "limit",
  ])?;
  let selection = ProfileHistoryQuery {
    machine_id: identifier(query, "machine_id")?.into(),
    start_ms: integer(query.required("start_ms")?, "start_ms", 0)?,
    end_ms: integer(query.required("end_ms")?, "end_ms", 0)?,
    resolution: match query.optional("resolution").unwrap_or("detail") {
      "detail" => ProfileResolution::Detail,
      "hourly" => ProfileResolution::Hourly,
      _ => return Err(Reply::error(400, "resolution must be detail or hourly")),
    },
    cursor: optional_integer(query, "cursor", 0)?,
    limit: limit(query, MAX_HISTORY_ROWS)?,
  };
  selection.validate().map_err(service_error)?;
  let page = dashboard
    .system_history(&selection)
    .map_err(service_error)?
    .ok_or_else(unavailable)?;
  history_reply(page, &selection)
}

fn history_reply(
  mut page: ProfileHistory,
  selection: &ProfileHistoryQuery,
) -> Result<Reply, Reply> {
  match selection.resolution {
    ProfileResolution::Detail => {
      let count = page_count(&page.samples, selection.limit)?;
      if count < page.samples.len() {
        page.next_cursor = page.samples.get(count - 1).map(|row| row.sequence as i64);
        page.samples.truncate(count);
      }
    }
    ProfileResolution::Hourly => {
      let count = page_count(&page.hours, selection.limit)?;
      if count < page.hours.len() {
        page.next_cursor = page.hours.get(count - 1).map(|row| row.hour_start_ms);
        page.hours.truncate(count);
      }
    }
  }
  Ok(Reply::serialized(&page))
}

/// Reserve the bounded envelope and cursor before admitting rows. If a page is
/// shortened, its caller resumes after the last returned row, never the store's
/// original cursor. A single oversize row is an explicit error, not omitted data.
fn page_count<T: Serialize>(rows: &[T], limit: usize) -> Result<usize, Reply> {
  let mut bytes = 1024;
  let mut count = 0;
  for row in rows.iter().take(limit) {
    let mut writer = CountingWriter { bytes: 0 };
    if serde_json::to_writer(&mut writer, row).is_err() || bytes + writer.bytes + 1 > JSON_LIMIT {
      if count == 0 {
        return Err(Reply::error(
          413,
          "one system monitor record exceeds the 512 KiB dashboard response limit",
        ));
      }
      break;
    }
    bytes += writer.bytes + 1;
    count += 1;
  }
  Ok(count)
}

struct CountingWriter {
  bytes: usize,
}

impl std::io::Write for CountingWriter {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    if self.bytes.saturating_add(bytes.len()) > JSON_LIMIT {
      return Err(std::io::Error::other(
        "system monitor row exceeds its byte limit",
      ));
    }
    self.bytes += bytes.len();
    Ok(bytes.len())
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

#[cfg(test)]
mod tests;
