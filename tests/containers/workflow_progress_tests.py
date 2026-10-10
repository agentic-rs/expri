"""Exercise progress while work blocks, fails, and shuts down."""

import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import time
import unittest

from workflow_progress import Progress


class HeartbeatStream(io.StringIO):
  def __init__(self):
    super().__init__()
    self.heartbeat = threading.Event()
    self.flush_count = 0

  def write(self, text):
    result = super().write(text)
    if "] RUNNING " in text:
      self.heartbeat.set()
    return result

  def flush(self):
    self.flush_count += 1
    return super().flush()


class ProgressTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory(prefix="expri-progress-tests-")
    self.addCleanup(self.temporary.cleanup)
    self.root = Path(self.temporary.name)

  def reporter(self, **kwargs):
    progress = Progress(**kwargs)
    self.addCleanup(progress.close)
    return progress

  def test_heartbeat_while_main_thread_blocks_and_log_matches_output(self):
    stream = HeartbeatStream()
    log_path = self.root / "progress.log"
    progress = self.reporter(name="Fixture workflow", heartbeat_seconds=0.02,
      stream=stream, log_path=log_path)
    with progress.phase("Build worker", log_name="/tmp/build-worker.log"):
      # A silent subprocess also blocks the main thread. The heartbeat must run
      # independently and flush both destinations before that work completes.
      self.assertTrue(stream.heartbeat.wait(timeout=2), "blocked work had no heartbeat")
      output = stream.getvalue()
      self.assertIn("START Build worker (log: build-worker.log)", output)
      self.assertIn("RUNNING Build worker (log: build-worker.log)", output)
      self.assertNotIn("/tmp/", output)
      self.assertEqual(log_path.read_text(), output)
    progress.close()
    self.assertIn("PASS Build worker", stream.getvalue())
    self.assertEqual(log_path.read_text(), stream.getvalue())
    self.assertGreaterEqual(stream.flush_count, 3)

  def test_nested_failure_unwinds_and_preserves_original_exception(self):
    stream = io.StringIO()
    summary_path = self.root / "summary.md"
    progress = self.reporter(stream=stream, summary_path=summary_path)
    failure = AssertionError("sensitive fixture detail must not appear")
    with self.assertRaises(AssertionError) as caught:
      with progress.phase("Recover publisher"):
        with progress.phase("Wait for training"):
          raise failure
    self.assertIs(caught.exception, failure)
    with progress.phase("Collect diagnostics"):
      progress.note("Saving bounded logs.")
    progress.close()
    output = stream.getvalue()
    self.assertLess(output.index("FAIL   Wait for training"),
      output.index("FAIL Recover publisher"))
    self.assertIn("PASS Collect diagnostics", output)
    self.assertIn("NOTE Saving bounded logs.", output)
    self.assertNotIn(str(failure), output)
    summary = summary_path.read_text()
    self.assertIn("| Recover publisher | FAIL |", summary)
    self.assertIn("| ↳ Wait for training | FAIL |", summary)
    self.assertIn("| Collect diagnostics | PASS |", summary)
    self.assertNotIn(str(failure), summary)

  def test_close_stops_heartbeat_even_during_an_active_phase(self):
    stream = HeartbeatStream()
    progress = self.reporter(stream=stream, heartbeat_seconds=0.02)
    with progress.phase("Blocked command"):
      self.assertTrue(stream.heartbeat.wait(timeout=2))
      progress.close()
      output = stream.getvalue()
      time.sleep(0.08)
      self.assertEqual(stream.getvalue(), output)
    self.assertEqual(stream.getvalue(), output)
    progress.close()
    with self.assertRaisesRegex(RuntimeError, "closed"):
      with progress.phase("Unexpected work"):
        pass

  def test_summary_appends_status_and_measured_duration_only_once(self):
    summary_path = self.root / "summary.md"
    summary_path.write_text("Existing CI summary\n\n")
    progress = self.reporter(name="Acceptance", stream=io.StringIO(),
      summary_path=summary_path)
    with progress.phase("Build | worker"):
      time.sleep(0.06)
    progress.close()
    summary = summary_path.read_text()
    self.assertTrue(summary.startswith("Existing CI summary\n\n### Acceptance"))
    self.assertIn("| Build \\| worker | PASS |", summary)
    durations = re.findall(r"\| PASS \| ([\d.]+)s \|", summary)
    self.assertEqual(len(durations), 1)
    self.assertGreaterEqual(float(durations[0]), 0.1)
    self.assertRegex(summary, r"Total elapsed: [\d.]+s\.")
    progress.close()
    self.assertEqual(summary_path.read_text(), summary)

  def test_broken_diagnostic_output_does_not_mask_the_test_failure(self):
    stream = io.StringIO()
    stream.close()
    # A directory cannot receive the Markdown summary. This too remains a
    # diagnostic failure, preserving the caller's test result.
    progress = self.reporter(stream=stream, summary_path=self.root)
    failure = RuntimeError("original test failure")
    with self.assertRaises(RuntimeError) as caught:
      try:
        with progress.phase("Fixture work"):
          raise failure
      finally:
        progress.close()
    self.assertIs(caught.exception, failure)


class ServiceWorkflowProgressTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory(prefix="expri-workflow-progress-tests-")
    self.addCleanup(self.temporary.cleanup)
    self.root = Path(self.temporary.name)
    self.bin = self.root / "bin"
    self.bin.mkdir()
    self.logs = self.root / "logs"
    self.summary = self.root / "summary.md"
    self.commands = self.root / "docker-commands.jsonl"
    self.tmp = self.root / "tmp"
    self.tmp.mkdir()

  def executable(self, name, contents):
    path = self.bin / name
    path.write_text(f"#!{sys.executable}\n{contents}")
    path.chmod(0o700)

  def run_workflow(self, failure):
    self.executable("docker", '''import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
with Path(os.environ["EXPRI_PROGRESS_TEST_COMMANDS"]).open("a") as output:
  output.write(json.dumps(args) + "\\n")
failure = os.environ["EXPRI_PROGRESS_TEST_FAILURE"]
if args[:2] == ["network", "create"] and failure == "network":
  print("fixture network-create failed", file=sys.stderr)
  raise SystemExit(17)
if args[0] == "cp" and args[1].endswith("key.pub") and failure == "cleanup":
  print("fixture worker-copy failed", file=sys.stderr)
  # Later diagnostic collection raises an OS error. The isolated PATH cannot
  # fall through to a real Docker engine after this executable is removed.
  Path(__file__).unlink()
  raise SystemExit(23)
''')
    self.executable("ssh-keygen", '''from pathlib import Path
import sys

key = Path(sys.argv[sys.argv.index("-f") + 1])
key.write_text("unused private fixture key")
Path(str(key) + ".pub").write_text("unused public fixture key")
''')
    env = dict(os.environ)
    env.pop("EXPRI_TEST_S3_IMAGE", None)
    env.update({
      "PATH": str(self.bin),
      "TMPDIR": str(self.tmp),
      "PYTHONDONTWRITEBYTECODE": "1",
      "EXPRI_CONTAINER_ARTIFACTS": str(self.logs),
      "GITHUB_STEP_SUMMARY": str(self.summary),
      "EXPRI_PROGRESS_TEST_COMMANDS": str(self.commands),
      "EXPRI_PROGRESS_TEST_FAILURE": failure,
    })
    workflow = Path(__file__).with_name("service_workflow.py")
    result = subprocess.run([sys.executable, "-B", str(workflow), "--no-build"],
      env=env, capture_output=True, text=True, timeout=8)
    self.assertNotEqual(result.returncode, 0)
    self.assertNotIn("Service workflow passed", result.stdout + result.stderr)
    progress = (self.logs / "workflow-progress.log").read_text()
    summary = self.summary.read_text()
    self.assertIn("FAIL Start isolated fixtures", progress)
    self.assertIn("PASS Collect diagnostics and remove fixtures", progress)
    self.assertIn("| Start isolated fixtures | FAIL |", summary)
    self.assertIn("| Collect diagnostics and remove fixtures | PASS |", summary)
    self.assertRegex(summary, r"Total elapsed: [\d.]+s\.")
    self.assertNotIn("During handling of the above exception", result.stderr)
    self.assertEqual(list(self.tmp.iterdir()), [], "temporary fixture state was retained")
    commands = [json.loads(line) for line in self.commands.read_text().splitlines()]
    return result, progress, summary, commands

  def test_real_workflow_reports_fixture_failure_and_cleanup_without_docker(self):
    result, progress, summary, commands = self.run_workflow("network")
    self.assertIn("AssertionError: docker network failed (17): fixture network-create failed", result.stderr)
    self.assertNotIn("fixture network-create failed", progress + summary)
    self.assertEqual(len(commands), 1)
    self.assertEqual(commands[0][:3], ["network", "create", "--internal"])

  def test_diagnostic_copy_failure_does_not_mask_original_workflow_failure(self):
    result, progress, summary, commands = self.run_workflow("cleanup")
    self.assertIn("AssertionError: docker cp failed (23): fixture worker-copy failed", result.stderr)
    self.assertNotIn("fixture worker-copy failed", progress + summary)
    roles = [command[command.index("--name") + 1].rsplit("-", 1)[-1]
      for command in commands if command[0] == "create"]
    self.assertEqual(roles, ["s3", "service", "host", "worker", "proxy", "browser"])
    self.assertIn("Could not collect a container log before cleanup.", progress)
    self.assertIn("Container cleanup did not finish; check the local container engine.", progress)
    self.assertIn("Network cleanup did not finish; check the local container engine.", progress)
    self.assertIn("PASS   Collect browser diagnostics and remove container", progress)
    self.assertIn("| ↳ Collect browser diagnostics and remove container | PASS |", summary)


if __name__ == "__main__":
  unittest.main()
