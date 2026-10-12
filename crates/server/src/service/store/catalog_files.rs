//! A bounded union of received run files and the worker's published inventory.

use std::time::{Duration, Instant};

use expri_core::service_catalog::{
  MAX_CATALOG_PAGE_BYTES, ReceivedRunFile, ReportedRunFile, RunFileEntry, RunFilesPage,
  RunFilesQuery, file_availability, file_labels_pending, validate_run_files_page,
};

use super::*;
use crate::dashboard::artifacts::parse_inventory;
use crate::service::dashboard_data::HostedDashboard;

const READ_BUDGET: Duration = Duration::from_secs(3);
// The keyset cursor repeats the last raw path. Reserve its worst-case JSON
// encoding and the response envelope independently of individual row lengths.
const CURSOR_RESERVE: usize = 8 * 1024;
const INVENTORY_UNAVAILABLE: &str =
  "Worker artifact inventory is unavailable or invalid; reported files may be missing.";
const INVENTORY_TRUNCATED: &str =
  "Worker artifact inventory is limited to 200 files and 64 KiB; some reported files are omitted.";

impl<S: ObjectStorage> Store<S> {
  pub(in crate::service) fn browse_run_files(
    &self,
    query: &RunFilesQuery,
  ) -> ApiResult<RunFilesPage> {
    query.validate().map_err(bad)?;
    {
      let db = self.db()?;
      ensure_run(&db, &query.scope)?;
    }
    let mut page = RunFilesPage {
      scope: query.scope.clone(),
      files: Vec::new(),
      next_cursor: None,
      inventory_recorded_at: None,
      reported_inventory_truncated: false,
      warnings: Vec::new(),
    };
    let mut entries = BTreeMap::new();
    // Do not hold SQLite or project locks across this one small metadata read.
    // Checkpoints, archives, arbitrary output data, and provenance are not read.
    let dashboard = HostedDashboard::new(self)
      .map_err(|_| ApiError::new(500, "cannot initialize run inventory reader"))?;
    let inventory = dashboard.catalog_document(
      &query.scope,
      crate::run_artifacts::INVENTORY_PATH,
      crate::run_artifacts::INVENTORY_LIMIT as u64,
      Instant::now() + READ_BUDGET,
    );
    match inventory {
      Ok(Some(value)) => match parse_inventory(&value) {
        Ok(inventory) => {
          page.inventory_recorded_at = inventory.recorded_at;
          page.reported_inventory_truncated = inventory.truncated;
          if inventory.truncated {
            page.warnings.push(INVENTORY_TRUNCATED.into());
          }
          for file in inventory.files {
            if selected_path(query, &file.path) {
              entries.insert(
                file.path.clone(),
                entry(
                  file.path,
                  None,
                  Some(ReportedRunFile {
                    size: file.size,
                    sha256: file.sha256,
                    labels: file.labels,
                    sync_status: file.sync_status,
                    sync_error: file.sync_error,
                  }),
                ),
              );
            }
          }
        }
        Err(_) => page.warnings.push(INVENTORY_UNAVAILABLE.into()),
      },
      Ok(None) => {}
      Err(error) if error.status == 410 => return Err(error),
      Err(_) => page.warnings.push(INVENTORY_UNAVAILABLE.into()),
    }
    {
      let db = self.db()?;
      // An archive may expire or a project may be deleted during the read.
      ensure_run(&db, &query.scope)?;
      for (path, received) in received_files(&db, query)? {
        let reported = entries.remove(&path).and_then(|entry| entry.reported);
        entries.insert(path.clone(), entry(path, Some(received), reported));
      }
    }
    let envelope_bytes = serde_json::to_vec(&page)
      .map_err(|_| ApiError::new(500, "cannot encode run files page"))?
      .len();
    let budget = MAX_CATALOG_PAGE_BYTES.saturating_sub(envelope_bytes + CURSOR_RESERVE);
    let mut bytes = 0;
    for file in entries.into_values() {
      let length = serde_json::to_vec(&file)
        .map_err(|_| ApiError::new(500, "cannot encode run file metadata"))?
        .len()
        + usize::from(!page.files.is_empty());
      if page.files.len() == query.limit || bytes + length > budget {
        page.next_cursor = page.files.last().map(|file| file.path.clone());
        if page.next_cursor.is_none() {
          return Err(ApiError::new(
            413,
            "run file metadata exceeds the page byte limit",
          ));
        }
        break;
      }
      bytes += length;
      page.files.push(file);
    }
    validate_run_files_page(query, &page)
      .map_err(|_| ApiError::new(500, "invalid generated run files page"))?;
    Ok(page)
  }
}

