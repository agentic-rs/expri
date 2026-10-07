"""Process-level fallback supervision tests without installing an environment."""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import jobs
import run_lifecycle
import run_logs


SOURCE_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SOURCE_DIR / "environment"))
import maintenance
import snapshot


def fixture_prepare(environment, repo_root, state_dir, *_args, process_runner, **_kwargs):
  code = environment.get("prep_code", "print('prepared')")
  result = process_runner([sys.executable, "-B", "-c", code], capture_stdout=True)
  if result.returncode:
    raise subprocess.CalledProcessError(result.returncode, [sys.executable])
  return {
    "manifest_path": str(Path(state_dir) / "environment/environment-state.json"),
    "environment_path": str(Path(state_dir) / "environment/.venv"), "python": sys.executable,
  }


def fixture_context():
  return {
    "create_snapshot": snapshot.create_snapshot, "acquire_run_lock": maintenance.acquire_run_lock,
    "RunLogs": run_logs.RunLogs, "ENV_REMOVE": [], "prepare_environment": fixture_prepare,
    "environment_cache_dir": lambda root, _args: str(Path(root) / ".expri/cache/uv"),
  }


def worker_source():
  return (
    "import json, pathlib, sys\n"
    + "sys.path.insert(0, " + repr(str(SOURCE_DIR)) + ")\n"
    + "import run_lifecycle, run_lifecycle_tests\n"
    + "payload = json.loads(pathlib.Path(sys.argv[1]).read_text())\n"
    + "raise SystemExit(run_lifecycle.execute_worker(payload, run_lifecycle_tests.fixture_context()))\n"
  )


class RunLifecycleTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory(prefix="expri-run-lifecycle-tests-")
    self.repo = Path(self.temporary.name).resolve()
    (self.repo / ".expri").mkdir()
    (self.repo / ".expri/checkout.manifest").write_text("task.py\n")
    self.bin = self.repo / "bin"
    self.bin.mkdir()
    uv = self.bin / "uv"
    uv.write_text("#!" + sys.executable + "\nimport os, sys\nargv = sys.argv[sys.argv.index('--') + 1:]\nos.execvp(argv[0], argv)\n")
    uv.chmod(0o700)
    self.request = {
      "name": "train", "command": [sys.executable, "task.py"], "detach": True,
      "environment": {"env": {"PATH": str(self.bin) + os.pathsep + os.environ["PATH"]}},
    }
    self.run = None
    self.addCleanup(self.cleanup)

  def cleanup(self):
    if self.run is not None:
      try:
        state = self.state()
        if state["status"] in ("preparing", "running"):
          (self.run / ".cancel-request").touch()
          self.wait_terminal()
      except (OSError, ValueError, AssertionError):
        pass
    self.temporary.cleanup()

  def start(self, code="print('task output')", **kwargs):
    (self.repo / "task.py").write_text(code)
    receipt = run_lifecycle.start_run(str(self.repo), self.request, fixture_context(), worker_source(), **kwargs)
    self.run = Path(receipt["run_dir"])
    return receipt

  def test_publishing_requires_native_worker_before_snapshot(self):
    self.request["service"] = {"project_id": "vision", "origin": "gpu-1"}
    with self.assertRaisesRegex(ValueError, "run-publishing-v1"):
      self.start()
    self.assertFalse((self.repo / ".expri/runs").exists())

  def state(self):
    return json.loads((self.run / "run-state.json").read_text())

  def wait_terminal(self, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
      state = self.state()
      if state["status"] not in ("preparing", "running"):
        return state
      time.sleep(0.02)
    self.fail("worker did not publish terminal state")

  def wait_file(self, path, timeout=3):
    deadline = time.monotonic() + timeout
    while not path.exists() and time.monotonic() < deadline:
      time.sleep(0.02)
    self.assertTrue(path.exists(), str(path) + " was not created")

  def test_detached_acknowledges_before_preparation_finishes_with_live_lease(self):
    gate = self.repo / "prep-gate"
    self.request["environment"]["prep_code"] = (
      "import pathlib, time; gate = pathlib.Path(" + repr(str(gate)) + "); "
      + "exec('while not gate.exists():\\n  time.sleep(0.01)'); print('prepared')"
    )
    started = time.monotonic()
    receipt = self.start("import os, pathlib; pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'], 'result').write_text('finished')")
    self.assertLess(time.monotonic() - started, 2)
    self.assertTrue(receipt["detached"])
    self.assertEqual(receipt["status"], "preparing")
    self.assertTrue((self.run / ".worker-ready").exists())
    self.assertTrue((self.run / "logs/stdout.log").exists())
    report = jobs.execute_job(self.repo, {"operation": "status", "run_id": self.run.name})
    self.assertTrue(report["alive"])
    self.assertEqual((self.run / "code/task.py").read_text(), (self.repo / "task.py").read_text())
    gate.touch()
    state = self.wait_terminal()
    self.assertEqual(state["status"], "completed", state)
    self.assertEqual((self.run / "outputs/result").read_text(), "finished")

  def test_worker_survives_parent_exit_and_session_hangup(self):
    gate = self.repo / "gate"
    task = "import os, pathlib, time; gate = pathlib.Path(" + repr(str(gate)) + "); print('ready', flush=True); exec('while not gate.exists():\\n  time.sleep(0.01)'); pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'], 'done').touch()"
    (self.repo / "task.py").write_text(task)
    launcher = (
      "import json, sys; sys.path.insert(0, " + repr(str(SOURCE_DIR)) + "); "
      + "import run_lifecycle, run_lifecycle_tests; "
      + "report = run_lifecycle.start_run(" + repr(str(self.repo)) + ", " + repr(self.request)
      + ", run_lifecycle_tests.fixture_context(), run_lifecycle_tests.worker_source()); "
      + "print(json.dumps(report), flush=True); import time; time.sleep(20)"
    )
    parent = subprocess.Popen([sys.executable, "-B", "-c", launcher], stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, start_new_session=True)
    try:
      receipt = json.loads(parent.stdout.readline())
      self.run = Path(receipt["run_dir"])
      os.killpg(parent.pid, signal.SIGHUP)
      parent.wait(timeout=3)
      self.assertEqual(parent.returncode, -signal.SIGHUP)
      report = jobs.execute_job(self.repo, {"operation": "status", "run_id": self.run.name})
      self.assertTrue(report["alive"])
      gate.touch()
      state = self.wait_terminal()
      self.assertEqual(state["status"], "completed", state)
      self.assertTrue((self.run / "outputs/done").exists())
      self.assertEqual((self.run / "logs/stdout.log").read_bytes(), b"ready\n")
    finally:
      if parent.poll() is None:
        parent.kill()
        parent.wait()
      parent.stdout.close()

  def test_cancel_during_preparation_prevents_task_launch(self):
    self.request["environment"]["prep_code"] = "import time; time.sleep(20)"
    self.start("import os, pathlib; pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'], 'unexpected').touch()")
    report = jobs.execute_job(self.repo, {"operation": "cancel", "run_id": self.run.name})
    self.assertTrue(report["cancel_requested"])
    state = self.wait_terminal()
    self.assertEqual(state["status"], "cancelled", state)
    self.assertEqual(state["exit_code"], 130)
    self.assertNotIn("task_exit_code", state)
    self.assertFalse((self.run / "outputs/unexpected").exists())

  def test_cancel_running_task_preserves_actual_child_exit_code(self):
    self.start("import os, pathlib, time; pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'], 'ready').touch(); print('running', flush=True); time.sleep(20)")
    self.wait_file(self.run / "outputs/ready")
    jobs.execute_job(self.repo, {"operation": "cancel", "run_id": self.run.name})
    state = self.wait_terminal()
    self.assertEqual(state["status"], "cancelled", state)
    self.assertEqual(state["exit_code"], 130)
    self.assertEqual(state["task_exit_code"], 143)
    self.assertEqual((self.run / "logs/stdout.log").read_bytes(), b"running\n")

  def test_failed_task_keeps_exit_code_and_logs(self):
    self.start("import os, sys; os.write(2, b'failed\\xff\\x00'); sys.exit(7)")
    state = self.wait_terminal()
    self.assertEqual(state["status"], "failed")
    self.assertEqual(state["task_exit_code"], 7)
    self.assertEqual(state["exit_code"], 7)
    self.assertEqual((self.run / "logs/stderr.log").read_bytes(), b"failed\xff\x00")

  def test_spawn_failure_records_failure_without_readiness(self):
    (self.repo / "task.py").write_text("pass")
    with patch.object(run_lifecycle._lifecycle_subprocess, "Popen", side_effect=OSError("spawn fixture")):
      with self.assertRaisesRegex(OSError, "spawn fixture"):
        run_lifecycle.start_run(self.repo, self.request, fixture_context(), worker_source())
    self.run = next((self.repo / ".expri/runs").iterdir())
    state = self.state()
    self.assertEqual(state["status"], "failed")
    self.assertEqual(state["exit_code"], 1)
    self.assertFalse((self.run / ".worker-ready").exists())

  def test_worker_log_failure_does_not_acknowledge_readiness_or_hide_error(self):
    (self.repo / "task.py").write_text("pass")
    worker = worker_source().replace(
      "raise SystemExit(run_lifecycle.execute_worker(payload, run_lifecycle_tests.fixture_context()))",
      "context = run_lifecycle_tests.fixture_context()\n"
      + "def fail_logs(*args, **kwargs): raise OSError('log fixture failure')\n"
      + "context['RunLogs'] = fail_logs\n"
      + "raise SystemExit(run_lifecycle.execute_worker(payload, context))",
    )
    with self.assertRaisesRegex(RuntimeError, "log fixture failure"):
      run_lifecycle.start_run(self.repo, self.request, fixture_context(), worker)
    self.run = next((self.repo / ".expri/runs").iterdir())
    self.assertEqual(self.state()["status"], "failed")
    self.assertEqual(self.state()["error"], "log fixture failure")
    self.assertFalse((self.run / ".worker-ready").exists())

  def test_invalid_cancel_marker_still_publishes_failure(self):
    self.start("import os, pathlib, time; pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'], 'ready').touch(); time.sleep(20)")
    self.wait_file(self.run / "outputs/ready")
    (self.run / ".cancel-request").write_text("invalid marker")
    state = self.wait_terminal()
    self.assertEqual(state["status"], "failed", state)
    self.assertEqual(state["exit_code"], 1)
    self.assertIn("empty regular file", state["error"])

  def test_cancellation_after_final_control_point_does_not_relabel_completion(self):
    (self.repo / "task.py").write_text("print('completed')")
    worker = worker_source().replace(
      "raise SystemExit(run_lifecycle.execute_worker(payload, run_lifecycle_tests.fixture_context()))",
      "context = run_lifecycle_tests.fixture_context()\n"
      + "class EndMarker(context['RunLogs']):\n"
      + "  def close(self):\n"
      + "    super().close()\n"
      + "    if self._cancel_path is not None: self._cancel_path.touch()\n"
      + "context['RunLogs'] = EndMarker\n"
      + "raise SystemExit(run_lifecycle.execute_worker(payload, context))",
    )
    receipt = run_lifecycle.start_run(self.repo, self.request, fixture_context(), worker)
    self.run = Path(receipt["run_dir"])
    state = self.wait_terminal()
    self.assertEqual(state["status"], "completed", state)
    self.assertEqual(state["exit_code"], 0)
    self.assertEqual(state["task_exit_code"], 0)
    self.assertTrue((self.run / ".cancel-request").exists())

  def test_readiness_timeout_requests_cancel_without_starting_task(self):
    (self.repo / "task.py").write_text("import os, pathlib; pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'], 'unexpected').touch()")
    worker = "import time; time.sleep(0.2)\n" + worker_source()
    with self.assertRaisesRegex(RuntimeError, "cancellation requested"):
      run_lifecycle.start_run(self.repo, self.request, fixture_context(), worker, ready_timeout=0.01)
    self.run = next((self.repo / ".expri/runs").iterdir())
    self.assertTrue((self.run / ".cancel-request").exists())
    state = self.wait_terminal()
    self.assertEqual(state["status"], "cancelled", state)
    self.assertEqual(state["exit_code"], 130)
    self.assertNotIn("task_exit_code", state)
    self.assertFalse((self.run / "outputs/unexpected").exists())


if __name__ == "__main__":
  unittest.main()
