//! Catalog pages cover all matching published runs without a dashboard scan window.

use std::time::Instant;

use expri_core::service_catalog::{
  MAX_CATALOG_PAGE_BYTES, RunArchiveFilter, RunListEntry, RunListPage, RunListQuery,
  decode_run_cursor, encode_run_cursor, validate_run_list_page,
};
use serde_json::Value;

use super::*;
use crate::service::dashboard_data::HostedDashboard;

const METADATA_LIMIT: u64 = 256 * 1024;
const METADATA_BUDGET: Duration = Duration::from_secs(3);

impl<S: ObjectStorage> Store<S> {
  pub(super) fn browse_runs(&self, query: &RunListQuery) -> ApiResult<RunListPage> {
    query.validate().map_err(bad)?;
    let cursor = query
      .cursor
      .as_deref()
      .map(decode_run_cursor)
      .transpose()
      .map_err(bad)?;
    if cursor.as_ref().is_some_and(|cursor| {
      query
        .project_id
        .as_ref()
        .is_some_and(|project| project != &cursor.project_id)
        || query
          .origin
          .as_ref()
          .is_some_and(|origin| origin != &cursor.origin)
    }) {
      return Err(ApiError::new(
        400,
        "run cursor is outside the selected project or origin",
      ));
    }
    let archival = match query.archival {
      RunArchiveFilter::Active => Some(false),
      RunArchiveFilter::Archived => Some(true),
      RunArchiveFilter::All => None,
    };
    let candidates = {
      let db = self.db()?;
      if let Some(project_id) = &query.project_id {
        projects::ensure_active(&db, project_id)?;
      }
      let mut statement = db.prepare(&selection_sql(query)).map_err(database)?;
      statement
        .query_map(
          params![
            query.project_id.as_deref(),
            query.origin.as_deref(),
            archival,
            cursor.as_ref().map(|cursor| cursor.run_id.as_str()),
            cursor.as_ref().map(|cursor| cursor.project_id.as_str()),
            cursor.as_ref().map(|cursor| cursor.origin.as_str()),
            run_retention::now(),
            query.limit + 1,
          ],
          |row| {
            Ok((
              RunScope {
                project_id: row.get(0)?,
                origin: row.get(1)?,
                run_id: row.get(2)?,
              },
              row.get::<_, bool>(3)?,
            ))
          },
        )
        .map_err(database)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(database)?
    };

    let reader = HostedDashboard::new(self).ok();
    let deadline = Instant::now() + METADATA_BUDGET;
    let mut has_more = candidates.len() > query.limit;
    let mut page = RunListPage {
      runs: Vec::new(),
      next_cursor: None,
    };
    // Reserve enough space for the envelope and the largest permitted scope cursor.
    let mut budget = MAX_CATALOG_PAGE_BYTES - 512;
    for (scope, archived) in candidates.into_iter().take(query.limit) {
      validate_scope(&scope).map_err(|_| ApiError::new(500, "invalid stored run scope"))?;
      let entry = run_entry(reader.as_ref(), scope, archived, deadline);
      let bytes = serde_json::to_vec(&entry)
        .map_err(|_| ApiError::new(500, "cannot encode run catalog metadata"))?
        .len()
        + usize::from(!page.runs.is_empty());
      if bytes > budget {
        has_more = true;
        break;
      }
      budget -= bytes;
      page.runs.push(entry);
    }
    if has_more {
      page.next_cursor = page
        .runs
        .last()
        .map(|run| encode_run_cursor(&run.scope))
        .transpose()
        .map_err(bad)?;
    }
    self.ensure_page_available(query, &page)?;
    validate_run_list_page(query, &page)
      .map_err(|_| ApiError::new(500, "invalid run catalog page"))?;
    Ok(page)
  }

