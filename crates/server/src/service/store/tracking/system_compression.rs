//! Lossless private system-stream storage. The wire protocol still uses raw byte offsets.
//!
//! Maintenance owns the same exclusive lease as the server. Each independent block
//! is verified and fsynced, its mapping is durably committed, and only then may its
//! original range become a hole. Readers never run concurrently with maintenance.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use expri_core::run_system::RUN_SYSTEM_PATH;
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::raw_io::{TrackingReader, io_error, open_path, open_raw, raw_path};
use super::{ApiError, ApiResult, FileTarget, database, target_json};

const BLOCK_SIZE: u64 = 1024 * 1024;
const MAX_PACKED_SIZE: u64 = BLOCK_SIZE + 64 * 1024;

#[derive(Clone)]
struct Block {
  offset: u64,
  size: u64,
  packed_offset: u64,
  packed_size: u64,
  sha256: String,
}

pub(super) fn initialize(connection: &Connection) -> crate::error::Result<()> {
  connection
    .execute_batch(
      "CREATE TABLE IF NOT EXISTS tracking_system_blocks (
    target TEXT NOT NULL, raw_offset INTEGER NOT NULL, raw_size INTEGER NOT NULL,
    packed_offset INTEGER NOT NULL, packed_size INTEGER NOT NULL, sha256 TEXT NOT NULL,
    PRIMARY KEY(target,raw_offset));
    CREATE TABLE IF NOT EXISTS tracking_system_packs (
      target TEXT PRIMARY KEY, raw_size INTEGER NOT NULL, packed_size INTEGER NOT NULL);",
    )
    .map_err(|_| {
      crate::error::ExpriError::Message("cannot initialize system capture compression".into())
    })
}

fn is_system(target: &FileTarget) -> bool {
  matches!(target, FileTarget::Run { path, .. } if path == RUN_SYSTEM_PATH)
}

fn pack_path(directory: &Path, target: &FileTarget) -> ApiResult<PathBuf> {
  Ok(raw_path(directory, target, 1)?.with_file_name("system-profile.jsonl.pack"))
}

fn blocks(connection: &Connection, target: &FileTarget) -> ApiResult<Vec<Block>> {
  let mut statement = connection
    .prepare(
      "SELECT raw_offset,raw_size,packed_offset,packed_size,sha256
    FROM tracking_system_blocks WHERE target=?1 ORDER BY raw_offset",
    )
    .map_err(database)?;
  let rows = statement
    .query_map([target_json(target)?], |row| {
      Ok(Block {
        offset: row.get(0)?,
        size: row.get(1)?,
        packed_offset: row.get(2)?,
        packed_size: row.get(3)?,
        sha256: row.get(4)?,
      })
    })
    .map_err(database)?;
  let mut result = Vec::new();
  let mut raw_end = 0_u64;
  let mut packed_end = 0_u64;
  for row in rows {
    let block = row.map_err(database)?;
    if block.offset != raw_end
      || block.packed_offset != packed_end
      || block.size != BLOCK_SIZE
      || block.packed_size == 0
      || block.packed_size > MAX_PACKED_SIZE
      || block.sha256.len() != 64
      || !block.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
      return Err(ApiError::new(500, "invalid system capture block mapping"));
    }
    raw_end = raw_end
      .checked_add(block.size)
      .filter(|value| *value <= i64::MAX as u64)
      .ok_or_else(|| ApiError::new(500, "invalid system capture block extent"))?;
    packed_end = packed_end
      .checked_add(block.packed_size)
      .filter(|value| *value <= i64::MAX as u64)
      .ok_or_else(|| ApiError::new(500, "invalid system capture pack extent"))?;
    result.push(block);
  }
  let frontier = frontier(connection, target)?;
  if frontier.unwrap_or((0, 0)) != (raw_end, packed_end) {
    return Err(ApiError::new(
      500,
      "system capture committed block mapping is incomplete",
    ));
  }
  Ok(result)
}

fn frontier(connection: &Connection, target: &FileTarget) -> ApiResult<Option<(u64, u64)>> {
  connection
    .query_row(
      "SELECT raw_size,packed_size FROM tracking_system_packs WHERE target=?1",
      [target_json(target)?],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)
}

