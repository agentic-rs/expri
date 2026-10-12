//! Durable run-owned copies of observations, separate from rolling machine history.

use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use expri_core::lock::{self, LockAttempt};
use expri_core::run_system::{RUN_SYSTEM_PATH, RUN_SYSTEM_SCHEMA_VERSION, RunSystemRecord};
use expri_core::storage::StoragePaths;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SCHEMA: &str = include_str!("run_capture.sql");
const CHECKPOINT: &str = ".system-profile-checkpoint.json";
const LEASE: &str = ".system-profile.lock";
const PAGE: usize = 64;
const PAGE_BYTES: usize = 900 * 1024;
const FINAL_WAIT: Duration = Duration::from_secs(130);
const UNSUPPORTED_WAIT: Duration = Duration::from_secs(5);
const UNAVAILABLE: &str = "System capture is unavailable. Use an upgraded local collector and keep outputs/system-profile.jsonl reserved for system readings.";

pub(super) fn initialize(connection: &Connection) -> Result<()> {
  sql(connection.execute_batch(SCHEMA))?;
  let present: bool = sql(connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('system_run_captures') WHERE name='coverage_lost')", [], |row| row.get(0)))?;
  if !present {
    sql(connection.execute_batch(
      "ALTER TABLE system_run_captures ADD COLUMN coverage_lost INTEGER NOT NULL DEFAULT 0",
    ))?;
  }
  Ok(())
}

pub(super) fn check_support(transaction: &Transaction<'_>, machine_id: &str) -> Result<()> {
  // A collector downgrade can still advance raw history without copying into
  // the outbox. Never erase that gap when a newer collector resumes.
  sql(transaction.execute(
    "UPDATE system_run_captures SET coverage_lost=1 WHERE machine_id=?1 AND closed=0 AND acknowledged=0 AND EXISTS(SELECT 1 FROM system_local WHERE sequence>0) AND NOT EXISTS(SELECT 1 FROM system_run_capture_support p JOIN system_local l ON l.singleton=p.singleton WHERE p.sequence=l.sequence)",
    [machine_id],
  ))?;
  Ok(())
}

pub(super) fn update_support(transaction: &Transaction<'_>) -> Result<()> {
  sql(transaction.execute("INSERT INTO system_run_capture_support(singleton,sequence) SELECT singleton,sequence FROM system_local WHERE singleton=1 ON CONFLICT(singleton) DO UPDATE SET sequence=excluded.sequence", []))?;
  Ok(())
}

/// Called in the same transaction as the authoritative local sample.
pub(super) fn record(
  transaction: &Transaction<'_>,
  machine_id: &str,
  sample: &SystemSample,
) -> Result<()> {
  let inventory: String = sql(transaction.query_row(
    "SELECT payload FROM system_inventories WHERE machine_id=?1 AND revision=?2",
    params![machine_id, integer(sample.inventory_revision)?],
    |row| row.get(0),
  ))?;
  let payload = json(sample)?;
  sql(transaction.execute(
    "INSERT OR IGNORE INTO system_run_capture_samples(capture_id,sequence,observed_at_ms,inventory,sample) SELECT capture_id,?2,?3,?4,?5 FROM system_run_captures WHERE machine_id=?1 AND closed=0 AND acknowledged=0 AND start_ms<=?3 AND (end_ms IS NULL OR ?3<end_ms)",
    params![machine_id, integer(sample.sequence)?, sample.observed_at_ms, inventory, payload],
  ))?;
  // The single collector sees this close request only after finishing its prior
  // collection. Its next committed observation is therefore a durable fence.
  sql(transaction.execute(
    "UPDATE system_run_captures SET acknowledged=1 WHERE machine_id=?1 AND closed=0 AND end_ms IS NOT NULL AND end_ms<=?2",
    params![machine_id, sample.observed_at_ms],
  ))?;
  update_support(transaction)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
  capture_id: String,
  file_identity: String,
  committed_bytes: u64,
  closed: bool,
}

