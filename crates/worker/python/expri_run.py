# Generated from sdk/python/expri_run.py; edit that source instead.
# Regenerate with python3 scripts/sync_python_sdk.py.
"""Dependency-free task status requests shared by the expri SDK and standalone tasks."""

from datetime import datetime
import errno
import fcntl
import json
import math
import os
from pathlib import Path
import re
import secrets
import stat
import time


MAX_STATE_BYTES = 256 * 1024
_LOCK_TIMEOUT_SECONDS = 5.0
_LOCK_POLL_SECONDS = 0.01
_RUN_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
_TIMESTAMP = re.compile(
  r"\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-](?:[01]\d|2[0-3]):[0-5]\d)",
  re.ASCII,
)


def _same_file(first, second):
  return (first.st_dev, first.st_ino) == (second.st_dev, second.st_ino)


def _open_directory(path):
  # Walk without following symlinks, including symlinked parent components.
  descriptor = os.open(path.anchor, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
  try:
    for name in path.parts[1:]:
      child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                      dir_fd=descriptor)
      os.close(descriptor)
      descriptor = child
    return descriptor
  except BaseException:
    os.close(descriptor)
    raise


def _open_file(directory, name, flags, create=False):
  open_flags = flags | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
  try:
    metadata = os.stat(name, dir_fd=directory, follow_symlinks=False)
  except FileNotFoundError:
    if not create:
      raise
    metadata = None
  if metadata is not None and not stat.S_ISREG(metadata.st_mode):
    raise ValueError(name + " must be a regular file, without a symlink")
  if create:
    try:
      descriptor = os.open(name, open_flags | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=directory)
    except FileExistsError:
      if flags & os.O_EXCL:
        raise
      descriptor = os.open(name, open_flags, dir_fd=directory)
  else:
    descriptor = os.open(name, open_flags, dir_fd=directory)
  try:
    opened = os.fstat(descriptor)
    current = os.stat(name, dir_fd=directory, follow_symlinks=False)
    if not stat.S_ISREG(opened.st_mode) or not _same_file(opened, current):
      raise ValueError(name + " must remain a regular file")
    return descriptor
  except BaseException:
    os.close(descriptor)
    raise


def _lock_state(directory):
  descriptor = _open_file(directory, ".run-state.lock", os.O_RDWR, create=True)
  try:
    deadline = time.monotonic() + _LOCK_TIMEOUT_SECONDS
    while True:
      try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        break
      except BlockingIOError:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
          raise TimeoutError("run state is busy; try again") from None
        time.sleep(min(_LOCK_POLL_SECONDS, remaining))
    if not _same_file(os.fstat(descriptor),
                      os.stat(".run-state.lock", dir_fd=directory, follow_symlinks=False)):
      raise ValueError(".run-state.lock changed while acquiring its lock")
    return descriptor
  except BaseException:
    os.close(descriptor)
    raise


def _json_object(pairs):
  value = {}
  for key, child in pairs:
    if key in value:
      raise ValueError("run-state.json contains duplicate object keys")
    value[key] = child
  return value


def _reject_constant(_value):
  raise ValueError("run-state.json must contain finite JSON values")


def _json_float(value):
  number = float(value)
  if not math.isfinite(number):
    raise ValueError("run-state.json must contain finite JSON values")
  return number


def _validate_json_strings(value):
  if isinstance(value, str):
    value.encode("utf-8")
  elif isinstance(value, dict):
    for key, child in value.items():
      key.encode("utf-8")
      _validate_json_strings(child)
  elif isinstance(value, list):
    for child in value:
      _validate_json_strings(child)


def _read_state(directory):
  descriptor = _open_file(directory, "run-state.json", os.O_RDONLY)
  with os.fdopen(descriptor, "rb") as handle:
    if os.fstat(handle.fileno()).st_size > MAX_STATE_BYTES:
      raise ValueError("run-state.json exceeds the 256 KiB metadata limit")
    raw = handle.read(MAX_STATE_BYTES + 1)
  if len(raw) > MAX_STATE_BYTES:
    raise ValueError("run-state.json exceeds the 256 KiB metadata limit")
  try:
    state = json.loads(raw.decode("utf-8"), object_pairs_hook=_json_object,
                       parse_constant=_reject_constant, parse_float=_json_float)
    _validate_json_strings(state)
    return state
  except (UnicodeError, json.JSONDecodeError, RecursionError) as error:
    raise ValueError("run-state.json contains invalid JSON") from error


def _valid_started_at(value):
  if not isinstance(value, str) or _TIMESTAMP.fullmatch(value) is None:
    return False
  # RFC3339 permits a leap second. datetime only represents seconds 0 through 59.
  normalized = value[:17] + ("59" if value[17:19] == "60" else value[17:19]) + value[19:]
  # Python 3.10 requires three or six fractional digits; only the validation
  # copy is normalized so native nanosecond timestamps remain unchanged.
  normalized = re.sub(r"\.(\d+)", lambda match: "." + match.group(1)[:6].ljust(6, "0"),
                      normalized, count=1)
  try:
    datetime.fromisoformat(normalized.upper().replace("Z", "+00:00"))
  except ValueError:
    return False
  return True


