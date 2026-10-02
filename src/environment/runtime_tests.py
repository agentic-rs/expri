"""Dependency-graph regressions and offline uv integration for runtime.py."""

import copy
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import runtime


MARKERS = {
  "implementation_name": "cpython",
  "implementation_version": "3.12.3",
  "os_name": "posix",
  "platform_machine": "x86_64",
  "platform_release": "test",
  "platform_system": "Linux",
  "platform_version": "test",
  "python_full_version": "3.12.3",
  "platform_python_implementation": "CPython",
  "python_version": "3.12",
  "sys_platform": "linux",
}


def package(name, version="1.0", requires=(), extras=(), prefix="/base"):
  return {
    "name": name,
    "version": version,
    "requires": list(requires),
    "requires_python": ">=3.8",
    "extras": list(extras),
    "metadata_path": f"{prefix}/{name}-{version}.dist-info",
    "modules": [name.replace("-", "_")],
  }


def inventory(*packages):
  return {
    "python": "/base/bin/python",
    "python_prefix": "/base",
    "base_prefix": "/base",
    "marker_env": MARKERS.copy(),
    "packages": {runtime.canonicalize_name(item["name"]): item for item in packages},
    "origins": {},
    "torch": None,
  }


def locked(text):
  return runtime.exported_requirements(text, MARKERS)