fn reject_unmapped_pack(
  directory: &Path,
  connection: &Connection,
  target: &FileTarget,
) -> ApiResult<()> {
  if frontier(connection, target)?.is_none() {
    let path = pack_path(directory, target)?;
    if std::fs::symlink_metadata(&path).is_ok()
      && open_path(directory, &path, false, false)?
        .metadata()
        .map_err(io_error)?
        .len()
        != 0
    {
      return Err(ApiError::new(
        500,
        "system capture pack has no committed mapping; restore its matching metadata",
      ));
    }
  }
  Ok(())
}

fn unpack(pack: &mut File, block: &Block) -> std::io::Result<Vec<u8>> {
  pack.seek(SeekFrom::Start(block.packed_offset))?;
  let mut compressed = vec![0; block.packed_size as usize];
  pack.read_exact(&mut compressed)?;
  let mut decoder = ZlibDecoder::new(compressed.as_slice());
  let mut bytes = Vec::with_capacity(block.size as usize);
  (&mut decoder)
    .take(block.size + 1)
    .read_to_end(&mut bytes)?;
  if bytes.len() as u64 != block.size
    || decoder.total_in() != block.packed_size
    || digest(&bytes) != block.sha256
  {
    return Err(std::io::Error::new(
      std::io::ErrorKind::InvalidData,
      "system capture block checksum mismatch",
    ));
  }
  Ok(bytes)
}

fn digest(bytes: &[u8]) -> String {
  Sha256::digest(bytes)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

struct SystemReader {
  raw: File,
  pack: File,
  blocks: Vec<Block>,
  position: u64,
  length: u64,
  cached: Option<(u64, Vec<u8>)>,
}

impl Read for SystemReader {
  fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
    if buffer.is_empty() || self.position >= self.length {
      return Ok(0);
    }
    let index = self
      .blocks
      .partition_point(|block| block.offset <= self.position);
    if let Some(block) = index.checked_sub(1).map(|index| &self.blocks[index])
      && self.position < block.offset + block.size
    {
      if self
        .cached
        .as_ref()
        .is_none_or(|(offset, _)| *offset != block.offset)
      {
        self.cached = Some((block.offset, unpack(&mut self.pack, block)?));
      }
      let bytes = &self.cached.as_ref().expect("loaded block").1;
      let start = (self.position - block.offset) as usize;
      let count = buffer.len().min(bytes.len() - start);
      buffer[..count].copy_from_slice(&bytes[start..start + count]);
      self.position += count as u64;
      return Ok(count);
    }
    self.raw.seek(SeekFrom::Start(self.position))?;
    let count = buffer.len().min((self.length - self.position) as usize);
    let count = self.raw.read(&mut buffer[..count])?;
    self.position += count as u64;
    Ok(count)
  }
}

impl Seek for SystemReader {
  fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
    let next = match position {
      SeekFrom::Start(value) => Some(value),
      SeekFrom::Current(value) => self.position.checked_add_signed(value),
      SeekFrom::End(value) => self.length.checked_add_signed(value),
    }
    .ok_or_else(|| {
      std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid tracking seek")
    })?;
    self.position = next;
    Ok(next)
  }
}

pub(super) fn reader(
  directory: &Path,
  connection: &Connection,
  target: &FileTarget,
  revision: u64,
) -> ApiResult<TrackingReader> {
  let raw = open_raw(directory, target, revision, false)?;
  if !is_system(target) {
    return Ok(TrackingReader::new(raw));
  }
  let blocks = blocks(connection, target)?;
  if blocks.is_empty() {
    reject_unmapped_pack(directory, connection, target)?;
    return Ok(TrackingReader::new(raw));
  }
  if revision != 1 {
    return Err(ApiError::new(
      500,
      "invalid compressed system capture revision",
    ));
  }
  let length = raw.metadata().map_err(io_error)?.len();
  let last = blocks.last().expect("nonempty blocks");
  let pack = open_path(directory, &pack_path(directory, target)?, false, false)?;
  if last.offset + last.size > length
    || last.packed_offset + last.packed_size > pack.metadata().map_err(io_error)?.len()
  {
    return Err(ApiError::new(
      500,
      "acknowledged system capture blocks are missing",
    ));
  }
  Ok(TrackingReader::new(SystemReader {
    raw,
    pack,
    blocks,
    position: 0,
    length,
    cached: None,
  }))
}

