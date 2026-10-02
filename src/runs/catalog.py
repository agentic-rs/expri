"""Read-only standard-library run catalog for Python-only targets."""

import datetime as _catalog_datetime
import json as _catalog_json
import os as _catalog_os
import pathlib as _catalog_pathlib
import re as _catalog_re
import stat as _catalog_stat


_CATALOG_METADATA_LIMIT = 256 * 1024
_CATALOG_DETAIL_LIMIT = 16 * 1024 * 1024
_CATALOG_EXCLUDED_COMPONENTS = {".venv", ".expri", ".git", ".cache", "cache", "__pycache__"}
_CATALOG_STATUSES = {"preparing", "running", "completed", "failed", "cancelled", "lost", "unknown"}
_CATALOG_METADATA_FILES = (
  "run-state.json", "snapshot.json", "environment/environment-state.json",
)
_CATALOG_TIMESTAMP = _catalog_re.compile(
  r"(?P<year>\d{4})-(?P<month>\d{2})-(?P<day>\d{2})[Tt]"
  r"(?P<hour>\d{2}):(?P<minute>\d{2}):(?P<second>\d{2})"
  r"(?:\.(?P<fraction>\d+))?(?P<offset>[Zz]|[+-](?:[01]\d|2[0-3]):[0-5]\d)",
  _catalog_re.ASCII,
)


def query_runs(repo_root, request):
  """Inspect the checkout's run catalog without creating state or taking leases."""
  root = _catalog_pathlib.Path(repo_root).resolve(strict=True)
  state_dir = root / ".expri"
  try:
    metadata = state_dir.lstat()
  except FileNotFoundError:
    metadata = None
  if metadata is not None and not _catalog_stat.S_ISDIR(metadata.st_mode):
    raise ValueError("run catalog directory has an unsafe parent directory")
  return query_runs_directory(state_dir / "runs", request)


def query_runs_directory(runs_dir, request):
  """Return a list, detail, or file listing using only catalog-owned paths."""
  request = _catalog_validate_request(request)
  runs_dir = _catalog_pathlib.Path(runs_dir).absolute()
  present = _catalog_directory(runs_dir, missing=True, catalog=True)
  if present:
    runs_dir = runs_dir.parent.resolve(strict=True) / runs_dir.name
  operation = request.get("operation", "list")
  if operation == "list":
    if not present:
      return {"runs": [], "warnings": []}
    return _catalog_list(runs_dir, request)
  run_id = request["run_id"]
  run_dir = runs_dir / run_id
  if not present or not _catalog_directory(run_dir, missing=True):
    raise ValueError("run is missing: " + run_id)
  if operation == "show":
    return _catalog_show(run_dir)
  return _catalog_files(run_dir, request.get("artifacts", []))


def _catalog_validate_request(request):
  if not isinstance(request, dict):
    raise ValueError("run catalog request must be an object")
  operation = request.get("operation", "list")
  if operation not in {"list", "show", "files"}:
    raise ValueError("invalid run catalog operation: " + str(operation))
  if operation == "list":
    task = request.get("task")
    if task is not None and (not isinstance(task, str) or not task.strip()):
      raise ValueError("task filter must be a nonempty string")
    status = request.get("status")
    if status is not None and (not isinstance(status, str) or status not in _CATALOG_STATUSES):
      raise ValueError("invalid run status: " + str(status))
    limit = request.get("limit")
    if limit is not None and (type(limit) is not int or limit < 0):
      raise ValueError("limit must be a nonnegative integer")
  else:
    _catalog_validate_id(request.get("run_id"))
    if operation == "files":
      artifacts = request.get("artifacts", [])
      if not isinstance(artifacts, list):
        raise ValueError("artifacts must be an array")
      request = dict(request)
      request["artifacts"] = [_catalog_validate_artifact(artifact) for artifact in artifacts]
  return request


def _catalog_validate_id(run_id):
  if not isinstance(run_id, str) or _catalog_re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", run_id) is None:
    raise ValueError("invalid run ID: " + str(run_id))
  return run_id


def _catalog_validate_artifact(artifact):
  if not isinstance(artifact, str):
    raise ValueError("invalid artifact path: " + str(artifact))
  normalized = artifact[:-1] if artifact.endswith("/") else artifact
  parts = normalized.split("/")
  if (
    parts[0] not in {"code", "outputs"} or
    any(part in {"", ".", ".."} for part in parts) or "\\" in artifact or
    any(ord(character) < 32 or 127 <= ord(character) < 160 for character in artifact) or
    any(part in _CATALOG_EXCLUDED_COMPONENTS for part in parts)
  ):
    raise ValueError("invalid artifact path: " + artifact)
  return normalized


def _catalog_warning(warnings, run_id, message):
  warnings.append({"run_id": run_id, "message": message})


