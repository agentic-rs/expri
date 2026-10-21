"""Local run capture parity for the standard-library worker fallback."""

import datetime as _capture_datetime
import json as _capture_json
import os as _capture_os
from pathlib import Path as _CapturePath
import stat as _capture_stat
import tempfile as _capture_tempfile
import uuid as _capture_uuid

_CAPTURE_SCHEMA = """__EXPRI_SYSTEM_CAPTURE_SCHEMA__"""
if _CAPTURE_SCHEMA.strip() == "__EXPRI_SYSTEM_CAPTURE_SCHEMA__":
  try:
    _CAPTURE_SCHEMA = (_CapturePath(__file__).resolve().parents[2] / "system/src/store/run_capture.sql").read_text()
  except OSError:
    _CAPTURE_SCHEMA = None

_CAPTURE_PATH = "outputs/system-profile.jsonl"
_CAPTURE_CHECKPOINT = ".system-profile-checkpoint.json"
_CAPTURE_UNAVAILABLE = "System capture is unavailable. Use an upgraded local collector and keep outputs/system-profile.jsonl reserved for system readings."


def _capture_database():
  override = _capture_os.environ.get("EXPRI_STORAGE_DIR")
  if override is not None:
    if not override or ".." in _CapturePath(override).parts:
      raise ValueError("invalid system storage path")
    root = _CapturePath(_capture_os.path.abspath(override))
    owned = [root]
  else:
    home = _capture_os.environ.get("HOME")
    if not home or ".." in _CapturePath(home).parts:
      raise ValueError("system storage unavailable")
    boundary = _CapturePath(_capture_os.path.abspath(home)) / ".tokn"
    root = boundary / "expri"
    owned = [boundary, root]
  for directory in owned:
    if not _capture_stat.S_ISDIR(directory.lstat().st_mode):
      raise ValueError("system storage directory is not owned")
  database = root / "profiles.sqlite3"
  if not _capture_stat.S_ISREG(database.lstat().st_mode):
    raise ValueError("system profile database is not regular")
  for suffix in ("-wal", "-shm", "-journal"):
    try:
      metadata = database.with_name(database.name + suffix).lstat()
    except FileNotFoundError:
      continue
    if not _capture_stat.S_ISREG(metadata.st_mode):
      raise ValueError("system database sidecar is not regular")
  return database


def _capture_connection():
  import sqlite3
  if _CAPTURE_SCHEMA is None:
    raise ValueError("capture schema unavailable")
  database = _capture_database()
  connection = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=5)
  try:
    connection.execute("SELECT sequence,inventory_revision FROM system_local WHERE singleton=1")
    connection.execute("SELECT sequence,observed_at_ms,payload FROM system_samples LIMIT 0")
    connection.execute("SELECT revision,payload FROM system_inventories LIMIT 0")
  finally:
    connection.close()
  connection = sqlite3.connect(database.as_uri() + "?mode=rw", uri=True, timeout=5)
  connection.row_factory = sqlite3.Row
  connection.execute("PRAGMA synchronous=FULL")
  connection.executescript(_CAPTURE_SCHEMA)
  if not any(row[1] == "coverage_lost" for row in connection.execute("PRAGMA table_info(system_run_captures)")):
    connection.execute("ALTER TABLE system_run_captures ADD COLUMN coverage_lost INTEGER NOT NULL DEFAULT 0")
    connection.commit()
  return database, connection


def _capture_time(value):
  timestamp = _capture_datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
  if timestamp.utcoffset() is None:
    raise ValueError("run timestamp requires a timezone")
  epoch = _capture_datetime.datetime(1970, 1, 1, tzinfo=_capture_datetime.timezone.utc)
  elapsed = timestamp - epoch
  return elapsed.days * 86400000 + elapsed.seconds * 1000 + elapsed.microseconds // 1000