pub(super) fn append(
  directory: &Path,
  connection: &Connection,
  target: &FileTarget,
  raw: &mut File,
  size: u64,
  offset: u64,
  bytes: &[u8],
) -> ApiResult<u64> {
  let end = offset
    .checked_add(bytes.len() as u64)
    .filter(|end| *end <= i64::MAX as u64)
    .ok_or_else(|| ApiError::new(413, "tracking offset exceeds its limit"))?;
  if offset > size {
    return Err(ApiError::new(
      409,
      "tracking offset or repeated bytes conflict",
    ));
  }
  if raw.metadata().map_err(io_error)?.len() < size {
    return Err(ApiError::new(
      500,
      "acknowledged tracking bytes are missing",
    ));
  }
  raw.set_len(size).map_err(io_error)?;
  let overlap = (size - offset).min(bytes.len() as u64) as usize;
  if overlap != 0 {
    let mut source = reader(directory, connection, target, 1)?;
    source.seek(SeekFrom::Start(offset)).map_err(io_error)?;
    let mut previous = vec![0; overlap];
    source.read_exact(&mut previous).map_err(io_error)?;
    if previous != bytes[..overlap] {
      return Err(ApiError::new(
        409,
        "tracking offset or repeated bytes conflict",
      ));
    }
  }
  if end > size {
    raw.seek(SeekFrom::Start(size)).map_err(io_error)?;
    raw.write_all(&bytes[overlap..]).map_err(io_error)?;
  }
  raw.sync_all().map_err(io_error)?;
  Ok(size.max(end))
}

fn streams(connection: &Connection) -> ApiResult<Vec<(FileTarget, u64)>> {
  let mut statement = connection
    .prepare(
      "SELECT target,size FROM tracking_versions WHERE document=0 AND revision=1
    AND json_extract(target,'$.path')=?1 ORDER BY target",
    )
    .map_err(database)?;
  let rows = statement
    .query_map([RUN_SYSTEM_PATH], |row| {
      Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
    })
    .map_err(database)?;
  rows
    .map(|row| {
      let (key, size) = row.map_err(database)?;
      let target: FileTarget = serde_json::from_str(&key)
        .map_err(|_| ApiError::new(500, "invalid system capture target"))?;
      if !is_system(&target) {
        return Err(ApiError::new(500, "invalid system capture target"));
      }
      Ok((target, size))
    })
    .collect()
}

pub(super) fn recover(directory: &Path, connection: &Connection) -> crate::error::Result<()> {
  (|| -> ApiResult<()> {
    // A mapping without its acknowledged stream is corruption, never ignorable data.
    let orphan: bool = connection
      .query_row(
        "SELECT EXISTS(SELECT 1 FROM
      (SELECT target FROM tracking_system_blocks UNION SELECT target FROM tracking_system_packs) b
      LEFT JOIN tracking_versions v ON v.target=b.target AND v.revision=1
      WHERE v.target IS NULL OR v.document!=0 OR json_extract(b.target,'$.path')!=?1)",
        [RUN_SYSTEM_PATH],
        |row| row.get(0),
      )
      .map_err(database)?;
    if orphan {
      return Err(ApiError::new(500, "orphaned system capture block mapping"));
    }
    for (target, size) in streams(connection)? {
      let blocks = blocks(connection, &target)?;
      reject_unmapped_pack(directory, connection, &target)?;
      if let Some(last) = blocks.last() {
        if last.offset + last.size > size {
          return Err(ApiError::new(
            500,
            "system capture mapping exceeds acknowledged bytes",
          ));
        }
        let pack = open_path(directory, &pack_path(directory, &target)?, false, true)?;
        let length = last.packed_offset + last.packed_size;
        if pack.metadata().map_err(io_error)?.len() < length {
          return Err(ApiError::new(
            500,
            "acknowledged system capture blocks are missing",
          ));
        }
        // Discard only an uncommitted compressed tail; mappings are immutable.
        if pack.metadata().map_err(io_error)?.len() > length {
          pack.set_len(length).map_err(io_error)?;
          pack.sync_all().map_err(io_error)?;
        }
      } else if frontier(connection, &target)? == Some((0, 0)) {
        let path = pack_path(directory, &target)?;
        if std::fs::symlink_metadata(&path).is_ok() {
          let pack = open_path(directory, &path, false, true)?;
          pack.set_len(0).map_err(io_error)?;
          pack.sync_all().map_err(io_error)?;
        }
      }
    }
    Ok(())
  })()
  .map_err(maintenance_error)
}

