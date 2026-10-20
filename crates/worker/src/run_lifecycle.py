"""Foreground and detached run supervision for the standard-library fallback."""

import datetime as _lifecycle_datetime
import fcntl as _lifecycle_fcntl
import json as _lifecycle_json
import os as _lifecycle_os
import pathlib as _lifecycle_pathlib
import stat as _lifecycle_stat
import subprocess as _lifecycle_subprocess
import sys as _lifecycle_sys
import tempfile as _lifecycle_tempfile
import threading as _lifecycle_threading
import time as _lifecycle_time

if "_capture_begin" not in globals():
  from system_capture import _capture_begin, _capture_close


def _lifecycle_now():
  return _lifecycle_datetime.datetime.now(_lifecycle_datetime.timezone.utc).isoformat()


def _lifecycle_write_json(path, value):
  """Serialize task updates and supervisor publication through the same lease."""
  path = _lifecycle_pathlib.Path(path)
  if not isinstance(value, dict):
    raise ValueError("run state must be an object")
  if not _lifecycle_stat.S_ISDIR(path.parent.lstat().st_mode):
    raise ValueError("run directory must be a real directory")
  lock_path = path.parent / ".run-state.lock"
  flags = _lifecycle_os.O_RDWR | _lifecycle_os.O_CREAT | _lifecycle_os.O_NONBLOCK
  flags |= _lifecycle_os.O_NOFOLLOW | _lifecycle_os.O_CLOEXEC
  descriptor = _lifecycle_os.open(lock_path, flags, 0o600)
  try:
    if not _lifecycle_stat.S_ISREG(_lifecycle_os.fstat(descriptor).st_mode):
      raise ValueError("run state lease must be a regular file")
    _lifecycle_fcntl.flock(descriptor, _lifecycle_fcntl.LOCK_EX)
    try:
      current = {}
      try:
        state_fd = _lifecycle_os.open(path, _lifecycle_os.O_RDONLY | _lifecycle_os.O_NOFOLLOW |
                                     _lifecycle_os.O_CLOEXEC | _lifecycle_os.O_NONBLOCK)
      except FileNotFoundError:
        pass
      else:
        with _lifecycle_os.fdopen(state_fd, "rb") as handle:
          if not _lifecycle_stat.S_ISREG(_lifecycle_os.fstat(handle.fileno()).st_mode):
            raise ValueError("run state must be a regular file")
          raw = handle.read(256 * 1024 + 1)
        if len(raw) > 256 * 1024:
          raise ValueError("run state exceeds metadata limit")
        current = _lifecycle_json.loads(raw)
        if not isinstance(current, dict):
          raise ValueError("run state must be an object")
      terminal = {"completed", "failed", "cancelled", "lost"}
      if current.get("status") in terminal and value.get("status") is not None and value["status"] not in terminal:
        raise ValueError("cannot restore an active status after the run has finished")
      current.update(value)
      raw = (_lifecycle_json.dumps(current, indent=2, sort_keys=True, allow_nan=False) + "\n").encode("utf-8")
      if len(raw) > 256 * 1024:
        raise ValueError("run state exceeds metadata limit")
      temporary = None
      try:
        with _lifecycle_tempfile.NamedTemporaryFile(mode="wb", dir=path.parent, delete=False) as handle:
          temporary = _lifecycle_pathlib.Path(handle.name)
          handle.write(raw)
          handle.flush()
          _lifecycle_os.fsync(handle.fileno())
        temporary.replace(path)
        directory_fd = _lifecycle_os.open(path.parent, _lifecycle_os.O_RDONLY | _lifecycle_os.O_DIRECTORY |
                                         _lifecycle_os.O_NOFOLLOW | _lifecycle_os.O_CLOEXEC)
        try:
          _lifecycle_os.fsync(directory_fd)
        finally:
          _lifecycle_os.close(directory_fd)
      finally:
        if temporary is not None:
          temporary.unlink(missing_ok=True)
    finally:
      _lifecycle_fcntl.flock(descriptor, _lifecycle_fcntl.LOCK_UN)
  finally:
    _lifecycle_os.close(descriptor)


