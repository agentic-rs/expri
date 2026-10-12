"""Process-level fallback supervision tests without installing an environment."""

import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import jobs
import run_lifecycle
import run_logs
import system_capture


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
    self.storage_env = patch.dict(os.environ, {"EXPRI_STORAGE_DIR": str(self.repo / "profiles")})
    self.storage_env.start()
    self.addCleanup(self.storage_env.stop)
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

  def test_disabled_publishing_preserves_ordinary_python_fallback(self):
    self.request["service"] = {
      "project_id": "vision", "origin": "gpu-1", "publish": False,
      "client_config": "/missing/credentials-are-unused.toml",
    }
    self.start()
    state = self.wait_terminal()
    self.assertEqual(state["status"], "completed", state)

  def profile_identity(self, machine_id="machine-fixture"):
    profiles = self.repo / "profiles"
    profiles.mkdir()
    database = profiles / "profiles.sqlite3"
    connection = sqlite3.connect(database)
    try:
      connection.execute("CREATE TABLE system_local(singleton INTEGER PRIMARY KEY, machine_id TEXT)")
      connection.execute("INSERT INTO system_local VALUES(1, ?)", (machine_id,))
      connection.commit()
    finally:
      connection.close()
    return database

  def capture_profile(self):
    database = self.profile_identity()
    connection = sqlite3.connect(database)
    try:
      connection.executescript("""
        ALTER TABLE system_local ADD COLUMN sequence INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE system_local ADD COLUMN inventory_revision INTEGER NOT NULL DEFAULT 0;
        CREATE TABLE system_samples(machine_id TEXT, sequence INTEGER, observed_at_ms INTEGER, inventory_revision INTEGER, payload TEXT);
        CREATE TABLE system_inventories(machine_id TEXT, revision INTEGER, payload TEXT);
        CREATE TABLE system_local_quarantine(machine_id TEXT, kind TEXT, record_key INTEGER);
      """)
    finally:
      connection.close()
    return database

  def test_foreground_and_detached_runs_finalize_optional_empty_capture(self):
    self.capture_profile()
    for detached in (False, True):
      with self.subTest(detached=detached):
        self.request["detach"] = detached
        (self.repo / "task.py").write_text("print('task output')")
        receipt = run_lifecycle.start_run(str(self.repo), self.request, fixture_context(), worker_source())
        if detached:
          self.run = Path(receipt["run_dir"])
          state = self.wait_terminal()
        else:
          self.assertEqual(receipt, 0)
          runs = sorted((self.repo / ".expri/runs").iterdir(), key=lambda path: path.stat().st_mtime_ns)
          self.run = runs[-1]
          state = self.state()
        self.assertEqual(state["status"], "completed", state)
        self.assertEqual(state["system_capture_status"], "complete", state)
        records = [json.loads(line) for line in (self.run / "outputs/system-profile.jsonl").read_text().splitlines()]
        self.assertEqual(records[0]["kind"], "capture")
        self.assertEqual(records[0]["capture_id"], state["system_capture_id"])
        self.assertEqual(records[0]["start_ms"], system_capture._capture_time(state["started_at"]))
        self.assertEqual(records[-1], {"kind": "end", "end_ms": system_capture._capture_time(state["finished_at"]), "complete": True, "warnings": []})

  def test_capture_io_failure_preserves_pending_state_for_recovery(self):
    self.capture_profile()
    run = self.repo / "run-capture"
    (run / "outputs").mkdir(parents=True)
    state = {"run_id": run.name, "machine_id": "machine-fixture", "started_at": "1970-01-01T00:00:01Z", "finished_at": "1970-01-01T00:00:02Z"}
    system_capture._capture_begin(run, state)
    with patch.object(system_capture, "_capture_checkpoint", side_effect=OSError("temporary disk failure")):
      system_capture._capture_close(run, state)
    self.assertEqual(state["system_capture_status"], "capturing")
    self.assertIn("system_capture_error", state)
    system_capture._capture_close(run, state)
    self.assertEqual(state["system_capture_status"], "complete")
    records = [json.loads(line) for line in (run / "outputs/system-profile.jsonl").read_text().splitlines()]
    self.assertEqual([record["kind"] for record in records], ["capture", "end"])

  def test_capture_preserves_user_output_and_refuses_unsupported_live_collector(self):
    import fcntl
    database = self.capture_profile()
    run = self.repo / "run-capture"
    (run / "outputs").mkdir(parents=True)
    output = run / "outputs/system-profile.jsonl"
    output.write_text("user data")
    state = {"run_id": run.name, "machine_id": "machine-fixture", "started_at": "1970-01-01T00:00:01Z"}
    system_capture._capture_begin(run, state)
    self.assertNotIn("system_capture_id", state)
    self.assertEqual(output.read_text(), "user data")
    output.unlink()
    with (database.parent / "collector.lock").open("wb") as lease:
      fcntl.flock(lease, fcntl.LOCK_EX)
      system_capture._capture_begin(run, state)
    self.assertNotIn("system_capture_id", state)
    self.assertFalse(output.exists())

  def test_capture_fsync_does_not_hold_the_independent_collector_write_lock(self):
    database = self.capture_profile()
    run = self.repo / "run-capture"
    (run / "outputs").mkdir(parents=True)
    state = {"run_id": run.name, "machine_id": "machine-fixture", "started_at": "1970-01-01T00:00:01Z", "finished_at": "1970-01-01T00:00:02Z"}
    system_capture._capture_begin(run, state)
    stream_identity = (run / "outputs/system-profile.jsonl").stat().st_ino
    fsync = system_capture._capture_os.fsync
    collector_writes = []
    def inspect_fsync(descriptor):
      if os.fstat(descriptor).st_ino == stream_identity:
        connection = sqlite3.connect(database, timeout=0)
        try:
          connection.execute("UPDATE system_local SET inventory_revision=inventory_revision")
          connection.commit()
          collector_writes.append(True)
        finally:
          connection.close()
      return fsync(descriptor)
    with patch.object(system_capture._capture_os, "fsync", side_effect=inspect_fsync):
      system_capture._capture_close(run, state)
    self.assertEqual(state["system_capture_status"], "complete", state)
    self.assertGreaterEqual(len(collector_writes), 2)

  def test_capture_retains_exact_window_and_counters_without_experiment_steps(self):
    database = self.capture_profile()
    inventory = {"revision": 1, "inventory": {"observed_at_ms": 999, "hostname": "host", "os_name": None, "os_version": None, "kernel_version": None, "architecture": "test", "cpu": {"brand": "cpu", "vendor": "test", "logical_cpu_count": 1, "physical_core_count": 1}, "memory_total_bytes": 1024, "measurement_scope": "whole machine", "devices": [], "warnings": []}}
    connection = sqlite3.connect(database)
    try:
      connection.execute("INSERT INTO system_inventories VALUES(?,?,?)", ("machine-fixture", 1, json.dumps(inventory)))
      for sequence, observed_at in enumerate([999, 1000, 1999, 2000], 1):
        sample = {"sequence": sequence, "session_id": "session-test", "observed_at_ms": observed_at, "interval_ms": 10, "collection_duration_ms": 1, "inventory_revision": 1, "metrics": {"cpu/usage_percent": {"kind": "gauge", "value": 30}}, "counters": {"network/test/received_bytes": "18446744073709551615"}, "warnings": []}
        connection.execute("INSERT INTO system_samples VALUES(?,?,?,?,?)", ("machine-fixture", sequence, observed_at, 1, json.dumps(sample)))
      connection.commit()
    finally:
      connection.close()
    run = self.repo / "run-capture"
    (run / "outputs").mkdir(parents=True)
    metrics = run / "outputs/metrics.jsonl"
    metrics.write_text('{"step":3,"loss":0.5}\n')
    state = {"run_id": run.name, "machine_id": "machine-fixture", "started_at": "1970-01-01T00:00:01Z", "finished_at": "1970-01-01T00:00:02Z"}
    system_capture._capture_begin(run, state)
    system_capture._capture_close(run, state)
    self.assertEqual(state["system_capture_status"], "complete", state)
    records = [json.loads(line) for line in (run / "outputs/system-profile.jsonl").read_text().splitlines()]
    samples = [record["sample"] for record in records if record["kind"] == "sample"]
    self.assertEqual([sample["observed_at_ms"] for sample in samples], [1000, 1999])
    self.assertEqual(samples[0]["counters"]["network/test/received_bytes"], "18446744073709551615")
    self.assertTrue(all("step" not in sample for sample in samples))
    self.assertEqual(metrics.read_text(), '{"step":3,"loss":0.5}\n')

  def test_foreground_and_detached_runs_preserve_existing_machine_identity(self):
    database = self.profile_identity()
    before = database.read_bytes()
    for detached in (False, True):
      with self.subTest(detached=detached):
        self.request["detach"] = detached
        (self.repo / "task.py").write_text("print('task output')")
        receipt = run_lifecycle.start_run(str(self.repo), self.request, fixture_context(), worker_source())
        if detached:
          self.run = Path(receipt["run_dir"])
          state = self.wait_terminal()
        else:
          self.assertEqual(receipt, 0)
          self.run = next((self.repo / ".expri/runs").iterdir())
          state = self.state()
        self.assertEqual(state["machine_id"], "machine-fixture")
        self.assertEqual(state["status"], "completed")
        self.assertEqual(database.read_bytes(), before)
        self.assertEqual(list(database.parent.iterdir()), [database])

  def test_missing_profile_storage_is_not_created_and_does_not_stop_runs(self):
    self.start()
    state = self.wait_terminal()
    self.assertEqual(state["status"], "completed")
    self.assertNotIn("machine_id", state)
    self.assertFalse((self.repo / "profiles").exists())

  def test_invalid_profile_storage_and_missing_sqlite_support_do_not_stop_runs(self):
    profiles = self.repo / "profiles"
    profiles.mkdir()
    database = profiles / "profiles.sqlite3"
    database.write_bytes(b"not a database")
    self.assertIsNone(run_lifecycle._lifecycle_existing_machine_id())
    self.assertEqual(database.read_bytes(), b"not a database")
    self.start()
    state = self.wait_terminal()
    self.assertEqual(state["status"], "completed")
    self.assertNotIn("machine_id", state)
    with patch.dict(sys.modules, {"sqlite3": None}):
      self.assertIsNone(run_lifecycle._lifecycle_existing_machine_id())

  def test_profile_identity_validation_rejects_non_component_names(self):
    database = self.profile_identity()
    for machine_id in ("", ".", "..", "x" * 97, "host/name", "é", "host\nname", None):
      with self.subTest(machine_id=machine_id):
        connection = sqlite3.connect(database)
        try:
          connection.execute("UPDATE system_local SET machine_id=?", (machine_id,))
          connection.commit()
        finally:
          connection.close()
        self.assertIsNone(run_lifecycle._lifecycle_existing_machine_id())

  def test_profile_lookup_rejects_database_owned_directory_and_sidecar_symlinks(self):
    database = self.profile_identity()
    original = self.repo / "original.sqlite3"
    database.replace(original)
    database.symlink_to(original)
    self.assertIsNone(run_lifecycle._lifecycle_existing_machine_id())
    database.unlink()
    original.replace(database)
    sidecar = database.with_name(database.name + "-wal")
    sidecar.symlink_to(database)
    self.assertIsNone(run_lifecycle._lifecycle_existing_machine_id())
    sidecar.unlink()
    actual = self.repo / "actual"
    database.parent.rename(actual)
    (self.repo / "profiles").symlink_to(actual, target_is_directory=True)
    self.assertIsNone(run_lifecycle._lifecycle_existing_machine_id())

  def test_profile_lookup_uses_home_default_and_allows_aliases_above_owned_boundary(self):
    database = self.profile_identity()
    home = self.repo / "home"
    (home / ".tokn").mkdir(parents=True)
    database.parent.rename(home / ".tokn/expri")
    alias = self.repo / "home-alias"
    alias.symlink_to(home, target_is_directory=True)
    with patch.dict(os.environ, {"HOME": str(alias)}):
      with patch.dict(os.environ):
        os.environ.pop("EXPRI_STORAGE_DIR", None)
        self.assertEqual(run_lifecycle._lifecycle_existing_machine_id(), "machine-fixture")
    actual = home / ".tokn/expri"
    outside = self.repo / "outside"
    actual.rename(outside)
    actual.symlink_to(outside, target_is_directory=True)
    with patch.dict(os.environ, {"HOME": str(home)}):
      with patch.dict(os.environ):
        os.environ.pop("EXPRI_STORAGE_DIR", None)
        self.assertIsNone(run_lifecycle._lifecycle_existing_machine_id())

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