fn maintenance_error(error: ApiError) -> crate::error::ExpriError {
  crate::error::ExpriError::Message(error.message)
}

/// Preview is a read-only metadata snapshot and may run while serving. Apply
/// requires the server to be stopped, Linux hole punching, and the current schema.
pub fn compact_system_captures(directory: &Path, apply: bool) -> crate::error::Result<Value> {
  let directory = std::fs::canonicalize(directory)?;
  let _lease = if apply {
    Some(
      match crate::lock::try_lock_file(&directory.join(".service.lock"), false)? {
        crate::lock::LockAttempt::Acquired(lease) => lease,
        _ => {
          return Err(crate::error::ExpriError::Message(
            "stop the service before applying system capture compaction".into(),
          ));
        }
      },
    )
  } else {
    None
  };
  let path = directory.join("metadata.sqlite3");
  let metadata = std::fs::symlink_metadata(&path)?;
  if !metadata.is_file() || metadata.file_type().is_symlink() {
    return Err(crate::error::ExpriError::Message(
      "service metadata must be a regular file".into(),
    ));
  }
  let flags = if apply {
    OpenFlags::SQLITE_OPEN_READ_WRITE
  } else {
    OpenFlags::SQLITE_OPEN_READ_ONLY
  };
  let mut connection = Connection::open_with_flags(path, flags)
    .map_err(|_| crate::error::ExpriError::Message("cannot open service metadata".into()))?;
  connection
    .busy_timeout(std::time::Duration::from_secs(5))
    .map_err(|_| crate::error::ExpriError::Message("cannot configure service metadata".into()))?;
  let version: i64 = connection
    .pragma_query_value(None, "user_version", |row| row.get(0))
    .map_err(|_| {
      crate::error::ExpriError::Message("cannot read service metadata version".into())
    })?;
  if version > super::super::METADATA_SCHEMA_VERSION
    || (apply && version != super::super::METADATA_SCHEMA_VERSION)
  {
    return Err(crate::error::ExpriError::Message(
      "start the matching upgraded service before compacting its storage".into(),
    ));
  }
  if apply {
    precheck(&directory)?;
    connection
      .execute_batch("PRAGMA synchronous=FULL;")
      .map_err(|_| {
        crate::error::ExpriError::Message("cannot configure durable compaction".into())
      })?;
    recover(&directory, &connection)?;
    super::raw_io::recover(&directory, &connection)?;
  }
  let before = preview(&directory, &connection, version).map_err(maintenance_error)?;
  let mut processed = 0_u64;
  let mut packed_bytes = 0_u64;
  if apply {
    for (target, size) in streams(&connection).map_err(maintenance_error)? {
      let mut raw = open_path(
        &directory,
        &raw_path(&directory, &target, 1).map_err(maintenance_error)?,
        false,
        true,
      )
      .map_err(maintenance_error)?;
      let existing = blocks(&connection, &target).map_err(maintenance_error)?;
      if !existing.is_empty() {
        let mut pack = open_path(
          &directory,
          &pack_path(&directory, &target).map_err(maintenance_error)?,
          false,
          true,
        )
        .map_err(maintenance_error)?;
        for block in &existing {
          // Re-verify before retrying a punch interrupted after the durable commit.
          unpack(&mut pack, block)?;
          punch(&raw, block.offset, block.size)?;
        }
        raw.sync_all()?;
      }
      let mut offset = existing.last().map_or(0, |block| block.offset + block.size);
      while size - offset >= BLOCK_SIZE {
        let written = compact_block(&directory, &mut connection, &target, &mut raw, offset)
          .map_err(maintenance_error)?;
        punch(&raw, offset, BLOCK_SIZE)?;
        raw.sync_all()?;
        offset += BLOCK_SIZE;
        processed += 1;
        packed_bytes += written;
      }
    }
  }
  let mut result = preview(&directory, &connection, version).map_err(maintenance_error)?;
  result["physical_bytes_reclaimed"] = json!(
    before["allocated_bytes"]
      .as_u64()
      .unwrap_or(0)
      .saturating_sub(result["allocated_bytes"].as_u64().unwrap_or(0))
  );
  result["apply"] = json!(apply);
  result["before"] = before;
  result["processed_blocks"] = json!(processed);
  result["compressed_bytes_written"] = json!(packed_bytes);
  result["block_bytes"] = json!(BLOCK_SIZE);
  result["hole_punch_supported_platform"] = json!(cfg!(target_os = "linux"));
  Ok(result)
}