def _lifecycle_create_file(path, raw=b""):
  flags = _lifecycle_os.O_WRONLY | _lifecycle_os.O_CREAT | _lifecycle_os.O_EXCL
  flags |= getattr(_lifecycle_os, "O_NOFOLLOW", 0) | getattr(_lifecycle_os, "O_CLOEXEC", 0)
  descriptor = _lifecycle_os.open(path, flags, 0o600)
  with _lifecycle_os.fdopen(descriptor, "wb") as handle:
    handle.write(raw)


def _lifecycle_marker(path):
  try:
    metadata = path.lstat()
  except FileNotFoundError:
    return False
  if not _lifecycle_stat.S_ISREG(metadata.st_mode) or metadata.st_size != 0:
    raise ValueError("run control marker must be an empty regular file: " + str(path))
  return True


def _lifecycle_message(message):
  try:
    print(message, file=_lifecycle_sys.stderr, flush=True)
  except (OSError, ValueError):
    pass


def _lifecycle_existing_machine_id():
  """Read optional host identity without creating storage or requiring SQLite."""
  try:
    import sqlite3
  except ImportError:
    return None
  try:
    override = _lifecycle_os.environ.get("EXPRI_STORAGE_DIR")
    if override is not None:
      if not override or ".." in _lifecycle_pathlib.Path(override).parts:
        return None
      root = _lifecycle_pathlib.Path(_lifecycle_os.path.abspath(override))
      owned = [root]
    else:
      home = _lifecycle_os.environ.get("HOME")
      if not home or ".." in _lifecycle_pathlib.Path(home).parts:
        return None
      boundary = _lifecycle_pathlib.Path(_lifecycle_os.path.abspath(home)) / ".tokn"
      root = boundary / "expri"
      owned = [boundary, root]
    for directory in owned:
      if not _lifecycle_stat.S_ISDIR(directory.lstat().st_mode):
        return None
    database = root / "profiles.sqlite3"
    if not _lifecycle_stat.S_ISREG(database.lstat().st_mode):
      return None
    for suffix in ("-wal", "-shm", "-journal"):
      try:
        metadata = database.with_name(database.name + suffix).lstat()
      except FileNotFoundError:
        continue
      if not _lifecycle_stat.S_ISREG(metadata.st_mode):
        return None
    connection = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=5)
    try:
      row = connection.execute("SELECT machine_id FROM system_local WHERE singleton=1").fetchone()
    finally:
      connection.close()
    machine_id = row[0] if row is not None else None
    if (not isinstance(machine_id, str) or not machine_id or len(machine_id) > 96
        or machine_id in (".", "..")
        or any(character not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-"
               for character in machine_id)):
      return None
    return machine_id
  except (OSError, ValueError, sqlite3.Error):
    return None


