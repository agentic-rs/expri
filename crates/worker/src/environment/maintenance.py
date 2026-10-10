"""Standard-library retention for per-run environments on Python-only targets."""

import datetime as _maintenance_datetime
import fcntl as _maintenance_fcntl
import json as _maintenance_json
import os as _maintenance_os
import pathlib as _maintenance_pathlib
import re as _maintenance_re
import shutil as _maintenance_shutil
import stat as _maintenance_stat
import tempfile as _maintenance_tempfile


class _MaintenanceLease:
  """Unlock explicitly even if a concurrent spawn inherited the description."""

  def __init__(self, handle):
    self._handle = handle

  def fileno(self):
    if self._handle is None:
      raise ValueError("lease is closed")
    return self._handle.fileno()

  def close(self):
    handle, self._handle = self._handle, None
    if handle is not None:
      try:
        _maintenance_fcntl.flock(handle, _maintenance_fcntl.LOCK_UN)
      finally:
        handle.close()

  def __enter__(self):
    return self

  def __exit__(self, *_):
    self.close()

  def __del__(self):
    try:
      self.close()
    except Exception:
      pass


def acquire_run_lock(run_dir):
  """Return a lease to keep open through preparation, execution and final state."""
  handle = _maintenance_open_lock(_maintenance_pathlib.Path(run_dir) / ".run.lock", True)
  try:
    _maintenance_fcntl.flock(handle, _maintenance_fcntl.LOCK_EX)
    return _MaintenanceLease(handle)
  except BaseException:
    handle.close()
    raise


def prune_environments(root, apply=False, keep_last=1):
  """Preview by default; remove only owned environments of verifiably finished runs."""
  if type(apply) is not bool or type(keep_last) is not int or keep_last < 0:
    raise ValueError("apply must be boolean and keep_last must be a nonnegative integer")
  root = _maintenance_pathlib.Path(root).resolve(strict=True)
  report = {"apply": apply, "keep_last": keep_last, "runs": [], "logical_bytes": 0, "pruned_runs": 0}
  state_dir = root / ".expri"
  if not _maintenance_directory(state_dir):
    return report
  runs_dir = state_dir / "runs"
  if not _maintenance_directory(runs_dir):
    return report
  candidates = []
  try:
    for run_dir in sorted(runs_dir.iterdir()):
      entry = {"run_id": run_dir.name, "status": None, "action": "skipped", "reason": "", "logical_bytes": 0}
      locks = []
      try:
        status, finished_at = _maintenance_finished(run_dir)
        entry["status"] = status
        if not _maintenance_environment(run_dir):
          entry.update(action="already_pruned", reason="run environment is absent")
          report["runs"].append(entry)
          continue
        for lock_path in (run_dir / ".run.lock", run_dir / "environment/.prepare.lock"):
          lock = _maintenance_available_lock(lock_path, apply)
          if lock is not None:
            locks.append(lock)
        status, finished_at = _maintenance_finished(run_dir)
        entry["status"] = status
        if not _maintenance_environment(run_dir):
          entry.update(action="already_pruned", reason="run environment is absent")
          report["runs"].append(entry)
          continue
        entry["logical_bytes"] = _maintenance_logical_bytes(run_dir / "environment/.venv")
        candidates.append((finished_at, run_dir.name, run_dir, entry, []))
      except (OSError, RuntimeError, ValueError) as error:
        entry["reason"] = str(error)
        report["runs"].append(entry)
      finally:
        for handle in locks:
          handle.close()
    candidates.sort(key=lambda candidate: (candidate[0], candidate[1]), reverse=True)
    for index, (finished_at, _, run_dir, entry, leases) in enumerate(candidates):
      if index < keep_last:
        entry.update(action="kept", reason="retained by keep_last")
      elif not apply:
        entry.update(action="preview", reason="finished run environment would be removed")
        report["logical_bytes"] += entry["logical_bytes"]
      else:
        try:
          # Bound descriptor use to one run; revalidate under both leases.
          for lock_path in (run_dir / ".run.lock", run_dir / "environment/.prepare.lock"):
            lease = _maintenance_available_lock(lock_path, True)
            if lease is not None:
              leases.append(lease)
          status, current_finished_at = _maintenance_finished(run_dir)
          if current_finished_at != finished_at or status != entry["status"]:
            raise RuntimeError("run state changed during cleanup")
          if not _maintenance_environment(run_dir):
            raise RuntimeError("run environment changed during cleanup")
          # Construct the deletion path; metadata never supplies it.
          _maintenance_shutil.rmtree(run_dir / "environment/.venv")
          entry.update(action="pruned", reason="finished run environment removed")
          report["pruned_runs"] += 1
          report["logical_bytes"] += entry["logical_bytes"]
          try:
            _maintenance_audit(run_dir, entry)
          except OSError as error:
            entry["reason"] = "finished run environment removed; could not save prune audit: " + str(error)
        except (OSError, RuntimeError, ValueError) as error:
          entry["reason"] = str(error)
        finally:
          for handle in leases:
            handle.close()
          leases.clear()
      report["runs"].append(entry)
    report["runs"].sort(key=lambda entry: entry["run_id"])
    return report
  finally:
    for _, _, _, _, locks in candidates:
      for handle in locks:
        handle.close()


