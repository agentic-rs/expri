//! Archives are built from a captured, acknowledged tracking prefix.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Body, Client};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

use super::*;

pub(super) fn initialize(connection: &Connection) -> rusqlite::Result<()> {
  connection.execute_batch(
    "CREATE TABLE IF NOT EXISTS result_archives (
    id INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL, snapshot TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending', last_error TEXT, retry_at INTEGER NOT NULL DEFAULT 0);
    CREATE INDEX IF NOT EXISTS result_archives_scope ON result_archives(scope,id);",
  )
}

/// Archive order is captured job order, independent of when multipart begins.
pub(super) fn can_publish(
  db: &Connection,
  upload_id: &str,
  target: &FileTarget,
) -> ApiResult<bool> {
  let FileTarget::Run { scope, path } = target else {
    return Ok(true);
  };
  if path != "result.zip" {
    return Ok(true);
  }
  let Some(job) = upload_id
    .strip_prefix("result-archive-")
    .and_then(|id| id.parse::<i64>().ok())
  else {
    return Err(ApiError::new(
      403,
      "result.zip uploads are managed by the server",
    ));
  };
  let latest: Option<i64> = db
    .query_row(
      "SELECT MAX(id) FROM result_archives WHERE scope=?1",
      [scope_key(scope)?],
      |row| row.get(0),
    )
    .map_err(database)?;
  Ok(latest == Some(job))
}

fn scope_key(scope: &RunScope) -> ApiResult<String> {
  validate_scope(scope).map_err(bad)?;
  serde_json::to_string(scope).map_err(|_| ApiError::new(500, "cannot encode archive scope"))
}

fn now() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs()
    .min(i64::MAX as u64) as i64
}

impl<S: ObjectStorage> Store<S> {
  pub(super) fn queue_archive(&self, snapshot: ArchiveSnapshot) -> ApiResult<ArchiveRecord> {
    let scope = scope_key(&snapshot.scope)?;
    if snapshot.files.is_empty() || snapshot.files.len() > 8 {
      return Err(ApiError::new(
        400,
        "archive requires a bounded tracking snapshot",
      ));
    }
    let mut paths = std::collections::BTreeSet::new();
    for record in &snapshot.files {
      let FileTarget::Run {
        scope: file_scope,
        path,
      } = &record.target
      else {
        return Err(ApiError::new(400, "archive contains a private input"));
      };
      if file_scope != &snapshot.scope
        || !(document_path(path) || stream_path(path))
        || !matches!(record.storage, FileStorage::Tracking { .. })
        || !paths.insert(path)
      {
        return Err(ApiError::new(
          400,
          "archive contains an invalid tracking file",
        ));
      }
    }
    let encoded = serde_json::to_string(&snapshot)
      .map_err(|_| ApiError::new(500, "cannot encode archive snapshot"))?;
    {
      let mut db = self.db()?;
      let transaction = db.transaction().map_err(database)?;
      let previous: Option<String> = transaction
        .query_row(
          "SELECT snapshot FROM result_archives WHERE scope=?1 ORDER BY id DESC LIMIT 1",
          [&scope],
          |row| row.get(0),
        )
        .optional()
        .map_err(database)?;
      if previous.as_deref() != Some(&encoded) {
        transaction
          .execute(
            "INSERT INTO result_archives(scope,snapshot) VALUES(?1,?2)",
            params![scope, encoded],
          )
          .map_err(database)?;
        record_run_activity(&transaction, &snapshot.scope)?;
      }
      transaction.commit().map_err(database)?;
    }
    self.archive_status(&snapshot.scope)
  }

  pub(super) fn archive_status(&self, scope: &RunScope) -> ApiResult<ArchiveRecord> {
    let key = scope_key(scope)?;
    let target = FileTarget::Run {
      scope: scope.clone(),
      path: "result.zip".into(),
    };
    // Keep the newest job's completeness and published file in the same read view.
    let row: Option<(String, String, Option<String>, Option<String>)> = self
      .db()?
      .query_row(
        "SELECT a.status,a.snapshot,a.last_error,f.record FROM result_archives a
       LEFT JOIN files f ON f.target=?2 WHERE a.scope=?1 ORDER BY a.id DESC LIMIT 1",
        params![key, target_json(&target)?],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
      )
      .optional()
      .map_err(database)?;
    let Some((status, snapshot, last_error, encoded_file)) = row else {
      return Ok(ArchiveRecord {
        status: "none".into(),
        incomplete: false,
        file: None,
        last_error: None,
      });
    };
    let snapshot: ArchiveSnapshot = serde_json::from_str(&snapshot)
      .map_err(|_| ApiError::new(500, "invalid archive snapshot"))?;
    let file = encoded_file
      .map(|encoded| {
        let record: FileRecord = serde_json::from_str(&encoded)
          .map_err(|_| ApiError::new(500, "invalid archive record"))?;
        if record.target != target || !matches!(record.storage, FileStorage::Object) {
          return Err(ApiError::new(500, "invalid archive record"));
        }
        Ok(record)
      })
      .transpose()?;
    Ok(ArchiveRecord {
      status,
      incomplete: snapshot.incomplete,
      file,
      last_error,
    })
  }