class DependencyGraphTests(unittest.TestCase):
  def test_inherited_closure_tracks_transitive_extras_and_target_markers(self):
    base = inventory(
      package("root", requires=["child[gpu]>=1", "windows-only; sys_platform == 'win32'"]),
      package("child", requires=["cuda-runtime==1.0; extra == 'gpu'", "linux-only; sys_platform == 'linux'"], extras=["gpu"]),
      package("cuda-runtime"),
      package("linux-only"),
    )
    closure = runtime.inherited_closure(base, ["root"])
    self.assertEqual(set(closure), {"root", "child", "cuda-runtime", "linux-only"})
    self.assertEqual(closure["child"], {"gpu"})

  def test_closure_revisits_packages_when_another_parent_activates_extras(self):
    base = inventory(
      package("left", requires=["shared"]),
      package("right", requires=["shared[accelerated]"]),
      package("shared", requires=["native; extra == 'accelerated'"], extras=["accelerated"]),
      package("native", requires=["shared"]),
    )
    self.assertEqual(set(runtime.inherited_closure(base, ["left", "right"])), {"left", "right", "shared", "native"})

  def test_missing_or_incompatible_inherited_dependency_fails(self):
    for dependency in ["missing", "child>=2"]:
      with self.subTest(dependency=dependency), self.assertRaises(runtime.RuntimeErrorDetail):
        runtime.inherited_closure(inventory(package("root", requires=[dependency]), package("child")), ["root"])

  def test_unknown_reused_extra_fails(self):
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "does not declare requested extras"):
      runtime.inherited_closure(inventory(package("root")), ["root[gpu]"])

  def test_lock_selection_uses_base_platform_and_python_markers(self):
    selected = locked("native==1.0; sys_platform == 'linux'\nnative==2.0; sys_platform == 'win32'\nold==1.0; python_version < '3.12'\n")
    self.assertEqual(set(selected), {"native"})
    self.assertEqual(str(selected["native"].specifier), "==1.0")

  def test_local_cuda_version_suffix_must_match_exact_lock_variant(self):
    base = inventory(package("torch", "2.5.0+cu121"))
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "uv.lock selects"):
      runtime.validate_locked_reuse(base, {"torch": set()}, locked("torch==2.5.0\n"))
    runtime.validate_locked_reuse(base, {"torch": set()}, locked("torch==2.5.0+cu121\n"))

  def test_reused_url_dependency_cannot_be_certified_by_version_only(self):
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "single exact"):
      runtime.validate_locked_reuse(inventory(package("native")), {"native": set()}, locked("native @ https://example.invalid/native.whl\n"))

  def test_reused_torch_cannot_silently_download_uninherited_cuda_wheels(self):
    base = inventory(package("torch", "2.5.0"))
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "will not silently download a second CUDA stack"):
      runtime.validate_locked_reuse(base, {"torch": set()}, locked("torch==2.5.0\nnvidia-cuda-runtime-cu12==12.1\n"))

  def test_complete_inherited_cuda_wheel_closure_matches_locked_graph(self):
    base = inventory(
      package("torch", "2.5.0", requires=["nvidia-cuda-runtime-cu12==12.1"]),
      package("nvidia-cuda-runtime-cu12", "12.1"),
    )
    closure = runtime.inherited_closure(base, ["torch"])
    runtime.validate_locked_reuse(base, closure, locked("torch==2.5.0\nnvidia-cuda-runtime-cu12==12.1\n"))

  def test_combined_graph_rejects_overlay_breaking_inherited_requirement(self):
    base = inventory(package("torch", requires=["numpy<2"]), package("numpy"))
    combined = copy.deepcopy(base)
    combined["packages"]["numpy"] = package("numpy", "2.0", prefix="/overlay")
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "numpy<2"):
      runtime.validate_combined(combined, locked("numpy==2.0"), {"torch": set()}, base)

  def test_unrelated_broken_base_package_does_not_reject_declared_run(self):
    base = inventory(package("reused"), package("unrelated", requires=["missing"]))
    runtime.validate_combined(base, locked("reused==1.0"), {"reused": set()}, base)

  def test_managed_editable_export_entries_leave_installation_to_uv_locked(self):
    requirements = runtime.exported_requirements("-e ../local-package\nindexed==1.0\n", MARKERS, strict=False)
    self.assertEqual(set(requirements), {"indexed"})
    combined = inventory(package("indexed"), package("project", requires=["local-package @ file:///local-package"]), package("local-package"))
    runtime.validate_combined(combined, requirements, {}, inventory(), project_name="project")
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "editable/path lock entries"):
      runtime.exported_requirements("-e ../local-package\n", MARKERS)

  def test_combined_graph_checks_own_project_extras(self):
    base = inventory(package("reused"))
    combined = inventory(package("reused"), package("project", requires=["optional; extra == 'gpu'"], extras=["gpu"]))
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "optional"):
      runtime.validate_combined(combined, {}, {"reused": set()}, base, project_name="project", project_extras=["gpu"])

  def test_metadata_and_module_shadowing_fail(self):
    base = inventory(package("reused"))
    base["origins"] = {"reused": {"origin": "/base/reused.py", "search_locations": []}}
    for shadow_metadata in [True, False]:
      combined = copy.deepcopy(base)
      if shadow_metadata:
        combined["packages"]["reused"] = package("reused", prefix="/overlay")
      else:
        combined["origins"]["reused"]["origin"] = "/repo/reused.py"
      with self.subTest(shadow_metadata=shadow_metadata), self.assertRaisesRegex(runtime.RuntimeErrorDetail, "shadows reused"):
        runtime.validate_combined(combined, {}, {"reused": set()}, base)

  def test_editable_build_requirements_are_checked_without_installing(self):
    project = {"build-system": {"requires": ["backend>=2"]}}
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "will not download isolated build dependencies"):
      runtime.ensure_build_requirements(project, inventory(package("backend")))
    runtime.ensure_build_requirements(project, inventory(package("backend", "2.0")))

  def test_sync_args_cannot_weaken_graph_or_redirect_environment(self):
    for argument in ["--frozen", "--no-install-package=child", "--active", "--python=/base/python", "--no-build-isolation", "--inexact"]:
      with self.subTest(argument=argument), self.assertRaises(runtime.RuntimeErrorDetail):
        runtime.normalize_sync_args([argument])
    self.assertEqual(runtime.normalize_sync_args(["--extra=gpu", "--no-dev", "--offline", "--locked"]), ["--extra", "gpu", "--no-dev", "--offline"])

  def test_ambient_python_paths_are_removed_explicit_native_env_is_preserved(self):
    with patch.dict(os.environ, {"PYTHONPATH": "/ambient", "UV_PROJECT": "/other", "VIRTUAL_ENV": "/helper"}):
      env = runtime.runtime_env({"env": {"PYTHONPATH": "/explicit", "CUDA_HOME": "/cuda"}})
    self.assertEqual(env["PYTHONPATH"], "/explicit")
    self.assertEqual(env["CUDA_HOME"], "/cuda")
    self.assertNotIn("UV_PROJECT", env)
    self.assertNotIn("VIRTUAL_ENV", env)
    self.assertEqual(env["PYTHONNOUSERSITE"], "1")


