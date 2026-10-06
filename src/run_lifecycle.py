"""Foreground and detached run supervision for the standard-library fallback."""

import datetime as _lifecycle_datetime
import json as _lifecycle_json
import os as _lifecycle_os
import pathlib as _lifecycle_pathlib
import stat as _lifecycle_stat
import subprocess as _lifecycle_subprocess
import sys as _lifecycle_sys
import tempfile as _lifecycle_tempfile
import threading as _lifecycle_threading
import time as _lifecycle_time


def _lifecycle_now():
  return _lifecycle_datetime.datetime.now(_lifecycle_datetime.timezone.utc).isoformat()


def _lifecycle_write_json(path, value):
  temporary = None
  try:
    with _lifecycle_tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as handle:
      temporary = _lifecycle_pathlib.Path(handle.name)
      _lifecycle_json.dump(value, handle, indent=2, sort_keys=True)
      handle.write("\n")
    temporary.replace(path)
  finally:
    if temporary is not None:
      temporary.unlink(missing_ok=True)


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


def start_run(repo_root, request, context, worker_source, ready_timeout=10.0):
  """Snapshot before acknowledgement; return a detached receipt or foreground exit."""
  if request.get("service") is not None:
    raise ValueError("automatic publishing requires a native expri worker with run-publishing-v1")
  if not request.get("command"):
    raise ValueError("task command must not be empty")
  detached = request.get("detach", False)
  if type(detached) is not bool:
    raise ValueError("detach must be boolean")
  repo_root = str(_lifecycle_pathlib.Path(repo_root).resolve(strict=True))
  snapshot = context["create_snapshot"](repo_root, request.get("remote_managed", []), request.get("expected_sync"))
  run_dir = _lifecycle_pathlib.Path(snapshot["run_dir"])
  state = {
    "schema_version": 1, "run_id": snapshot["run_id"], "task": request["name"],
    "command": request["command"], "code_dir": snapshot["code_dir"],
    "output_dir": str(run_dir / "outputs"), "detached": detached,
    "logs": {"stdout": "logs/stdout.log", "stderr": "logs/stderr.log"},
    "status": "preparing", "started_at": _lifecycle_now(),
  }
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
  return {"run_id": snapshot["run_id"], "run_dir": str(run_dir), "status": current["status"], "detached": True}


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
        result = logs.run(["uv", "run", "--no-sync", "--no-env-file", "--", *request["command"]],
                          cwd=snapshot["code_dir"], env=child_env)
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
      _lifecycle_write_json(state_path, state)
    finally:
      lease.close()
  return exit_code
