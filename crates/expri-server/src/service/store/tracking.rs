//! Durable raw tracking bytes and their incrementally maintained SQLite projection.
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::*;

mod projection;
mod raw_io;

use projection::{index_metrics, seal_metric_tail};
use raw_io::{append_raw, io_error, open_raw};

const DOCUMENT_LIMIT: u64 = 16 * 1024 * 1024;

pub(super) fn recover(directory: &Path, connection: &Connection) -> crate::error::Result<()> {
  raw_io::recover(directory, connection)
}

pub(super) fn initialize(connection: &Connection) -> crate::error::Result<()> {
  connection.execute_batch("CREATE TABLE IF NOT EXISTS tracking_versions (
    target TEXT NOT NULL, revision INTEGER NOT NULL, size INTEGER NOT NULL,
    total_size INTEGER NOT NULL, document INTEGER NOT NULL, complete INTEGER NOT NULL,
    sealed INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(target,revision));
    CREATE TABLE IF NOT EXISTS tracking_run_seals (scope TEXT PRIMARY KEY);
    CREATE TABLE IF NOT EXISTS tracking_metric_state (target TEXT PRIMARY KEY, record TEXT NOT NULL, pending BLOB NOT NULL);
    CREATE TABLE IF NOT EXISTS tracking_metric_rows (
      target TEXT NOT NULL, name TEXT NOT NULL, ordinal INTEGER NOT NULL, raw_offset INTEGER NOT NULL,
      step TEXT NOT NULL, value REAL NOT NULL, timestamp TEXT, PRIMARY KEY(target,name,ordinal),
      UNIQUE(target,raw_offset,name));
    CREATE TABLE IF NOT EXISTS tracking_metric_summaries (
      target TEXT NOT NULL, name TEXT NOT NULL, record TEXT NOT NULL,
      first_timestamp_ordinal INTEGER, last_timestamp_ordinal INTEGER, PRIMARY KEY(target,name));")
    .map_err(|_| crate::error::ExpriError::Message("cannot initialize tracking storage".into()))
}

fn decode(data: &str) -> ApiResult<Vec<u8>> {
  if data.len() > STREAM_BATCH.div_ceil(3) * 4 {
    return Err(ApiError::new(413, "tracking batch exceeds 64 KiB"));
  }
  let bytes = base64::engine::general_purpose::STANDARD
    .decode(data)
    .map_err(|_| ApiError::new(400, "invalid tracking encoding"))?;
  if bytes.len() > STREAM_BATCH {
    return Err(ApiError::new(413, "tracking batch exceeds 64 KiB"));
  }
  Ok(bytes)
}

fn target(scope: RunScope, path: String, document: bool) -> ApiResult<FileTarget> {
  validate_scope(&scope).map_err(bad)?;
  if !(if document {
    document_path(&path)
  } else {
    stream_path(&path)
  }) {
    return Err(ApiError::new(400, "invalid tracking path"));
  }
  Ok(FileTarget::Run { scope, path })
}

pub(super) fn reject_managed(db: &Connection, target: &FileTarget) -> ApiResult<()> {
  let exists: bool = db
    .query_row(
      "SELECT EXISTS(SELECT 1 FROM tracking_versions WHERE target=?1)",
      [target_json(target)?],
      |row| row.get(0),
    )
    .map_err(database)?;
  if exists {
    return Err(ApiError::new(409, "file is managed by tracking sync"));
  }
  Ok(())
}

fn scope_key(target: &FileTarget) -> ApiResult<String> {
  let FileTarget::Run { scope, .. } = target else {
    return Err(ApiError::new(400, "tracking files belong to runs"));
  };
  serde_json::to_string(scope).map_err(|_| ApiError::new(500, "cannot encode tracking scope"))
}

fn run_sealed(db: &Connection, target: &FileTarget) -> ApiResult<bool> {
  db.query_row(
    "SELECT EXISTS(SELECT 1 FROM tracking_run_seals WHERE scope=?1)",
    [scope_key(target)?],
    |row| row.get(0),
  )
  .map_err(database)
}

fn current(db: &Connection, target: &FileTarget) -> ApiResult<Option<FileRecord>> {
  let raw: Option<String> = db
    .query_row(
      "SELECT record FROM files WHERE target=?1",
      [target_json(target)?],
      |row| row.get(0),
    )
    .optional()
    .map_err(database)?;
  raw
    .map(|raw| {
      serde_json::from_str::<FileRecord>(&raw)
        .map_err(|_| ApiError::new(500, "invalid tracking record"))
    })
    .transpose()
    .map(|record| record.filter(|record| matches!(record.storage, FileStorage::Tracking { .. })))
}