def start_run(repo_root, request, context, worker_source, ready_timeout=10.0):
  """Snapshot before acknowledgement; return a detached receipt or foreground exit."""
  if request.get("requirements", {}).get("disk"):
    raise ValueError("resource requirements require a native Linux detached worker with run-requirements-v1")
  if request.get("service") is not None and request["service"].get("publish", True):
    raise ValueError("automatic publishing requires a native expri worker with run-publishing-v1")
  if not request.get("command"):
    raise ValueError("task command must not be empty")
  detached = request.get("detach", False)
  if type(detached) is not bool:
    raise ValueError("detach must be boolean")
  smoke = request.get("smoke", False)
  if type(smoke) is not bool:
    raise ValueError("smoke must be boolean")
  repo_root = str(_lifecycle_pathlib.Path(repo_root).resolve(strict=True))
  snapshot = context["create_snapshot"](
    repo_root, request.get("remote_managed", []), request.get("expected_sync"), smoke=smoke,
  )
  run_dir = _lifecycle_pathlib.Path(snapshot["run_dir"])
  state = {
    "schema_version": 1, "run_id": snapshot["run_id"], "task": request["name"],
    "command": request["command"], "code_dir": snapshot["code_dir"],
    "output_dir": str(run_dir / "outputs"), "detached": detached,
    "logs": {"stdout": "logs/stdout.log", "stderr": "logs/stderr.log"},
    "status": "preparing", "started_at": _lifecycle_now(),
  }
  if smoke:
    state["smoke"] = True
  machine_id = _lifecycle_existing_machine_id()
  if machine_id is not None:
    state["machine_id"] = machine_id
  _capture_begin(run_dir, state)
  _lifecycle_write_json(run_dir / "run-state.json", state)
  _lifecycle_message("run: " + snapshot["run_id"])
  _lifecycle_message("run directory: " + str(run_dir))
  _lifecycle_write_json(run_dir / "run-request.json", request)
  payload = {"request": request, "snapshot": snapshot, "state": state, "repo_root": repo_root}
  if not detached:
    return execute_worker(payload, context)
  try:
    payload_path = run_dir / ".worker-request.json"
    _lifecycle_write_json(payload_path, payload)
    worker_path = run_dir / ".worker.py"
    _lifecycle_create_file(worker_path, worker_source.encode("utf-8"))
    worker = _lifecycle_subprocess.Popen(
      [_lifecycle_sys.executable, "-B", "-I", str(worker_path), str(payload_path)],
      cwd=repo_root, stdin=_lifecycle_subprocess.DEVNULL,
      stdout=_lifecycle_subprocess.DEVNULL, stderr=_lifecycle_subprocess.DEVNULL,
      start_new_session=True, close_fds=True,
    )
  except Exception as error:
    state.update(status="failed", exit_code=1, finished_at=_lifecycle_now(), error="worker launch failed: " + str(error))
    _capture_close(run_dir, state)
    _lifecycle_write_json(run_dir / "run-state.json", state)
    raise
  # Reap if the launcher remains alive; this thread never delays CLI/SSH exit.
  _lifecycle_threading.Thread(target=worker.wait, daemon=True).start()
  deadline = _lifecycle_time.monotonic() + ready_timeout
  while not _lifecycle_marker(run_dir / ".worker-ready"):
    exit_code = worker.poll()
    if exit_code is not None:
      current = _lifecycle_json.loads((run_dir / "run-state.json").read_text())
      if current.get("status") not in ("completed", "failed", "cancelled"):
        current.update(status="failed", exit_code=exit_code if exit_code >= 0 else 128 - exit_code,
                       finished_at=_lifecycle_now(), error="worker exited before acquiring its run lease")
        _capture_close(run_dir, current)
        _lifecycle_write_json(run_dir / "run-state.json", current)
      raise RuntimeError(current.get("error", "worker exited before acknowledging readiness"))
    if _lifecycle_time.monotonic() >= deadline:
      try:
        _lifecycle_create_file(run_dir / ".cancel-request")
      except FileExistsError:
        _lifecycle_marker(run_dir / ".cancel-request")
      raise RuntimeError("worker did not acknowledge its run lease; cancellation requested")
    _lifecycle_time.sleep(0.02)
  current = _lifecycle_json.loads((run_dir / "run-state.json").read_text())
  receipt = {"run_id": snapshot["run_id"], "run_dir": str(run_dir), "status": current["status"], "detached": True}
  if smoke:
    receipt["smoke"] = True
  return receipt


