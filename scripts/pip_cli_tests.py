"""Inspect the native CLI wheel and exercise independent CLI/SDK installations."""

import argparse
from email.parser import BytesParser
import os
from pathlib import Path
import subprocess
import tempfile
import venv
import zipfile


NATIVE_MAGICS = (
  b"\x7fELF",
  b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe",
  b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe",
  b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca",
  b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca",
)


def inspect_wheel(path, distribution, version):
  with zipfile.ZipFile(path) as wheel:
    prefix = f"{distribution.replace('-', '_')}-{version}"
    metadata = BytesParser().parsebytes(wheel.read(f"{prefix}.dist-info/METADATA"))
    assert metadata["Name"] == distribution, metadata["Name"]
    assert metadata["Version"] == version, metadata["Version"]
    assert not metadata.get_all("Requires-Dist"), "unexpected runtime dependency"
    if distribution == "expri":
      script = f"{prefix}.data/scripts/expri"
      assert wheel.read(script)[:4] in NATIVE_MAGICS, "CLI must be a native executable"
      assert wheel.getinfo(script).external_attr >> 16 & 0o111, "CLI is not executable"
      for name in wheel.namelist():
        assert (name == script or name.startswith(f"{prefix}.dist-info/")
                or name.startswith("expri.libs/")), f"unexpected CLI wheel file: {name}"
      tags = BytesParser().parsebytes(wheel.read(f"{prefix}.dist-info/WHEEL")).get_all("Tag")
      assert tags and all(tag.startswith("py3-none-") and not tag.endswith("-any")
                          for tag in tags), tags
    else:
      assert "expri/__init__.py" in wheel.namelist(), "SDK must own the expri package"
      assert not any(".data/scripts/" in name for name in wheel.namelist()), "SDK owns a command"


def isolated_environment(scripts):
  environment = dict(os.environ)
  for name in list(environment):
    if name in ("PYTHONPATH", "PYTHONHOME") or name.startswith(("EXPRI_", "PIP_")):
      del environment[name]
  environment.update({
    "PATH": str(scripts) + os.pathsep + os.defpath,
    "PYTHONDONTWRITEBYTECODE": "1",
    "PIP_CONFIG_FILE": os.devnull,
    "PIP_DISABLE_PIP_VERSION_CHECK": "1",
  })
  return environment


CLI_CHECK = """
import importlib.metadata
import os
from pathlib import Path
import shutil
import subprocess
import sys
import sysconfig

version = sys.argv[1]
executable = Path(sysconfig.get_path('scripts')) / 'expri'
assert Path(shutil.which('expri')).resolve() == executable.resolve()
assert executable.is_file() and os.access(executable, os.X_OK)
assert executable.read_bytes()[:2] != b'#!', 'CLI is unexpectedly a Python wrapper'
distribution = importlib.metadata.distribution('expri')
assert distribution.version == version
recorded = [Path(distribution.locate_file(name)).resolve() for name in distribution.files]
assert recorded.count(executable.resolve()) == 1, 'native command missing from RECORD'
assert subprocess.check_output(['expri', '--version'], text=True).strip() == 'expri ' + version
"""

SDK_CHECK = """
import importlib.metadata
import json
from pathlib import Path
import sys
import expri

assert importlib.metadata.version('expri-sdk') == sys.argv[1] == expri.__version__
assert Path(expri.__file__).is_relative_to(Path(sys.prefix))
with expri.init(output_dir='outputs') as run:
  assert run.initial_step == 0
  run.params({'batch_size': 32})
  run.log(0, {'loss': 0.5})
row = json.loads(Path('outputs/metrics.jsonl').read_text())
assert row['step'] == 0 and row['metrics'] == {'loss': 0.5}
"""

NO_SDK_CHECK = """
import importlib.util
assert importlib.util.find_spec('expri') is None, 'CLI distribution owns a Python package'
"""


def exercise_installations(cli_wheel, sdk_wheel, version):
  with tempfile.TemporaryDirectory(prefix="expri-pip-cli-") as temporary:
    root = Path(temporary).resolve()
    venv.EnvBuilder(with_pip=True).create(root / "venv")
    scripts = root / "venv/bin"
    python = scripts / "python"
    environment = isolated_environment(scripts)

    def python_command(*arguments):
      subprocess.run([str(python), "-I", *arguments], check=True, cwd=root, env=environment)

    def install(wheel):
      python_command("-m", "pip", "install", "--no-index", "--no-deps", str(wheel))

    install(cli_wheel)
    python_command("-c", CLI_CHECK, version)
    python_command("-c", NO_SDK_CHECK)
    install(sdk_wheel)
    python_command("-c", CLI_CHECK, version)
    python_command("-c", SDK_CHECK, version)
    python_command("-c", """
import importlib.metadata
from pathlib import Path

def owned(name):
  distribution = importlib.metadata.distribution(name)
  return {Path(distribution.locate_file(path)).resolve() for path in distribution.files}

assert not owned('expri') & owned('expri-sdk'), 'CLI and SDK own overlapping paths'
""")
    python_command("-m", "pip", "uninstall", "--yes", "expri")
    python_command("-c", "import expri; assert expri.__version__ == __import__('sys').argv[1]", version)
    install(cli_wheel)
    python_command("-m", "pip", "uninstall", "--yes", "expri-sdk")
    python_command("-c", CLI_CHECK, version)
    python_command("-c", NO_SDK_CHECK)


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("--cli-wheel", type=Path, required=True)
  parser.add_argument("--sdk-wheel", type=Path, required=True)
  parser.add_argument("--expected-version", required=True)
  parser.add_argument("--inspect-only", action="store_true",
                      help="Inspect archives without executing or installing their contents")
  args = parser.parse_args()
  cli_wheel = args.cli_wheel.resolve(strict=True)
  sdk_wheel = args.sdk_wheel.resolve(strict=True)
  for wheel in (cli_wheel, sdk_wheel):
    if wheel.suffix != ".whl":
      parser.error("both package paths must be wheels")
  inspect_wheel(cli_wheel, "expri", args.expected_version)
  inspect_wheel(sdk_wheel, "expri-sdk", args.expected_version)
  if not args.inspect_only:
    exercise_installations(cli_wheel, sdk_wheel, args.expected_version)
  print("Native CLI and Python SDK wheel checks passed")


if __name__ == "__main__":
  main()
