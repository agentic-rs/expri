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


def capture_observation(sequence, observed_at):
  return {"sequence": sequence, "session_id": "session-test", "observed_at_ms": observed_at,
    "interval_ms": 10, "collection_duration_ms": 1, "inventory_revision": 1,
    "metrics": {"cpu/usage_percent": {"kind": "gauge", "value": 30}},
    "counters": {"network/test/received_bytes": "18446744073709551615"}, "warnings": []}


def capture_inventory():
  return {"revision": 1, "inventory": {"observed_at_ms": 999, "hostname": "host", "os_name": None,
    "os_version": None, "kernel_version": None, "architecture": "test",
    "cpu": {"brand": "cpu", "vendor": "test", "logical_cpu_count": 1, "physical_core_count": 1},
    "memory_total_bytes": 1024, "measurement_scope": "whole machine", "devices": [], "warnings": []}}


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
        if state["status"] in ("preparing", "running", "waiting"):
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

  def test_disk_requirements_require_native_worker_before_snapshot(self):
    self.request["requirements"] = {"disk": [{"path": "outputs", "min_free_bytes": 100, "resume_free_bytes": 120}]}
    with self.assertRaisesRegex(ValueError, "run-requirements-v1"):
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

  def test_initial_task_does_not_inherit_parent_continuation_environment(self):
    inherited = {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "900", "EXPRI_RESUME_CHECKPOINT": "/parent/checkpoint"}
    self.request["environment"]["env"].update(inherited)
    with patch.dict(os.environ, inherited):
      self.start(
        "import json, os, pathlib\n"
        "keys = ['EXPRI_ATTEMPT', 'EXPRI_STEP_OFFSET', 'EXPRI_RESUME_CHECKPOINT']\n"
        "pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'], 'inherited.json').write_text(json.dumps({key: os.environ.get(key) for key in keys}))\n"
      )
      state = self.wait_terminal()
    self.assertEqual(state["status"], "completed", state)
    actual = json.loads((self.run / "outputs/inherited.json").read_text())
    self.assertEqual(actual, dict.fromkeys(inherited))

  def test_smoke_mark_survives_python_supervision_without_local_archival(self):
    self.request["smoke"] = True
    receipt = self.start()
    self.assertTrue(receipt["smoke"])
    self.assertTrue(receipt["run_id"].startswith("smoke-run-"))
    state = self.wait_terminal()
    self.assertEqual(state["status"], "completed", state)
    self.assertTrue(state["smoke"])
    self.assertTrue(json.loads((self.run / "snapshot.json").read_text())["smoke"])
    self.assertTrue(json.loads((self.run / "run-request.json").read_text())["smoke"])
    self.assertNotIn("archived_at", state)
    self.assertTrue((self.run / "outputs").is_dir())

  def test_smoke_failed_and_cancelled_python_runs_keep_the_mark(self):
    self.request["smoke"] = True
    self.start("raise SystemExit(7)")
    state = self.wait_terminal()
    self.assertEqual(state["status"], "failed", state)
    self.assertTrue(state["smoke"])
    self.start("import time; print('ready', flush=True); time.sleep(30)")
    self.wait_file(self.run / "logs/stdout.log")
    jobs.execute_job(self.repo, {"operation": "cancel", "run_id": self.run.name})
    state = self.wait_terminal()
    self.assertEqual(state["status"], "cancelled", state)
    self.assertTrue(state["smoke"])

  def test_smoke_marker_rejects_non_boolean_values_before_snapshot(self):
    for value in ("true", 1, None):
      self.request["smoke"] = value
      with self.assertRaisesRegex(ValueError, "smoke must be boolean"):
        self.start()
      self.assertFalse((self.repo / ".expri/runs").exists())

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

  def test_capture_selects_real_minutes_with_exact_boundaries_and_counters(self):
    database = self.capture_profile()
    inventory = capture_inventory()
    connection = sqlite3.connect(database)
    try:
      connection.execute("INSERT INTO system_inventories VALUES(?,?,?)", ("machine-fixture", 1, json.dumps(inventory)))
      for sequence, observed_at in enumerate([999, 1000, 1999, 60_000, 61_000, 62_000], 1):
        sample = capture_observation(sequence, observed_at)
        connection.execute("INSERT INTO system_samples VALUES(?,?,?,?,?)", ("machine-fixture", sequence, observed_at, 1, json.dumps(sample)))
      connection.commit()
    finally:
      connection.close()
    run = self.repo / "run-capture"
    (run / "outputs").mkdir(parents=True)
    metrics = run / "outputs/metrics.jsonl"
    metrics.write_text('{"step":3,"loss":0.5}\n')
    state = {"run_id": run.name, "machine_id": "machine-fixture", "started_at": "1970-01-01T00:00:01Z", "finished_at": "1970-01-01T00:01:02Z"}
    system_capture._capture_begin(run, state)
    system_capture._capture_close(run, state)
    self.assertEqual(state["system_capture_status"], "complete", state)
    records = [json.loads(line) for line in (run / "outputs/system-profile.jsonl").read_text().splitlines()]
    samples = [record["sample"] for record in records if record["kind"] == "sample"]
    self.assertEqual([sample["observed_at_ms"] for sample in samples], [1000, 60_000])
    self.assertTrue(all(sample["interval_ms"] == 10 for sample in samples))
    self.assertEqual(samples[0]["counters"]["network/test/received_bytes"], "18446744073709551615")
    self.assertTrue(all("step" not in sample for sample in samples))
    self.assertEqual(metrics.read_text(), '{"step":3,"loss":0.5}\n')

  def capture_fixture(self):
    self.capture_profile()
    run = self.repo / "run-capture"
    (run / "outputs").mkdir(parents=True)
    state = {"run_id": run.name, "machine_id": "machine-fixture", "started_at": "1970-01-01T00:00:01Z"}
    system_capture._capture_begin(run, state)
    self.assertEqual(state.get("system_capture_status"), "capturing", state)
    _database, connection = system_capture._capture_connection()
    self.addCleanup(connection.close)
    return run, state, connection

  def insert_capture_sample(self, connection, capture_id, sequence, time):
    # These direct inserts represent an old collector with no Python/Rust gate.
    inventory = capture_inventory()
    connection.execute("INSERT INTO system_run_capture_samples(capture_id,sequence,observed_at_ms,inventory,sample) VALUES(?,?,?,?,?)",
      (capture_id, sequence, time, json.dumps(inventory), json.dumps(capture_observation(sequence, time))))
    connection.commit()

  def test_capture_minute_ledger_survives_export_reopen_and_clock_regression(self):
    run, state, connection = self.capture_fixture()
    capture_id = state["system_capture_id"]
    self.insert_capture_sample(connection, capture_id, 1, 121_000)
    system_capture._capture_export(connection, capture_id)
    connection.close()
    _database, connection = system_capture._capture_connection()
    self.addCleanup(connection.close)
    for sequence, time in enumerate([1000, 122_000, 61_000, 1500], 2):
      self.insert_capture_sample(connection, capture_id, sequence, time)
      system_capture._capture_export(connection, capture_id)
    samples = [record["sample"] for record in map(json.loads, (run / "outputs/system-profile.jsonl").read_text().splitlines()) if record["kind"] == "sample"]
    self.assertEqual([sample["observed_at_ms"] for sample in samples], [121_000, 1000, 61_000])
    self.assertTrue(all(sample["interval_ms"] == 10 for sample in samples))

  def test_legacy_capture_migration_preserves_committed_prefix_and_pending_rows(self):
    run, state, connection = self.capture_fixture()
    capture_id = state["system_capture_id"]
    connection.executescript("DROP TRIGGER system_run_capture_minute_gate; DROP TRIGGER system_run_capture_minute_reserve;")
    for sequence, time in enumerate([1000, 1500], 1):
      self.insert_capture_sample(connection, capture_id, sequence, time)
    system_capture._capture_export(connection, capture_id)
    prefix = (run / "outputs/system-profile.jsonl").read_bytes()
    self.insert_capture_sample(connection, capture_id, 3, 61_000)
    self.insert_capture_sample(connection, capture_id, 4, 62_000)
    connection.executescript("DELETE FROM system_run_capture_policy; DELETE FROM system_run_capture_minutes;")
    connection.executescript(system_capture._CAPTURE_SCHEMA)
    system_capture._capture_export(connection, capture_id)
    self.insert_capture_sample(connection, capture_id, 5, 1800)
    self.insert_capture_sample(connection, capture_id, 6, 121_000)
    system_capture._capture_export(connection, capture_id)
    data = (run / "outputs/system-profile.jsonl").read_bytes()
    self.assertTrue(data.startswith(prefix))
    samples = [record["sample"]["observed_at_ms"] for record in map(json.loads, data.splitlines()) if record["kind"] == "sample"]
    self.assertEqual(samples, [1000, 1500, 61_000, 121_000])
    self.assertEqual(connection.execute("SELECT COUNT(*) FROM system_run_capture_minute_seed").fetchone()[0], 0)

  def test_legacy_seed_is_bounded_resumable_and_does_not_rescan_initialized_files(self):
    run, state, connection = self.capture_fixture()
    capture_id = state["system_capture_id"]
    stream = run / "outputs/system-profile.jsonl"
    with stream.open("ab") as output:
      for sequence in range(1, 5001):
        output.write(json.dumps({"kind": "sample", "sample": capture_observation(sequence, 1000 + sequence)}).encode() + b"\n")
    target = stream.stat().st_size
    connection.execute("UPDATE system_run_captures SET file_offset=?,exported_sequence=5000 WHERE capture_id=?", (target, capture_id))
    connection.commit()
    connection.executescript("DELETE FROM system_run_capture_policy;")
    connection.executescript(system_capture._CAPTURE_SCHEMA)
    row = dict(connection.execute("SELECT * FROM system_run_captures WHERE capture_id=?", (capture_id,)).fetchone())
    self.assertFalse(system_capture._capture_seed_minutes(connection, row))
    offset = connection.execute("SELECT scan_offset FROM system_run_capture_minute_seed WHERE capture_id=?", (capture_id,)).fetchone()[0]
    self.assertGreater(offset, 0)
    self.assertLess(offset, target)
    connection.close()
    _database, connection = system_capture._capture_connection()
    self.addCleanup(connection.close)
    self.assertEqual(connection.execute("SELECT scan_offset FROM system_run_capture_minute_seed WHERE capture_id=?", (capture_id,)).fetchone()[0], offset)
    self.assertTrue(system_capture._capture_seed_minutes(connection, row))
    stream.unlink()
    self.assertTrue(system_capture._capture_seed_minutes(connection, row))

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
      if state["status"] not in ("preparing", "running", "waiting"):
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

  def start_waiting_task(self, continuation):
    helper = SOURCE_DIR.parents[2] / "sdk/python/expri_run.py"
    (self.repo / "expri_run.py").write_bytes(helper.read_bytes())
    with (self.repo / ".expri/checkout.manifest").open("a") as manifest:
      manifest.write("expri_run.py\n")
    return self.start(
      "import json, os, pathlib, time\n"
      "from expri_run import set_run_status\n"
      "run = pathlib.Path(os.environ['EXPRI_RUN_DIR'])\n"
      "outputs = pathlib.Path(os.environ['EXPRI_OUTPUT_DIR'])\n"
      "set_run_status('waiting')\n"
      "outputs.joinpath('waiting-state.json').write_text(run.joinpath('run-state.json').read_text())\n"
      "print('waiting', flush=True)\n"
      + continuation,
    )

  def test_waiting_task_resumes_running_and_preserves_start_until_completion(self):
    gate = self.repo / "waiting-gate"
    self.start_waiting_task(
      "gate = pathlib.Path(" + repr(str(gate)) + ")\n"
      "deadline = time.monotonic() + 5\n"
      "while not gate.exists():\n"
      "  if time.monotonic() > deadline: raise RuntimeError('gate timeout')\n"
      "  time.sleep(0.01)\n"
      "set_run_status('running')\n"
      "outputs.joinpath('resumed-state.json').write_text(run.joinpath('run-state.json').read_text())\n"
      "print('resumed', flush=True)\n"
    )
    self.wait_file(self.run / "outputs/waiting-state.json")
    waiting = json.loads((self.run / "outputs/waiting-state.json").read_text())
    self.assertEqual(waiting["status"], "waiting")
    live = jobs.execute_job(self.repo, {"operation": "status", "run_id": self.run.name})
    self.assertEqual(live["status"], "waiting")
    self.assertTrue(live["alive"])
    gate.touch()
    state = self.wait_terminal()
    resumed = json.loads((self.run / "outputs/resumed-state.json").read_text())
    self.assertEqual(resumed["status"], "running")
    self.assertEqual(state["status"], "completed")
    self.assertEqual(state["exit_code"], 0)
    self.assertEqual(state["started_at"], waiting["started_at"])
    self.assertEqual(resumed["started_at"], waiting["started_at"])
    self.assertEqual((self.run / "logs/stdout.log").read_bytes(), b"waiting\nresumed\n")

  def test_waiting_task_can_finish_without_reporting_running_again(self):
    self.start_waiting_task("print('completed while waiting', flush=True)\n")
    state = self.wait_terminal()
    self.assertEqual(state["status"], "completed")
    self.assertEqual(state["exit_code"], 0)
    self.assertEqual(state["task_exit_code"], 0)
    waiting = json.loads((self.run / "outputs/waiting-state.json").read_text())
    self.assertEqual(state["started_at"], waiting["started_at"])

  def test_waiting_task_remains_cancellable(self):
    self.start_waiting_task("time.sleep(20)\n")
    self.wait_file(self.run / "outputs/waiting-state.json")
    report = jobs.execute_job(self.repo, {"operation": "cancel", "run_id": self.run.name})
    self.assertEqual(report["status"], "waiting")
    self.assertTrue(report["cancel_requested"])
    state = self.wait_terminal()
    self.assertEqual(state["status"], "cancelled", state)
    self.assertEqual(state["exit_code"], 130)
    self.assertEqual(state["task_exit_code"], 143)
    self.assertEqual((self.run / "logs/stdout.log").read_bytes(), b"waiting\n")

  def test_supervisor_write_preserves_new_metadata_and_rejects_terminal_restoration(self):
    path = self.repo / "run-state.json"
    original = {"status": "running", "started_at": "2026-10-14T01:00:00Z"}
    run_lifecycle._lifecycle_write_json(path, original)
    run_lifecycle._lifecycle_write_json(path, {"status": "waiting", "extra": "preserved"})
    run_lifecycle._lifecycle_write_json(path, {**original, "status": "completed", "exit_code": 0})
    final = json.loads(path.read_text())
    self.assertEqual(final["extra"], "preserved")
    self.assertEqual(final["started_at"], original["started_at"])
    with self.assertRaisesRegex(ValueError, "finished"):
      run_lifecycle._lifecycle_write_json(path, original)
    self.assertEqual(json.loads(path.read_text()), final)

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