def execute_worker(payload, context):
  """Hold the run lease through preparation, task execution, and final publication."""
  request, snapshot, state = payload["request"], payload["snapshot"], payload["state"]
  run_dir = _lifecycle_pathlib.Path(snapshot["run_dir"])
  state_path = run_dir / "run-state.json"
  detached = state["detached"]
  cancel_path = run_dir / ".cancel-request" if detached else None
  lease = context["acquire_run_lock"](run_dir)
  logs = None
  exit_code = 1
  cancelled_before_launch = False
  try:
    _lifecycle_write_json(state_path, state)
    logs = context["RunLogs"](run_dir, cancel_path=cancel_path, mirror=not detached)
    if detached:
      _lifecycle_create_file(run_dir / ".worker-ready")
    cache_dir = context["environment_cache_dir"](payload["repo_root"], request.get("sync_args", []))
    if logs.cancellation_requested():
      cancelled_before_launch = True
      exit_code = 143
    else:
      prepared = context["prepare_environment"](
        request["environment"], snapshot["code_dir"], str(run_dir), request.get("extras", []),
        request.get("sync_args", []), True, cache_dir=cache_dir, process_runner=logs.run,
      )
      if logs.cancellation_requested():
        cancelled_before_launch = True
        exit_code = 143
      else:
        state.update(status="running", environment_manifest=prepared["manifest_path"], python=prepared["python"])
        _lifecycle_write_json(state_path, state)
        child_env = _lifecycle_os.environ.copy()
        for key in [*context["ENV_REMOVE"], *prepared.get("env_remove", [])]:
          child_env.pop(key, None)
        child_env.update(request["environment"].get("env", {}))
        child_env.update(prepared.get("run_env", {}))
        child_env.update(UV_PROJECT_ENVIRONMENT=prepared["environment_path"], PYTHONNOUSERSITE="1",
                         EXPRI_RUN_ID=snapshot["run_id"], EXPRI_RUN_DIR=str(run_dir),
                         EXPRI_OUTPUT_DIR=str(run_dir / "outputs"))
        for key in ["EXPRI_ATTEMPT", "EXPRI_STEP_OFFSET", "EXPRI_RESUME_CHECKPOINT"]:
          child_env.pop(key, None)
        result = logs.run(["uv", "run", "--no-sync", "--no-env-file", "--", *request["command"]],
                          cwd=snapshot["code_dir"], env=child_env,
                          on_started=lambda: _lifecycle_write_json(state_path, {"task_started_at": _lifecycle_now()}))
        exit_code = result.returncode if result.returncode >= 0 else 128 - result.returncode
        if result.started:
          state["task_exit_code"] = exit_code
        if exit_code:
          state["error"] = "task exited with status " + str(exit_code)
  except _lifecycle_subprocess.CalledProcessError as error:
    exit_code = error.returncode if error.returncode >= 0 else 128 - error.returncode
    state["error"] = "environment preparation exited with status " + str(exit_code)
  except Exception as error:
    state["error"] = str(error)
    _lifecycle_message("error: " + str(error))
  finally:
    try:
      if logs is not None:
        logs.close()
        if logs.log_error:
          state["logging_error"] = logs.log_error
          if exit_code == 0:
            exit_code = 1
            state["error"] = "log capture failed: " + logs.log_error
      try:
        if cancel_path is not None:
          _lifecycle_marker(cancel_path)
        # Cancellation is asynchronous: completion wins after the supervisor's
        # final safe group-control point, even if a request then reaches disk.
        cancelled = cancelled_before_launch or (logs is not None and logs.cancelled)
      except (OSError, ValueError) as error:
        cancelled = False
        exit_code = 1
        state["error"] = "could not inspect cancellation request: " + str(error)
      if cancelled:
        exit_code = 130
        state["error"] = "run cancellation requested"
      status = "cancelled" if cancelled else ("completed" if exit_code == 0 else "failed")
      state.update(status=status, exit_code=exit_code, finished_at=_lifecycle_now())
      _capture_close(run_dir, state)
      _lifecycle_write_json(state_path, state)
    finally:
      lease.close()
  return exit_code
