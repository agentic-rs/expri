"""Exercise the local producer protocol with actual Python and Rust processes.

Build `cargo build -p expri-sdk --example contract_probe` before running this file.
The probe is a test adapter, not a published SDK interface.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
PYTHON_SDK = ROOT / "sdk" / "python"
PROBE = ROOT / "target" / "debug" / "examples" / "contract_probe"
MAX_STEP = 2 ** 64 - 1
PYTHON_PROBE = """
import json
import sys
from expri import MetricsLogger

request = json.load(sys.stdin)
with MetricsLogger(request["output_dir"]) as logger:
  if "params" in request:
    logger.params(request["params"])
  for call in request.get("calls", []):
    logger.log(call["step"], call["metrics"],
               step_mode=call.get("step_mode", "relative"))
  print(json.dumps({"initial_step": logger.initial_step}))
"""


def environment(attempt=None, offset=None):
  env = {key: value for key, value in os.environ.items() if not key.startswith("EXPRI_")}
  env["PYTHONPATH"] = str(PYTHON_SDK)
  env["PYTHONDONTWRITEBYTECODE"] = "1"
  if attempt is not None:
    env["EXPRI_ATTEMPT"] = str(attempt)
  if offset is not None:
    env["EXPRI_STEP_OFFSET"] = str(offset)
  return env


def call(step, step_mode="relative", **metrics):
  return {"step": step, "step_mode": step_mode, "metrics": metrics or {"loss": 0.125}}


class ProducerInteropTests(unittest.TestCase):
  def setUp(self):
    self.directory = tempfile.TemporaryDirectory(prefix="expri-sdk-contract-")
    self.addCleanup(self.directory.cleanup)
    self.output = Path(self.directory.name) / "outputs"
    self.output.mkdir()
    self.path = self.output / "metrics.jsonl"

  def prefix(self, maximum=1000):
    # Maximum, not last: copied histories can contain out-of-order observations.
    raw = (json.dumps({"schema_version": 1, "step": maximum, "metrics": {"loss": 0.5}})
           + "\n\n" + json.dumps({"step": 4, "metrics": {"loss": 0.25}}) + "\n").encode()
    self.path.write_bytes(raw)
    return raw

  def start(self, language, calls=(), env=None, params=None):
    request = {"output_dir": str(self.output), "calls": list(calls)}
    if params is not None:
      request["params"] = params
    command = [sys.executable, "-c", PYTHON_PROBE] if language == "python" else [str(PROBE)]
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, encoding="utf-8",
                               env=environment() if env is None else env)
    process.stdin.write(json.dumps(request))
    process.stdin.close()
    process.stdin = None
    return process

  def finish(self, process, succeeds=True):
    try:
      stdout, stderr = process.communicate(timeout=30)
    except subprocess.TimeoutExpired:
      process.kill()
      stdout, stderr = process.communicate()
      self.fail("producer timed out: " + stderr)
    if succeeds:
      self.assertEqual(process.returncode, 0, stderr)
      return json.loads(stdout)
    self.assertNotEqual(process.returncode, 0, "invalid producer operation unexpectedly succeeded")
    return stderr

  def run_writer(self, language, calls=(), env=None, params=None, succeeds=True):
    return self.finish(self.start(language, calls, env, params), succeeds)

  def events(self):
    return [json.loads(row) for row in self.path.read_text().splitlines() if row.strip()]

  def origin(self):
    return self.output / "attempts" / "2" / "step-origin.json"

  def test_python_origin_is_reused_by_rust_and_python(self):
    self.assert_shared_origin("python", "rust")

  def test_rust_origin_is_reused_by_python_and_rust(self):
    self.assert_shared_origin("rust", "python")

  def assert_shared_origin(self, first, second):
    prefix = self.prefix()
    env = environment(attempt=2, offset=1001)
    result = self.run_writer(first, [call(10), call(2000, "global")], env)
    self.assertEqual(result["initial_step"], 1001)
    origin = self.origin().read_bytes()
    self.assertEqual(json.loads(origin), {"schema_version": 2, "attempt": 2, "step_offset": "1001"})
    result = self.run_writer(second, [call(0), call(42, "global")], env)
    self.assertEqual(result["initial_step"], 1001)
    result = self.run_writer(first, [call(3)], env)
    self.assertEqual(result["initial_step"], 1001)
    self.assertEqual(self.origin().read_bytes(), origin)
    self.assertTrue(self.path.read_bytes().startswith(prefix))
    added = self.events()[2:]
    self.assertEqual([row["step"] for row in added], [1011, 2000, 1001, 42, 1004])
    self.assertEqual([row["step_mode"] for row in added], ["relative", "global", "relative", "global", "relative"])
    for row in added:
      self.assertEqual(row["schema_version"], 1)
      self.assertEqual(row["attempt"], 2)
      self.assertIn("timestamp", row)
      self.assertEqual("local_step" in row, row["step_mode"] == "relative")

  def test_standalone_reopen_starts_a_new_session_across_languages(self):
    prefix = self.prefix()
    self.assertEqual(self.run_writer("python", [call(10)])["initial_step"], 1001)
    self.assertEqual(self.run_writer("rust", [call(0)])["initial_step"], 1012)
    self.assertEqual(self.run_writer("python", [call(0)])["initial_step"], 1013)
    self.assertFalse((self.output / "attempts").exists())
    self.assertTrue(self.path.read_bytes().startswith(prefix))
    self.assertEqual([row["step"] for row in self.events()[2:]], [1011, 1012, 1013])

  def test_explicit_output_ignores_an_unrelated_managed_context(self):
    prefix = self.prefix()
    env = environment()
    env.update({"EXPRI_OUTPUT_DIR": str(self.output.parent / "other" / "outputs"),
                "EXPRI_RUN_DIR": str(self.output.parent / "other"),
                "EXPRI_RUN_ID": "other", "EXPRI_ATTEMPT": "invalid",
                "EXPRI_STEP_OFFSET": "invalid"})
    self.assertEqual(self.run_writer("python", [call(0)], env)["initial_step"], 1001)
    self.assertEqual(self.run_writer("rust", [call(0)], env)["initial_step"], 1002)
    self.assertTrue(self.path.read_bytes().startswith(prefix))
    self.assertFalse((self.output / "attempts").exists())
    self.assertTrue(all("attempt" not in row for row in self.events()[2:]))

  def test_mixed_writers_share_the_metrics_flock_and_origin(self):
    prefix = self.prefix()
    env = environment(attempt=2, offset=1001)
    processes = []
    try:
      with self.path.open("a+b") as handle:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        try:
          for writer, language in enumerate(("python", "rust", "python", "rust")):
            calls = [call(writer * 100 + step, writer=writer, loss=0.125) for step in range(32)]
            processes.append(self.start(language, calls, env))
          # Both implementations must wait for the existing inode's flock.
          time.sleep(0.15)
          self.assertTrue(all(process.poll() is None for process in processes))
          self.assertEqual(self.path.read_bytes(), prefix)
          self.assertFalse(self.origin().exists())
        finally:
          fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
      for process in processes:
        self.assertEqual(self.finish(process)["initial_step"], 1001)
    finally:
      for process in processes:
        if process.poll() is None:
          process.kill()
          process.communicate()
    self.assertTrue(self.path.read_bytes().startswith(prefix))
    added = self.events()[2:]
    self.assertEqual(len(added), 128)
    expected = sorted(1001 + writer * 100 + step for writer in range(4) for step in range(32))
    self.assertEqual(sorted(row["step"] for row in added), expected)

  def test_parameters_are_immutable_across_languages(self):
    env = environment(attempt=2, offset=0)
    params = {"seed": 42, "learning_rate": 0.125, "nested": {"enabled": True, "names": ["a", None]}}
    self.run_writer("rust", env=env, params=params)
    path = self.output / "attempts" / "2" / "params.json"
    original = path.read_bytes()
    self.run_writer("python", env=env, params=params)
    self.assertEqual(path.read_bytes(), original)
    self.run_writer("python", env=env, params={"seed": 43}, succeeds=False)
    self.run_writer("rust", env=env, params={"seed": 43}, succeeds=False)
    self.assertEqual(path.read_bytes(), original)

  def test_legacy_origin_keeps_global_mode_without_changing_mapping(self):
    self.prefix()
    self.origin().parent.mkdir(parents=True)
    origin = b'{"schema_version":1,"attempt":2,"step_offset":"1001","first_local_step":"10"}\n'
    self.origin().write_bytes(origin)
    env = environment(attempt=2, offset=1001)
    for language in ("python", "rust"):
      self.assertEqual(self.run_writer(language, [call(1200, "global")], env)["initial_step"], 1001)
      before = self.path.read_bytes()
      self.run_writer(language, [call(0)], env, succeeds=False)
      self.assertEqual(self.path.read_bytes(), before)
      self.assertEqual(self.origin().read_bytes(), origin)

  def test_incomplete_history_is_rejected_without_altering_bytes(self):
    before = self.prefix() + b'{"step":1002'
    self.path.write_bytes(before)
    for language in ("python", "rust"):
      self.run_writer(language, [call(0)], succeeds=False)
      self.assertEqual(self.path.read_bytes(), before)

  def test_managed_global_max_does_not_break_a_saved_base(self):
    self.prefix()
    env = environment(attempt=2, offset=1001)
    self.run_writer("python", [call(MAX_STEP, "global")], env)
    self.assertEqual(self.run_writer("rust", [call(0)], env)["initial_step"], 1001)
    self.assertEqual(self.run_writer("python", [call(1)], env)["initial_step"], 1001)
    before = self.path.read_bytes()
    for language in ("python", "rust"):
      self.run_writer(language, [call(MAX_STEP)], env, succeeds=False)
      self.assertEqual(self.path.read_bytes(), before)


if __name__ == "__main__":
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("--probe", type=Path, default=PROBE)
  options, unittest_args = parser.parse_known_args()
  PROBE = options.probe.resolve()
  if not PROBE.is_file():
    parser.error("Rust contract probe is missing; build it first or pass --probe PATH")
  unittest.main(argv=[sys.argv[0], *unittest_args])