def _validate_state(state, run_dir, directory):
  if not isinstance(state, dict) or state.get("run_id") != run_dir.name:
    raise ValueError("run state does not match its directory")
  if type(state.get("schema_version")) is not int or state["schema_version"] != 1:
    raise ValueError("run state uses an unsupported schema_version")
  if state.get("status") not in ("running", "waiting"):
    raise ValueError("status changes require a running or waiting run")
  if not _valid_started_at(state.get("started_at")):
    raise ValueError("run-state.json has invalid or missing started_at")
  if state.get("finished_at") is not None:
    raise ValueError("status changes require an unfinished run")
  if "detached" in state and type(state["detached"]) is not bool:
    raise ValueError("run state has an invalid detached field")
  for field, child in (("code_dir", "code"), ("output_dir", "outputs")):
    if field not in state and state.get("detached") is not True:
      continue
    if state.get(field) != str(run_dir / child):
      raise ValueError("run state does not match its " + field)
    child_fd = os.open(child, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                       dir_fd=directory)
    os.close(child_fd)


def _require_live_lease(directory):
  try:
    descriptor = _open_file(directory, ".run.lock", os.O_RDWR)
  except FileNotFoundError:
    raise ValueError("run supervisor is not active; status was not changed") from None
  try:
    try:
      fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as error:
      if error.errno not in (errno.EAGAIN, errno.EACCES):
        raise
      return
    fcntl.flock(descriptor, fcntl.LOCK_UN)
    raise ValueError("run supervisor is not active; status was not changed")
  finally:
    os.close(descriptor)


def _write_state(directory, state):
  try:
    raw = (json.dumps(state, ensure_ascii=False, allow_nan=False, indent=2) + "\n").encode("utf-8")
  except (ValueError, OverflowError, RecursionError, UnicodeError) as error:
    raise ValueError("run state must contain supported, finite JSON values") from error
  if len(raw) > MAX_STATE_BYTES:
    raise ValueError("run-state.json exceeds the 256 KiB metadata limit")
  name = ".run-state-" + secrets.token_hex(12) + ".tmp"
  descriptor = _open_file(directory, name, os.O_WRONLY | os.O_EXCL, create=True)
  try:
    with os.fdopen(descriptor, "wb", buffering=0) as handle:
      pending = memoryview(raw)
      while pending:
        written = os.write(handle.fileno(), pending)
        if written <= 0:
          raise OSError("run-state.json did not accept bytes")
        pending = pending[written:]
      os.fsync(handle.fileno())
    # Refuse a substituted destination even though rename would not follow it.
    metadata = os.stat("run-state.json", dir_fd=directory, follow_symlinks=False)
    if not stat.S_ISREG(metadata.st_mode):
      raise ValueError("run-state.json must remain a regular file, without a symlink")
    os.replace(name, "run-state.json", src_dir_fd=directory, dst_dir_fd=directory)
    os.fsync(directory)
  finally:
    try:
      os.unlink(name, dir_fd=directory)
    except FileNotFoundError:
      pass


def _sync_attempt_phase(state, status):
  if "task_attempts" not in state:
    return False
  history = state["task_attempts"]
  if not isinstance(history, dict) or type(history.get("protocol_version")) is not int or history.get("protocol_version") != 1 or history.get("truncated") is not False or not isinstance(history.get("attempts"), list) or type(history.get("total_count")) is not int or history.get("total_count") != len(history["attempts"]):
    raise ValueError("invalid task attempt history")
  current = history.get("current_attempt")
  if isinstance(current, bool) or not isinstance(current, int) or not 1 <= current <= 2 ** 64 - 1:
    raise ValueError("invalid current task attempt")
  ids = set()
  active = None
  for attempt in history["attempts"]:
    number = attempt.get("attempt") if isinstance(attempt, dict) else None
    if isinstance(number, bool) or not isinstance(number, int) or not 1 <= number <= 2 ** 64 - 1 or number in ids:
      raise ValueError("invalid task attempt number")
    ids.add(number)
    if number == current:
      active = attempt
  if active is None or active.get("status") not in ("running", "waiting") or active.get("finished_at") is not None:
    raise ValueError("current task attempt is not active")
  changed = active["status"] != status
  active["status"] = status
  return changed


def set_run_status(status, run_dir=None):
  """Request running or waiting for a live managed run; return change details.

  By default, use EXPRI_RUN_DIR. This is observational: waiting does not suspend
  the task, change its original start time, or stop capture and publication.
  Cooperating task writers and the supervisor use the same short state lock.
  An already requested status is accepted without rewriting run-state.json.
  """
  if not isinstance(status, str) or status not in ("running", "waiting"):
    raise ValueError("status must be running or waiting")
  if run_dir is None:
    run_dir = os.environ.get("EXPRI_RUN_DIR")
    if not run_dir:
      raise ValueError("EXPRI_RUN_DIR is unset; pass run_dir explicitly")
  run_dir = Path(run_dir)
  if ".." in run_dir.parts:
    raise ValueError("run_dir must not contain parent-directory traversal")
  run_dir = run_dir.absolute()
  if len(run_dir.name.encode("utf-8")) > 96 or _RUN_ID.fullmatch(run_dir.name) is None:
    raise ValueError("invalid run directory name")
  directory = _open_directory(run_dir)
  lock = None
  try:
    lock = _lock_state(directory)
    state = _read_state(directory)
    _validate_state(state, run_dir, directory)
    _require_live_lease(directory)
    previous = state["status"]
    attempt_changed = _sync_attempt_phase(state, status)
    changed = previous != status or attempt_changed
    if changed:
      state["status"] = status
      _write_state(directory, state)
    return {"run_id": state["run_id"], "previous_status": previous,
            "status": status, "changed": changed}
  finally:
    if lock is not None:
      os.close(lock)
    os.close(directory)