fn preview(directory: &Path, connection: &Connection, version: i64) -> ApiResult<Value> {
  let transaction = connection.unchecked_transaction().map_err(database)?;
  let (count, size, eligible): (u64, u64, u64) = transaction
    .query_row(
      "SELECT COUNT(*),COALESCE(SUM(size),0),COALESCE(SUM((size / ?2) * ?2),0)
    FROM tracking_versions WHERE document=0 AND revision=1 AND json_extract(target,'$.path')=?1",
      params![RUN_SYSTEM_PATH, BLOCK_SIZE],
      |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .map_err(database)?;
  let has_table: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='tracking_system_blocks')", [], |row| row.get(0)).map_err(database)?;
  let (blocks, covered, packed): (u64, u64, u64) = if has_table {
    transaction.query_row("SELECT COUNT(*),COALESCE(SUM(raw_size),0),COALESCE(SUM(packed_size),0) FROM tracking_system_blocks", [],
      |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).map_err(database)?
  } else {
    (0, 0, 0)
  };
  let mut allocated_raw = 0_u64;
  let mut allocated_pack = 0_u64;
  for (target, _) in streams(&transaction)? {
    let raw = open_raw(directory, &target, 1, false)?;
    allocated_raw += allocated(&raw).map_err(io_error)?;
    let path = pack_path(directory, &target)?;
    if std::fs::symlink_metadata(&path).is_ok() {
      allocated_pack += allocated(&open_path(directory, &path, false, false)?).map_err(io_error)?;
    }
  }
  transaction.commit().map_err(database)?;
  Ok(
    json!({"schema_version":version,"stream_count":count,"logical_bytes":size,
    "compressed_blocks":blocks,"compressed_logical_bytes":covered,"pack_bytes":packed,
    "eligible_bytes":eligible.saturating_sub(covered),"allocated_raw_bytes":allocated_raw,
    "allocated_pack_bytes":allocated_pack,"allocated_bytes":allocated_raw + allocated_pack}),
  )
}