#[derive(Clone)]
struct Capture {
  capture_id: String,
  machine_id: String,
  run_id: String,
  run_dir: PathBuf,
  start_ms: i64,
  end_ms: Option<i64>,
  acknowledged: bool,
  coverage_lost: bool,
  closed: bool,
  complete: bool,
  file_offset: u64,
  file_identity: String,
  inventory_revision: u64,
  warnings: Vec<String>,
}

fn capture(connection: &Connection, capture_id: &str) -> Result<Capture> {
  validate_component(capture_id)?;
  let (mut capture, warnings) = sql(connection.query_row(
    "SELECT machine_id,run_id,run_dir,start_ms,end_ms,acknowledged,closed,complete,file_offset,file_identity,inventory_revision,warnings,coverage_lost FROM system_run_captures WHERE capture_id=?1",
    [capture_id],
    |row| {
      Ok((Capture {
        capture_id: capture_id.into(),
        machine_id: row.get(0)?,
        run_id: row.get(1)?,
        run_dir: PathBuf::from(row.get::<_, String>(2)?),
        start_ms: row.get(3)?,
        end_ms: row.get(4)?,
        acknowledged: row.get(5)?,
        closed: row.get(6)?,
        complete: row.get(7)?,
        file_offset: row.get(8)?,
        file_identity: row.get(9)?,
        inventory_revision: row.get(10)?,
        coverage_lost: row.get(12)?,
        warnings: Vec::new(),
      }, row.get::<_, String>(11)?))
    },
  ))?;
  capture.warnings = decode(&warnings)?;
  Ok(capture)
}

fn identity(file: &File) -> Result<String> {
  let metadata = file.metadata()?;
  if !metadata.is_file() {
    return Err(rejection(400, "run system stream must be a regular file"));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
  }
  #[cfg(not(unix))]
  {
    Ok("capture-header".into())
  }
}

fn directory(path: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(rejection(
      400,
      "run system capture directories must not be symlinks",
    ));
  }
  Ok(())
}

fn bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
    return Err(rejection(
      400,
      "run capture document must be a bounded regular file",
    ));
  }
  let file = File::open(path)?;
  if !file.metadata()?.is_file() {
    return Err(rejection(400, "run capture document is not regular"));
  }
  let mut bytes = Vec::new();
  file.take(limit + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > limit {
    return Err(rejection(
      400,
      "run capture document exceeds its read limit",
    ));
  }
  Ok(bytes)
}

fn open_stream(run_dir: &Path, create: bool) -> Result<File> {
  directory(run_dir)?;
  directory(&run_dir.join("outputs"))?;
  let path = run_dir.join(RUN_SYSTEM_PATH);
  if !create {
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
      return Err(rejection(400, "run system stream must not be a symlink"));
    }
  }
  let mut options = OpenOptions::new();
  options.read(true).write(true).create_new(create);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  let file = options.open(&path)?;
  if fs::symlink_metadata(&path)?.file_type().is_symlink() {
    return Err(rejection(400, "run system stream changed while opening"));
  }
  identity(&file)?;
  Ok(file)
}

fn line(file: &mut File, record: &RunSystemRecord) -> Result<()> {
  record.validate()?;
  serde_json::to_writer(&mut *file, record)?;
  file.write_all(b"\n")?;
  Ok(())
}

fn header(file: &File) -> Result<RunSystemRecord> {
  let mut copy = file.try_clone()?;
  copy.seek(SeekFrom::Start(0))?;
  let mut bytes = Vec::new();
  BufReader::new(copy.take(1024)).read_until(b'\n', &mut bytes)?;
  let result: RunSystemRecord = serde_json::from_slice(&bytes)?;
  result.validate()?;
  Ok(result)
}

