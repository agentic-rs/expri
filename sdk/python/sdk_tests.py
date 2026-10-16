"""Public package behavior, local CLI delegation, and managed-run isolation."""

import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import expri
from expri_metrics import MetricsLogger
from expri_run import set_run_status


class SdkTests(unittest.TestCase):
  def setUp(self):
    temporary = tempfile.TemporaryDirectory(prefix="expri-python-sdk-tests-")
    self.addCleanup(temporary.cleanup)
    self.root = Path(temporary.name).resolve()
    self.environment = patch.dict(os.environ, {}, clear=True)
    self.environment.start()
    self.addCleanup(self.environment.stop)

  def managed_run(self):
    run_dir = self.root / "run-fixture"
    (run_dir / "code").mkdir(parents=True)
    (run_dir / "outputs").mkdir()
    state = {
      "schema_version": 1, "run_id": run_dir.name, "status": "running",
      "started_at": "2026-10-15T01:23:45Z", "finished_at": None,
      "detached": True, "code_dir": str(run_dir / "code"),
      "output_dir": str(run_dir / "outputs"),
    }
    (run_dir / "run-state.json").write_text(json.dumps(state))
    lease = (run_dir / ".run.lock").open("a+b")
    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
    self.addCleanup(lease.close)
    return run_dir

  def fake_binary(self, *, status=0):
    binary = self.root / "expri fake; command"
    record = self.root / "arguments.json"
    binary.write_text(
      "#!" + sys.executable + "\n"
      "import json, pathlib, sys\n"
      "pathlib.Path(" + repr(str(record)) + ").write_text(json.dumps(sys.argv[1:]))\n"
      "print(json.dumps({'registered': True}))\n"
      "print('local registration failed', file=sys.stderr)\n"
      "sys.exit(" + str(status) + ")\n"
    )
    binary.chmod(0o700)
    return binary, record

  def test_installed_api_reuses_canonical_standalone_implementations(self):
    self.assertIs(expri.MetricsLogger, MetricsLogger)
    self.assertIs(expri.set_run_status, set_run_status)

  def test_initial_step_append_and_explicit_global_preserve_history(self):
    output = self.root / "outputs"
    output.mkdir()
    prefix = b'{"step":1000,"metrics":{"loss":0.5}}\n'
    (output / "metrics.jsonl").write_bytes(prefix)
    with expri.init(output_dir=output) as run:
      self.assertEqual(run.initial_step, 1001)
      run.params({"epochs": 20})
      run.log(0, {"loss": 0.4})
      run.log(10, {"loss": 0.3}, step_mode="relative")
      run.log(1200, {"loss": 0.2}, step_mode="global")
      run.flush()
      self.assertEqual(run.initial_step, 1001)
      raw = (output / "metrics.jsonl").read_bytes()
      self.assertTrue(raw.startswith(prefix))
      rows = [json.loads(line) for line in raw.splitlines()]
      self.assertEqual([row["step"] for row in rows], [1000, 1001, 1011, 1200])
      self.assertNotIn("local_step", rows[-1])
      self.assertFalse(run.closed)
    self.assertTrue(run.closed)
    with expri.init(output_dir=output) as reopened:
      self.assertEqual(reopened.initial_step, 1201)

  def test_status_works_without_native_binary_and_close_preserves_lifecycle(self):
    run_dir = self.managed_run()
    os.environ["PATH"] = ""
    with expri.init(run_dir=run_dir) as run:
      self.assertEqual(run.output_dir, run_dir / "outputs")
      changed = run.set_status("waiting")
      self.assertEqual(changed["status"], "waiting")
      self.assertTrue(changed["changed"])
      self.assertFalse(run.set_status("waiting")["changed"])
    state = json.loads((run_dir / "run-state.json").read_bytes())
    self.assertEqual(state["status"], "waiting")
    self.assertEqual(state["started_at"], "2026-10-15T01:23:45Z")
    self.assertIsNone(state["finished_at"])

  def test_initialization_captures_binary_and_directory_for_cli_arguments(self):
    run_dir = self.managed_run()
    binary, record = self.fake_binary()
    os.environ.update({"EXPRI_RUN_DIR": str(run_dir), "EXPRI_RUN_ID": run_dir.name,
                       "EXPRI_OUTPUT_DIR": str(run_dir / "outputs"), "EXPRI_BIN": str(binary)})
    with expri.init() as run:
      os.environ["EXPRI_RUN_DIR"] = str(self.root / "unrelated-run")
      os.environ["EXPRI_BIN"] = "/missing/expri"
      name = "checkpoint ; $(touch injected).pt"
      (run.output_dir / name).write_bytes(b"checkpoint")
      self.assertEqual(run.register_artifact(name, labels=["best", "latest"]), {"registered": True})
      self.assertEqual(json.loads(record.read_bytes()), [
        "artifact", "--json", "register", str(run.output_dir / name), "--run-dir", str(run_dir),
        "--label", "best", "--label", "latest",
      ])
      self.assertEqual(run.set_status("waiting")["run_id"], run_dir.name)
    self.assertFalse((self.root / "injected").exists())

  def test_redirected_output_does_not_inherit_status_artifact_or_attempt(self):
    run_dir = self.managed_run()
    os.environ.update({"EXPRI_RUN_DIR": str(run_dir), "EXPRI_OUTPUT_DIR": str(run_dir / "outputs"),
                       "EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "100"})
    with expri.init(output_dir=self.root / "independent") as run:
      self.assertIsNone(run.run_dir)
      run.log(0, {"loss": 1})
      row = json.loads((run.output_dir / "metrics.jsonl").read_bytes())
      self.assertNotIn("attempt", row)
      self.assertEqual(row["step"], 0)
      for operation in (lambda: run.set_status("waiting"), lambda: run.register_artifact("checkpoint.pt")):
        with self.assertRaisesRegex(ValueError, "not attached"):
          operation()
    self.assertEqual(json.loads((run_dir / "run-state.json").read_bytes())["status"], "running")

  def test_explicit_run_and_output_must_match_without_creating_wrong_outputs(self):
    run_dir = self.managed_run()
    output = self.root / "wrong-output"
    with self.assertRaisesRegex(ValueError, "selected run"):
      expri.init(output_dir=output, run_dir=run_dir)
    self.assertFalse(output.exists())
    with self.assertRaisesRegex(ValueError, "traversal"):
      expri.init(run_dir=run_dir / ".." / "other")

  def test_invalid_labels_rejected_before_invoking_binary(self):
    run_dir = self.managed_run()
    with expri.init(run_dir=run_dir) as run:
      for labels in ("best", ["invalid"], ["best", "best"], [["best"]]):
        with self.assertRaisesRegex(ValueError, "labels"):
          run.register_artifact("checkpoint.pt", labels=labels)

  def test_command_failures_include_local_process_output(self):
    run_dir = self.managed_run()
    binary, _ = self.fake_binary(status=7)
    os.environ["EXPRI_BIN"] = str(binary)
    with expri.init(run_dir=run_dir) as run:
      with self.assertRaises(subprocess.CalledProcessError) as failure:
        run.register_artifact("checkpoint.pt")
      self.assertEqual(failure.exception.returncode, 7)
      self.assertIn("local registration failed", failure.exception.stderr)

  def test_closed_handle_rejects_operations_and_exception_exit_closes(self):
    with self.assertRaisesRegex(RuntimeError, "training failed"):
      with expri.init(output_dir=self.root / "outputs") as run:
        raise RuntimeError("training failed")
    for operation in (run.flush, lambda: run.log(0, {"loss": 1}), lambda: run.params({}),
                      lambda: run.set_status("waiting"), lambda: run.register_artifact("x")):
      with self.assertRaisesRegex(ValueError, "closed"):
        operation()
    run.close()

  def test_forked_handle_cannot_flush_change_status_or_register_artifacts(self):
    run_dir = self.managed_run()
    with expri.init(run_dir=run_dir) as run:
      reader, writer = os.pipe()
      child = os.fork()
      if child == 0:
        os.close(reader)
        try:
          for operation in (run.flush, lambda: run.set_status("waiting"),
                            lambda: run.register_artifact("checkpoint.pt")):
            try:
              operation()
            except ValueError as error:
              if "fork" not in str(error):
                raise
            else:
              raise AssertionError("inherited Run operation was accepted")
          run.close()
          os.write(writer, b"ok")
          os._exit(0)
        except BaseException as error:
          os.write(writer, repr(error).encode("utf-8"))
          os._exit(1)
      os.close(writer)
      try:
        result = os.read(reader, 4096)
      finally:
        os.close(reader)
      _, status = os.waitpid(child, 0)
      self.assertEqual(os.waitstatus_to_exitcode(status), 0, result)
      self.assertEqual(result, b"ok")
      run.log(0, {"loss": 1})
      self.assertEqual(json.loads((run_dir / "run-state.json").read_bytes())["status"], "running")


if __name__ == "__main__":
  unittest.main()
