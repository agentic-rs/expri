"""Keep every host/worker scenario when CI supplies prebuilt images."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCENARIOS = [
  "matched-native", "matched-python", "matched-auto", "torch-mismatch",
  "dependency-mismatch", "cuda-unavailable",
]


class PrebuiltImagesTests(unittest.TestCase):
  def setUp(self):
    temporary = tempfile.TemporaryDirectory(prefix="expri-prebuilt-images-")
    self.addCleanup(temporary.cleanup)
    self.root = Path(temporary.name)
    self.bin = self.root / "bin"
    self.bin.mkdir()
    self.tmp = self.root / "tmp"
    self.tmp.mkdir()
    self.logs = self.root / "logs"
    self.commands = self.root / "commands.jsonl"

  def executable(self, name, source):
    path = self.bin / name
    path.write_text(f"#!{sys.executable}\n{source}")
    path.chmod(0o700)

  def run_workflow(self, scenarios=()):
    self.executable("docker", '''import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
with Path(os.environ["EXPRI_BUILD_CACHE_TEST_COMMANDS"]).open("a") as output:
  output.write(json.dumps(args) + "\\n")
if args[0] in {"build", "buildx", "pull"}:
  raise SystemExit("prebuilt workflow attempted to rebuild or pull an image")
if args[:2] == ["inspect", "--format"]:
  assert args[2] == "{{.State.ExitCode}}", args
  print("0")
elif args[0] == "exec":
  if args[2:] == ["cat", "/etc/ssh/ssh_host_ed25519_key.pub"]:
    print("ssh-ed25519 unused-host-fixture-key")
  else:
    assert args[2:] == ["test", "-f", "/tmp/expri-worker.ready"], args
elif args[0] not in {"network", "create", "cp", "start", "logs", "rm"}:
  raise SystemExit(f"unexpected Docker command: {args!r}")
''')
    self.executable("ssh-keygen", '''from pathlib import Path
import sys

key = Path(sys.argv[sys.argv.index("-f") + 1])
key.write_text("unused private fixture key")
Path(str(key) + ".pub").write_text("unused public fixture key")
''')
    environment = {**os.environ,
      "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
      "TMPDIR": str(self.tmp),
      "EXPRI_CONTAINER_ARTIFACTS": str(self.logs),
      "EXPRI_BUILD_CACHE_TEST_COMMANDS": str(self.commands),
      "PYTHONDONTWRITEBYTECODE": "1",
    }
    runner = Path(__file__).with_name("run.sh")
    result = subprocess.run([shutil.which("bash"), str(runner), "--no-build", *scenarios],
      env=environment, capture_output=True, text=True, timeout=30)
    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
    self.assertIn("All container scenarios passed.", result.stdout)
    self.assertEqual(list(self.tmp.iterdir()), [], "temporary fixture state was retained")
    commands = [json.loads(line) for line in self.commands.read_text().splitlines()]
    self.assertFalse(any(command[0] in {"build", "buildx", "pull"} for command in commands))
    self.assertEqual(commands[0][:3], ["network", "create", "--internal"])
    return commands

  def assert_scenarios_executed(self, commands, expected):
    creates = [command for command in commands if command[0] == "create"]
    self.assertEqual(len(creates), 2 * len(expected))
    for image in ["expri-ci-worker", "expri-ci-host"]:
      selected = [command for command in creates if command[-1] == image]
      self.assertEqual([
        command[command.index("--env") + 1] for command in selected
      ], [f"EXPRI_TEST_CASE={scenario}" for scenario in expected])
    host_names = [command[command.index("--name") + 1]
      for command in creates if command[-1] == "expri-ci-host"]
    attached_hosts = [command[-1] for command in commands if command[:2] == ["start", "--attach"]]
    self.assertEqual(attached_hosts, host_names)
    for scenario in expected:
      self.assertTrue((self.logs / f"{scenario}.host.log").is_file())
      self.assertTrue((self.logs / f"{scenario}.worker.log").is_file())

  def test_prebuilt_images_run_every_existing_scenario(self):
    self.assert_scenarios_executed(self.run_workflow(), SCENARIOS)

  def test_prebuilt_images_preserve_requested_scenarios_and_order(self):
    selected = ["cuda-unavailable", "matched-python"]
    self.assert_scenarios_executed(self.run_workflow(selected), selected)


if __name__ == "__main__":
  unittest.main()