fn ensure_run(db: &Connection, scope: &RunScope) -> ApiResult<()> {
  projects::ensure_active(db, &scope.project_id)?;
  run_retention::ensure_available(db, scope, run_retention::now())?;
  let exists: bool = db
    .query_row(
      "SELECT EXISTS(SELECT 1 FROM files WHERE json_extract(target,'$.kind')='run' \
         AND json_extract(target,'$.scope.project_id')=?1 \
         AND json_extract(target,'$.scope.origin')=?2 \
         AND json_extract(target,'$.scope.run_id')=?3 \
       UNION ALL SELECT 1 FROM streams WHERE json_extract(target,'$.kind')='run' \
         AND json_extract(target,'$.scope.project_id')=?1 \
         AND json_extract(target,'$.scope.origin')=?2 \
         AND json_extract(target,'$.scope.run_id')=?3)",
      params![scope.project_id, scope.origin, scope.run_id],
      |row| row.get(0),
    )
    .map_err(database)?;
  if !exists {
    return Err(ApiError::new(404, "run is missing"));
  }
  Ok(())
}

fn selected_path(query: &RunFilesQuery, path: &str) -> bool {
  query
    .prefix
    .as_ref()
    .is_none_or(|prefix| path.starts_with(prefix))
    && query
      .cursor
      .as_ref()
      .is_none_or(|cursor| path > cursor.as_str())
}

fn entry(
  path: String,
  received: Option<ReceivedRunFile>,
  reported: Option<ReportedRunFile>,
) -> RunFileEntry {
  RunFileEntry {
    path,
    availability: file_availability(received.as_ref(), reported.as_ref())
      .expect("catalog entries always have received or reported metadata"),
    labels_pending: file_labels_pending(received.as_ref(), reported.as_ref()),
    received,
    reported,
  }
}

fn received_files(
  db: &Connection,
  query: &RunFilesQuery,
) -> ApiResult<Vec<(String, ReceivedRunFile)>> {
  // Extract only scalar receipt metadata. Parsing FileRecord here would also
  // materialize provenance for every checkpoint even though no row exposes it.
  // Files contains both completed objects and acknowledged tracking prefixes.
  // A file receipt wins over an older legacy stream for the same target.
  let mut statement = db
    .prepare(
      "SELECT path,size,sha256,storage,revision,sealed FROM (\
        SELECT json_extract(target,'$.path') AS path, \
          json_extract(record,'$.size') AS size, \
          json_extract(record,'$.sha256') AS sha256, \
          CASE WHEN json_type(record,'$.storage')='object' THEN 'tracking' \
            ELSE json_extract(record,'$.storage') END AS storage, \
          json_extract(record,'$.storage.tracking.revision') AS revision, \
          json_extract(record,'$.storage.tracking.sealed') AS sealed \
        FROM files WHERE json_extract(target,'$.kind')='run' \
          AND json_extract(target,'$.scope.project_id')=?1 \
          AND json_extract(target,'$.scope.origin')=?2 \
          AND json_extract(target,'$.scope.run_id')=?3 \
        UNION ALL SELECT json_extract(s.target,'$.path'),s.size,NULL,'stream',NULL,NULL \
        FROM streams s WHERE json_extract(s.target,'$.kind')='run' \
          AND json_extract(s.target,'$.scope.project_id')=?1 \
          AND json_extract(s.target,'$.scope.origin')=?2 \
          AND json_extract(s.target,'$.scope.run_id')=?3 \
          AND NOT EXISTS(SELECT 1 FROM files f WHERE f.target=s.target)) \
        WHERE (?4 IS NULL OR substr(path,1,length(?4))=?4) \
          AND (?5 IS NULL OR path COLLATE BINARY>?5) \
        ORDER BY path COLLATE BINARY LIMIT ?6",
    )
    .map_err(database)?;
  let rows = statement
    .query_map(
      params![
        query.scope.project_id,
        query.scope.origin,
        query.scope.run_id,
        query.prefix,
        query.cursor,
        query.limit + 1
      ],
      |row| {
        Ok((
          row.get::<_, String>(0)?,
          row.get::<_, u64>(1)?,
          row.get::<_, Option<String>>(2)?,
          row.get::<_, String>(3)?,
          row.get::<_, Option<u64>>(4)?,
          row.get::<_, Option<bool>>(5)?,
        ))
      },
    )
    .map_err(database)?;
  let mut files = Vec::with_capacity(query.limit + 1);
  for row in rows {
    let (path, size, sha256, storage, revision, sealed) = row.map_err(database)?;
    validate_run_path(&path).map_err(|_| ApiError::new(500, "invalid stored run file path"))?;
    let storage = match storage.as_str() {
      "object" => FileStorage::Object,
      "stream" => FileStorage::Stream,
      "tracking" if revision.is_some_and(|value| value > 0) && sealed.is_some() => {
        FileStorage::Tracking {
          revision: revision.unwrap(),
          sealed: sealed.unwrap(),
        }
      }
      _ => return Err(ApiError::new(500, "invalid stored run file storage")),
    };
    files.push((
      path,
      ReceivedRunFile {
        size,
        sha256,
        storage,
      },
    ));
  }
  Ok(files)
}

#[cfg(test)]
mod tests;