def _maintenance_finished(run_dir):
  try:
    if not _maintenance_directory(run_dir):
      raise RuntimeError("missing run directory")
  except (OSError, RuntimeError):
    raise RuntimeError("run directory is not a real directory") from None
  try:
    state = _maintenance_read_json(run_dir / "run-state.json")
  except (OSError, RuntimeError, ValueError, UnicodeDecodeError):
    raise RuntimeError("missing or invalid run state") from None
  if not isinstance(state, dict) or state.get("run_id") != run_dir.name or state.get("code_dir") != str(run_dir / "code"):
    raise RuntimeError("run state does not match its directory")
  status = state.get("status")
  if status not in ("completed", "failed", "cancelled"):
    raise RuntimeError("run has not verifiably finished")
  timestamp = state.get("finished_at")
  try:
    match = _maintenance_re.fullmatch(
      r"\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.(?P<fraction>\d+))?(?:[Zz]|[+-]\d{2}:\d{2})", timestamp
    ) if isinstance(timestamp, str) else None
    if match is None:
      raise ValueError("invalid timestamp")
    parsed = _maintenance_datetime.datetime.fromisoformat(timestamp.replace("z", "+00:00").replace("Z", "+00:00"))
    # Rust timestamps carry nanoseconds; datetime alone truncates to microseconds
    # and could retain an older run when two completion times are very close.
    fraction = (match.group("fraction") or "").ljust(9, "0")[:9]
    finished_at = (parsed.replace(microsecond=0), fraction)
  except ValueError:
    raise RuntimeError("missing or invalid run completion time") from None
  try:
    if not _maintenance_directory(run_dir / "code"):
      raise RuntimeError("missing run code")
  except (OSError, RuntimeError):
    raise RuntimeError("run code directory is not a real directory") from None
  return status, finished_at


def _maintenance_environment(run_dir):
  environment_dir = run_dir / "environment"
  if not _maintenance_directory(environment_dir) or not _maintenance_directory(environment_dir / ".venv"):
    return False
  try:
    owner = _maintenance_read_json(environment_dir / "owner.json")
  except (OSError, RuntimeError, ValueError, UnicodeDecodeError):
    raise RuntimeError("missing or invalid environment owner") from None
  if not isinstance(owner, dict) or type(owner.get("schema_version")) is not int or owner.get("schema_version") != 1 or owner.get("repo_root") != str(run_dir / "code"):
    raise RuntimeError("environment owner does not match run code")
  return True


def _maintenance_directory(path):
  try:
    metadata = path.lstat()
  except FileNotFoundError:
    return False
  if not _maintenance_stat.S_ISDIR(metadata.st_mode):
    raise RuntimeError("directory must not be a symlink or file: " + str(path))
  return True


def _maintenance_read_json(path):
  if not _maintenance_stat.S_ISREG(path.lstat().st_mode):
    raise RuntimeError("metadata must be a regular file: " + str(path))
  return _maintenance_json.loads(path.read_text(encoding="utf-8"))


def _maintenance_open_lock(path, create):
  for parent in reversed(path.parents):
    if not _maintenance_directory(parent):
      raise RuntimeError("lock directory is missing: " + str(parent))
  try:
    metadata = path.lstat()
  except FileNotFoundError:
    metadata = None
  if metadata is not None and not _maintenance_stat.S_ISREG(metadata.st_mode):
    raise RuntimeError("lock must be a regular file: " + str(path))
  if metadata is None and not create:
    return None
  flags = _maintenance_os.O_RDWR | getattr(_maintenance_os, "O_NOFOLLOW", 0) | getattr(_maintenance_os, "O_CLOEXEC", 0)
  if metadata is None:
    flags |= _maintenance_os.O_CREAT | _maintenance_os.O_EXCL
  try:
    descriptor = _maintenance_os.open(path, flags, 0o666)
  except FileExistsError:
    return _maintenance_open_lock(path, create)
  handle = _maintenance_os.fdopen(descriptor, "r+b")
  try:
    opened = _maintenance_os.fstat(handle.fileno())
    current = path.lstat()
    if not _maintenance_stat.S_ISREG(opened.st_mode) or not _maintenance_stat.S_ISREG(current.st_mode) or (opened.st_dev, opened.st_ino) != (current.st_dev, current.st_ino):
      raise RuntimeError("lock changed while opening: " + str(path))
    return handle
  except BaseException:
    handle.close()
    raise


def _maintenance_available_lock(path, create):
  handle = _maintenance_open_lock(path, create)
  if handle is None:
    return None
  try:
    _maintenance_fcntl.flock(handle, _maintenance_fcntl.LOCK_EX | _maintenance_fcntl.LOCK_NB)
    return _MaintenanceLease(handle)
  except BlockingIOError:
    handle.close()
    raise RuntimeError("run or environment preparation is active") from None
  except BaseException:
    handle.close()
    raise


def _maintenance_logical_bytes(path):
  metadata = path.lstat()
  if _maintenance_stat.S_ISLNK(metadata.st_mode):
    return 0
  if _maintenance_stat.S_ISREG(metadata.st_mode):
    return metadata.st_size
  if not _maintenance_stat.S_ISDIR(metadata.st_mode):
    raise RuntimeError("environment contains a special file")
  return sum(_maintenance_logical_bytes(child) for child in path.iterdir())


def _maintenance_audit(run_dir, entry):
  audit = {"schema_version": 1, "run_id": entry["run_id"], "status": "pruned",
           "pruned_at": _maintenance_datetime.datetime.now(_maintenance_datetime.timezone.utc).isoformat(),
           "logical_bytes": entry["logical_bytes"]}
  temporary = None
  try:
    with _maintenance_tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=run_dir / "environment", delete=False) as handle:
      temporary = _maintenance_pathlib.Path(handle.name)
      _maintenance_json.dump(audit, handle, indent=2, sort_keys=True)
    temporary.replace(run_dir / "environment/prune-state.json")
  finally:
    if temporary is not None:
      temporary.unlink(missing_ok=True)