fn compact_block(
  directory: &Path,
  connection: &mut Connection,
  target: &FileTarget,
  raw: &mut File,
  offset: u64,
) -> ApiResult<u64> {
  let current = frontier(connection, target)?;
  if offset != current.map_or(0, |(raw, _)| raw) {
    return Err(ApiError::new(
      500,
      "system capture compaction prefix changed",
    ));
  }
  let packed_offset = current.map_or(0, |(_, packed)| packed);
  raw.seek(SeekFrom::Start(offset)).map_err(io_error)?;
  let mut bytes = vec![0; BLOCK_SIZE as usize];
  raw.read_exact(&mut bytes).map_err(io_error)?;
  let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
  encoder.write_all(&bytes).map_err(io_error)?;
  let compressed = encoder.finish().map_err(io_error)?;
  if compressed.len() as u64 > MAX_PACKED_SIZE {
    return Err(ApiError::new(
      500,
      "system capture compressed block exceeds its bound",
    ));
  }
  let block = Block {
    offset,
    size: BLOCK_SIZE,
    packed_offset,
    packed_size: compressed.len() as u64,
    sha256: digest(&bytes),
  };
  // A durable zero frontier distinguishes an interrupted first write from a
  // missing/restored pre-compaction index. No range can be punched at this point.
  if current.is_none() {
    reject_unmapped_pack(directory, connection, target)?;
    let transaction = connection.transaction().map_err(database)?;
    transaction
      .execute(
        "INSERT INTO tracking_system_packs(target,raw_size,packed_size) VALUES(?1,0,0)",
        [target_json(target)?],
      )
      .map_err(database)?;
    transaction.commit().map_err(database)?;
  }
  let mut pack = open_path(directory, &pack_path(directory, target)?, true, true)?;
  if pack.metadata().map_err(io_error)?.len() < packed_offset {
    return Err(ApiError::new(
      500,
      "acknowledged system capture blocks are missing",
    ));
  }
  pack.set_len(packed_offset).map_err(io_error)?;
  pack
    .seek(SeekFrom::Start(packed_offset))
    .map_err(io_error)?;
  pack.write_all(&compressed).map_err(io_error)?;
  pack.sync_all().map_err(io_error)?;
  if unpack(&mut pack, &block).map_err(io_error)? != bytes {
    return Err(ApiError::new(
      500,
      "system capture compression verification failed",
    ));
  }
  super::checkpoint(directory, "system_pack_synced");
  let transaction = connection.transaction().map_err(database)?;
  transaction.execute("INSERT INTO tracking_system_blocks(target,raw_offset,raw_size,packed_offset,packed_size,sha256)
    VALUES(?1,?2,?3,?4,?5,?6)", params![target_json(target)?, block.offset, block.size, block.packed_offset, block.packed_size, block.sha256]).map_err(database)?;
  transaction
    .execute(
      "INSERT INTO tracking_system_packs(target,raw_size,packed_size) VALUES(?1,?2,?3)
    ON CONFLICT(target) DO UPDATE SET raw_size=excluded.raw_size,packed_size=excluded.packed_size",
      params![
        target_json(target)?,
        block.offset + block.size,
        block.packed_offset + block.packed_size
      ],
    )
    .map_err(database)?;
  transaction.commit().map_err(database)?;
  super::checkpoint(directory, "system_mapping_committed");
  // Caller may punch only after this durable commit succeeds.
  Ok(block.packed_size)
}

fn allocated(file: &File) -> std::io::Result<u64> {
  use std::os::unix::fs::MetadataExt;
  Ok(file.metadata()?.blocks().saturating_mul(512))
}

#[cfg(target_os = "linux")]
fn punch(file: &File, offset: u64, length: u64) -> std::io::Result<()> {
  use std::os::fd::AsRawFd;
  let result = unsafe {
    libc::fallocate(
      file.as_raw_fd(),
      libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
      offset as libc::off_t,
      length as libc::off_t,
    )
  };
  if result == 0 {
    Ok(())
  } else {
    Err(std::io::Error::last_os_error())
  }
}

#[cfg(not(target_os = "linux"))]
fn punch(_: &File, _: u64, _: u64) -> std::io::Result<()> {
  Err(std::io::Error::new(
    std::io::ErrorKind::Unsupported,
    "system capture compaction requires Linux hole punching",
  ))
}

fn precheck(directory: &Path) -> crate::error::Result<()> {
  let mut probe = tempfile::tempfile_in(directory)?;
  probe.write_all(&vec![1; BLOCK_SIZE as usize])?;
  probe.sync_all()?;
  let allocated_before = allocated(&probe)?;
  punch(&probe, 0, BLOCK_SIZE)?;
  probe.sync_all()?;
  if probe.metadata()?.len() != BLOCK_SIZE {
    return Err(crate::error::ExpriError::Message(
      "filesystem cannot preserve system capture logical sizes".into(),
    ));
  }
  if allocated(&probe)? >= allocated_before {
    return Err(crate::error::ExpriError::Message(
      "filesystem did not release punched disk blocks".into(),
    ));
  }
  probe.seek(SeekFrom::Start(0))?;
  let mut bytes = vec![1; BLOCK_SIZE as usize];
  probe.read_exact(&mut bytes)?;
  if bytes.iter().any(|byte| *byte != 0) {
    return Err(crate::error::ExpriError::Message(
      "filesystem did not reclaim system capture block".into(),
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests;
