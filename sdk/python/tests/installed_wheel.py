"""Install and exercise a built wheel from outside the checkout, without an index."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import venv


def main():
  wheel = Path(sys.argv[1]).resolve(strict=True)
  if wheel.suffix != ".whl":
    raise ValueError("pass the built expri-sdk wheel path")
  with tempfile.TemporaryDirectory(prefix="expri-installed-sdk-") as temporary:
    root = Path(temporary).resolve()
    environment = dict(os.environ)
    for name in list(environment):
      if name == "PYTHONPATH" or name.startswith("EXPRI_"):
        del environment[name]
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    venv.EnvBuilder(with_pip=True).create(root / "venv")
    python = root / "venv/bin/python"
    subprocess.run([str(python), "-m", "pip", "install", "--no-index", "--no-deps", str(wheel)],
                   check=True, cwd=root, env=environment)
    subprocess.run([str(python), "-I", "-c", """
import importlib.metadata
import json
from pathlib import Path
import expri
import expri_metrics
import expri_run

assert importlib.metadata.version('expri-sdk') == expri.__version__
assert expri.MetricsLogger is expri_metrics.MetricsLogger
assert expri.set_run_status is expri_run.set_run_status
assert 'site-packages' in str(Path(expri.__file__))
assert Path(expri.__file__).with_name('py.typed').is_file()
output = Path('outputs')
output.mkdir()
prefix = b'{"step":1000,"metrics":{"loss":0.5}}\\n'
(output / 'metrics.jsonl').write_bytes(prefix)
with expri.init(output_dir=output) as run:
  assert run.initial_step == 1001
  run.params({'batch_size': 32})
  run.log(0, {'loss': 0.4})
  run.log(10, {'loss': 0.3})
  run.log(1200, {'loss': 0.2}, step_mode='global')
  run.flush()
assert run.closed
raw = (output / 'metrics.jsonl').read_bytes()
assert raw.startswith(prefix)
rows = [json.loads(row) for row in raw.splitlines()]
assert [row['step'] for row in rows] == [1000, 1001, 1011, 1200]
assert 'local_step' not in rows[-1]
assert json.loads((output / 'params.json').read_bytes())['params'] == {'batch_size': 32}
print('Installed Python SDK wheel smoke passed')
"""], check=True, cwd=root, env=environment)


if __name__ == "__main__":
  main()
