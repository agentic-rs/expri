"""Live lease, metadata validation, persistence, and status race tests."""

import fcntl
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

import expri_run
from expri_run import set_run_status


class RunStatusTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory(prefix="expri-run-status-tests-")
    self.addCleanup(self.temporary.cleanup)
    self.root = Path(self.temporary.name).resolve()
    self.run = self.root / ".expri/runs/run-fixture"
    (self.run / "code").mkdir(parents=True)
    (self.run / "outputs").mkdir()
    self.path = self.run / "run-state.json"
    self.state = {
      "schema_version": 1, "run_id": self.run.name, "task": "train",
      "status": "running", "detached": True,
      "started_at": "2026-10-14T01:23:45.123456+08:00", "finished_at": None,
      "code_dir": str(self.run / "code"), "output_dir": str(self.run / "outputs"),
      "machine_id": "machine-original", "system_capture": {"capture_id": "capture-original"},
      "command": ["python", "train.py"], "logs": {"stdout": "logs/stdout.log"},
      "annotations": {"nickname": "日本語", "tags": ["baseline", "waiting-test"]},
      "unknown": {"nested": [True, None, 1234567890123456789, 0.5]},
    }
    self.save_state(self.state)
    self.lease = (self.run / ".run.lock").open("a+b")
    fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
    self.addCleanup(self.lease.close)

  def save_state(self, state):
    temporary = self.run / "run-state.test.tmp"
    temporary.write_text(json.dumps(state, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(self.path)

  def read_state(self):
    return json.loads(self.path.read_bytes())

  def assert_rejected_without_rewrite(self, message=None):
    before = self.path.read_bytes()
    before_stat = self.path.stat()
    with self.assertRaisesRegex(ValueError, message or ".*"):
      set_run_status("waiting", self.run)
    self.assertEqual(self.path.read_bytes(), before)
    self.assertEqual(self.path.stat().st_ino, before_stat.st_ino)

  def test_round_trip_preserves_start_capture_and_all_other_metadata(self):
    result = set_run_status("waiting", self.run)
    self.assertEqual(result, {"run_id": self.run.name, "previous_status": "running",
                              "status": "waiting", "changed": True})
    self.assertEqual(self.read_state(), {**self.state, "status": "waiting"})
    result = set_run_status("running", self.run)
    self.assertEqual(result, {"run_id": self.run.name, "previous_status": "waiting",
                              "status": "running", "changed": True})
    self.assertEqual(self.read_state(), self.state)
    self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o600)
    self.assertFalse(list(self.run.glob(".run-state-*.tmp")))

  def test_default_environment_and_explicit_path_need_no_binary_or_path(self):
    with patch.dict(os.environ, {"EXPRI_RUN_DIR": str(self.run), "PATH": ""}, clear=True):
      self.assertTrue(set_run_status("waiting")["changed"])
    with patch.dict(os.environ, {}, clear=True):
      self.assertTrue(set_run_status("running", self.run)["changed"])
      with self.assertRaisesRegex(ValueError, "EXPRI_RUN_DIR"):
        set_run_status("waiting")

  def test_vendored_helper_works_in_isolated_python_without_native_binary(self):
    vendor = self.run / "code/expri_run.py"
    vendor.write_bytes(Path(expri_run.__file__).read_bytes())
    child = subprocess.run(
      [sys.executable, "-I", "-B", "-c",
       "import sys; sys.path.insert(0, sys.argv[1]); "
       "from expri_run import set_run_status; "
       "print(set_run_status('waiting')['status'])", str(vendor.parent)],
      env={"EXPRI_RUN_DIR": str(self.run), "PATH": ""}, capture_output=True, text=True,
      timeout=10,
    )
    self.assertEqual(child.returncode, 0, child.stderr)
    self.assertEqual(child.stdout.strip(), "waiting")
    self.assertEqual(self.read_state()["status"], "waiting")

  def test_idempotent_request_preserves_original_bytes_inode_and_timestamp(self):
    before = self.path.read_bytes()
    metadata = self.path.stat()
    with patch.object(expri_run, "_write_state", side_effect=AssertionError("must not rewrite")):
      result = set_run_status("running", self.run)
    self.assertFalse(result["changed"])
    self.assertEqual(self.path.read_bytes(), before)
    after = self.path.stat()
    self.assertEqual((after.st_ino, after.st_mtime_ns), (metadata.st_ino, metadata.st_mtime_ns))

  def test_invalid_requested_status_does_not_create_state_lock(self):
    for status in ("pending", "preparing", "completed", "lost", "WAITING", "", None, 1, True, []):
      with self.subTest(status=status), self.assertRaisesRegex(ValueError, "running or waiting"):
        set_run_status(status, self.run)
    self.assertFalse((self.run / ".run-state.lock").exists())
    self.assertEqual(self.read_state(), self.state)

  def test_run_directory_name_obeys_the_shared_identifier_length_limit(self):
    with self.assertRaisesRegex(ValueError, "invalid run directory name"):
      set_run_status("waiting", self.root / ("r" * 97))
    self.assertFalse((self.root / ("r" * 97)).exists())

  def test_only_running_and_waiting_records_can_change(self):
    for status in ("preparing", "completed", "failed", "cancelled", "lost", "unknown", None, []):
      with self.subTest(status=status):
        self.save_state({**self.state, "status": status})
        self.assert_rejected_without_rewrite("running or waiting")

  def test_finished_active_record_cannot_be_revived(self):
    for finished in ("2026-10-14T01:24:00Z", "", False, 0, {}):
      with self.subTest(finished=finished):
        self.save_state({**self.state, "finished_at": finished})
        self.assert_rejected_without_rewrite("unfinished")

  def test_only_exact_supported_schema_version_is_accepted(self):
    for schema in (None, True, False, 0, 2, 1.0, "1", {}, []):
      with self.subTest(schema=schema):
        self.save_state({**self.state, "schema_version": schema})
        self.assert_rejected_without_rewrite("schema_version")
    state = dict(self.state)
    del state["schema_version"]
    self.save_state(state)
    self.assert_rejected_without_rewrite("schema_version")

  def test_missing_invalid_and_non_rfc3339_start_is_rejected(self):
    for started in (None, False, 1, "", "yesterday", "2026-10-14", "2026-10-14T01:23:45",
                    "2026-02-30T01:23:45Z", "2026-10-14T25:23:45Z", "2026-10-14T01:61:45Z",
                    "2026-10-14T01:23:61Z", "2026-10-14T01:23:45+24:00"):
      with self.subTest(started=started):
        self.save_state({**self.state, "started_at": started})
        self.assert_rejected_without_rewrite("started_at")

  def test_valid_utc_offsets_fraction_and_leap_second_preserve_exact_timestamp(self):
    for started in ("2026-10-14T01:23:45Z", "2026-10-14t01:23:45.123z",
                    "2026-10-14T01:23:45-08:30", "2016-12-31T23:59:60Z"):
      with self.subTest(started=started):
        self.save_state({**self.state, "started_at": started})
        set_run_status("waiting", self.run)
        self.assertEqual(self.read_state()["started_at"], started)

  def test_identity_and_owned_paths_are_checked(self):
    for field, value in (("run_id", "run-other"), ("code_dir", str(self.root)),
                         ("output_dir", str(self.root)), ("detached", "true")):
      with self.subTest(field=field):
        self.save_state({**self.state, field: value})
        self.assert_rejected_without_rewrite()
    for field in ("code_dir", "output_dir"):
      state = dict(self.state)
      del state[field]
      self.save_state(state)
      self.assert_rejected_without_rewrite()

  def test_foreground_live_record_can_request_waiting(self):
    self.save_state({**self.state, "detached": False})
    self.assertTrue(set_run_status("waiting", self.run)["changed"])
    self.assertFalse(self.read_state()["detached"])

  def test_released_missing_or_stale_supervisor_lease_is_rejected(self):
    self.lease.close()
    self.assert_rejected_without_rewrite("supervisor is not active")
    (self.run / ".run.lock").unlink()
    self.assert_rejected_without_rewrite("supervisor is not active")
    self.assertFalse((self.run / ".run.lock").exists())

  def test_idempotent_request_still_requires_live_lease(self):
    self.lease.close()
    with self.assertRaisesRegex(ValueError, "supervisor is not active"):
      set_run_status("running", self.run)

  def test_symlink_files_and_nonregular_files_are_rejected_without_touching_victim(self):
    victim = self.root / "victim"
    victim.write_bytes(b"preserve")
    for name in ("run-state.json", ".run-state.lock", ".run.lock"):
      path = self.run / name
      saved = path.read_bytes() if path.exists() else None
      if path.exists():
        path.unlink()
      path.symlink_to(victim)
      with self.subTest(name=name), self.assertRaisesRegex(ValueError, "regular file"):
        set_run_status("waiting", self.run)
      self.assertEqual(victim.read_bytes(), b"preserve")
      path.unlink()
      if saved is not None:
        path.write_bytes(saved)
      else:
        # Restore a plain state-lock file for the next file case.
        path.touch()

  def test_fifo_files_are_rejected_without_blocking(self):
    for name in ("run-state.json", ".run-state.lock", ".run.lock"):
      with self.subTest(name=name):
        path = self.run / name
        saved = path.read_bytes() if path.exists() else None
        if path.exists():
          path.unlink()
        os.mkfifo(path)
        with self.assertRaisesRegex(ValueError, "regular file"):
          set_run_status("waiting", self.run)
        path.unlink()
        if saved is not None:
          path.write_bytes(saved)
        else:
          path.touch()

  def test_symlink_run_parent_and_child_directories_are_rejected(self):
    alias = self.root / "alias"
    alias.symlink_to(self.run, target_is_directory=True)
    with self.assertRaises(OSError):
      set_run_status("waiting", alias)
    alias.unlink()
    alias.symlink_to(self.root / ".expri", target_is_directory=True)
    with self.assertRaises(OSError):
      set_run_status("waiting", alias / "runs" / self.run.name)
    for child in ("code", "outputs"):
      path = self.run / child
      path.rmdir()
      path.symlink_to(self.root, target_is_directory=True)
      with self.subTest(child=child), self.assertRaises(OSError):
        set_run_status("waiting", self.run)
      path.unlink()
      path.mkdir()
    self.assertEqual(self.read_state(), self.state)

  def test_missing_directory_and_traversal_do_not_create_directories(self):
    missing = self.root / "run-missing"
    with self.assertRaises(FileNotFoundError):
      set_run_status("waiting", missing)
    self.assertFalse(missing.exists())
    with self.assertRaisesRegex(ValueError, "traversal"):
      set_run_status("waiting", self.run / "code/..")

  def test_malformed_nonobject_duplicate_nonfinite_and_oversized_state_are_preserved(self):
    for raw in (b"{", b"[]", b"null", b"true", b"{}", b"\xff", b'{"status":"running","status":"waiting"}',
                b'{"extra":NaN}', b'{"extra":Infinity}', b'{"extra":1e999}',
                b" " * (expri_run.MAX_STATE_BYTES + 1)):
      with self.subTest(raw=raw[:40]):
        self.path.write_bytes(raw)
        self.assert_rejected_without_rewrite()

  def test_escaped_unpaired_unicode_is_rejected_even_for_idempotent_requests(self):
    for extra in ({"unknown": "\ud800"}, {"unknown": ["\udfff"]}, {"\ud800": "value"}):
      with self.subTest(extra=repr(extra)):
        self.path.write_text(json.dumps({**self.state, **extra}, ensure_ascii=True))
        before = self.path.read_bytes()
        with self.assertRaisesRegex(ValueError, "invalid JSON"):
          set_run_status("running", self.run)
        self.assertEqual(self.path.read_bytes(), before)

  def test_serialized_state_limit_preserves_original_compact_record(self):
    self.path.write_text(json.dumps({**self.state, "extra": [None] * 40000}, separators=(",", ":")))
    self.assertLess(self.path.stat().st_size, expri_run.MAX_STATE_BYTES)
    self.assert_rejected_without_rewrite("256 KiB")
    self.assertFalse(list(self.run.glob(".run-state-*.tmp")))

  def test_atomic_write_failure_preserves_state_and_removes_temporary_file(self):
    before = self.path.read_bytes()
    with patch.object(expri_run.os, "replace", side_effect=OSError("injected replacement failure")):
      with self.assertRaisesRegex(OSError, "replacement failure"):
        set_run_status("waiting", self.run)
    self.assertEqual(self.path.read_bytes(), before)
    self.assertFalse(list(self.run.glob(".run-state-*.tmp")))

  def test_file_and_directory_sync_happen_before_success(self):
    original = expri_run.os.fsync
    synced = []
    def sync(descriptor):
      synced.append(stat.S_ISDIR(os.fstat(descriptor).st_mode))
      original(descriptor)
    with patch.object(expri_run.os, "fsync", side_effect=sync):
      set_run_status("waiting", self.run)
    self.assertEqual(synced, [False, True])

  def test_state_lock_wait_is_bounded_without_modifying_state(self):
    before = self.path.read_bytes()
    with (self.run / ".run-state.lock").open("a+b") as lock:
      fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
      with patch.object(expri_run, "_LOCK_TIMEOUT_SECONDS", 0.02):
        with self.assertRaisesRegex(TimeoutError, "busy"):
          set_run_status("waiting", self.run)
    self.assertEqual(self.path.read_bytes(), before)

  def test_concurrent_requests_serialize_complete_read_modify_write(self):
    barrier = threading.Barrier(9)
    results = []
    failures = []
    def request():
      try:
        barrier.wait(timeout=5)
        results.append(set_run_status("waiting", self.run))
      except BaseException as error:
        failures.append(error)
    threads = [threading.Thread(target=request) for _ in range(8)]
    for thread in threads:
      thread.start()
    barrier.wait(timeout=5)
    for thread in threads:
      thread.join(timeout=10)
      self.assertFalse(thread.is_alive())
    self.assertEqual(failures, [])
    self.assertEqual(len(results), 8)
    self.assertEqual(sum(result["changed"] for result in results), 1)
    self.assertEqual(self.read_state(), {**self.state, "status": "waiting"})

  def test_terminal_publisher_wins_while_setter_waits_for_shared_lock(self):
    barrier = threading.Barrier(2)
    errors = []
    original = expri_run._lock_state
    def acquire(directory):
      barrier.wait(timeout=5)
      return original(directory)
    def request():
      try:
        set_run_status("waiting", self.run)
      except BaseException as error:
        errors.append(error)
    finished = {**self.state, "status": "completed", "finished_at": "2026-10-14T02:00:00Z",
                "exit_code": 0, "final_metadata": {"preserve": True}}
    with (self.run / ".run-state.lock").open("a+b") as lock:
      fcntl.flock(lock, fcntl.LOCK_EX)
      with patch.object(expri_run, "_lock_state", side_effect=acquire):
        thread = threading.Thread(target=request)
        thread.start()
        barrier.wait(timeout=5)
        self.save_state(finished)
        fcntl.flock(lock, fcntl.LOCK_UN)
        thread.join(timeout=10)
        self.assertFalse(thread.is_alive())
    self.assertEqual(len(errors), 1)
    self.assertIsInstance(errors[0], ValueError)
    self.assertEqual(self.read_state(), finished)

  def test_supervisor_finishes_after_setter_without_losing_task_or_final_metadata(self):
    barrier = threading.Barrier(2)
    errors = []
    def publish_terminal():
      try:
        barrier.wait(timeout=5)
        with (self.run / ".run-state.lock").open("a+b") as lock:
          fcntl.flock(lock, fcntl.LOCK_EX)
          current = self.read_state()
          current.update(status="completed", finished_at="2026-10-14T02:00:00Z", exit_code=0)
          self.save_state(current)
      except BaseException as error:
        errors.append(error)
    thread = threading.Thread(target=publish_terminal)
    thread.start()
    self.assertTrue(set_run_status("waiting", self.run)["changed"])
    barrier.wait(timeout=5)
    thread.join(timeout=10)
    self.assertFalse(thread.is_alive())
    self.assertEqual(errors, [])
    finished = self.read_state()
    self.assertEqual(finished, {**self.state, "status": "completed",
                                "finished_at": "2026-10-14T02:00:00Z", "exit_code": 0})
    self.assert_rejected_without_rewrite("running or waiting")


if __name__ == "__main__":
  unittest.main()
