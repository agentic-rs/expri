"""Behavioral tests for job ownership, liveness, cancellation, and binary logs."""

import fcntl
import io
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest

import jobs


class JobTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory(prefix="expri-job-tests-")
    self.addCleanup(self.temporary.cleanup)
    self.repo = Path(self.temporary.name).resolve()
    self.run = self.repo / ".expri/runs/run-fixture"
    (self.run / "code").mkdir(parents=True)
    (self.run / "outputs").mkdir()
    self.state = {
      "schema_version": 1, "run_id": self.run.name, "task": "train",
      "status": "running", "detached": True,
      "code_dir": str(self.run / "code"), "output_dir": str(self.run / "outputs"),
    }
    self.save_state()

  def save_state(self):
    temporary = self.run / "run-state.tmp"
    temporary.write_text(json.dumps(self.state))
    temporary.replace(self.run / "run-state.json")

  def hold_lease(self):
    handle = (self.run / ".run.lock").open("a+b")
    fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    self.addCleanup(handle.close)
    return handle

  def request(self, operation, **kwargs):
    return jobs.execute_job(self.repo, {"operation": operation, "run_id": self.run.name, **kwargs})

  def test_lost_status_preserves_recorded_state_without_creating_lock(self):
    original = (self.run / "run-state.json").read_bytes()
    report = self.request("status")
    self.assertEqual(report["status"], "lost")
    self.assertEqual(report["recorded_status"], "running")
    self.assertFalse(report["alive"])
    self.assertEqual((self.run / "run-state.json").read_bytes(), original)
    self.assertFalse((self.run / ".run.lock").exists())

  def test_live_status_uses_lease_and_cancel_only_creates_sentinel(self):
    lease = self.hold_lease()
    original = (self.run / "run-state.json").read_bytes()
    report = self.request("status")
    self.assertEqual(report["status"], "running")
    self.assertTrue(report["alive"])
    cancelled = self.request("cancel")
    self.assertTrue(cancelled["cancel_requested"])
    self.assertEqual(cancelled["status"], "running")
    self.assertEqual((self.run / ".cancel-request").read_bytes(), b"")
    self.assertEqual((self.run / "run-state.json").read_bytes(), original)
    self.assertTrue(self.request("cancel")["cancel_requested"])
    lease.close()
    self.assertEqual(self.request("status")["status"], "lost")

  def test_cancel_refuses_lost_and_foreground_runs(self):
    with self.assertRaisesRegex(ValueError, "lost"):
      self.request("cancel")
    self.state["detached"] = False
    self.save_state()
    self.hold_lease()
    with self.assertRaisesRegex(ValueError, "detached"):
      self.request("cancel")
    self.assertFalse((self.run / ".cancel-request").exists())

  def test_terminal_cancel_is_idempotent_without_marker(self):
    self.state["status"] = "cancelled"
    self.save_state()
    report = self.request("cancel")
    self.assertTrue(report["already_finished"])
    self.assertFalse(report["cancel_requested"])
    self.assertFalse((self.run / ".cancel-request").exists())

  def test_legacy_foreground_status_does_not_require_snapshot(self):
    self.state.pop("detached")
    self.state.pop("code_dir")
    self.state.pop("output_dir")
    (self.run / "code").rmdir()
    (self.run / "outputs").rmdir()
    self.save_state()
    self.assertEqual(self.request("status")["status"], "running")

  def test_rejects_symlink_paths_and_metadata_owned_elsewhere(self):
    outside = self.repo / "outside"
    outside.mkdir()
    (self.run / "code").rmdir()
    (self.run / "code").symlink_to(outside, target_is_directory=True)
    with self.assertRaisesRegex(ValueError, "real directory"):
      self.request("status")
    (self.run / "code").unlink()
    (self.run / "code").mkdir()
    self.state["output_dir"] = str(outside)
    self.save_state()
    with self.assertRaisesRegex(ValueError, "output_dir"):
      self.request("status")

  def test_cancel_rejects_existing_symlink_sentinel(self):
    self.hold_lease()
    victim = self.repo / "victim"
    victim.write_bytes(b"preserve")
    (self.run / ".cancel-request").symlink_to(victim)
    with self.assertRaisesRegex(ValueError, "regular file"):
      self.request("cancel")
    self.assertEqual(victim.read_bytes(), b"preserve")

  def test_binary_tail_preserves_line_boundaries_and_invalid_utf8(self):
    self.state["status"] = "completed"
    self.save_state()
    (self.run / "logs").mkdir()
    path = self.run / "logs/stdout.log"
    for raw, tail, expected in (
      (b"a\nb\n\xff\x00\n", 2, b"b\n\xff\x00\n"),
      (b"a\nb\n\xff\x00", 1, b"\xff\x00"),
      (b"a\nb\n", 0, b""),
      (b"a\n" + b"z" * 100000 + b"\nlast\n", 1, b"last\n"),
    ):
      path.write_bytes(raw)
      output = io.BytesIO()
      report = jobs.execute_job(self.repo, {"operation": "logs", "run_id": self.run.name, "tail": tail}, output)
      self.assertIsNone(report)
      self.assertEqual(output.getvalue(), expected)

  def test_follow_waits_for_log_creation_and_drains_final_bytes(self):
    self.hold_lease()
    output = io.BytesIO()
    failures = []
    def follow():
      try:
        jobs.execute_job(self.repo, {"operation": "logs", "run_id": self.run.name, "follow": True, "tail": 0}, output)
      except Exception as error:
        failures.append(error)
    thread = threading.Thread(target=follow)
    thread.start()
    try:
      time.sleep(0.15)
      (self.run / "logs").mkdir()
      with (self.run / "logs/stdout.log").open("wb", buffering=0) as handle:
        handle.write(b"first\n")
        time.sleep(0.15)
        handle.write(b"last\xff\x00")
      self.state["status"] = "completed"
      self.save_state()
      thread.join(3)
    finally:
      if thread.is_alive():
        self.state["status"] = "failed"
        self.save_state()
        thread.join(3)
    self.assertFalse(thread.is_alive())
    self.assertFalse(failures, failures)
    self.assertEqual(output.getvalue(), b"first\nlast\xff\x00")

  def test_follow_on_lost_run_finishes_without_log_creation(self):
    output = io.BytesIO()
    jobs.execute_job(self.repo, {"operation": "logs", "run_id": self.run.name, "follow": True}, output)
    self.assertEqual(output.getvalue(), b"")
    self.assertFalse((self.run / "logs").exists())

  def test_log_symlink_is_rejected(self):
    self.state["status"] = "completed"
    self.save_state()
    (self.run / "logs").mkdir()
    victim = self.repo / "victim"
    victim.write_bytes(b"private")
    (self.run / "logs/stdout.log").symlink_to(victim)
    with self.assertRaises((OSError, ValueError)):
      jobs.execute_job(self.repo, {"operation": "logs", "run_id": self.run.name}, io.BytesIO())

  def test_invalid_job_requests_are_rejected(self):
    for request in ({"operation": "status", "run_id": "../outside"},
                    {"operation": "remove", "run_id": self.run.name}):
      with self.assertRaises(ValueError):
        jobs.execute_job(self.repo, request)
    with self.assertRaisesRegex(ValueError, "tail"):
      self.request("logs", tail=True)


if __name__ == "__main__":
  unittest.main()