fn validate_file(file: &File, row: &Capture) -> Result<()> {
  if identity(file)? != row.file_identity
    || file.metadata()?.len() < row.file_offset
    || header(file)?
      != (RunSystemRecord::Capture {
        schema_version: RUN_SYSTEM_SCHEMA_VERSION,
        capture_id: row.capture_id.clone(),
        machine_id: row.machine_id.clone(),
        start_ms: row.start_ms,
      })
  {
    return Err(rejection(
      409,
      "run system stream identity or committed prefix changed",
    ));
  }
  Ok(())
}

fn save_checkpoint(row: &Capture) -> Result<()> {
  let path = row.run_dir.join(CHECKPOINT);
  if let Ok(metadata) = fs::symlink_metadata(&path) {
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16 * 1024 {
      return Err(rejection(
        400,
        "run system checkpoint must be a bounded regular file",
      ));
    }
    let current: Checkpoint = serde_json::from_slice(&bounded_file(&path, 16 * 1024)?)?;
    if current.capture_id != row.capture_id || current.file_identity != row.file_identity {
      return Err(rejection(
        409,
        "run system checkpoint belongs to another producer",
      ));
    }
  }
  let mut temporary = tempfile::NamedTempFile::new_in(&row.run_dir)?;
  serde_json::to_writer(
    &mut temporary,
    &Checkpoint {
      capture_id: row.capture_id.clone(),
      file_identity: row.file_identity.clone(),
      committed_bytes: row.file_offset,
      closed: row.closed,
    },
  )?;
  temporary.as_file().sync_all()?;
  temporary.persist(path).map_err(|error| error.error)?;
  File::open(&row.run_dir)?.sync_all()?;
  Ok(())
}

mod export;

fn backfill(transaction: &Transaction<'_>, capture_id: &str) -> Result<()> {
  sql(transaction.execute("INSERT OR IGNORE INTO system_run_capture_samples(capture_id,sequence,observed_at_ms,inventory,sample) SELECT c.capture_id,s.sequence,s.observed_at_ms,i.payload,s.payload FROM system_run_captures c JOIN system_samples s ON s.machine_id=c.machine_id JOIN system_inventories i ON i.machine_id=s.machine_id AND i.revision=s.inventory_revision WHERE c.capture_id=?1 AND c.closed=0 AND s.sequence>c.exported_sequence AND s.observed_at_ms>=c.start_ms AND (c.end_ms IS NULL OR s.observed_at_ms<c.end_ms) AND NOT EXISTS(SELECT 1 FROM system_local_quarantine q WHERE q.machine_id=s.machine_id AND q.kind='sample' AND q.record_key=s.sequence)", [capture_id]))?;
  Ok(())
}

fn existing_store() -> Result<Option<(StoragePaths, LocalStore)>> {
  let paths = StoragePaths::resolve(None)?;
  paths.validate_existing()?;
  let Some(store) = LocalStore::open_existing(&paths.profile_database())? else {
    return Ok(None);
  };
  // Storage allows trusted aliases above its owned boundary (including /var
  // on macOS), while the generic lock API requires an absolute real path.
  let paths = StoragePaths::from_root(fs::canonicalize(paths.root())?)?;
  Ok(Some((paths, store)))
}

fn known_end_from_store(store: &LocalStore, run_dir: &Path, state: &Value) -> Result<Option<i64>> {
  let Some(id) = state["system_capture_id"].as_str() else {
    return Ok(None);
  };
  let row = capture(&store.connection, id)?;
  directory(run_dir)?;
  if fs::canonicalize(run_dir)? != row.run_dir
    || state["run_id"] != row.run_id
    || state["machine_id"].as_str() != Some(row.machine_id.as_str())
    || store.machine_id() != row.machine_id
    || time(&state["started_at"])? != row.start_ms
  {
    return Ok(None);
  }
  Ok(row.end_ms.filter(|end| *end >= row.start_ms))
}

