"""Owned-run status, cooperative cancellation, and binary logs for Python targets."""

import fcntl as _jobs_fcntl
import json as _jobs_json
import os as _jobs_os
import pathlib as _jobs_pathlib
import re as _jobs_re
import stat as _jobs_stat
import sys as _jobs_sys
import time as _jobs_time


_JOBS_ACTIVE = {"preparing", "running"}
_JOBS_TERMINAL = {"completed", "failed", "cancelled", "lost"}
_JOBS_STATE_LIMIT = 256 * 1024


def _jobs_directory(path):
  if not _jobs_stat.S_ISDIR(path.lstat().st_mode):
    raise ValueError("run directory must be a real directory: " + str(path))


def _jobs_open(path, flags, missing=False):
  try:
    metadata = path.lstat()
  except FileNotFoundError:
    if not flags & _jobs_os.O_CREAT:
      if missing:
        return None
      raise
  else:
    if not _jobs_stat.S_ISREG(metadata.st_mode):
      raise ValueError("run file must be a regular file: " + str(path))
  try:
    descriptor = _jobs_os.open(path, flags | getattr(_jobs_os, "O_NOFOLLOW", 0) |
                               getattr(_jobs_os, "O_CLOEXEC", 0) | _jobs_os.O_NONBLOCK, 0o600)
  except FileNotFoundError:
    if missing:
      return None
    raise
  try:
    metadata = _jobs_os.fstat(descriptor)
    if not _jobs_stat.S_ISREG(metadata.st_mode):
      raise ValueError("run file must be a regular file: " + str(path))
    return descriptor
  except BaseException:
    _jobs_os.close(descriptor)
    raise


def _jobs_run_dir(repo_root, run_id):
  if not isinstance(run_id, str) or _jobs_re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", run_id) is None:
    raise ValueError("invalid run ID: " + str(run_id))
  root = _jobs_pathlib.Path(repo_root).resolve(strict=True)
  run_dir = root / ".expri" / "runs" / run_id
  for path in (root / ".expri", root / ".expri/runs", run_dir):
    _jobs_directory(path)
  return run_dir


def _jobs_state(run_dir):
  descriptor = _jobs_open(run_dir / "run-state.json", _jobs_os.O_RDONLY)
  with _jobs_os.fdopen(descriptor, "rb") as handle:
    raw = handle.read(_JOBS_STATE_LIMIT + 1)
  if len(raw) > _JOBS_STATE_LIMIT:
    raise ValueError("run state exceeds metadata limit")
  state = _jobs_json.loads(raw)
  if not isinstance(state, dict) or state.get("run_id") != run_dir.name:
    raise ValueError("run state does not match its directory")
  schema = state.get("schema_version", 0)
  if type(schema) is not int or schema not in (0, 1):
    raise ValueError("unsupported run schema version")
  if state.get("detached", False) is not False and state.get("detached") is not True:
    raise ValueError("run detached flag must be boolean")
  if state.get("status") not in _JOBS_ACTIVE | _JOBS_TERMINAL:
    raise ValueError("invalid run status: " + str(state.get("status")))
  if state.get("detached") is True:
    for field, child in (("code_dir", "code"), ("output_dir", "outputs")):
      if state.get(field) != str(run_dir / child):
        raise ValueError("run state does not match its " + field)
      _jobs_directory(run_dir / child)
  return state


def _jobs_alive(run_dir):
  descriptor = _jobs_open(run_dir / ".run.lock", _jobs_os.O_RDWR, missing=True)
  if descriptor is None:
    return False
  try:
    try:
      _jobs_fcntl.flock(descriptor, _jobs_fcntl.LOCK_EX | _jobs_fcntl.LOCK_NB)
    except BlockingIOError:
      return True
    _jobs_fcntl.flock(descriptor, _jobs_fcntl.LOCK_UN)
    return False
  finally:
    _jobs_os.close(descriptor)


def _jobs_cancel_requested(run_dir):
  try:
    metadata = (run_dir / ".cancel-request").lstat()
  except FileNotFoundError:
    return False
  if not _jobs_stat.S_ISREG(metadata.st_mode) or metadata.st_size != 0:
    raise ValueError("cancel request must be an empty regular file")
  return True


