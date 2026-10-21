//! One-time, bounded migration of append-only legacy capture prefixes.

use super::*;
use expri_core::run_system::RUN_SYSTEM_SAMPLE_INTERVAL_MS;

const SEED_BYTES: usize = 4 * 1024 * 1024;
const SEED_RECORDS: usize = 4096;
const LINE_BYTES: usize = MAX_PROFILE_RECORD_BYTES + 1024;

/// Called under the stream lease, outside the collector's write transaction.
/// A false result asks the independent exporter to resume this migration later.
pub(super) fn seed(connection: &Connection, row: &Capture) -> Result<bool> {
  let pending: Option<(u64, String, u64)> = sql(connection.query_row(
    "SELECT file_offset,file_identity,scan_offset FROM system_run_capture_minute_seed WHERE capture_id=?1",
    [&row.capture_id],
    |record| Ok((record.get(0)?, record.get(1)?, record.get(2)?)),
  ).optional())?;
  let Some((target, expected_identity, checkpoint)) = pending else {
    return Ok(true);
  };
  if row.closed {
    sql(connection.execute(
      "DELETE FROM system_run_capture_minute_seed WHERE capture_id=?1",
      [&row.capture_id],
    ))?;
    return Ok(true);
  }
  directory(&row.run_dir)?;
  directory(&row.run_dir.join("outputs"))?;
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let mut file = options.open(row.run_dir.join(RUN_SYSTEM_PATH))?;
  if identity(&file)? != expected_identity
    || expected_identity != row.file_identity
    || target > row.file_offset
    || file.metadata()?.len() < target
  {
    return Err(rejection(
      409,
      "legacy capture prefix changed during minute sampling migration",
    ));
  }
  validate_file(&file, row)?;
  let mut offset = checkpoint.max(row.segment_offset);
  if offset > target {
    return Err(rejection(
      409,
      "legacy capture sampling checkpoint exceeds its prefix",
    ));
  }
  file.seek(SeekFrom::Start(offset))?;
  let mut input = BufReader::new(file.take(target - offset));
  let mut minutes = BTreeMap::<i64, u64>::new();
  let mut consumed = 0;
  for _ in 0..SEED_RECORDS {
    if offset == target || consumed >= SEED_BYTES {
      break;
    }
    let mut line = Vec::new();
    input
      .by_ref()
      .take((LINE_BYTES + 1) as u64)
      .read_until(b'\n', &mut line)?;
    if line.len() > LINE_BYTES || line.last() != Some(&b'\n') {
      return Err(rejection(
        409,
        "legacy capture prefix has an invalid sampling record",
      ));
    }
    let record: RunSystemRecord = serde_json::from_slice(&line)?;
    if let RunSystemRecord::Sample { sample } = record {
      minutes
        .entry(
          sample
            .observed_at_ms
            .div_euclid(RUN_SYSTEM_SAMPLE_INTERVAL_MS),
        )
        .and_modify(|sequence| *sequence = (*sequence).min(sample.sequence))
        .or_insert(sample.sequence);
    }
    offset += line.len() as u64;
    consumed += line.len();
  }
  let transaction = sql(Transaction::new_unchecked(
    connection,
    TransactionBehavior::Immediate,
  ))?;
  for (minute, sequence) in minutes {
    sql(transaction.execute(
      "INSERT INTO system_run_capture_minutes(capture_id,minute,sequence) VALUES(?1,?2,?3) ON CONFLICT(capture_id,minute) DO UPDATE SET sequence=MIN(sequence,excluded.sequence)",
      params![row.capture_id, minute, integer(sequence)?],
    ))?;
  }
  if offset == target {
    // Only the unpublished outbox is compacted. The stream lease excludes an
    // older exporter's already-read batch, and committed byte prefixes stay put.
    sql(transaction.execute(
      "DELETE FROM system_run_capture_samples WHERE capture_id=?1 AND sequence!=(SELECT m.sequence FROM system_run_capture_minutes m WHERE m.capture_id=system_run_capture_samples.capture_id AND m.minute=system_run_capture_samples.observed_at_ms/60000-CASE WHEN system_run_capture_samples.observed_at_ms%60000<0 THEN 1 ELSE 0 END)",
      [&row.capture_id],
    ))?;
    sql(transaction.execute(
      "DELETE FROM system_run_capture_minute_seed WHERE capture_id=?1",
      [&row.capture_id],
    ))?;
  } else {
    sql(transaction.execute(
      "UPDATE system_run_capture_minute_seed SET scan_offset=?2 WHERE capture_id=?1",
      params![row.capture_id, offset],
    ))?;
  }
  sql(transaction.commit())?;
  Ok(offset == target)
}
