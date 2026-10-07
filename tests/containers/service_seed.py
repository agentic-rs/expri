import subprocess
from pathlib import Path

root = Path('/home/tester/experiment')
root.mkdir(exist_ok=True)
(root / '.gitignore').write_text('.expri/\nresults/\n.venv/\n')
(root / 'pyproject.toml').write_text('''[project]
name = "service-fixture"
version = "0.1.0"
requires-python = ">=3.12,<3.13"
dependencies = ["torch==2.10.0+cu128", "nvidia-cuda-runtime-cu12==12.8.90", "nvidia-cublas-cu12==12.8.4.1", "fixture-extra==0.1.0"]
[tool.uv]
package = false
[[tool.uv.index]]
name = "fixture"
url = "http://worker:8000/simple"
default = true
''')
(root / 'expri.toml').write_text('''[project]
name = "Service acceptance"
[environment]
base_python = "/usr/local/bin/python3"
reuse_packages = ["torch"]
require_cuda = true
[service]
client_config = "/tmp/worker.toml"
project_id = "demo"
origin = "worker"
dashboard_url = "https://expri.example.net/"
[tasks]
train = ["python", "train.py"]
fail = ["python", "-c", "raise SystemExit(7)"]
wait = ["python", "-c", "import time; print('waiting', flush=True); time.sleep(120)"]
''')
for command in [
  ['uv', 'lock', '--python', '/usr/local/bin/python3'],
  ['git', 'init', '--quiet'], ['git', 'add', '.'],
  ['git', '-c', 'user.name=Expri CI', '-c', 'user.email=ci@expri.invalid',
    '-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'fixture'],
]:
  subprocess.run(command, cwd=root, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