def _jobs_status(run_dir):
  state = _jobs_state(run_dir)
  alive = _jobs_alive(run_dir)
  if not alive:
    # A worker can publish terminal state and release its lease during the probe.
    state = _jobs_state(run_dir)
  recorded = state["status"]
  detached = state.get("detached", False)
  alive = alive and recorded in _JOBS_ACTIVE
  status = "lost" if detached and recorded in _JOBS_ACTIVE and not alive else recorded
  return {
    "run_id": run_dir.name, "status": status, "recorded_status": recorded,
    "detached": detached, "alive": alive,
    "cancel_requested": _jobs_cancel_requested(run_dir), "state": state,
  }


def _jobs_cancel(run_dir):
  report = _jobs_status(run_dir)
  if report["status"] in {"completed", "failed", "cancelled"}:
    report["already_finished"] = True
    return report
  if not report["detached"]:
    raise ValueError("cancellation requires a detached run")
  if report["status"] == "lost" or not report["alive"]:
    raise ValueError("run worker is lost; cannot request cancellation")
  try:
    descriptor = _jobs_open(run_dir / ".cancel-request", _jobs_os.O_WRONLY | _jobs_os.O_CREAT | _jobs_os.O_EXCL)
  except FileExistsError:
    _jobs_cancel_requested(run_dir)
  else:
    _jobs_os.close(descriptor)
  return _jobs_status(run_dir)


def _jobs_tail_offset(handle, tail):
  handle.seek(0, 2)
  end = handle.tell()
  if tail == 0 or end == 0:
    return end
  handle.seek(end - 1)
  needed = tail + (handle.read(1) == b"\n")
  cursor = end
  while cursor:
    start = max(0, cursor - 65536)
    handle.seek(start)
    block = handle.read(cursor - start)
    index = len(block)
    while needed:
      index = block.rfind(b"\n", 0, index)
      if index < 0:
        break
      needed -= 1
      if needed == 0:
        return start + index + 1
    cursor = start
  return 0


def _jobs_write(output, raw):
  pending = memoryview(raw)
  while pending:
    written = output.write(pending)
    if written is None or written <= 0:
      raise OSError("log output did not accept bytes")
    pending = pending[written:]
  output.flush()


def _jobs_log_handle(run_dir, stream):
  logs_dir = run_dir / "logs"
  try:
    _jobs_directory(logs_dir)
  except FileNotFoundError:
    return None
  descriptor = _jobs_open(logs_dir / (stream + ".log"), _jobs_os.O_RDONLY, missing=True)
  return _jobs_os.fdopen(descriptor, "rb") if descriptor is not None else None


def _jobs_logs(run_dir, request, output):
  stream = request.get("stream", "stdout")
  if stream not in ("stdout", "stderr"):
    raise ValueError("log stream must be stdout or stderr")
  tail = request.get("tail", 100)
  follow = request.get("follow", False)
  if type(tail) is not int or tail < 0:
    raise ValueError("tail must be a nonnegative integer")
  if type(follow) is not bool:
    raise ValueError("follow must be boolean")
  handle = None
  first_poll = True
  try:
    while True:
      report = _jobs_status(run_dir)
      if handle is None:
        handle = _jobs_log_handle(run_dir, stream)
        if handle is not None and first_poll:
          handle.seek(_jobs_tail_offset(handle, tail))
      first_poll = False
      if handle is not None:
        position = handle.tell()
        handle.seek(0, 2)
        remaining = max(0, handle.tell() - position)
        handle.seek(position)
        if follow and report["status"] in _JOBS_ACTIVE:
          remaining = min(remaining, 1024 * 1024)
        while remaining:
          raw = handle.read(min(65536, remaining))
          if not raw:
            break
          _jobs_write(output, raw)
          remaining -= len(raw)
      if not follow or report["status"] not in _JOBS_ACTIVE:
        # Terminal publication follows closed log files, so the last read drains them.
        return None
      _jobs_time.sleep(0.1)
  finally:
    if handle is not None:
      handle.close()


def execute_job(repo_root, request, output=None):
  """Return status/cancel JSON, or stream logs as raw bytes and return None."""
  if not isinstance(request, dict):
    raise ValueError("job request must be an object")
  operation = request.get("operation")
  if operation not in ("status", "logs", "cancel"):
    raise ValueError("unsupported job operation: " + str(operation))
  run_dir = _jobs_run_dir(repo_root, request.get("run_id"))
  if operation == "status":
    return _jobs_status(run_dir)
  if operation == "cancel":
    return _jobs_cancel(run_dir)
  return _jobs_logs(run_dir, request, _jobs_sys.stdout.buffer if output is None else output)
