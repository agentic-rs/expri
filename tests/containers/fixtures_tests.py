"""Check fixture wheel integrity and the fake probe contract without a GPU."""

import base64
import csv
from email.parser import BytesParser
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from urllib.parse import unquote, urljoin, urlparse
import zipfile

import build_wheels


class FixtureTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory(prefix="expri-fixture-wheels-")
    self.addCleanup(self.temporary.cleanup)
    self.root = Path(self.temporary.name)
    self.wheels = self.root / "wheels"
    build_wheels.build_fixtures(self.wheels)

  def test_distribution_records_and_namespace_metadata(self):
    self.assertEqual(len(list(self.wheels.glob("*.whl"))), 9)
    for path in self.wheels.glob("*.whl"):
      with self.subTest(wheel=path.name), zipfile.ZipFile(path) as wheel:
        record_path = next(name for name in wheel.namelist() if name.endswith(".dist-info/RECORD"))
        rows = list(csv.reader(io.StringIO(wheel.read(record_path).decode())))
        self.assertEqual({row[0] for row in rows}, set(wheel.namelist()))
        for filename, digest, length in rows:
          if filename == record_path:
            self.assertEqual((digest, length), ("", ""))
            continue
          content = wheel.read(filename)
          expected = base64.urlsafe_b64encode(hashlib.sha256(content).digest()).rstrip(b"=").decode()
          self.assertEqual(digest, f"sha256={expected}")
          self.assertEqual(int(length), len(content))
    for name, declared in [
      ("nvidia_cublas_cu12-12.8.4.1", "nvidia/cublas"),
      ("nvidia_cuda_runtime_cu12-12.8.90", "nvidia\\cuda_runtime"),
    ]:
      with zipfile.ZipFile(self.wheels / f"{name}-py3-none-any.whl") as wheel:
        self.assertNotIn("nvidia/__init__.py", wheel.namelist())
        self.assertEqual(wheel.read(f"{name}.dist-info/top_level.txt").decode().strip(), declared)
    with zipfile.ZipFile(self.wheels / "torch-2.10.0+cu128-py3-none-any.whl") as wheel:
      metadata = BytesParser().parsebytes(wheel.read("torch-2.10.0+cu128.dist-info/METADATA"))
      self.assertEqual(metadata.get_all("Requires-Dist"), [
        "nvidia-cuda-runtime-cu12>=12.8,<12.9", "nvidia-cublas-cu12>=12.8,<12.9",
      ])

  def probe(self, version, *, available="1"):
    installation = self.root / (version + "-" + available)
    installation.mkdir()
    names = [f"torch-{version}-py3-none-any.whl"]
    if version.endswith("cu128"):
      names.extend([
        "nvidia_cuda_runtime_cu12-12.8.90-py3-none-any.whl",
        "nvidia_cublas_cu12-12.8.4.1-py3-none-any.whl",
      ])
    for name in names:
      with zipfile.ZipFile(self.wheels / name) as wheel:
        wheel.extractall(installation)
    code = """
import importlib.metadata, json, sys
sys.path.insert(0, sys.argv[1])
import torch
if torch.cuda.is_available():
  value = torch.ones(1, device='cuda') + 1
  torch.cuda.synchronize()
  assert value.item() == 2
  assert 'CI Fake GPU' in torch.cuda.get_device_name(0)
else:
  assert (torch.ones(1, device='cpu') + 1).item() == 2
  try:
    torch.ones(1, device='cuda')
  except RuntimeError:
    pass
  else:
    raise AssertionError('unavailable fake CUDA accepted a CUDA tensor')
if torch.version.cuda == '12.8':
  import nvidia.cublas, nvidia.cuda_runtime
  assert nvidia.cublas.__version__ == '12.8.4.1'
  assert nvidia.cuda_runtime.__version__ == '12.8.90'
assert importlib.metadata.version('torch') == torch.__version__
entry = next(item for item in importlib.metadata.distribution('torch').entry_points if item.name == 'torchrun')
entry.load()()
"""
    result = subprocess.run([sys.executable, "-I", "-B", "-c", code, str(installation)],
      env={**os.environ, "EXPRI_FAKE_CUDA_AVAILABLE": available},
      capture_output=True, text=True, check=True)
    return json.loads(result.stdout), installation

  def test_cpu_cuda_and_unavailable_probe_contract(self):
    for version, available, expected in [
      ("2.9.0+cpu", "1", False), ("2.10.0+cu128", "1", True), ("2.10.0+cu128", "0", False),
    ]:
      with self.subTest(version=version, available=available):
        result, installation = self.probe(version, available=available)
        self.assertEqual(result["torch_version"], version)
        self.assertEqual(result["cuda_available"], expected)
        self.assertEqual(Path(result["torch_origin"]), installation / "torch/__init__.py")
        self.assertEqual(result["python"], sys.executable)
        self.assertIn("no GPU computation", result["fixture"])

  def test_index_only_includes_existing_wheels_and_usable_encoded_links(self):
    build_wheels.build_wheel(self.wheels, "external-fixture", "1.0", {"external_fixture.py": "value = 1\n"})
    before = {path.name: (path.read_bytes(), path.stat().st_mtime_ns) for path in self.wheels.glob("*.whl")}
    subprocess.run([sys.executable, "-B", str(Path(build_wheels.__file__)), str(self.wheels),
      "--index-root", str(self.root), "--index-only"], check=True)
    after = {path.name: (path.read_bytes(), path.stat().st_mtime_ns) for path in self.wheels.glob("*.whl")}
    self.assertEqual(before, after)
    self.assertIn('href="external-fixture/"', (self.root / "simple/index.html").read_text())
    index = self.root / "simple/torch/index.html"
    page = index.read_text()
    self.assertIn("%2Bcu128", page)
    for line in page.splitlines()[1:]:
      href = line.split('href="', 1)[1].split('"', 1)[0]
      artifact = Path(unquote(urlparse(urljoin(index.as_uri(), href)).path))
      self.assertTrue(artifact.is_file(), artifact)


if __name__ == "__main__":
  unittest.main()