@unittest.skipUnless(shutil.which("uv"), "uv is required for offline environment integration")
class UvIntegrationTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory(prefix="expri-runtime-tests-")
    self.root = Path(self.temporary.name)
    self.repo = self.root / "repo"
    self.repo.mkdir()
    (self.repo / "pyproject.toml").write_text(f'[project]\nname = "fixture-project"\nversion = "1.0"\nrequires-python = ">={sys.version_info.major}.{sys.version_info.minor}"\ndependencies = []\n')
    self.cache = self.root / "cache"
    self.cache_patch = patch.dict(os.environ, {"UV_CACHE_DIR": str(self.cache)})
    self.cache_patch.start()
    self.env = {**os.environ, "UV_CACHE_DIR": str(self.cache)}
    result = subprocess.run(["uv", "lock", "--offline", "--python", sys.executable], cwd=self.repo, env=self.env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
      raise RuntimeError(result.stderr)
    self.request = {
      "repo_root": str(self.repo),
      "state_dir": str(self.root / "state"),
      "operation": "setup",
      "environment": {"base_python": sys.executable},
      "sync_args": ["--offline", "--cache-dir", str(self.cache)],
      "install_project": False,
    }

  def tearDown(self):
    self.cache_patch.stop()
    self.temporary.cleanup()

  def test_managed_prepare_validate_and_metadata_drift(self):
    result = runtime.prepare(self.request)
    self.assertTrue(Path(result["python"]).exists())
    self.assertTrue(Path(result["manifest_path"]).exists())
    self.assertEqual(runtime.prepare({**self.request, "operation": "validate"})["environment_path"], result["environment_path"])
    (self.repo / "pyproject.toml").write_text((self.repo / "pyproject.toml").read_text() + "\n[tool.test]\nchanged = true\n")
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "does not match"):
      runtime.prepare({**self.request, "operation": "validate"})
    self.assertFalse(Path(result["manifest_path"]).exists())

  def test_reuses_real_base_pip_without_installing_it_in_overlay(self):
    try:
      baseline = runtime.inspect_python(sys.executable, self.repo, runtime.runtime_env({}))
    except runtime.RuntimeErrorDetail as error:
      self.skipTest(str(error))
    if "pip" not in baseline["packages"]:
      self.skipTest("base interpreter has no pip package for reuse fixture")
    request = {**self.request, "environment": {"base_python": sys.executable, "reuse_packages": ["pip"]}}
    result = runtime.prepare(request)
    manifest = json.loads(Path(result["manifest_path"]).read_text())
    self.assertEqual(manifest["combined_manifest"]["packages"]["pip"]["metadata_path"], baseline["packages"]["pip"]["metadata_path"])
    self.assertEqual(manifest["reuse_packages"], ["pip"])
    self.assertIn("include-system-site-packages = true", (Path(result["environment_path"]) / "pyvenv.cfg").read_text())
    self.assertFalse(any(Path(result["environment_path"]).glob("lib/python*/site-packages/pip-*.dist-info")))
    if any(entry["name"] == "pip" for entry in baseline["packages"]["pip"].get("entry_points", [])):
      command = Path(result["environment_path"]) / "bin" / "pip"
      self.assertIn(str(Path(result["environment_path"]) / "bin" / "python"), command.read_text())
      output = subprocess.check_output([str(command), "--version"], cwd=self.root, env={**os.environ, **result["run_env"]}, text=True)
      self.assertIn("pip " + baseline["packages"]["pip"]["version"], output)
    after = runtime.inspect_python(sys.executable, self.repo, runtime.runtime_env({}))
    self.assertEqual(after["packages"], baseline["packages"])
    self.assertEqual(runtime.prepare({**request, "operation": "validate"})["reused_packages"], ["pip"])

  def test_own_project_editable_build_uses_existing_build_requirements(self):
    baseline = runtime.inspect_python(sys.executable, self.repo, runtime.runtime_env({}))
    if "pip" not in baseline["packages"]:
      self.skipTest("base interpreter has no pip package for reuse fixture")
    (self.repo / "pyproject.toml").write_text((self.repo / "pyproject.toml").read_text() + '\n[build-system]\nrequires = ["pip>=24"]\nbuild-backend = "fixture_backend"\nbackend-path = ["."]\n')
    (self.repo / "fixture_project.py").write_text("value = 42\n")
    (self.repo / "fixture_backend.py").write_text('''
from pathlib import Path
import zipfile

def build_editable(wheel_directory, config_settings=None, metadata_directory=None):
  name = "fixture_project-1.0-py3-none-any.whl"
  metadata = "fixture_project-1.0.dist-info"
  with zipfile.ZipFile(Path(wheel_directory) / name, "w") as wheel:
    wheel.writestr(metadata + "/METADATA", "Metadata-Version: 2.1\\nName: fixture-project\\nVersion: 1.0\\n")
    wheel.writestr(metadata + "/WHEEL", "Wheel-Version: 1.0\\nGenerator: fixture\\nRoot-Is-Purelib: true\\nTag: py3-none-any\\n")
    wheel.writestr(metadata + "/RECORD", "")
    wheel.writestr("fixture_project.pth", str(Path(__file__).parent) + "\\n")
  return name

build_wheel = build_editable
''')
    subprocess.run(["uv", "lock", "--offline", "--python", sys.executable], cwd=self.repo, env=self.env, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    request = {
      **self.request,
      "operation": "run",
      "install_project": True,
      "environment": {"base_python": sys.executable, "reuse_packages": ["pip"]},
    }
    result = runtime.prepare(request)
    output = subprocess.check_output([result["python"], "-c", "import fixture_project; print(fixture_project.value)"], cwd=self.root, text=True)
    self.assertEqual(output.strip(), "42")
    manifest = json.loads(Path(result["manifest_path"]).read_text())
    self.assertIn("fixture-project", manifest["combined_manifest"]["packages"])
    self.assertTrue(manifest["install_project"])
    self.assertEqual(runtime.inspect_python(sys.executable, self.repo, runtime.runtime_env({}))["packages"], baseline["packages"])

  def test_origin_drift_invalidates_previously_current_manifest(self):
    baseline = runtime.inspect_python(sys.executable, self.repo, runtime.runtime_env({}))
    if "pip" not in baseline["packages"]:
      self.skipTest("base interpreter has no pip package for reuse fixture")
    request = {**self.request, "environment": {"base_python": sys.executable, "reuse_packages": ["pip"]}}
    result = runtime.prepare(request)
    (self.repo / "pip.py").write_text("# This source file must not override the reused package.\n")
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "shadows reused module"):
      runtime.prepare({**request, "operation": "validate"})
    self.assertFalse(Path(result["manifest_path"]).exists())

  def test_optional_driver_inventory_runs_only_when_torch_is_probed(self):
    bin_path = self.root / "bin"
    bin_path.mkdir()
    sentinel = self.root / "driver-probed"
    command = bin_path / "nvidia-smi"
    command.write_text(f'#!/bin/sh\ntouch "{sentinel}"\nprintf "Fake GPU, 555.10\\n"\n')
    command.chmod(0o755)
    (self.repo / "torch.py").write_text('__version__ = "2.0"\nclass version:\n  cuda = "12.1"\nclass cuda:\n  @staticmethod\n  def is_available():\n    return False\n')
    env = runtime.runtime_env({"env": {"PYTHONPATH": str(self.repo), "PATH": str(bin_path) + os.pathsep + os.environ.get("PATH", "")}})
    plain = runtime.inspect_python(sys.executable, self.repo, env, isolated=False, include_packages=False)
    self.assertIsNone(plain["gpu_driver"])
    self.assertFalse(sentinel.exists())
    probed = runtime.inspect_python(sys.executable, self.repo, env, isolated=False, torch=True, include_packages=False)
    self.assertEqual(probed["gpu_driver"], [{"name": "Fake GPU", "driver_version": "555.10"}])
    self.assertTrue(sentinel.exists())

  def test_unowned_environment_is_never_modified(self):
    environment_path = self.root / "state" / "environment" / ".venv"
    environment_path.mkdir(parents=True)
    sentinel = environment_path / "sentinel"
    sentinel.write_text("keep")
    with self.assertRaisesRegex(runtime.RuntimeErrorDetail, "unowned"):
      runtime.prepare(self.request)
    self.assertEqual(sentinel.read_text(), "keep")

  def test_changed_inheritance_mode_recreates_only_owned_environment(self):
    baseline = runtime.inspect_python(sys.executable, self.repo, runtime.runtime_env({}))
    if "pip" not in baseline["packages"]:
      self.skipTest("base interpreter has no pip package for reuse fixture")
    result = runtime.prepare(self.request)
    sentinel = Path(result["environment_path"]) / "sentinel"
    sentinel.write_text("old")
    request = {**self.request, "environment": {"base_python": sys.executable, "reuse_packages": ["pip"]}}
    reused = runtime.prepare(request)
    self.assertFalse(sentinel.exists())
    self.assertIn("include-system-site-packages = true", (Path(reused["environment_path"]) / "pyvenv.cfg").read_text())


if __name__ == "__main__":
  unittest.main()