  /// One dedicated archive worker calls this; network I/O never holds the database lock.
  pub(in crate::service) fn archive_cycle(&self) -> ApiResult<bool> {
    let row: Option<(i64, String)> = self
      .db()?
      .query_row(
        "SELECT id,snapshot FROM result_archives AS candidate WHERE status IN ('pending','uploading','failed')
        AND retry_at<=?1 AND id=(SELECT MAX(id) FROM result_archives WHERE scope=candidate.scope) ORDER BY id LIMIT 1",
        [now()],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .optional()
      .map_err(database)?;
    let Some((id, encoded)) = row else {
      return Ok(false);
    };
    let gate = self.upload_gate(&format!("archive-job-{id}"))?;
    let _gate = gate
      .lock()
      .map_err(|_| ApiError::new(503, "archive worker unavailable"))?;
    let snapshot: ArchiveSnapshot =
      serde_json::from_str(&encoded).map_err(|_| ApiError::new(500, "invalid archive snapshot"))?;
    {
      let mut db = self.db()?;
      let transaction = db.transaction().map_err(database)?;
      let updated = transaction.execute(
        "UPDATE result_archives SET status='uploading',last_error=NULL WHERE id=?1 AND status!='archived'", [id])
        .map_err(database)?;
      if updated == 0 {
        return Ok(false);
      }
      record_run_activity(&transaction, &snapshot.scope)?;
      transaction.commit().map_err(database)?;
    }
    let result = self.upload_archive(id, &snapshot);
    let mut db = self.db()?;
    let transaction = db.transaction().map_err(database)?;
    match &result {
      Ok(()) => {
        transaction
          .execute(
            "UPDATE result_archives SET status='archived',last_error=NULL,retry_at=0 WHERE id=?1",
            [id],
          )
          .map_err(database)?;
      }
      Err(_) => {
        transaction.execute("UPDATE result_archives SET status='failed',last_error='Archive upload is pending; stored tracking data is retained.',retry_at=?2 WHERE id=?1", params![id, now().saturating_add(30)]).map_err(database)?;
      }
    }
    record_run_activity(&transaction, &snapshot.scope)?;
    transaction.commit().map_err(database)?;
    result.map(|()| true)
  }

  fn build_archive(&self, id: i64, snapshot: &ArchiveSnapshot) -> ApiResult<PathBuf> {
    let parent = self.directory.join("archives").join(id.to_string());
    private_directory(&self.directory.join("archives"))?;
    private_directory(&parent)?;
    let destination = parent.join("result.zip");
    let manifest = json!({"schema_version": 1, "scope": snapshot.scope,
      "incomplete": snapshot.incomplete, "files": snapshot.files.iter().map(|record| {
        let FileTarget::Run { path, .. } = &record.target else { unreachable!() };
        json!({"path": path, "size": record.size, "storage": record.storage})
      }).collect::<Vec<_>>()});
    let manifest = serde_json::to_vec_pretty(&manifest)
      .map_err(|_| ApiError::new(500, "cannot encode archive manifest"))?;
    if destination.exists() {
      let file = regular_file(&destination)?;
      let mut archive = zip::ZipArchive::new(file)
        .map_err(|_| ApiError::new(500, "stored result archive is invalid"))?;
      let mut original = Vec::new();
      archive
        .by_name("manifest.json")
        .map_err(|_| ApiError::new(500, "stored archive manifest is missing"))?
        .take(64 * 1024 + 1)
        .read_to_end(&mut original)
        .map_err(|_| ApiError::new(500, "cannot read stored archive manifest"))?;
      if original != manifest {
        return Err(ApiError::new(
          409,
          "stored archive has a different snapshot",
        ));
      }
      return Ok(destination);
    }
    let temporary = tempfile::NamedTempFile::new_in(&parent)
      .map_err(|_| ApiError::new(507, "cannot stage result archive"))?;
    let mut writer = zip::ZipWriter::new(
      temporary
        .as_file()
        .try_clone()
        .map_err(|_| ApiError::new(500, "cannot open result archive"))?,
    );
    let options = SimpleFileOptions::default()
      .compression_method(zip::CompressionMethod::Deflated)
      .unix_permissions(0o600)
      .large_file(true);
    for record in &snapshot.files {
      let FileTarget::Run { path, .. } = &record.target else {
        unreachable!()
      };
      writer
        .start_file(path, options)
        .map_err(|_| ApiError::new(507, "cannot write result archive"))?;
      let source = self.tracking_open(record)?;
      let written = std::io::copy(&mut source.take(record.size), &mut writer)
        .map_err(|_| ApiError::new(507, "cannot copy tracking data into result archive"))?;
      if written != record.size {
        return Err(ApiError::new(409, "tracking snapshot is incomplete"));
      }
    }
    writer
      .start_file("manifest.json", options)
      .map_err(|_| ApiError::new(507, "cannot write archive manifest"))?;
    std::io::Write::write_all(&mut writer, &manifest)
      .map_err(|_| ApiError::new(507, "cannot write archive manifest"))?;
    let file = writer
      .finish()
      .map_err(|_| ApiError::new(507, "cannot finish result archive"))?;
    file
      .sync_all()
      .map_err(|_| ApiError::new(507, "cannot save result archive"))?;
    temporary
      .persist_noclobber(&destination)
      .map_err(|_| ApiError::new(507, "cannot publish result archive"))?;
    File::open(&parent)
      .and_then(|directory| directory.sync_all())
      .map_err(|_| ApiError::new(507, "cannot save result archive directory"))?;
    Ok(destination)
  }

  fn upload_archive(&self, id: i64, snapshot: &ArchiveSnapshot) -> ApiResult<()> {
    let path = self.build_archive(id, snapshot)?;
    let mut file = regular_file(&path)?;
    let size = file
      .metadata()
      .map_err(|_| ApiError::new(500, "cannot inspect archive"))?
      .len();
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
      let count = file
        .read(&mut buffer)
        .map_err(|_| ApiError::new(500, "cannot read archive"))?;
      if count == 0 {
        break;
      }
      digest.update(&buffer[..count]);
    }
    let sha256 = digest
      .finalize()
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect::<String>();
    let upload_id = format!("result-archive-{id}");
    let target = FileTarget::Run {
      scope: snapshot.scope.clone(),
      path: "result.zip".into(),
    };
    let Response::Upload { upload } = self.begin(&upload_id, target, size, sha256)? else {
      unreachable!()
    };
    if !upload.complete {
      let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|_| ApiError::new(503, "cannot initialize archive transfer"))?;
      for number in 1..=size.div_ceil(upload.part_size).max(1) as u32 {
        if upload.parts.iter().any(|part| part.part_number == number) {
          continue;
        }
        let Response::Url { url } = self.execute(Request::PartUrl {
          upload_id: upload_id.clone(),
          part_number: number,
        })?
        else {
          unreachable!()
        };
        let offset = (number as u64 - 1) * upload.part_size;
        let length = size.saturating_sub(offset).min(upload.part_size);
        file
          .seek(SeekFrom::Start(offset))
          .map_err(|_| ApiError::new(500, "cannot seek archive"))?;
        let source = file
          .try_clone()
          .map_err(|_| ApiError::new(500, "cannot read archive part"))?;
        let reply = client
          .put(url)
          .body(Body::sized(source.take(length), length))
          .send()
          .map_err(|_| ApiError::new(502, "archive part upload failed"))?;
        if !reply.status().is_success() {
          return Err(ApiError::new(502, "archive part upload failed"));
        }
        let etag = reply
          .headers()
          .get(reqwest::header::ETAG)
          .and_then(|value| value.to_str().ok())
          .ok_or_else(|| ApiError::new(502, "archive part receipt is missing"))?
          .to_owned();
        self.record_part(
          &upload_id,
          CompletedPart {
            part_number: number,
            etag,
          },
        )?;
      }
    }
    self.complete(&upload_id).map(|_| ())
  }
}

fn private_directory(path: &Path) -> ApiResult<()> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    if !path.exists() {
      fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|_| ApiError::new(507, "cannot create archive directory"))?;
    }
  }
  #[cfg(not(unix))]
  {
    if !path.exists() {
      fs::create_dir(path).map_err(|_| ApiError::new(507, "cannot create archive directory"))?;
    }
  }
  if !fs::symlink_metadata(path)
    .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
  {
    return Err(ApiError::new(
      500,
      "archive directory must be a real directory",
    ));
  }
  Ok(())
}

fn regular_file(path: &Path) -> ApiResult<File> {
  let mut options = fs::OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
  }
  let file = options
    .open(path)
    .map_err(|_| ApiError::new(500, "cannot open archive file"))?;
  if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
    return Err(ApiError::new(500, "archive must be a regular file"));
  }
  Ok(file)
}

#[cfg(test)]
mod tests;
