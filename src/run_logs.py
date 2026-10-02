"""Binary subprocess logs and a live terminal tee for the Python run fallback."""

import os as _run_logs_os
import pathlib as _run_logs_pathlib
import select as _run_logs_select
import selectors as _run_logs_selectors
import subprocess as _run_logs_subprocess
import sys as _run_logs_sys
import time as _run_logs_time


class RunLogResult:
  def __init__(self, returncode, stdout, log_error):
    self.returncode = returncode
    self.stdout = stdout
    self.log_error = log_error


class RunLogs:
  """Own fresh log files and drain both child pipes without collecting task output."""

  def __init__(self, run_dir, *, stdout_sink=None, stderr_sink=None, drain_grace=1.0):
    if drain_grace < 0:
      raise ValueError("drain_grace must not be negative")
    self._errors = []
    self._closed = False
    self._drain_grace = drain_grace
    self._files = {}
    self._sinks = {
      "stdout": self._terminal_sink(_run_logs_sys.stdout) if stdout_sink is None else stdout_sink,
      "stderr": self._terminal_sink(_run_logs_sys.stderr) if stderr_sink is None else stderr_sink,
    }
    logs_dir = _run_logs_pathlib.Path(run_dir) / "logs"
    logs_dir.mkdir(mode=0o700)
    try:
      for name in ("stdout", "stderr"):
        flags = _run_logs_os.O_WRONLY | _run_logs_os.O_CREAT | _run_logs_os.O_EXCL
        flags |= getattr(_run_logs_os, "O_NOFOLLOW", 0)
        flags |= getattr(_run_logs_os, "O_CLOEXEC", 0)
        descriptor = _run_logs_os.open(logs_dir / (name + ".log"), flags, 0o600)
        try:
          self._files[name] = _run_logs_os.fdopen(descriptor, "wb", buffering=0)
        except BaseException:
          _run_logs_os.close(descriptor)
          raise
    except BaseException:
      self.close()
      raise

  @staticmethod
  def _terminal_sink(stream):
    return getattr(stream, "buffer", stream)

  @property
  def log_error(self):
    return "; ".join(self._errors) if self._errors else None

  def _record_error(self, message):
    if message not in self._errors:
      self._errors.append(message)

  @staticmethod
  def _write_all(sink, data):
    remaining = memoryview(data)
    while remaining:
      written = sink.write(remaining)
      if written is None or written <= 0 or written > len(remaining):
        raise OSError("stream did not accept output")
      remaining = remaining[written:]

  def _log_and_tee(self, name, data):
    log = self._files.get(name)
    if log is not None:
      try:
        self._write_all(log, data)
      except (OSError, ValueError, TypeError) as error:
        self._record_error(name + " log write failed: " + str(error))
        self._close_file(name)
    terminal = self._sinks.get(name)
    if terminal is not None:
      try:
        self._write_all(terminal, data)
        terminal.flush()
      except (OSError, ValueError, TypeError):
        # A disconnected terminal must not prevent complete file logging or reaping.
        self._sinks[name] = None

  def _close_file(self, name):
    handle = self._files.pop(name, None)
    if handle is not None:
      try:
        handle.close()
      except (OSError, ValueError) as error:
        self._record_error(name + " log close failed: " + str(error))

  def close(self):
    self._closed = True
    for name in tuple(self._files):
      self._close_file(name)

  def __enter__(self):
    if self._closed:
      raise ValueError("run logs are closed")
    return self

  def __exit__(self, exception_type, exception, traceback):
    self.close()

  @staticmethod
  def _stop_child(child):
    if child.poll() is None:
      try:
        child.terminate()
      except ProcessLookupError:
        pass
      try:
        child.wait(timeout=1)
      except _run_logs_subprocess.TimeoutExpired:
        try:
          child.kill()
        except ProcessLookupError:
          pass
        child.wait()

  @staticmethod
  def _writers_closed(pipe):
    poller = _run_logs_select.poll()
    poller.register(pipe, _run_logs_select.POLLHUP)
    return any(events & _run_logs_select.POLLHUP for _, events in poller.poll(0))

  def run(self, argv, *, cwd=None, env=None, capture_stdout=False):
    """Return the real child status, capturing only private helper stdout on request."""
    if self._closed:
      raise ValueError("run logs are closed")
    child_env = dict(_run_logs_os.environ if env is None else env)
    child_env.setdefault("PYTHONUNBUFFERED", "1")
    child = _run_logs_subprocess.Popen(
      argv, cwd=cwd, env=child_env,
      stdout=_run_logs_subprocess.PIPE, stderr=_run_logs_subprocess.PIPE,
    )
    captured = bytearray() if capture_stdout else None
    selector = None
    deadline = None
    try:
      selector = _run_logs_selectors.DefaultSelector()
      for name, pipe in (("stdout", child.stdout), ("stderr", child.stderr)):
        _run_logs_os.set_blocking(pipe.fileno(), False)
        selector.register(pipe, _run_logs_selectors.EVENT_READ, name)
      while selector.get_map():
        if child.poll() is not None and deadline is None:
          deadline = _run_logs_time.monotonic() + self._drain_grace
        timeout = 0.05
        if deadline is not None:
          timeout = min(timeout, max(0, deadline - _run_logs_time.monotonic()))
        events = selector.select(timeout)
        for key, _ in events:
          try:
            data = _run_logs_os.read(key.fileobj.fileno(), 65536)
          except BlockingIOError:
            continue
          except OSError as error:
            self._record_error(key.data + " pipe read failed: " + str(error))
            data = b""
          if not data:
            selector.unregister(key.fileobj)
            key.fileobj.close()
          elif key.data == "stdout" and capture_stdout:
            captured.extend(data)
          else:
            self._log_and_tee(key.data, data)
        if deadline is not None and _run_logs_time.monotonic() >= deadline:
          # A descendant may inherit a pipe after the direct child has exited.
          # Bound that wait, but keep draining closed pipes' buffered output.
          for key in tuple(selector.get_map().values()):
            if not self._writers_closed(key.fileobj):
              self._record_error(key.data + " pipe remained open after child exited; stopped draining")
              selector.unregister(key.fileobj)
              key.fileobj.close()
      returncode = child.wait()
    except BaseException:
      self._stop_child(child)
      raise
    finally:
      if selector is not None:
        selector.close()
      for pipe in (child.stdout, child.stderr):
        pipe.close()
    return RunLogResult(returncode, bytes(captured) if captured is not None else b"", self.log_error)