  fn ensure_page_available(&self, query: &RunListQuery, page: &RunListPage) -> ApiResult<()> {
    // Metadata reads release SQLite and project locks. Recheck the complete page
    // in one database view so a concurrent lifecycle change cannot leave stale
    // rows or a cursor that silently skips a removed selection.
    let db = self.db()?;
    if let Some(project_id) = &query.project_id {
      projects::ensure_active(&db, project_id)?;
    }
    let at = run_retention::now();
    let mut statement = db.prepare("SELECT
      EXISTS(SELECT 1 FROM project_deletions WHERE project_id=?1),
      (SELECT status='archived' AND delete_after>?4 FROM run_retention WHERE project_id=?1 AND origin=?2 AND run_id=?3),
      EXISTS(
        SELECT 1 FROM files WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3
        UNION ALL
        SELECT 1 FROM streams WHERE json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3
      )").map_err(database)?;
    for run in &page.runs {
      let (deleted_project, retention, exists): (bool, Option<bool>, bool) = statement
        .query_row(
          params![run.scope.project_id, run.scope.origin, run.scope.run_id, at],
          |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(database)?;
      let archived = retention.is_some();
      if deleted_project
        || retention == Some(false)
        || !exists
        || archived != run.archived
        || (query.archival == RunArchiveFilter::Active && archived)
        || (query.archival == RunArchiveFilter::Archived && !archived)
      {
        return Err(ApiError::new(
          409,
          "run catalog changed during metadata reads; retry the page",
        ));
      }
    }
    Ok(())
  }
}

fn selection_sql(query: &RunListQuery) -> String {
  // Direct clauses keep scoped queries seekable; values remain bound parameters.
  let project = if query.project_id.is_some() {
    "AND project_id=?1"
  } else {
    ""
  };
  let origin = if query.origin.is_some() {
    "AND origin=?2"
  } else {
    ""
  };
  let position = if query.cursor.is_some() {
    "AND run_id<=?4 AND (run_id<?4 OR (run_id=?4 AND (project_id>?5 OR (project_id=?5 AND origin>?6))))"
  } else {
    ""
  };
  // Unary + removes TEXT-column affinity from correlated scope values, allowing
  // every JSON expression-index term to participate in the existence lookup.
  format!("SELECT project_id,origin,run_id,
    EXISTS(SELECT 1 FROM run_retention r WHERE r.project_id=scopes.project_id AND r.origin=scopes.origin AND r.run_id=scopes.run_id) AS archived
    FROM dashboard_run_activity scopes WHERE 1=1 {project} {origin} {position}
    AND (?3 IS NULL OR ?3=EXISTS(SELECT 1 FROM run_retention r WHERE r.project_id=scopes.project_id AND r.origin=scopes.origin AND r.run_id=scopes.run_id))
    AND NOT EXISTS(SELECT 1 FROM project_deletions p WHERE p.project_id=scopes.project_id)
    AND NOT EXISTS(SELECT 1 FROM run_retention r WHERE r.project_id=scopes.project_id AND r.origin=scopes.origin AND r.run_id=scopes.run_id AND (r.status<>'archived' OR r.delete_after<=?7))
    AND EXISTS(
      SELECT 1 FROM files f WHERE json_extract(f.target,'$.kind')='run' AND json_extract(f.target,'$.scope.project_id')=+scopes.project_id AND json_extract(f.target,'$.scope.origin')=+scopes.origin AND json_extract(f.target,'$.scope.run_id')=+scopes.run_id
      UNION ALL
      SELECT 1 FROM streams s WHERE json_extract(s.target,'$.kind')='run' AND json_extract(s.target,'$.scope.project_id')=+scopes.project_id AND json_extract(s.target,'$.scope.origin')=+scopes.origin AND json_extract(s.target,'$.scope.run_id')=+scopes.run_id
    )
    ORDER BY run_id DESC,project_id ASC,origin ASC LIMIT ?8")
}

fn run_entry<S: ObjectStorage>(
  reader: Option<&HostedDashboard<'_, S>>,
  scope: RunScope,
  archived: bool,
  deadline: Instant,
) -> RunListEntry {
  let mut warnings = Vec::new();
  let state = match reader {
    Some(reader) => {
      match reader.catalog_document(&scope, "run-state.json", METADATA_LIMIT, deadline) {
        Ok(Some(state)) => Some(state),
        Ok(None) => {
          warn(
            &mut warnings,
            "run-state.json is missing; execution state is unknown",
          );
          None
        }
        Err(error) => {
          warn(&mut warnings, &error.message);
          None
        }
      }
    }
    None => {
      warn(
        &mut warnings,
        "published execution metadata is temporarily unavailable",
      );
      None
    }
  };
  let normalized = crate::runs::summary_from_state(&scope.run_id, state);
  for warning in normalized["warnings"].as_array().into_iter().flatten() {
    if let Some(message) = warning["message"].as_str() {
      warn(&mut warnings, message);
    }
  }
  let run = &normalized["run"];
  RunListEntry {
    scope,
    task: field(run, "task", 4096, &mut warnings),
    status: run["status"].as_str().unwrap_or("unknown").into(),
    started_at: field(run, "started_at", 128, &mut warnings),
    finished_at: field(run, "finished_at", 128, &mut warnings),
    exit_code: run["exit_code"].as_i64(),
    archived,
    warnings,
  }
}

fn field(run: &Value, name: &str, limit: usize, warnings: &mut Vec<String>) -> Option<String> {
  let text = run[name].as_str()?;
  if text.len() > limit || text.chars().any(char::is_control) {
    warn(
      warnings,
      &format!(
        "published {name} exceeds the catalog field limit or contains control characters; omitted"
      ),
    );
    None
  } else {
    Some(text.into())
  }
}

fn warn(warnings: &mut Vec<String>, message: &str) {
  if warnings.len() == 32 {
    return;
  }
  let mut text = String::new();
  for character in message.chars().filter(|character| !character.is_control()) {
    if text.len() + character.len_utf8() > 512 {
      break;
    }
    text.push(character);
  }
  if !warnings.contains(&text) {
    warnings.push(text);
  }
}

#[cfg(test)]
mod tests;