def _capture_sync_directory(path):
  descriptor = _capture_os.open(path, _capture_os.O_RDONLY | getattr(_capture_os, "O_DIRECTORY", 0))
  try:
    _capture_os.fsync(descriptor)
  finally:
    _capture_os.close(descriptor)


def _capture_identity(descriptor):
  metadata = _capture_os.fstat(descriptor)
  if not _capture_stat.S_ISREG(metadata.st_mode):
    raise ValueError("system capture output is not regular")
  return str(metadata.st_dev) + ":" + str(metadata.st_ino)


def _capture_open(run_dir, create=False, read_only=False):
  run_dir = _CapturePath(run_dir)
  for path in (run_dir, run_dir / "outputs"):
    if not _capture_stat.S_ISDIR(path.lstat().st_mode):
      raise ValueError("capture directory must not be a symlink")
  flags = (_capture_os.O_RDONLY if read_only else _capture_os.O_RDWR) | getattr(_capture_os, "O_NOFOLLOW", 0) | getattr(_capture_os, "O_CLOEXEC", 0) | getattr(_capture_os, "O_NONBLOCK", 0)
  if create:
    flags |= _capture_os.O_CREAT | _capture_os.O_EXCL
  descriptor = _capture_os.open(run_dir / _CAPTURE_PATH, flags, 0o600)
  try:
    _capture_identity(descriptor)
    return descriptor
  except Exception:
    _capture_os.close(descriptor)
    raise


def _capture_line(descriptor, value):
  pending = memoryview((_capture_json.dumps(value, ensure_ascii=False, allow_nan=False, separators=(",", ":")) + "\n").encode("utf-8"))
  while pending:
    written = _capture_os.write(descriptor, pending)
    if written <= 0:
      raise OSError("system capture write did not progress")
    pending = pending[written:]


def _capture_checkpoint(row, create=False):
  run_dir = _CapturePath(row["run_dir"])
  destination = run_dir / _CAPTURE_CHECKPOINT
  value = {"capture_id": row["capture_id"], "file_identity": row["file_identity"], "committed_bytes": row["file_offset"], "closed": bool(row["closed"])}
  raw = _capture_json.dumps(value, separators=(",", ":")).encode("utf-8")
  if create:
    descriptor = _capture_os.open(destination, _capture_os.O_WRONLY | _capture_os.O_CREAT | _capture_os.O_EXCL | getattr(_capture_os, "O_NOFOLLOW", 0), 0o600)
    with _capture_os.fdopen(descriptor, "wb") as handle:
      handle.write(raw)
      handle.flush()
      _capture_os.fsync(handle.fileno())
  else:
    metadata = destination.lstat()
    if not _capture_stat.S_ISREG(metadata.st_mode) or metadata.st_size > 16384:
      raise ValueError("invalid capture checkpoint")
    previous = _capture_json.loads(destination.read_bytes())
    if previous["capture_id"] != row["capture_id"] or previous["file_identity"] != row["file_identity"]:
      raise ValueError("capture checkpoint belongs to another producer")
    temporary = None
    try:
      with _capture_tempfile.NamedTemporaryFile(dir=run_dir, delete=False) as handle:
        temporary = _CapturePath(handle.name)
        handle.write(raw)
        handle.flush()
        _capture_os.fsync(handle.fileno())
      temporary.replace(destination)
    finally:
      if temporary is not None:
        temporary.unlink(missing_ok=True)
  _capture_sync_directory(run_dir)


