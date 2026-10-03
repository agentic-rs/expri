"""Optional, dependency-free metrics writer for expri training scripts on Unix."""

from collections.abc import Mapping
from contextlib import contextmanager
from datetime import datetime, timezone
import fcntl
import json
import math
from numbers import Integral, Real
import os
from pathlib import Path
import secrets
import stat
import threading


MAX_EVENT_BYTES = 1024 * 1024
MAX_PARAMS_BYTES = 1024 * 1024
MAX_METRIC_NAME_BYTES = 256
MAX_PARAMS_DEPTH = 64
MAX_STEP = 2 ** 64 - 1
MIN_JSON_INTEGER = -(2 ** 63)


def _json_bytes(value, limit, label):
  try:
    raw = (json.dumps(value, ensure_ascii=False, allow_nan=False,
                      separators=(",", ":"), sort_keys=True) + "\n").encode("utf-8")
  except (TypeError, ValueError, OverflowError, RecursionError, UnicodeError) as error:
    raise ValueError(label + " must contain supported, finite JSON values") from error
  if len(raw) > limit:
    raise ValueError(label + " exceeds the 1 MiB limit")
  return raw


def _metric_values(metrics):
  if not isinstance(metrics, Mapping) or not metrics:
    raise ValueError("metrics must be a nonempty mapping")
  values = {}
  for name, value in metrics.items():
    if not isinstance(name, str) or not name.strip():
      raise ValueError("metric names must be nonempty strings")
    if any(ord(character) < 32 or 127 <= ord(character) < 160 for character in name):
      raise ValueError("metric names must not contain control characters")
    try:
      size = len(name.encode("utf-8"))
    except UnicodeError as error:
      raise ValueError("metric names must be valid UTF-8") from error
    if size > MAX_METRIC_NAME_BYTES:
      raise ValueError("metric names must not exceed 256 UTF-8 bytes")
    if isinstance(value, bool) or not isinstance(value, Real):
      raise ValueError("metric values must be finite numbers, excluding booleans")
    try:
      finite = math.isfinite(float(value))
    except (OverflowError, ValueError, TypeError) as error:
      raise ValueError("metric values must fit a finite JSON number") from error
    if not finite:
      raise ValueError("metric values must be finite numbers")
    integer = int(value) if isinstance(value, Integral) else None
    values[name] = integer if integer is not None and MIN_JSON_INTEGER <= integer <= MAX_STEP else float(value)
  return values


def _params_value(value, ancestors=None, depth=0):
  """Accept JSON types without silently converting keys, tuples, or objects."""
  if value is None or isinstance(value, (bool, str)):
    return value
  if isinstance(value, int):
    if not MIN_JSON_INTEGER <= value <= MAX_STEP:
      raise ValueError("params integers must fit i64 or u64")
    return value
  if isinstance(value, float):
    if not math.isfinite(value):
      raise ValueError("params must contain finite JSON numbers")
    return value
  if not isinstance(value, (Mapping, list)):
    raise ValueError("params must contain supported JSON values")
  depth += 1
  if depth > MAX_PARAMS_DEPTH:
    raise ValueError("params must not exceed 64 object or array levels, including the parameter root")
  ancestors = set() if ancestors is None else ancestors
  identity = id(value)
  if identity in ancestors:
    raise ValueError("params must not contain circular references")
  ancestors.add(identity)
  try:
    if isinstance(value, Mapping):
      normalized = {}
      for key, child in value.items():
        if not isinstance(key, str):
          raise ValueError("params object keys must be strings")
        normalized[key] = _params_value(child, ancestors, depth)
      return normalized
    return [_params_value(child, ancestors, depth) for child in value]
  finally:
    ancestors.remove(identity)


def _write_all(descriptor, raw):
  pending = memoryview(raw)
  while pending:
    written = os.write(descriptor, pending)
    if written <= 0:
      raise OSError("metrics file did not accept bytes")
    pending = pending[written:]