/// Recover a finish already recorded by the supervisor before a crash interrupted
/// terminal metadata publication. Inspecting an unavailable store creates nothing.
pub fn known_end_ms(run_dir: &Path, state: &Value) -> Option<i64> {
  let lookup = (|| -> Result<Option<i64>> {
    let paths = StoragePaths::resolve(None)?;
    paths.validate_existing()?;
    let Some(store) = LocalStore::open_readonly(&paths.profile_database())? else {
      return Ok(None);
    };
    known_end_from_store(&store, run_dir, state)
  })();
  lookup.ok().flatten()
}

pub fn begin_state(run_dir: &Path, state: &mut Value) {
  let begin = (|| -> Result<Option<String>> {
    let Some((_, mut store)) = existing_store()? else {
      return Ok(None);
    };
    if state["machine_id"].as_str() != Some(store.machine_id()) {
      return Ok(None);
    }
    let run_id = state["run_id"]
      .as_str()
      .ok_or_else(|| rejection(400, "missing run identity"))?;
    let start_ms = time(&state["started_at"])?;
    store.begin_run_capture(run_dir, run_id, start_ms).map(Some)
  })();
  if let Ok(Some(capture_id)) = begin {
    state["system_capture_id"] = Value::String(capture_id);
    state["system_capture_status"] = Value::String("capturing".into());
  } else {
    state["system_capture_error"] = Value::String(UNAVAILABLE.into());
  }
}

fn terminal(state: &Value) -> bool {
  matches!(
    state["status"].as_str(),
    Some("completed" | "failed" | "cancelled" | "lost")
  )
}

fn read_state(path: &Path) -> Result<Value> {
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 256 * 1024 {
    return Err(rejection(400, "invalid run capture metadata"));
  }
  Ok(serde_json::from_slice(&bounded_file(path, 256 * 1024)?)?)
}

fn set_status(state: &mut Value, complete: bool) {
  state["system_capture_status"] =
    Value::String(if complete { "complete" } else { "incomplete" }.into());
  if complete {
    if let Some(object) = state.as_object_mut() {
      object.remove("system_capture_error");
    }
  } else {
    state["system_capture_error"] = Value::String("System capture could not confirm every observation through the run end; recorded local readings were retained.".into());
  }
}

fn pending_error(state: &mut Value) {
  state["system_capture_error"] = Value::String(
    "System capture is awaiting local recovery; recorded readings remain in durable storage."
      .into(),
  );
}

fn publish_capture_metadata(connection: &Connection, row: &Capture) -> Result<()> {
  let _lease = match lock::try_lock_file(&row.run_dir.join(".run.lock"), false)? {
    LockAttempt::Acquired(lease) => lease,
    _ => return Ok(()),
  };
  let path = row.run_dir.join("run-state.json");
  let mut state = read_state(&path)?;
  if state["system_capture_id"] != row.capture_id
    || state["run_id"] != row.run_id
    || !terminal(&state)
  {
    return Ok(());
  }
  if state["system_capture_status"] == "capturing" {
    set_status(&mut state, row.complete);
    let mut temporary = tempfile::NamedTempFile::new_in(&row.run_dir)?;
    serde_json::to_writer_pretty(&mut temporary, &state)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    File::open(&row.run_dir)?.sync_all()?;
  }
  sql(connection.execute(
    "UPDATE system_run_captures SET metadata_saved=1 WHERE capture_id=?1",
    [&row.capture_id],
  ))?;
  Ok(())
}