def _capture_begin(run_dir, state):
  connection = None
  descriptor = None
  try:
    database, connection = _capture_connection()
    machine_id = connection.execute("SELECT machine_id FROM system_local WHERE singleton=1").fetchone()[0]
    if machine_id != state.get("machine_id"):
      state["system_capture_error"] = _CAPTURE_UNAVAILABLE
      return
    import fcntl
    supported, empty = connection.execute("SELECT EXISTS(SELECT 1 FROM system_run_capture_support p WHERE p.singleton=l.singleton AND p.sequence=l.sequence),l.sequence=0 FROM system_local l WHERE l.singleton=1").fetchone()
    live = False
    try:
      lease = database.parent / "collector.lock"
      if not _capture_stat.S_ISREG(lease.lstat().st_mode):
        raise ValueError("invalid collector lease")
      with lease.open("rb") as handle:
        try:
          fcntl.flock(handle, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except BlockingIOError:
          live = True
    except FileNotFoundError:
      pass
    if not supported and (live or not empty):
      state["system_capture_error"] = _CAPTURE_UNAVAILABLE
      return
    run_dir = _CapturePath(run_dir).resolve(strict=True)
    if run_dir.name != state["run_id"]:
      raise ValueError("capture directory identity mismatch")
    start_ms = _capture_time(state["started_at"])
    capture_id = "capture-" + _capture_uuid.uuid4().hex
    descriptor = _capture_open(run_dir, create=True)
    file_identity = _capture_identity(descriptor)
    _capture_line(descriptor, {"kind": "capture", "schema_version": 1, "capture_id": capture_id, "machine_id": machine_id, "start_ms": start_ms})
    _capture_os.fsync(descriptor)
    _capture_sync_directory(run_dir / "outputs")
    file_offset = _capture_os.fstat(descriptor).st_size
    _capture_checkpoint({"capture_id": capture_id, "run_dir": str(run_dir), "file_identity": file_identity, "file_offset": file_offset, "closed": False}, create=True)
    connection.execute("BEGIN IMMEDIATE")
    connection.execute("INSERT INTO system_run_captures(capture_id,machine_id,run_id,run_dir,start_ms,file_offset,file_identity) VALUES(?,?,?,?,?,?,?)", (capture_id, machine_id, state["run_id"], str(run_dir), start_ms, file_offset, file_identity))
    connection.execute("UPDATE system_run_captures SET coverage_lost=1 WHERE capture_id=? AND acknowledged=0 AND NOT EXISTS(SELECT 1 FROM system_run_capture_support p JOIN system_local l ON l.singleton=p.singleton WHERE p.sequence=l.sequence) AND EXISTS(SELECT 1 FROM system_local WHERE sequence>0)", (capture_id,))
    connection.execute("INSERT OR IGNORE INTO system_run_capture_samples(capture_id,sequence,observed_at_ms,inventory,sample) SELECT c.capture_id,s.sequence,s.observed_at_ms,i.payload,s.payload FROM system_run_captures c JOIN system_samples s ON s.machine_id=c.machine_id JOIN system_inventories i ON i.machine_id=s.machine_id AND i.revision=s.inventory_revision WHERE c.capture_id=? AND s.observed_at_ms>=c.start_ms AND s.sequence>c.exported_sequence AND NOT EXISTS(SELECT 1 FROM system_local_quarantine q WHERE q.machine_id=s.machine_id AND q.kind='sample' AND q.record_key=s.sequence) ORDER BY s.sequence", (capture_id,))
    connection.commit()
    state.update(system_capture_id=capture_id, system_capture_status="capturing")
    state.pop("system_capture_error", None)
  except Exception:
    # A missing optional collector or reserved user file cannot fail training.
    if connection is not None:
      connection.rollback()
    state["system_capture_error"] = _CAPTURE_UNAVAILABLE
  finally:
    if descriptor is not None:
      _capture_os.close(descriptor)
    if connection is not None:
      connection.close()


def _capture_validate(descriptor, row):
  if _capture_identity(descriptor) != row["file_identity"] or _capture_os.fstat(descriptor).st_size < row["file_offset"]:
    raise ValueError("capture identity or committed prefix changed")
  raw = _capture_os.pread(descriptor, 1024, 0).split(b"\n", 1)[0]
  header = _capture_json.loads(raw)
  if header != {"kind": "capture", "schema_version": 1, "capture_id": row["capture_id"], "machine_id": row["machine_id"], "start_ms": row["start_ms"]}:
    raise ValueError("capture header changed")


def _capture_seed_minutes(connection, row):
  pending = connection.execute("SELECT * FROM system_run_capture_minute_seed WHERE capture_id=?", (row["capture_id"],)).fetchone()
  if pending is None:
    return True
  if row["closed"]:
    connection.execute("DELETE FROM system_run_capture_minute_seed WHERE capture_id=?", (row["capture_id"],))
    connection.commit()
    return True
  target = pending["file_offset"]
  offset = max(pending["scan_offset"], row.get("segment_offset", 0))
  minutes = {}
  consumed = 0
  descriptor = _capture_open(_CapturePath(row["run_dir"]), read_only=True)
  with _capture_os.fdopen(descriptor, "rb") as source:
    if (_capture_identity(source.fileno()) != pending["file_identity"]
        or pending["file_identity"] != row["file_identity"]
        or target > row["file_offset"] or _capture_os.fstat(source.fileno()).st_size < target
        or offset > target):
      raise ValueError("legacy capture prefix changed during minute sampling migration")
    header = _capture_json.loads(_capture_os.pread(source.fileno(), 1024, row.get("segment_offset", 0)).split(b"\n", 1)[0])
    if header != {"kind": "capture", "schema_version": 1, "capture_id": row["capture_id"], "machine_id": row["machine_id"], "start_ms": row["start_ms"]}:
      raise ValueError("legacy capture sampling scope changed")
    source.seek(offset)
    for _ in range(4096):
      if offset == target or consumed >= 4 * 1024 * 1024:
        break
      line = source.readline(min(800 * 1024 + 1025, target - offset))
      if len(line) > 800 * 1024 + 1024 or not line.endswith(b"\n"):
        raise ValueError("legacy capture prefix has an invalid sampling record")
      record = _capture_json.loads(line)
      if record.get("kind") == "sample":
        sample = record["sample"]
        observed = sample["observed_at_ms"]
        sequence = sample["sequence"]
        if type(observed) is not int or type(sequence) is not int:
          raise ValueError("legacy capture sample identity is invalid")
        minute = observed // 60000
        minutes[minute] = min(minutes.get(minute, sequence), sequence)
      offset += len(line)
      consumed += len(line)
  connection.execute("BEGIN IMMEDIATE")
  try:
    for minute, sequence in minutes.items():
      connection.execute("INSERT INTO system_run_capture_minutes(capture_id,minute,sequence) VALUES(?,?,?) ON CONFLICT(capture_id,minute) DO UPDATE SET sequence=MIN(sequence,excluded.sequence)", (row["capture_id"], minute, sequence))
    if offset == target:
      connection.execute("DELETE FROM system_run_capture_samples WHERE capture_id=? AND sequence!=(SELECT m.sequence FROM system_run_capture_minutes m WHERE m.capture_id=system_run_capture_samples.capture_id AND m.minute=system_run_capture_samples.observed_at_ms/60000-CASE WHEN system_run_capture_samples.observed_at_ms%60000<0 THEN 1 ELSE 0 END)", (row["capture_id"],))
      connection.execute("DELETE FROM system_run_capture_minute_seed WHERE capture_id=?", (row["capture_id"],))
    else:
      connection.execute("UPDATE system_run_capture_minute_seed SET scan_offset=? WHERE capture_id=?", (offset, row["capture_id"]))
    connection.commit()
  except Exception:
    connection.rollback()
    raise
  return offset == target


def _capture_export(connection, capture_id, end=None):
  import fcntl
  row = connection.execute("SELECT * FROM system_run_captures WHERE capture_id=?", (capture_id,)).fetchone()
  lease_path = _CapturePath(row["run_dir"]) / ".system-profile.lock"
  lease_descriptor = _capture_os.open(lease_path, _capture_os.O_RDWR | _capture_os.O_CREAT | getattr(_capture_os, "O_NOFOLLOW", 0), 0o600)
  descriptor = None
  try:
    fcntl.flock(lease_descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    row = dict(connection.execute("SELECT * FROM system_run_captures WHERE capture_id=?", (capture_id,)).fetchone())
    if not _capture_seed_minutes(connection, row):
      return row, None
    connection.execute("BEGIN")
    row = dict(connection.execute("SELECT * FROM system_run_captures WHERE capture_id=?", (capture_id,)).fetchone())
    records = []
    bytes_read = 0
    cursor = connection.execute("SELECT sequence,inventory,sample FROM system_run_capture_samples WHERE capture_id=? ORDER BY sequence LIMIT 64", (capture_id,))
    for sample_row in cursor:
      size = len(sample_row["inventory"].encode("utf-8")) + len(sample_row["sample"].encode("utf-8"))
      if records and bytes_read + size > 900 * 1024:
        break
      bytes_read += size
      records.append(dict(sample_row))
    cursor.close()
    connection.commit()
    previous = dict(row)
    descriptor = _capture_open(row["run_dir"])
    _capture_validate(descriptor, row)
    _capture_os.ftruncate(descriptor, row["file_offset"])
    _capture_os.lseek(descriptor, row["file_offset"], _capture_os.SEEK_SET)
    last_sequence = None
    for sample_row in records:
      inventory = _capture_json.loads(sample_row["inventory"])
      sample = _capture_json.loads(sample_row["sample"])
      if inventory["revision"] != row["inventory_revision"]:
        _capture_line(descriptor, {"kind": "inventory", "inventory": inventory})
        row["inventory_revision"] = inventory["revision"]
      _capture_line(descriptor, {"kind": "sample", "sample": sample})
      last_sequence = sample_row["sequence"]
    _capture_os.fsync(descriptor)
    row["file_offset"] = _capture_os.fstat(descriptor).st_size
    connection.execute("BEGIN IMMEDIATE")
    current = connection.execute("SELECT * FROM system_run_captures WHERE capture_id=?", (capture_id,)).fetchone()
    if any(current[key] != previous[key] for key in ("file_offset", "file_identity", "end_ms", "closed")):
      raise ValueError("capture scope changed during export; retry its durable outbox")
    if last_sequence is not None:
      connection.execute("DELETE FROM system_run_capture_samples WHERE capture_id=? AND sequence<=?", (capture_id, last_sequence))
      connection.execute("UPDATE system_run_captures SET exported_sequence=MAX(exported_sequence,?) WHERE capture_id=?", (last_sequence, capture_id))
    connection.execute("UPDATE system_run_captures SET file_offset=?,inventory_revision=? WHERE capture_id=?", (row["file_offset"], row["inventory_revision"], capture_id))
    connection.commit()
    _capture_checkpoint(row)
    pending = connection.execute("SELECT COUNT(*) FROM system_run_capture_samples WHERE capture_id=?", (capture_id,)).fetchone()[0]
    if end is not None and pending == 0 and not row["closed"]:
      row = dict(connection.execute("SELECT * FROM system_run_captures WHERE capture_id=?", (capture_id,)).fetchone())
      previous = dict(row)
      end = bool(end) and not bool(row["coverage_lost"])
      _capture_line(descriptor, {"kind": "end", "end_ms": row["end_ms"], "complete": bool(end), "warnings": [] if end else ["The collector did not acknowledge the run end; retained observations may be incomplete."]})
      _capture_os.fsync(descriptor)
      connection.execute("BEGIN IMMEDIATE")
      current = connection.execute("SELECT * FROM system_run_captures WHERE capture_id=?", (capture_id,)).fetchone()
      pending = connection.execute("SELECT COUNT(*) FROM system_run_capture_samples WHERE capture_id=?", (capture_id,)).fetchone()[0]
      if pending or any(current[key] != previous[key] for key in ("file_offset", "file_identity", "end_ms", "closed", "coverage_lost")):
        raise ValueError("capture changed while recording its end; retry finalization")
      row.update(closed=1, complete=bool(end), file_offset=_capture_os.fstat(descriptor).st_size)
      connection.execute("UPDATE system_run_captures SET closed=1,complete=? WHERE capture_id=?", (bool(end), capture_id))
      connection.execute("UPDATE system_run_captures SET file_offset=? WHERE capture_id=?", (row["file_offset"], capture_id))
      connection.commit()
      _capture_checkpoint(row)
    return row, pending
  except Exception:
    connection.rollback()
    raise
  finally:
    if descriptor is not None:
      _capture_os.close(descriptor)
    _capture_os.close(lease_descriptor)


def _capture_close(run_dir, state):
  if state.get("system_capture_status") != "capturing":
    return
  connection = None
  proof = None
  try:
    import fcntl
    database, connection = _capture_connection()
    capture_id = state["system_capture_id"]
    row = connection.execute("SELECT * FROM system_run_captures WHERE capture_id=?", (capture_id,)).fetchone()
    if row["run_dir"] != str(_CapturePath(run_dir).resolve(strict=True)):
      raise ValueError("capture belongs to another run")
    end_ms = _capture_time(state["finished_at"])
    if end_ms < row["start_ms"] or (row["end_ms"] is not None and row["end_ms"] != end_ms):
      raise ValueError("capture end changed")
    connection.execute("BEGIN IMMEDIATE")
    connection.execute("UPDATE system_run_captures SET end_ms=? WHERE capture_id=? AND closed=0", (end_ms, capture_id))
    connection.execute("UPDATE system_run_captures SET coverage_lost=1 WHERE capture_id=? AND acknowledged=0 AND NOT EXISTS(SELECT 1 FROM system_run_capture_support p JOIN system_local l ON l.singleton=p.singleton WHERE p.sequence=l.sequence) AND EXISTS(SELECT 1 FROM system_local WHERE sequence>0)", (capture_id,))
    connection.execute("DELETE FROM system_run_capture_minutes WHERE capture_id=? AND sequence IN (SELECT sequence FROM system_run_capture_samples WHERE capture_id=? AND observed_at_ms>=?) AND sequence>(SELECT exported_sequence FROM system_run_captures WHERE capture_id=?)", (capture_id, capture_id, end_ms, capture_id))
    connection.execute("DELETE FROM system_run_capture_samples WHERE capture_id=? AND observed_at_ms>=?", (capture_id, end_ms))
    connection.commit()
    row, pending = _capture_export(connection, capture_id)
    while pending:
      row, pending = _capture_export(connection, capture_id)
    if row["closed"]:
      state["system_capture_status"] = "complete" if row["complete"] else "incomplete"
      if row["complete"]:
        state.pop("system_capture_error", None)
      return
    complete = bool(row["acknowledged"])
    if not complete:
      try:
        lease_path = database.parent / "collector.lock"
        metadata = lease_path.lstat()
        if not _capture_stat.S_ISREG(metadata.st_mode):
          raise ValueError("collector lease is invalid")
        proof = _capture_os.open(lease_path, _capture_os.O_RDONLY | getattr(_capture_os, "O_NOFOLLOW", 0))
        try:
          fcntl.flock(proof, fcntl.LOCK_SH | fcntl.LOCK_NB)
          complete = True
        except BlockingIOError:
          pass
      except FileNotFoundError:
        complete = True
    if complete and pending == 0:
      row, _ = _capture_export(connection, capture_id, end=True)
      state["system_capture_status"] = "complete" if row["complete"] else "incomplete"
      if row["complete"]:
        state.pop("system_capture_error", None)
  except Exception:
    if connection is not None:
      connection.rollback()
    state.update(system_capture_error="System capture is awaiting local recovery; recorded readings remain in durable storage.")
  finally:
    if proof is not None:
      _capture_os.close(proof)
    if connection is not None:
      connection.close()