class MetricsLogger:
  """Append flushed JSONL events and save immutable effective run parameters.

  By default, use EXPRI_OUTPUT_DIR. Pass a directory explicitly outside expri.
  Threads and cooperating Unix writers serialize complete rows. Distributed
  training should write from rank 0; separate workers need separate run folders.
  """

  def __init__(self, output_dir=None):
    self._thread_lock = threading.RLock()
    self._directory_fd = None
    self._metrics_fd = None
    self._closed = False
    self._owner_pid = os.getpid()
    if output_dir is None:
      output_dir = os.environ.get("EXPRI_OUTPUT_DIR")
      if not output_dir:
        raise ValueError("EXPRI_OUTPUT_DIR is unset; pass output_dir explicitly")
    self.output_dir = Path(output_dir).absolute()
    try:
      metadata = self.output_dir.lstat()
    except FileNotFoundError:
      metadata = None
    if metadata is not None and not stat.S_ISDIR(metadata.st_mode):
      raise ValueError("output_dir must be a real directory, without a symlink")
    self.output_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    self._directory_fd = os.open(self.output_dir, flags)
    if not stat.S_ISDIR(os.fstat(self._directory_fd).st_mode):
      self.close()
      raise ValueError("output_dir must be a real directory")

  @property
  def closed(self):
    return self._closed

  def _check_open(self):
    self._check_owner()
    if self._closed:
      raise ValueError("metrics logger is closed")

  def _check_owner(self):
    if self._owner_pid != os.getpid():
      raise ValueError("create a MetricsLogger in each process; do not reuse one inherited through fork")

  def _open_file(self, name, flags, missing=False):
    try:
      metadata = os.stat(name, dir_fd=self._directory_fd, follow_symlinks=False)
    except FileNotFoundError:
      if not flags & os.O_CREAT:
        if missing:
          return None
        raise
    else:
      if not stat.S_ISREG(metadata.st_mode):
        raise ValueError(name + " must be a regular file, without a symlink")
    open_flags = flags | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
    create_or_open = flags & os.O_CREAT and not flags & os.O_EXCL
    try:
      try:
        descriptor = os.open(name, open_flags | (os.O_EXCL if create_or_open else 0),
                             0o600, dir_fd=self._directory_fd)
      except FileExistsError:
        if not create_or_open:
          raise
        # Keep creation exclusive, then open the winning regular file. Darwin's
        # O_CREAT|O_NOFOLLOW path can fail during concurrent first creation.
        descriptor = os.open(name, open_flags & ~os.O_CREAT, dir_fd=self._directory_fd)
    except FileNotFoundError:
      if missing:
        return None
      raise
    if not stat.S_ISREG(os.fstat(descriptor).st_mode):
      os.close(descriptor)
      raise ValueError(name + " must be a regular file")
    return descriptor

  @contextmanager
  def _append_lease(self):
    if self._metrics_fd is None:
      self._metrics_fd = self._open_file("metrics.jsonl", os.O_RDWR | os.O_CREAT | os.O_APPEND)
    fcntl.flock(self._metrics_fd, fcntl.LOCK_EX)
    try:
      yield self._metrics_fd
    finally:
      fcntl.flock(self._metrics_fd, fcntl.LOCK_UN)

  def log(self, step, metrics):
    """Append one event; steps need not be unique or consecutive."""
    self._check_owner()
    if isinstance(step, bool) or not isinstance(step, Integral) or not 0 <= step <= MAX_STEP:
      raise ValueError("step must be a nonnegative integer at most u64::MAX")
    event = {
      "schema_version": 1, "step": int(step),
      "timestamp": datetime.now(timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z"),
      "metrics": _metric_values(metrics),
    }
    raw = _json_bytes(event, MAX_EVENT_BYTES, "metrics event")
    with self._thread_lock:
      self._check_open()
      with self._append_lease() as descriptor:
        size = os.fstat(descriptor).st_size
        if size and os.pread(descriptor, 1, size - 1) != b"\n":
          raise ValueError("metrics.jsonl ends without a newline; repair the incomplete row before appending")
        # Unbuffered writes make the complete event visible before returning.
        _write_all(descriptor, raw)

  def _existing_params(self):
    descriptor = self._open_file("params.json", os.O_RDONLY, missing=True)
    if descriptor is None:
      return None
    with os.fdopen(descriptor, "rb") as handle:
      raw = handle.read(MAX_PARAMS_BYTES + 1)
    if len(raw) > MAX_PARAMS_BYTES:
      raise ValueError("existing params.json exceeds the 1 MiB limit")
    try:
      existing = json.loads(raw)
    except (ValueError, UnicodeError) as error:
      raise ValueError("existing params.json is invalid JSON") from error
    return _json_bytes(existing, MAX_PARAMS_BYTES, "existing params.json")

  def params(self, mapping):
    """Save effective parameters once; identical subsequent values are allowed."""
    self._check_owner()
    if not isinstance(mapping, Mapping):
      raise ValueError("params must be a mapping with string keys")
    try:
      value = _params_value(mapping)
    except RecursionError as error:
      raise ValueError("params nesting is too deep") from error
    raw = _json_bytes({"schema_version": 1, "params": value}, MAX_PARAMS_BYTES, "params")
    with self._thread_lock:
      self._check_open()
      existing = self._existing_params()
      if existing is not None:
        if existing != raw:
          raise ValueError("params.json already contains different effective parameters")
        return
      name = ".params-" + secrets.token_hex(12) + ".tmp"
      descriptor = self._open_file(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL)
      try:
        with os.fdopen(descriptor, "wb", buffering=0) as handle:
          _write_all(handle.fileno(), raw)
        try:
          # Publish only complete bytes, without replacing another writer's file.
          os.link(name, "params.json", src_dir_fd=self._directory_fd,
                  dst_dir_fd=self._directory_fd, follow_symlinks=False)
        except FileExistsError:
          if self._existing_params() != raw:
            raise ValueError("params.json already contains different effective parameters") from None
      finally:
        os.unlink(name, dir_fd=self._directory_fd)

  def close(self):
    """Close owned descriptors; events have already been flushed after each log."""
    if self._owner_pid != os.getpid():
      # An inherited thread lock can be held by a thread absent in the child.
      self._close_descriptors()
      return
    with self._thread_lock:
      self._close_descriptors()

  def _close_descriptors(self):
    self._closed = True
    descriptors = (self._metrics_fd, self._directory_fd)
    self._metrics_fd = self._directory_fd = None
    try:
      if descriptors[0] is not None:
        os.close(descriptors[0])
    finally:
      if descriptors[1] is not None:
        os.close(descriptors[1])

  def __enter__(self):
    self._check_open()
    return self

  def __exit__(self, *_):
    self.close()

  def __del__(self):
    try:
      self.close()
    except Exception:
      pass
