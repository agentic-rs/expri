"""Local subprocess regressions for binary run logging and live terminal output."""

import io
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import run_logs


class RecordingSink(io.BytesIO):
  def __init__(self, marker=b""):
    super().__init__()
    self.marker = marker
    self.received = threading.Event()

  def write(self, data):
    result = super().write(data)
    if self.marker in self.getvalue():
      self.received.set()
    return result


class BrokenPipeSink:
  def __init__(self):
    self.writes = 0

  def write(self, data):
    self.writes += 1
    raise BrokenPipeError("terminal disconnected")


class SlowSink(io.BytesIO):
  def write(self, data):
    time.sleep(0.02)
    return super().write(data)


class FailingLog:
  def __init__(self, wrapped):
    self.wrapped = wrapped

  def write(self, data):
    raise OSError("disk full fixture")

  def close(self):
    self.wrapped.close()


def python(code, *args):
  return [sys.executable, "-B", "-c", code, *map(str, args)]


class RunLogsTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory(prefix="expri-run-logs-tests-")
    self.addCleanup(self.temporary.cleanup)
    self.run_dir = Path(self.temporary.name)
    self.stdout = io.BytesIO()
    self.stderr = io.BytesIO()

  def logs(self, **kwargs):
    return run_logs.RunLogs(
      self.run_dir, stdout_sink=self.stdout, stderr_sink=self.stderr, **kwargs,
    )

  def read_log(self, name):
    return (self.run_dir / "logs" / (name + ".log")).read_bytes()

  def test_binary_output_is_logged_and_teed_without_task_capture(self):
    stdout = bytes(range(256)) * 100
    stderr = b"\xff\x00stderr\r\n" * 100
    with self.logs() as logs:
      result = logs.run(python(
        "import os; os.write(1, bytes(range(256)) * 100); os.write(2, b'\\xff\\x00stderr\\r\\n' * 100)",
      ))
    self.assertEqual(result.returncode, 0)
    self.assertEqual(result.stdout, b"")
    self.assertIsNone(result.log_error)
    self.assertEqual(self.read_log("stdout"), stdout)
    self.assertEqual(self.read_log("stderr"), stderr)
    self.assertEqual(self.stdout.getvalue(), stdout)
    self.assertEqual(self.stderr.getvalue(), stderr)

  def test_private_helper_stdout_is_neither_logged_nor_teed(self):
    with self.logs() as logs:
      prepared = logs.run(python(
        "import os; os.write(1, b'{\"secret\": 3}'); os.write(2, b'prepare stderr\\n')",
      ), capture_stdout=True)
      task = logs.run(python(
        "import os; os.write(1, b'task stdout\\n'); os.write(2, b'task stderr\\n')",
      ))
    self.assertEqual(prepared.stdout, b'{"secret": 3}')
    self.assertEqual(task.stdout, b"")
    self.assertEqual(self.read_log("stdout"), b"task stdout\n")
    self.assertEqual(self.stdout.getvalue(), b"task stdout\n")
    self.assertEqual(self.read_log("stderr"), b"prepare stderr\ntask stderr\n")
    self.assertEqual(self.stderr.getvalue(), self.read_log("stderr"))

  def test_output_is_live_before_child_exits(self):
    gate = self.run_dir / "gate"
    self.stdout = RecordingSink(b"stdout ready\n")
    self.stderr = RecordingSink(b"stderr ready\n")
    results = []
    failures = []
    code = """
import pathlib
import sys
import time

print("stdout ready")
print("stderr ready", file=sys.stderr)
gate = pathlib.Path(sys.argv[1])
deadline = time.monotonic() + 5
while not gate.exists():
  if time.monotonic() > deadline:
    sys.exit(9)
  time.sleep(0.01)
print("after gate")
"""
    with self.logs() as logs:
      def execute():
        try:
          results.append(logs.run(python(code, gate)))
        except BaseException as error:
          failures.append(error)

      worker = threading.Thread(target=execute)
      worker.start()
      try:
        self.assertTrue(self.stdout.received.wait(3), "stdout was not streamed")
        self.assertTrue(self.stderr.received.wait(3), "stderr was not streamed")
        self.assertTrue(worker.is_alive(), "child exited before gate release")
        self.assertEqual(self.read_log("stdout"), b"stdout ready\n")
        self.assertEqual(self.read_log("stderr"), b"stderr ready\n")
      finally:
        gate.touch()
        worker.join(6)
      self.assertFalse(worker.is_alive())
    self.assertFalse(failures, failures)
    self.assertEqual(results[0].returncode, 0)
    self.assertEqual(self.read_log("stdout"), b"stdout ready\nafter gate\n")

  def test_large_output_on_both_streams_does_not_deadlock(self):
    code = """
import os

for _ in range(32):
  os.write(1, b"a" * 65536)
for _ in range(32):
  os.write(2, b"b" * 65536)
"""
    with self.logs() as logs:
      result = logs.run(python(code))
    self.assertEqual(result.returncode, 0)
    self.assertEqual(self.read_log("stdout"), b"a" * 2097152)
    self.assertEqual(self.read_log("stderr"), b"b" * 2097152)

  def test_disconnected_terminal_does_not_break_logs(self):
    self.stdout = BrokenPipeSink()
    self.stderr = BrokenPipeSink()
    code = """
import os

for _ in range(32):
  os.write(1, b"a" * 65536)
  os.write(2, b"b" * 65536)
"""
    with self.logs() as logs:
      result = logs.run(python(code))
    self.assertEqual(result.returncode, 0)
    self.assertIsNone(result.log_error)
    self.assertEqual(self.read_log("stdout"), b"a" * 2097152)
    self.assertEqual(self.read_log("stderr"), b"b" * 2097152)
    self.assertEqual(self.stdout.writes, 1)
    self.assertEqual(self.stderr.writes, 1)

  def test_log_failure_still_drains_and_retains_child_status(self):
    code = """
import os
import sys

for _ in range(32):
  os.write(1, b"a" * 65536)
  os.write(2, b"b" * 65536)
sys.exit(7)
"""
    with self.logs() as logs:
      logs._files["stdout"] = FailingLog(logs._files["stdout"])
      result = logs.run(python(code))
    self.assertEqual(result.returncode, 7)
    self.assertEqual(result.stdout, b"")
    self.assertIn("stdout log write failed: disk full fixture", result.log_error)
    self.assertEqual(logs.log_error, result.log_error)
    self.assertEqual(self.stdout.getvalue(), b"a" * 2097152)
    self.assertEqual(self.read_log("stderr"), b"b" * 2097152)

  def test_nonzero_exit_is_preserved(self):
    with self.logs() as logs:
      result = logs.run(python("import sys; print('failed'); sys.exit(23)"))
    self.assertEqual(result.returncode, 23)
    self.assertIsNone(result.log_error)
    self.assertEqual(self.read_log("stdout"), b"failed\n")

  def test_signal_exit_is_preserved(self):
    with self.logs() as logs:
      result = logs.run(python(
        "import os, signal; os.write(2, b'before signal\\n'); os.kill(os.getpid(), signal.SIGTERM)",
      ))
    self.assertEqual(result.returncode, -signal.SIGTERM)
    self.assertEqual(self.read_log("stderr"), b"before signal\n")

  def test_configured_python_buffering_is_preserved(self):
    environment = os.environ.copy()
    environment["PYTHONUNBUFFERED"] = "configured-value"
    with self.logs() as logs:
      result = logs.run(python(
        "import os; print(os.environ['PYTHONUNBUFFERED'])",
      ), env=environment)
    self.assertEqual(result.returncode, 0)
    self.assertEqual(self.read_log("stdout"), b"configured-value\n")
    self.assertEqual(environment["PYTHONUNBUFFERED"], "configured-value")

  def test_descendant_with_inherited_pipes_does_not_delay_result(self):
    pid_file = self.run_dir / "descendant.pid"
    code = """
import pathlib
import subprocess
import sys

child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(20)"])
pathlib.Path(sys.argv[1]).write_text(str(child.pid))
print("parent done")
"""
    started = time.monotonic()
    try:
      with self.logs(drain_grace=0.1) as logs:
        result = logs.run(python(code, pid_file))
      self.assertLess(time.monotonic() - started, 3)
      self.assertEqual(result.returncode, 0)
      self.assertEqual(self.read_log("stdout"), b"parent done\n")
      self.assertIn("pipe remained open after child exited", result.log_error)
    finally:
      if pid_file.exists():
        try:
          os.kill(int(pid_file.read_text()), signal.SIGTERM)
        except ProcessLookupError:
          pass

  def test_slow_terminal_keeps_all_closed_pipe_output_after_grace(self):
    self.stdout = SlowSink()
    self.stderr = SlowSink()
    code = "import os; os.write(1, b'a' * 8192); os.write(2, b'b' * 8192)"
    real_read = os.read

    def short_read(descriptor, count):
      # Short reads are legal even when a closed pipe still has buffered bytes.
      return real_read(descriptor, min(count, 1024))

    with self.logs(drain_grace=0.001) as logs:
      with patch.object(run_logs._run_logs_os, "read", short_read):
        result = logs.run(python(code))
    self.assertEqual(result.returncode, 0)
    self.assertIsNone(result.log_error)
    self.assertEqual(self.read_log("stdout"), b"a" * 8192)
    self.assertEqual(self.read_log("stderr"), b"b" * 8192)
    self.assertEqual(self.stdout.getvalue(), self.read_log("stdout"))
    self.assertEqual(self.stderr.getvalue(), self.read_log("stderr"))

  def test_existing_logs_are_not_overwritten(self):
    with self.logs():
      with self.assertRaises(FileExistsError):
        self.logs()

  def test_spawn_failure_remains_exception_and_files_close(self):
    with self.assertRaises(FileNotFoundError):
      with self.logs() as logs:
        logs.run([str(self.run_dir / "missing-command")])
    self.assertTrue(logs._closed)
    self.assertEqual(logs._files, {})

  def test_unexpected_drain_failure_terminates_and_reaps_own_child(self):
    children = []
    real_popen = subprocess.Popen

    def remember_child(*args, **kwargs):
      child = real_popen(*args, **kwargs)
      children.append(child)
      return child

    with self.logs() as logs:
      with patch.object(run_logs._run_logs_subprocess, "Popen", remember_child):
        with patch.object(run_logs._run_logs_os, "set_blocking", side_effect=RuntimeError("drain fixture")):
          with self.assertRaisesRegex(RuntimeError, "drain fixture"):
            logs.run(python("import time; time.sleep(20)"))
    self.assertEqual(len(children), 1)
    self.assertIsNotNone(children[0].returncode)
    self.assertTrue(children[0].stdout.closed)
    self.assertTrue(children[0].stderr.closed)

  def test_selector_creation_failure_terminates_and_reaps_own_child(self):
    children = []
    real_popen = subprocess.Popen

    def remember_child(*args, **kwargs):
      child = real_popen(*args, **kwargs)
      children.append(child)
      return child

    with self.logs() as logs:
      with patch.object(run_logs._run_logs_subprocess, "Popen", remember_child):
        with patch.object(run_logs._run_logs_selectors, "DefaultSelector", side_effect=RuntimeError("selector fixture")):
          with self.assertRaisesRegex(RuntimeError, "selector fixture"):
            logs.run(python("import time; time.sleep(20)"))
    self.assertIsNotNone(children[0].returncode)
    self.assertTrue(children[0].stdout.closed)
    self.assertTrue(children[0].stderr.closed)

  def test_cancel_before_launch_does_not_start_task(self):
    cancel = self.run_dir / ".cancel-request"
    cancel.touch()
    side_effect = self.run_dir / "started"
    with self.logs(cancel_path=cancel) as logs:
      result = logs.run(python("import pathlib, sys; pathlib.Path(sys.argv[1]).touch()", side_effect))
    self.assertTrue(result.cancelled)
    self.assertEqual(result.returncode, -signal.SIGTERM)
    self.assertFalse(side_effect.exists())

  def test_cancel_is_checked_after_child_closes_output_pipes(self):
    cancel = self.run_dir / ".cancel-request"
    ready = self.run_dir / "ready"
    code = "import os, pathlib, sys, time; os.close(1); os.close(2); pathlib.Path(sys.argv[1]).touch(); time.sleep(20)"
    def request_cancel():
      deadline = time.monotonic() + 3
      while not ready.exists() and time.monotonic() < deadline:
        time.sleep(0.01)
      cancel.touch()
    requester = threading.Thread(target=request_cancel)
    requester.start()
    try:
      with self.logs(cancel_path=cancel, cancel_grace=0.1) as logs:
        result = logs.run(python(code, ready))
    finally:
      requester.join(4)
    self.assertTrue(ready.exists())
    self.assertTrue(result.cancelled)
    self.assertEqual(result.returncode, -signal.SIGTERM)

  def test_cancel_escalates_for_descendants_after_direct_child_exits(self):
    cancel = self.run_dir / ".cancel-request"
    ready = self.run_dir / "ready"
    descendant_code = "import pathlib, signal, sys, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); pathlib.Path(sys.argv[1]).touch(); print('descendant ready', flush=True); time.sleep(20)"
    code = "import subprocess, sys, time; subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]]); time.sleep(20)"
    def request_cancel():
      deadline = time.monotonic() + 3
      while not ready.exists() and time.monotonic() < deadline:
        time.sleep(0.01)
      cancel.touch()
    requester = threading.Thread(target=request_cancel)
    requester.start()
    started = time.monotonic()
    try:
      with self.logs(cancel_path=cancel, cancel_grace=0.2) as logs:
        result = logs.run(python(code, descendant_code, ready))
    finally:
      requester.join(4)
    self.assertTrue(result.cancelled)
    self.assertEqual(result.returncode, -signal.SIGTERM)
    self.assertIsNone(result.log_error)
    self.assertLess(time.monotonic() - started, 3)
    self.assertEqual(self.read_log("stdout"), b"descendant ready\n")

  def test_detached_logging_does_not_mirror_to_terminal(self):
    with self.logs(mirror=False) as logs:
      result = logs.run(python("import os; os.write(1, b'output'); os.write(2, b'error')"))
    self.assertEqual(result.returncode, 0)
    self.assertEqual(self.stdout.getvalue(), b"")
    self.assertEqual(self.stderr.getvalue(), b"")
    self.assertEqual(self.read_log("stdout"), b"output")
    self.assertEqual(self.read_log("stderr"), b"error")

  def test_cancel_during_natural_exit_drain_stops_surviving_descendant(self):
    cancel = self.run_dir / ".cancel-request"
    ready = self.run_dir / "ready"
    observed_exit = threading.Event()
    descendant_code = "import pathlib, signal, sys, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); pathlib.Path(sys.argv[1]).touch(); print('descendant ready', flush=True); time.sleep(20)"
    code = "import pathlib, subprocess, sys, time; subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]]); exec('while not pathlib.Path(sys.argv[2]).exists():\\n  time.sleep(0.01)'); print('parent done', flush=True)"
    real_exited = run_logs._RunChildExit.exited
    def remember_exit(observer):
      done = real_exited(observer)
      if done:
        observed_exit.set()
      return done
    def request_cancel():
      if observed_exit.wait(3):
        cancel.touch()
    requester = threading.Thread(target=request_cancel)
    requester.start()
    try:
      with self.logs(cancel_path=cancel, cancel_grace=0.2, drain_grace=1) as logs:
        with patch.object(run_logs._RunChildExit, "exited", remember_exit):
          result = logs.run(python(code, descendant_code, ready))
    finally:
      requester.join(4)
    self.assertTrue(observed_exit.is_set())
    self.assertTrue(result.cancelled)
    self.assertEqual(result.returncode, 0)
    self.assertIsNone(result.log_error)
    self.assertIn(b"descendant ready\n", self.read_log("stdout"))
    self.assertIn(b"parent done\n", self.read_log("stdout"))


if __name__ == "__main__":
  unittest.main()
