//! Bounded server browsing contracts. Cursors identify immutable scope/path keys.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};
use crate::service::{
  FileStorage, RunScope, document_path, stream_path, validate_component, validate_run_path,
  validate_scope,
};

pub const SERVICE_CATALOG_CAPABILITY: &str = "service-catalog-v1";
pub const RUN_SUMMARY_CAPABILITY: &str = "run-summary-v1";
pub const MAX_CATALOG_ROWS: usize = 100;
pub const MAX_CATALOG_PAGE_BYTES: usize = 900 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunArchiveFilter {
  #[default]
  Active,
  Archived,
  All,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunListQuery {
  pub project_id: Option<String>,
  pub origin: Option<String>,
  #[serde(default)]
  pub archival: RunArchiveFilter,
  pub limit: usize,
  pub cursor: Option<String>,
}

impl RunListQuery {
  pub fn validate(&self) -> Result<()> {
    for component in [&self.project_id, &self.origin].into_iter().flatten() {
      validate_component(component)?;
    }
    validate_limit(self.limit)?;
    if let Some(cursor) = &self.cursor {
      decode_run_cursor(cursor)?;
    }
    Ok(())
  }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunListEntry {
  pub scope: RunScope,
  pub task: Option<String>,
  /// Published state for catalog-v1, or effective state for negotiated summaries.
  pub status: String,
  pub started_at: Option<String>,
  pub finished_at: Option<String>,
  pub exit_code: Option<i64>,
  pub archived: bool,
  pub warnings: Vec<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub observation: Option<RunObservation>,
}

/// Provenance of a compatible last-known worker observation over published history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunObservation {
  pub recorded_status: String,
  pub observed_at_ms: i64,
  pub online: bool,
  pub attempt: u64,
  pub generation: u64,
  pub available_actions: Vec<crate::run_control::ControlAction>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunListPage {
  pub runs: Vec<RunListEntry>,
  pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFilesQuery {
  pub scope: RunScope,
  pub prefix: Option<String>,
  pub limit: usize,
  pub cursor: Option<String>,
}

/// A CLI selection whose omitted scope components can be resolved from the catalog.
#[derive(Clone, Debug)]
pub struct RunSelector {
  pub run_id: String,
  pub project_id: Option<String>,
  pub origin: Option<String>,
}

impl RunSelector {
  pub fn validate(&self) -> Result<()> {
    validate_component(&self.run_id)?;
    for component in [&self.project_id, &self.origin].into_iter().flatten() {
      validate_component(component)?;
    }
    Ok(())
  }

  pub fn explicit_scope(&self) -> Option<RunScope> {
    Some(RunScope {
      run_id: self.run_id.clone(),
      project_id: self.project_id.clone()?,
      origin: self.origin.clone()?,
    })
  }
}

impl RunFilesQuery {
  pub fn validate(&self) -> Result<()> {
    validate_scope(&self.scope)?;
    validate_run_files_options(self.prefix.as_deref(), self.limit, self.cursor.as_deref())
  }
}

/// Validate file-page options before resolving a partially specified run scope.
pub fn validate_run_files_options(
  prefix: Option<&str>,
  limit: usize,
  cursor: Option<&str>,
) -> Result<()> {
  validate_limit(limit)?;
  if let Some(prefix) = prefix
    && (prefix.is_empty()
      || prefix.len() > 1024
      || prefix.starts_with('/')
      || prefix.contains('\\')
      || prefix.chars().any(char::is_control)
      || prefix
        .split('/')
        .any(|component| matches!(component, "." | "..")))
  {
    return Err(invalid("invalid literal file prefix"));
  }
  if let Some(cursor) = cursor {
    validate_run_path(cursor)?;
  }
  Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunFileAvailability {
  Received,
  ReportedOnly,
  ReceivedDifferentVersion,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivedRunFile {
  pub size: u64,
  pub sha256: Option<String>,
  pub storage: FileStorage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedRunFile {
  pub size: u64,
  pub sha256: Option<String>,
  pub labels: Vec<String>,
  pub sync_status: Option<String>,
  pub sync_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFileEntry {
  pub path: String,
  pub availability: RunFileAvailability,
  pub received: Option<ReceivedRunFile>,
  pub reported: Option<ReportedRunFile>,
  pub labels_pending: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFilesPage {
  pub scope: RunScope,
  pub files: Vec<RunFileEntry>,
  pub next_cursor: Option<String>,
  pub inventory_recorded_at: Option<String>,
  pub reported_inventory_truncated: bool,
  pub warnings: Vec<String>,
}

fn invalid(message: &str) -> ExpriError {
  ExpriError::Message(message.into())
}

fn validate_limit(limit: usize) -> Result<()> {
  if !(1..=MAX_CATALOG_ROWS).contains(&limit) {
    return Err(invalid("server catalog pages support 1 to 100 rows"));
  }
  Ok(())
}

fn text(value: &str, limit: usize) -> Result<()> {
  if value.len() > limit || value.chars().any(char::is_control) {
    return Err(invalid(
      "server catalog text exceeds its limit or is invalid",
    ));
  }
  Ok(())
}

pub fn validate_catalog_warnings(warnings: &[String]) -> Result<()> {
  if warnings.len() > 32 {
    return Err(invalid("too many server catalog warnings"));
  }
  for warning in warnings {
    text(warning, 512)?;
  }
  Ok(())
}

pub fn validate_run_entry(run: &RunListEntry) -> Result<()> {
  validate_scope(&run.scope)?;
  if !crate::run_state::STATUSES.contains(&run.status.as_str()) {
    return Err(invalid("invalid published run status"));
  }
  if let Some(task) = &run.task {
    text(task, 4096)?;
  }
  for time in [&run.started_at, &run.finished_at].into_iter().flatten() {
    text(time, 128)?;
  }
  if let Some(observation) = &run.observation
    && (!crate::run_state::STATUSES.contains(&observation.recorded_status.as_str())
      || observation.observed_at_ms < 0
      || observation.attempt == 0
      || observation.available_actions.len() > 6)
  {
    return Err(invalid("invalid run observation"));
  }
  validate_catalog_warnings(&run.warnings)
}

fn digest(value: &Option<String>) -> Result<()> {
  if value.as_ref().is_some_and(|value| {
    value.len() != 64
      || !value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
  }) {
    return Err(invalid("invalid catalog file checksum"));
  }
  Ok(())
}

pub fn validate_file_entry(file: &RunFileEntry) -> Result<()> {
  validate_run_path(&file.path)?;
  if file.received.is_none() && file.reported.is_none() {
    return Err(invalid("catalog file has no received or reported metadata"));
  }
  if let Some(received) = &file.received {
    digest(&received.sha256)?;
    match received.storage {
      FileStorage::Object => {}
      FileStorage::Stream if !stream_path(&file.path) || received.sha256.is_some() => {
        return Err(invalid("invalid catalog stream metadata"));
      }
      FileStorage::Stream => {}
      FileStorage::Tracking { revision, .. }
        if revision == 0
          || revision > i64::MAX as u64
          || (!stream_path(&file.path) && !document_path(&file.path)) =>
      {
        return Err(invalid("invalid catalog tracking metadata"));
      }
      FileStorage::Tracking { .. } => {}
    }
  }
  if let Some(reported) = &file.reported {
    digest(&reported.sha256)?;
    if reported.labels.len() > 2
      || reported
        .labels
        .iter()
        .any(|label| !["best", "latest"].contains(&label.as_str()))
    {
      return Err(invalid("invalid catalog checkpoint labels"));
    }
    if let Some(status) = &reported.sync_status
      && !["registered", "uploading", "cloud", "needs_attention"].contains(&status.as_str())
    {
      return Err(invalid("invalid reported file sync status"));
    }
    if let Some(error) = &reported.sync_error {
      text(error, 512)?;
    }
  }
  if file_availability(file.received.as_ref(), file.reported.as_ref()) != Some(file.availability)
    || file_labels_pending(file.received.as_ref(), file.reported.as_ref()) != file.labels_pending
  {
    return Err(invalid("inconsistent catalog file availability"));
  }
  Ok(())
}

pub fn file_availability(
  received: Option<&ReceivedRunFile>,
  reported: Option<&ReportedRunFile>,
) -> Option<RunFileAvailability> {
  match (received, reported) {
    (None, None) => None,
    (None, Some(_)) => Some(RunFileAvailability::ReportedOnly),
    (Some(received), Some(reported))
      if received.size != reported.size
        || received
          .sha256
          .as_ref()
          .zip(reported.sha256.as_ref())
          .is_some_and(|(left, right)| left != right) =>
    {
      Some(RunFileAvailability::ReceivedDifferentVersion)
    }
    (Some(_), _) => Some(RunFileAvailability::Received),
  }
}

pub fn file_labels_pending(
  received: Option<&ReceivedRunFile>,
  reported: Option<&ReportedRunFile>,
) -> bool {
  let Some(reported) = reported.filter(|file| !file.labels.is_empty()) else {
    return false;
  };
  !received.is_some_and(|received| {
    matches!(received.storage, FileStorage::Object)
      && received.size == reported.size
      && received
        .sha256
        .as_ref()
        .zip(reported.sha256.as_ref())
        .is_some_and(|(left, right)| left == right)
  })
}

pub fn validate_run_list_page(query: &RunListQuery, page: &RunListPage) -> Result<()> {
  query.validate()?;
  if page.runs.len() > query.limit {
    return Err(invalid("server returned too many catalog runs"));
  }
  let cursor = query.cursor.as_deref().map(decode_run_cursor).transpose()?;
  for run in &page.runs {
    validate_run_entry(run)?;
    if query
      .project_id
      .as_ref()
      .is_some_and(|value| value != &run.scope.project_id)
      || query
        .origin
        .as_ref()
        .is_some_and(|value| value != &run.scope.origin)
      || (query.archival == RunArchiveFilter::Active && run.archived)
      || (query.archival == RunArchiveFilter::Archived && !run.archived)
      || cursor
        .as_ref()
        .is_some_and(|cursor| compare_run_scopes(cursor, &run.scope) != Ordering::Less)
    {
      return Err(invalid(
        "server returned runs outside the requested catalog page",
      ));
    }
  }
  if page
    .runs
    .windows(2)
    .any(|rows| compare_run_scopes(&rows[0].scope, &rows[1].scope) != Ordering::Less)
  {
    return Err(invalid(
      "server returned unordered or duplicate catalog runs",
    ));
  }
  if let Some(next) = &page.next_cursor {
    let scope = decode_run_cursor(next)?;
    if page.runs.last().is_none_or(|run| run.scope != scope) {
      return Err(invalid("server returned an inconsistent run cursor"));
    }
  }
  validate_page_size(page)
}

pub fn validate_run_files_page(query: &RunFilesQuery, page: &RunFilesPage) -> Result<()> {
  query.validate()?;
  if page.scope != query.scope || page.files.len() > query.limit {
    return Err(invalid(
      "server returned an inconsistent file page scope or size",
    ));
  }
  if let Some(recorded_at) = &page.inventory_recorded_at {
    text(recorded_at, 128)?;
  }
  validate_catalog_warnings(&page.warnings)?;
  for file in &page.files {
    validate_file_entry(file)?;
    if query
      .prefix
      .as_ref()
      .is_some_and(|prefix| !file.path.starts_with(prefix))
      || query
        .cursor
        .as_ref()
        .is_some_and(|cursor| &file.path <= cursor)
    {
      return Err(invalid("server returned files outside the requested page"));
    }
  }
  if page
    .files
    .windows(2)
    .any(|files| files[0].path >= files[1].path)
  {
    return Err(invalid(
      "server returned unordered or duplicate catalog files",
    ));
  }
  if let Some(next) = &page.next_cursor
    && page.files.last().is_none_or(|file| &file.path != next)
  {
    return Err(invalid("server returned an inconsistent file cursor"));
  }
  validate_page_size(page)
}

fn validate_page_size(page: &impl Serialize) -> Result<()> {
  if serde_json::to_vec(page)?.len() > MAX_CATALOG_PAGE_BYTES {
    return Err(invalid("server catalog page exceeds its byte limit"));
  }
  Ok(())
}

/// Scope components exclude ':', so this versioned cursor has one unambiguous encoding.
pub fn encode_run_cursor(scope: &RunScope) -> Result<String> {
  validate_scope(scope)?;
  Ok(format!(
    "v1:{}:{}:{}",
    scope.project_id, scope.origin, scope.run_id
  ))
}

pub fn decode_run_cursor(cursor: &str) -> Result<RunScope> {
  if cursor.len() > 300 {
    return Err(invalid("invalid run catalog cursor"));
  }
  let parts: Vec<_> = cursor.split(':').collect();
  if parts.len() != 4 || parts[0] != "v1" {
    return Err(invalid("invalid run catalog cursor"));
  }
  let scope = RunScope {
    project_id: parts[1].into(),
    origin: parts[2].into(),
    run_id: parts[3].into(),
  };
  validate_scope(&scope)?;
  Ok(scope)
}

/// Run IDs descend, with project/origin as ascending tie breakers.
pub fn compare_run_scopes(left: &RunScope, right: &RunScope) -> Ordering {
  right
    .run_id
    .cmp(&left.run_id)
    .then_with(|| left.project_id.cmp(&right.project_id))
    .then_with(|| left.origin.cmp(&right.origin))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn published_run_entries_accept_waiting_and_paused_and_reject_unknown_phases() {
    let mut run = RunListEntry {
      scope: RunScope {
        project_id: "project".into(),
        origin: "worker".into(),
        run_id: "run-one".into(),
      },
      task: Some("train".into()),
      status: "waiting".into(),
      started_at: Some("2026-10-14T00:00:00Z".into()),
      finished_at: None,
      exit_code: None,
      archived: false,
      warnings: Vec::new(),
      observation: None,
    };
    validate_run_entry(&run).unwrap();
    run.status = "paused".into();
    validate_run_entry(&run).unwrap();
    run.status = "unsupported-phase".into();
    assert!(validate_run_entry(&run).is_err());
  }

  #[test]
  fn partial_run_selectors_validate_each_supplied_component() {
    let mut selector = RunSelector {
      run_id: "run-one".into(),
      project_id: None,
      origin: None,
    };
    selector.validate().unwrap();
    assert!(selector.explicit_scope().is_none());
    selector.project_id = Some("vision".into());
    selector.validate().unwrap();
    assert!(selector.explicit_scope().is_none());
    selector.origin = Some("gpu-1".into());
    selector.validate().unwrap();
    assert_eq!(selector.explicit_scope().unwrap().project_id, "vision");
    for field in ["run_id", "project_id", "origin"] {
      let mut invalid = selector.clone();
      match field {
        "run_id" => invalid.run_id = "../must-not-echo".into(),
        "project_id" => invalid.project_id = Some("../must-not-echo".into()),
        _ => invalid.origin = Some("../must-not-echo".into()),
      }
      assert!(invalid.validate().is_err());
    }
  }

  #[test]
  fn received_storage_rejects_impossible_streams_and_tracking_revisions() {
    let mut file = RunFileEntry {
      path: "logs/stdout.log".into(),
      availability: RunFileAvailability::Received,
      received: Some(ReceivedRunFile {
        size: 4,
        sha256: None,
        storage: FileStorage::Tracking {
          revision: 1,
          sealed: false,
        },
      }),
      reported: None,
      labels_pending: false,
    };
    validate_file_entry(&file).unwrap();
    for revision in [0, i64::MAX as u64 + 1] {
      file.received.as_mut().unwrap().storage = FileStorage::Tracking {
        revision,
        sealed: false,
      };
      assert!(validate_file_entry(&file).is_err());
    }
    file.received.as_mut().unwrap().storage = FileStorage::Tracking {
      revision: 1,
      sealed: true,
    };
    file.path = "outputs/checkpoint.pt".into();
    assert!(validate_file_entry(&file).is_err());
    file.received.as_mut().unwrap().storage = FileStorage::Stream;
    assert!(validate_file_entry(&file).is_err());
    file.path = "logs/stdout.log".into();
    validate_file_entry(&file).unwrap();
    file.received.as_mut().unwrap().sha256 = Some("a".repeat(64));
    assert!(validate_file_entry(&file).is_err());
    file.received.as_mut().unwrap().storage = FileStorage::Object;
    file.received.as_mut().unwrap().sha256 = None;
    validate_file_entry(&file).unwrap();
  }

  #[test]
  fn cursors_keep_duplicate_run_ids_on_different_workers_distinct() {
    let left = RunScope {
      project_id: "project".into(),
      origin: "a".into(),
      run_id: "run-one".into(),
    };
    let right = RunScope {
      origin: "b".into(),
      ..left.clone()
    };
    assert_eq!(
      decode_run_cursor(&encode_run_cursor(&left).unwrap()).unwrap(),
      left
    );
    assert_ne!(
      encode_run_cursor(&left).unwrap(),
      encode_run_cursor(&right).unwrap()
    );
    assert_eq!(compare_run_scopes(&left, &right), Ordering::Less);
    for cursor in [
      "",
      "v2:project:a:run",
      "v1:project:a:../run",
      "v1:project:a:run:extra",
    ] {
      assert!(decode_run_cursor(cursor).is_err());
    }
  }

  #[test]
  fn file_prefix_is_literal_bounded_and_cannot_traverse_paths() {
    let mut query = RunFilesQuery {
      scope: RunScope {
        project_id: "project".into(),
        origin: "worker".into(),
        run_id: "run".into(),
      },
      prefix: Some("outputs/a_%/".into()),
      limit: 100,
      cursor: None,
    };
    query.validate().unwrap();
    for prefix in [
      "",
      "../outputs",
      "/outputs",
      "outputs/../secret",
      "outputs\\x",
    ] {
      query.prefix = Some(prefix.into());
      assert!(query.validate().is_err());
    }
  }
}