/// Normal task completion never waits for the next collection interval. The
/// collector or independently running publisher finishes the recorded close.
pub fn close_state(run_dir: &Path, state: &mut Value) {
  let Some(id) = state["system_capture_id"].as_str().map(str::to_owned) else {
    return;
  };
  if state["system_capture_status"] != "capturing" {
    return;
  }
  let result = (|| -> Result<Option<bool>> {
    let Some((paths, mut store)) = existing_store()? else {
      return Err(rejection(404, "local run capture storage is unavailable"));
    };
    let row = capture(&store.connection, &id)?;
    if fs::canonicalize(run_dir)? != row.run_dir
      || state["machine_id"].as_str() != Some(row.machine_id.as_str())
      || state["run_id"] != row.run_id
    {
      return Err(rejection(409, "run capture belongs to another run"));
    }
    if row.closed {
      save_checkpoint(&row)?;
      return Ok(Some(row.complete));
    }
    store.close_run_capture(&id, time(&state["finished_at"])?)?;
    while store.export_run_capture(&id)? {}
    let pending: u64 = sql(store.connection.query_row(
      "SELECT COUNT(*) FROM system_run_capture_samples WHERE capture_id=?1",
      [&id],
      |row| row.get(0),
    ))?;
    if pending != 0 {
      return Ok(None);
    }
    if capture(&store.connection, &id)?.acknowledged {
      store.finish_capture(&id, true, Vec::new())?;
      return Ok(Some(capture(&store.connection, &id)?.complete));
    }
    match lock::try_lock_file_readonly(&paths.root().join("collector.lock"))? {
      LockAttempt::Acquired(_proof) => {
        store.finish_capture(&id, true, Vec::new())?;
        Ok(Some(capture(&store.connection, &id)?.complete))
      }
      LockAttempt::Missing => {
        store.finish_capture(&id, true, Vec::new())?;
        Ok(Some(capture(&store.connection, &id)?.complete))
      }
      LockAttempt::Busy => Ok(None),
    }
  })();
  match result {
    Ok(Some(complete)) => set_status(state, complete),
    Err(_) => pending_error(state),
    Ok(None) => {}
  }
}

fn time(value: &Value) -> Result<i64> {
  chrono::DateTime::parse_from_rfc3339(
    value
      .as_str()
      .ok_or_else(|| rejection(400, "missing run timestamp"))?,
  )
  .map(|value| value.timestamp_millis())
  .map_err(|_| rejection(400, "invalid run timestamp"))
}

/// Best-effort finalization used by supervisors and abandoned-run recovery.
/// It never rewrites task status/exit code. Storage errors retain the capture
/// for later recovery rather than sealing a stream with stranded observations.
pub fn finalize_state(run_dir: &Path, state: &mut Value) {
  let Some(capture_id) = state["system_capture_id"].as_str().map(str::to_owned) else {
    return;
  };
  if state["system_capture_status"] != "capturing" {
    return;
  }
  let outcome = (|| -> Result<bool> {
    let Some((paths, mut store)) = existing_store()? else {
      return Err(rejection(404, "local run capture storage is unavailable"));
    };
    let row = capture(&store.connection, &capture_id)?;
    if fs::canonicalize(run_dir)? != row.run_dir
      || state["machine_id"].as_str() != Some(row.machine_id.as_str())
    {
      return Err(rejection(409, "run capture belongs to another run"));
    }
    if row.closed {
      save_checkpoint(&row)?;
      return Ok(row.complete);
    }
    store.close_run_capture(&capture_id, time(&state["finished_at"])?)?;
    let started = Instant::now();
    let mut warnings = Vec::new();
    let complete = loop {
      while store.export_run_capture(&capture_id)? {}
      if capture(&store.connection, &capture_id)?.acknowledged {
        break true;
      }
      match lock::try_lock_file_readonly(&paths.root().join("collector.lock"))? {
        LockAttempt::Acquired(_proof) => {
          store.finish_capture(&capture_id, true, Vec::new())?;
          return Ok(capture(&store.connection, &capture_id)?.complete);
        }
        LockAttempt::Missing => {
          break true;
        }
        LockAttempt::Busy => {}
      }
      let supported: bool = sql(store.connection.query_row("SELECT EXISTS(SELECT 1 FROM system_run_capture_support p JOIN system_local l ON l.singleton=p.singleton WHERE p.sequence=l.sequence)", [], |row| row.get(0)))?;
      if (!supported && started.elapsed() >= UNSUPPORTED_WAIT) || started.elapsed() >= FINAL_WAIT {
        warnings.push("The collector did not acknowledge the run end; retained observations may omit an in-flight reading. Restart an upgraded collector for complete captures.".into());
        break false;
      }
      thread::sleep(Duration::from_millis(100));
    };
    store.finish_capture(&capture_id, complete, warnings)?;
    Ok(capture(&store.connection, &capture_id)?.complete)
  })();
  match outcome {
    Ok(complete) => set_status(state, complete),
    Err(_) => pending_error(state),
  }
}