fn publish(db: &Connection, record: &FileRecord) -> ApiResult<()> {
  let sequence: i64 = db
    .query_row("SELECT COALESCE(MAX(sequence),0)+1 FROM files", [], |row| {
      row.get(0)
    })
    .map_err(database)?;
  db.execute("INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,'',?3)
    ON CONFLICT(target) DO UPDATE SET record=excluded.record,object_key='',sequence=excluded.sequence",
    params![target_json(&record.target)?, serde_json::to_string(record).map_err(|_| ApiError::new(500,"cannot encode tracking record"))?, sequence]).map_err(database)?;
  let FileTarget::Run { scope, .. } = &record.target else {
    unreachable!("tracking run target")
  };
  record_run_activity(db, scope)
}

impl<S: ObjectStorage> Store<S> {
  pub(super) fn reject_managed_tracking(&self, target: &FileTarget) -> ApiResult<()> {
    reject_managed(&*self.db()?, target)
  }

  pub(in crate::service) fn tracking_file(
    &self,
    target: &FileTarget,
  ) -> ApiResult<Option<FileRecord>> {
    validate_target(target).map_err(bad)?;
    current(&*self.db()?, target)
  }

  pub(super) fn put_document(
    &self,
    scope: RunScope,
    path: String,
    revision: u64,
    offset: u64,
    total_size: u64,
    encoded: &str,
  ) -> ApiResult<Response> {
    let target = target(scope, path.clone(), true)?;
    if revision == 0 || revision > i64::MAX as u64 || total_size > DOCUMENT_LIMIT {
      return Err(ApiError::new(
        413,
        "document revision or size exceeds its limit",
      ));
    }
    let bytes = decode(encoded)?;
    if offset
      .checked_add(bytes.len() as u64)
      .is_none_or(|end| end > total_size)
    {
      return Err(ApiError::new(
        409,
        "document batch exceeds its declared size",
      ));
    }
    let key = target_json(&target)?;
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    let latest: Option<u64> = tx
      .query_row(
        "SELECT MAX(revision) FROM tracking_versions WHERE target=?1",
        [&key],
        |row| row.get(0),
      )
      .map_err(database)?;
    if latest.is_some_and(|latest| revision < latest) {
      return Err(ApiError::new(409, "document revision is stale"));
    }
    if path == "outputs/params.json" && latest.is_some_and(|latest| latest != revision) {
      return Err(ApiError::new(409, "run parameters are immutable"));
    }
    let previous: Option<(u64, u64, bool)> = tx
      .query_row(
        "SELECT size,total_size,complete FROM tracking_versions WHERE target=?1 AND revision=?2",
        params![key, revision],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
      )
      .optional()
      .map_err(database)?;
    if run_sealed(&tx, &target)? && !previous.is_some_and(|(_, _, complete)| complete) {
      return Err(ApiError::new(409, "tracking run is sealed"));
    }
    let size = if let Some((size, total, _)) = previous {
      if total != total_size {
        return Err(ApiError::new(
          409,
          "document revision is assigned to different bytes",
        ));
      }
      size
    } else {
      0
    };
    let mut file = open_raw(&self.directory, &target, revision, true)?;
    let end = append_raw(&mut file, size, offset, &bytes)?;
    checkpoint(&self.directory, "raw_synced");
    let complete = end == total_size;
    if complete {
      file.seek(SeekFrom::Start(0)).map_err(io_error)?;
      serde_json::from_reader::<_, Value>((&mut file).take(total_size))
        .map_err(|_| ApiError::new(422, "tracking document contains invalid JSON"))?;
    }
    tx.execute("INSERT INTO tracking_versions(target,revision,size,total_size,document,complete,sealed) VALUES(?1,?2,?3,?4,1,?5,?5)
      ON CONFLICT(target,revision) DO UPDATE SET size=excluded.size,complete=excluded.complete,sealed=excluded.sealed", params![key,revision,end,total_size,complete]).map_err(database)?;
    if complete && !previous.is_some_and(|(_, _, complete)| complete) {
      publish(
        &tx,
        &FileRecord {
          target,
          size: total_size,
          sha256: None,
          storage: FileStorage::Tracking {
            revision,
            sealed: true,
          },
        },
      )?;
    }
    tx.commit().map_err(database)?;
    Ok(Response::DocumentAcknowledged {
      offset: end,
      revision,
      complete,
    })
  }

  pub(super) fn append_tracking(
    &self,
    scope: RunScope,
    path: String,
    offset: u64,
    encoded: &str,
  ) -> ApiResult<Response> {
    let target = target(scope, path.clone(), false)?;
    let bytes = decode(encoded)?;
    let key = target_json(&target)?;
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    let previous: Option<(u64, bool)> = tx
      .query_row(
        "SELECT size,sealed FROM tracking_versions WHERE target=?1 AND revision=1",
        [&key],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .optional()
      .map_err(database)?;
    if run_sealed(&tx, &target)? && previous.is_none() {
      return Err(ApiError::new(409, "tracking run is sealed"));
    }
    let size = previous.map_or(0, |(size, _)| size);
    if previous.is_some_and(|(_, sealed)| sealed)
      && offset
        .checked_add(bytes.len() as u64)
        .is_none_or(|end| end > size)
    {
      return Err(ApiError::new(409, "tracking stream is sealed"));
    }
    let mut file = open_raw(&self.directory, &target, 1, true)?;
    let end = append_raw(&mut file, size, offset, &bytes)?;
    checkpoint(&self.directory, "raw_synced");
    tx.execute("INSERT INTO tracking_versions(target,revision,size,total_size,document,complete,sealed) VALUES(?1,1,?2,?2,0,1,0)
      ON CONFLICT(target,revision) DO UPDATE SET size=excluded.size,total_size=excluded.total_size", params![key,end]).map_err(database)?;
    if path == "outputs/metrics.jsonl" && end > size {
      index_metrics(
        &tx,
        &key,
        size,
        &bytes[(size - offset).min(bytes.len() as u64) as usize..],
      )?;
    }
    if end > size || previous.is_none() {
      publish(
        &tx,
        &FileRecord {
          target,
          size: end,
          sha256: None,
          storage: FileStorage::Tracking {
            revision: 1,
            sealed: previous.is_some_and(|(_, sealed)| sealed),
          },
        },
      )?;
    }
    checkpoint(&self.directory, "projection_written");
    tx.commit().map_err(database)?;
    checkpoint(&self.directory, "tracking_committed");
    Ok(Response::Acknowledged {
      offset: offset + bytes.len() as u64,
    })
  }

  pub(super) fn seal_tracking(
    &self,
    scope: RunScope,
    documents: BTreeMap<String, u64>,
    streams: BTreeMap<String, u64>,
    incomplete: bool,
  ) -> ApiResult<ArchiveRecord> {
    validate_scope(&scope).map_err(bad)?;
    let mut db = self.db()?;
    let tx = db.transaction().map_err(database)?;
    let mut statement = tx.prepare("SELECT record FROM files WHERE json_extract(target,'$.scope.project_id')=?1 AND json_extract(target,'$.scope.origin')=?2 AND json_extract(target,'$.scope.run_id')=?3").map_err(database)?;
    let all = statement
      .query_map(
        params![scope.project_id, scope.origin, scope.run_id],
        |row| row.get::<_, String>(0),
      )
      .map_err(database)?
      .map(|row| {
        row.map_err(database).and_then(|raw| {
          serde_json::from_str::<FileRecord>(&raw)
            .map_err(|_| ApiError::new(500, "invalid tracking record"))
        })
      })
      .collect::<ApiResult<Vec<_>>>()?;
    drop(statement);
    let mut files = Vec::new();
    let mut actual_documents = BTreeMap::new();
    let mut actual_streams = BTreeMap::new();
    for mut record in all {
      let FileStorage::Tracking { revision, .. } = record.storage else {
        continue;
      };
      let FileTarget::Run { path, .. } = &record.target else {
        continue;
      };
      if document_path(path) {
        actual_documents.insert(path.clone(), revision);
      } else if stream_path(path) {
        actual_streams.insert(path.clone(), record.size);
        if !incomplete {
          record.storage = FileStorage::Tracking {
            revision,
            sealed: true,
          };
        }
      } else {
        continue;
      }
      files.push(record);
    }
    if incomplete {
      files.clear();
      for (path, revision) in &documents {
        let file_target = target(scope.clone(), path.clone(), true)?;
        let size:Option<u64>=tx.query_row("SELECT size FROM tracking_versions WHERE target=?1 AND revision=?2 AND document=1 AND complete=1",params![target_json(&file_target)?,revision],|row|row.get(0)).optional().map_err(database)?;
        let size =
          size.ok_or_else(|| ApiError::new(409, "captured document revision is unavailable"))?;
        files.push(FileRecord {
          target: file_target,
          size,
          sha256: None,
          storage: FileStorage::Tracking {
            revision: *revision,
            sealed: true,
          },
        });
      }
      for (path, size) in &streams {
        let file_target = target(scope.clone(), path.clone(), false)?;
        let current:Option<(u64,bool)>=tx.query_row("SELECT size,sealed FROM tracking_versions WHERE target=?1 AND revision=1 AND document=0",[target_json(&file_target)?],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(database)?;
        let (current, sealed) =
          current.ok_or_else(|| ApiError::new(409, "captured stream is unavailable"))?;
        if *size > current {
          return Err(ApiError::new(409, "captured stream prefix is unavailable"));
        }
        files.push(FileRecord {
          target: file_target,
          size: *size,
          sha256: None,
          storage: FileStorage::Tracking {
            revision: 1,
            sealed: sealed && *size == current,
          },
        });
      }
    } else if actual_documents != documents || actual_streams != streams {
      return Err(ApiError::new(
        409,
        "tracking snapshot changed or is incomplete; refresh and retry",
      ));
    }
    if files.is_empty() {
      return Err(ApiError::new(409, "tracking snapshot is empty"));
    }
    files.sort_by(|first, second| {
      let FileTarget::Run { path: first, .. } = &first.target else {
        unreachable!()
      };
      let FileTarget::Run { path: second, .. } = &second.target else {
        unreachable!()
      };
      first.cmp(second)
    });
    if !incomplete {
      let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM tracking_versions v WHERE json_extract(v.target,'$.scope.project_id')=?1 AND json_extract(v.target,'$.scope.origin')=?2 AND json_extract(v.target,'$.scope.run_id')=?3 AND v.document=1 AND v.complete=0 AND v.revision=(SELECT MAX(revision) FROM tracking_versions newer WHERE newer.target=v.target))",params![scope.project_id,scope.origin,scope.run_id],|row|row.get(0)).map_err(database)?;
      if pending {
        return Err(ApiError::new(
          409,
          "tracking document publication is incomplete",
        ));
      }
      let state = files
        .iter()
        .find(|record| matches!(&record.target,FileTarget::Run{path,..} if path=="run-state.json"))
        .ok_or_else(|| ApiError::new(409, "run state is missing"))?;
      let FileStorage::Tracking { revision, .. } = state.storage else {
        unreachable!()
      };
      let mut file = open_raw(&self.directory, &state.target, revision, false)?;
      let state: Value = serde_json::from_reader((&mut file).take(state.size))
        .map_err(|_| ApiError::new(409, "run state is invalid"))?;
      if !matches!(
        state["status"].as_str(),
        Some("completed" | "failed" | "cancelled" | "lost")
      ) {
        return Err(ApiError::new(409, "run is not terminal"));
      }
      for record in &files {
        if let FileTarget::Run { path, .. } = &record.target
          && stream_path(path)
        {
          let already_sealed: bool = tx
            .query_row(
              "SELECT sealed FROM tracking_versions WHERE target=?1 AND revision=1",
              [target_json(&record.target)?],
              |row| row.get(0),
            )
            .map_err(database)?;
          if already_sealed {
            continue;
          }
          if path == "outputs/metrics.jsonl" {
            seal_metric_tail(&tx, &record.target, record.size)?;
          }
          tx.execute(
            "UPDATE tracking_versions SET sealed=1 WHERE target=?1 AND revision=1",
            [target_json(&record.target)?],
          )
          .map_err(database)?;
          publish(&tx, record)?;
        }
      }
    }
    if !incomplete {
      tx.execute(
        "INSERT OR IGNORE INTO tracking_run_seals(scope) VALUES(?1)",
        [serde_json::to_string(&scope)
          .map_err(|_| ApiError::new(500, "cannot encode tracking scope"))?],
      )
      .map_err(database)?;
    }
    tx.commit().map_err(database)?;
    drop(db);
    self.queue_archive(ArchiveSnapshot {
      scope,
      files,
      incomplete,
    })
  }
}

#[cfg(test)]
fn checkpoint(directory: &Path, phase: &str) {
  if std::env::var("EXPRI_TRACKING_CRASH_PHASE").ok().as_deref() == Some(phase) {
    std::fs::write(directory.join("tracking-test-ready"), b"ready").unwrap();
    loop {
      std::thread::park();
    }
  }
}
#[cfg(not(test))]
fn checkpoint(_: &Path, _: &str) {}

#[cfg(test)]
mod tests;