def _catalog_list(runs_dir, request):
  warnings = []
  records = []
  for run_dir in sorted(runs_dir.iterdir()):
    run_id = run_dir.name
    try:
      _catalog_validate_id(run_id)
    except ValueError:
      continue
    metadata = run_dir.lstat()
    if _catalog_stat.S_ISLNK(metadata.st_mode):
      _catalog_warning(warnings, run_id, "run directory must be a real directory")
      continue
    if not _catalog_stat.S_ISDIR(metadata.st_mode):
      continue
    state = _catalog_optional_json(run_dir, "run-state.json", warnings, missing=True)
    summary, timestamp = _catalog_summary(run_id, state, warnings)
    if request.get("task") is not None and summary["task"] != request["task"]:
      continue
    if request.get("status") is not None and summary["status"] != request["status"]:
      continue
    records.append((timestamp, run_id, summary))
  records.sort(key=lambda record: (record[0] is not None, record[0] or (0, 0), record[1]), reverse=True)
  limit = request.get("limit")
  if limit is not None:
    records = records[:limit]
  return {"runs": [record[2] for record in records], "warnings": warnings}


def _catalog_show(run_dir):
  warnings = []
  state = _catalog_optional_json(run_dir, "run-state.json", warnings, missing=True)
  summary, _ = _catalog_summary(run_dir.name, state, warnings)
  snapshot = _catalog_optional_json(run_dir, "snapshot.json", warnings, missing=True)
  environment = _catalog_optional_json(run_dir, "environment/environment-state.json", warnings, missing=True)
  return {"run": summary, "state": state, "snapshot": snapshot, "environment": environment, "warnings": warnings}


def _catalog_summary(run_id, state, warnings):
  summary = {
    "run_id": run_id, "task": None, "status": "unknown", "started_at": None,
    "finished_at": None, "exit_code": None, "schema_version": 0,
  }
  if state is None:
    return summary, None
  if not isinstance(state, dict):
    _catalog_warning(warnings, run_id, "run-state.json must contain a JSON object")
    return summary, None
  schema = state.get("schema_version", 0)
  if type(schema) is not int or not 0 <= schema < 2 ** 64:
    summary["schema_version"] = None
    _catalog_warning(warnings, run_id, "run-state.json has invalid schema_version")
    return summary, None
  summary["schema_version"] = schema
  if schema > 1:
    _catalog_warning(warnings, run_id, "run-state.json uses unsupported schema_version " + str(schema))
    return summary, None
  if state.get("run_id") != run_id:
    _catalog_warning(warnings, run_id, "run-state.json run_id does not match its directory")
    return summary, None
  task = state.get("task")
  if isinstance(task, str) and task.strip():
    summary["task"] = task
  else:
    _catalog_warning(warnings, run_id, "run-state.json has invalid or missing task")
  status = state.get("status")
  if isinstance(status, str) and status in _CATALOG_STATUSES and status != "unknown":
    summary["status"] = status
  else:
    _catalog_warning(warnings, run_id, "run-state.json has invalid or missing status")
  started_at = state.get("started_at")
  timestamp = _catalog_parse_timestamp(started_at)
  if timestamp is not None:
    summary["started_at"] = started_at
  else:
    _catalog_warning(warnings, run_id, "run-state.json has invalid or missing started_at")
  finished_at = state.get("finished_at")
  if "finished_at" in state:
    if _catalog_parse_timestamp(finished_at) is not None:
      summary["finished_at"] = finished_at
    else:
      _catalog_warning(warnings, run_id, "run-state.json has invalid or missing finished_at")
  elif summary["status"] in {"completed", "failed", "cancelled"}:
    _catalog_warning(warnings, run_id, "run-state.json has invalid or missing finished_at")
  exit_code = state.get("exit_code")
  if type(exit_code) is int and -(2 ** 31) <= exit_code < 2 ** 31:
    summary["exit_code"] = exit_code
  elif "exit_code" in state or summary["status"] in {"completed", "failed", "cancelled"}:
    _catalog_warning(warnings, run_id, "run-state.json has invalid or missing exit_code")
  return summary, timestamp


def _catalog_parse_timestamp(timestamp):
  if not isinstance(timestamp, str):
    return None
  match = _CATALOG_TIMESTAMP.fullmatch(timestamp)
  if match is None:
    return None
  try:
    normalized = timestamp[:-1] + "+00:00" if timestamp[-1] in {"Z", "z"} else timestamp
    parsed = _catalog_datetime.datetime.fromisoformat(normalized)
    seconds = parsed.replace(microsecond=0).astimezone(_catalog_datetime.timezone.utc)
  except (ValueError, OverflowError):
    return None
  nanos = int((match.group("fraction") or "").ljust(9, "0")[:9])
  return seconds, nanos


def _catalog_directory(path, missing=False, catalog=False):
  try:
    metadata = path.lstat()
  except FileNotFoundError:
    if missing:
      return False
    raise ValueError("directory is missing: " + str(path)) from None
  if not _catalog_stat.S_ISDIR(metadata.st_mode):
    if catalog:
      raise ValueError("run catalog directory must be a real directory")
    raise ValueError("directory must not be a symlink or file: " + str(path))
  return True