/// Only this checkpointed prefix may be uploaded. Fsynced but uncommitted tail
/// bytes remain private until the exporter commits its offset and checkpoint.
fn terminal_capture(state: &Value) -> bool {
  terminal(state)
    && matches!(
      state["system_capture_status"].as_str(),
      Some("complete" | "incomplete")
    )
}

fn matching_end(file: &File, extent: u64, state: &Value) -> Result<()> {
  let mut tail = file.try_clone()?;
  let start = extent.saturating_sub(64 * 1024);
  tail.seek(SeekFrom::Start(start))?;
  let mut bytes = Vec::new();
  tail.take(extent - start).read_to_end(&mut bytes)?;
  if bytes.last() != Some(&b'\n') {
    return Err(rejection(409, "run system prefix has no complete end"));
  }
  bytes.pop();
  let start = bytes
    .iter()
    .rposition(|byte| *byte == b'\n')
    .map_or(0, |index| index + 1);
  let end: RunSystemRecord = serde_json::from_slice(&bytes[start..])?;
  end.validate()?;
  if !matches!(end, RunSystemRecord::End { end_ms, complete, .. } if time(&state["finished_at"])? == end_ms && complete == (state["system_capture_status"] == "complete"))
  {
    return Err(rejection(
      409,
      "run system end does not match terminal metadata",
    ));
  }
  Ok(())
}

/// Return the durably published prefix without requiring output write access.
pub fn committed_prefix(run_dir: &Path, state: &Value) -> Result<Option<u64>> {
  let Some(capture_id) = state["system_capture_id"].as_str() else {
    return Ok(None);
  };
  let path = run_dir.join(CHECKPOINT);
  let stream_path = run_dir.join(RUN_SYSTEM_PATH);
  directory(run_dir)?;
  directory(&run_dir.join("outputs"))?;
  let stream_metadata = fs::symlink_metadata(&stream_path)?;
  if !stream_metadata.is_file() || stream_metadata.file_type().is_symlink() {
    return Err(rejection(400, "run system stream must be a regular file"));
  }
  let file = File::open(stream_path)?;
  let matches_header = matches!(header(&file)?, RunSystemRecord::Capture { capture_id: actual, machine_id, start_ms, .. } if actual == capture_id && state["machine_id"].as_str() == Some(machine_id.as_str()) && time(&state["started_at"])? == start_ms);
  if !matches_header {
    return Err(rejection(409, "run system capture header changed"));
  }
  let metadata = match fs::symlink_metadata(&path) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound && terminal_capture(state) => {
      let size = file.metadata()?.len();
      matching_end(&file, size, state)?;
      return Ok(Some(size));
    }
    Err(error) => return Err(error.into()),
  };
  if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16 * 1024 {
    return Err(rejection(400, "invalid run system checkpoint"));
  }
  let checkpoint: Checkpoint = serde_json::from_slice(&bounded_file(&path, 16 * 1024)?)?;
  if checkpoint.capture_id != capture_id
    || identity(&file)? != checkpoint.file_identity
    || checkpoint.committed_bytes > file.metadata()?.len()
  {
    return Err(rejection(
      409,
      "run system checkpoint identity or extent changed",
    ));
  }
  if terminal_capture(state) {
    if !checkpoint.closed {
      return Err(rejection(
        409,
        "terminal run system checkpoint is not closed",
      ));
    }
    matching_end(&file, checkpoint.committed_bytes, state)?;
  }
  Ok(Some(checkpoint.committed_bytes))
}

#[cfg(test)]
mod tests;