def _catalog_safe_path(run_dir, relative):
  current = run_dir
  for part in _catalog_pathlib.PurePosixPath(relative).parts[:-1]:
    current = current / part
    try:
      metadata = current.lstat()
    except FileNotFoundError:
      raise FileNotFoundError(relative) from None
    if not _catalog_stat.S_ISDIR(metadata.st_mode):
      raise ValueError(relative + " has an unsafe parent directory")
  return current / _catalog_pathlib.PurePosixPath(relative).name


def _catalog_optional_json(run_dir, relative, warnings, missing=False):
  limit = _CATALOG_METADATA_LIMIT if relative == "run-state.json" else _CATALOG_DETAIL_LIMIT
  try:
    path = _catalog_safe_path(run_dir, relative)
    metadata = path.lstat()
    if not _catalog_stat.S_ISREG(metadata.st_mode):
      raise ValueError(relative + " must be a regular file")
    if metadata.st_size > limit:
      raise ValueError(relative + " exceeds the metadata size limit")
    flags = _catalog_os.O_RDONLY | getattr(_catalog_os, "O_NOFOLLOW", 0) | getattr(_catalog_os, "O_CLOEXEC", 0)
    with _catalog_os.fdopen(_catalog_os.open(path, flags), "rb") as handle:
      opened = _catalog_os.fstat(handle.fileno())
      if not _catalog_stat.S_ISREG(opened.st_mode):
        raise ValueError(relative + " must be a regular file")
      if (metadata.st_dev, metadata.st_ino) != (opened.st_dev, opened.st_ino):
        raise ValueError(relative + " could not be read")
      _catalog_safe_path(run_dir, relative)
      raw = handle.read(limit + 1)
    if len(raw) > limit:
      raise ValueError(relative + " exceeds the metadata size limit")
    value = _catalog_json.loads(raw.decode("utf-8"), parse_constant=_catalog_invalid_json_constant)
    if (relative != "run-state.json" or value is None) and not isinstance(value, dict):
      raise ValueError(relative + " must contain a JSON object")
    return value
  except FileNotFoundError:
    if missing:
      _catalog_warning(warnings, run_dir.name, relative + " is missing")
  except (ValueError, UnicodeDecodeError, RecursionError) as error:
    if isinstance(error, (_catalog_json.JSONDecodeError, UnicodeDecodeError, RecursionError)):
      message = relative + " contains invalid JSON"
    else:
      message = str(error)
    _catalog_warning(warnings, run_dir.name, message)
  except OSError:
    _catalog_warning(warnings, run_dir.name, relative + " could not be read")
  return None


def _catalog_invalid_json_constant(_):
  raise _catalog_json.JSONDecodeError("invalid JSON constant", "", 0)


def _catalog_files(run_dir, artifacts):
  warnings = []
  files = set()
  for relative in _CATALOG_METADATA_FILES:
    _catalog_collect_files(run_dir, relative, files, warnings, explicit=False, recurse=False)
  _catalog_collect_files(run_dir, "logs", files, warnings, explicit=False)
  for artifact in artifacts:
    _catalog_collect_files(run_dir, artifact, files, warnings, explicit=True)
  return {"run_id": run_dir.name, "run_dir": str(run_dir), "files": sorted(files), "warnings": warnings}


def _catalog_collect_files(run_dir, relative, files, warnings, explicit, recurse=True):
  if any(part in _CATALOG_EXCLUDED_COMPONENTS for part in relative.split("/")):
    _catalog_warning(warnings, run_dir.name, relative + " is excluded from artifact selection")
    return
  if "\\" in relative or any(ord(character) < 32 or 127 <= ord(character) < 160 for character in relative):
    message = "invalid run file path: " + relative
    if explicit:
      raise ValueError(message)
    _catalog_warning(warnings, run_dir.name, message)
    return
  try:
    path = _catalog_safe_path(run_dir, relative)
    metadata = path.lstat()
    if relative == "logs" and not explicit and not _catalog_stat.S_ISDIR(metadata.st_mode):
      raise ValueError("logs must be a real directory")
    if _catalog_stat.S_ISREG(metadata.st_mode):
      files.add(relative)
    elif _catalog_stat.S_ISDIR(metadata.st_mode) and recurse:
      for entry in sorted(path.iterdir()):
        _catalog_collect_files(run_dir, relative + "/" + entry.name, files, warnings, explicit)
    else:
      raise ValueError(relative + " must be a regular file" if not recurse else relative + " must not be a symlink or special file")
  except FileNotFoundError:
    if explicit:
      raise ValueError("artifact is missing: " + relative) from None
  except (OSError, ValueError) as error:
    if explicit:
      raise ValueError(str(error)) from None
    _catalog_warning(warnings, run_dir.name, str(error))
