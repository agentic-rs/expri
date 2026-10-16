"""Release metadata and publication boundary regression tests."""

import importlib.util
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("release", ROOT / "scripts/release.py")
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
  def setUp(self):
    self.temp = tempfile.TemporaryDirectory()
    self.addCleanup(self.temp.cleanup)
    self.root = Path(self.temp.name)
    paths = [ROOT / "Cargo.toml", ROOT / "Cargo.lock", ROOT / "pyproject.toml",
             ROOT / "sdk/python/pyproject.toml", ROOT / "sdk/python/expri/__init__.py"]
    paths.extend(ROOT.glob("crates/*/Cargo.toml"))
    paths.append(ROOT / "sdk/rust/Cargo.toml")
    paths.extend(ROOT / relative_path for relative_path in [
      "scripts/release.py", "scripts/sync_python_sdk.py",
      "sdk/python/expri_metrics.py", "sdk/python/expri_run.py",
      "crates/worker/python/expri_metrics.py", "crates/worker/python/expri_run.py",
    ])
    for path in paths:
      target = self.root / path.relative_to(ROOT)
      target.parent.mkdir(parents=True, exist_ok=True)
      shutil.copyfile(path, target)
    self.version = release.read_toml(self.root / "Cargo.toml")["workspace"]["package"]["version"]

  def replace(self, relative_path, old, new):
    path = self.root / relative_path
    contents = path.read_text()
    self.assertIn(old, contents)
    path.write_text(contents.replace(old, new, 1))

  def test_current_release_metadata_is_consistent(self):
    self.assertEqual(release.check_versions(self.root), self.version)

  def test_python_sdk_mismatch_is_rejected(self):
    self.replace("sdk/python/pyproject.toml", f'version = "{self.version}"', 'version = "9.9.9"')
    with self.assertRaisesRegex(ValueError, "Python SDK version"):
      release.check_versions(self.root)

  def test_python_runtime_version_mismatch_is_rejected(self):
    self.replace("sdk/python/expri/__init__.py", f'__version__ = "{self.version}"',
                 '__version__ = "9.9.9"')
    with self.assertRaisesRegex(ValueError, "Python SDK __version__"):
      release.check_versions(self.root)

  def test_rust_sdk_mismatch_is_rejected(self):
    self.replace("sdk/rust/Cargo.toml", f'version = "{self.version}"', 'version = "9.9.9"')
    with self.assertRaisesRegex(ValueError, "sdk/rust/Cargo.toml"):
      release.check_versions(self.root)

  def test_stale_internal_dependency_pin_is_rejected(self):
    self.replace("Cargo.toml", f'version = "={self.version}"', 'version = "=9.9.9"')
    with self.assertRaisesRegex(ValueError, "dependency pin"):
      release.check_versions(self.root)

  def test_stale_lockfile_is_rejected(self):
    self.replace("Cargo.lock", f'name = "expri"\nversion = "{self.version}"',
                 'name = "expri"\nversion = "9.9.9"')
    with self.assertRaisesRegex(ValueError, "Cargo.lock expri"):
      release.check_versions(self.root)

  def test_only_an_actual_matching_tag_push_is_an_eligible_release_event(self):
    tag = f"refs/tags/v{self.version}"
    self.assertTrue(release.eligible_release_event("push", tag, self.version))
    for event in ["workflow_dispatch", "pull_request", "workflow_call", "release", ""]:
      with self.subTest(event=event):
        self.assertFalse(release.eligible_release_event(event, tag, self.version))
    self.assertFalse(release.eligible_release_event("push", "refs/heads/main", self.version))

  def test_mismatched_tag_is_rejected_before_building(self):
    for event in ["push", "workflow_dispatch"]:
      with self.subTest(event=event), self.assertRaisesRegex(ValueError, "Release tag must"):
        release.eligible_release_event(event, "refs/tags/v9.9.9", self.version)

  def test_manual_dispatch_of_a_tag_stays_build_only(self):
    output = self.root / "github-output"
    subprocess.run([sys.executable, "-B", str(self.root / "scripts/release.py"), "check",
                    "--github-event", "workflow_dispatch", "--github-ref",
                    f"refs/tags/v{self.version}", "--github-output", str(output)],
                   check=True, capture_output=True, text=True)
    self.assertEqual(output.read_text(),
                     f"version={self.version}\neligible_release_event=false\n")

  def test_generated_sdk_drift_fails_before_writing_outputs(self):
    with (self.root / "crates/worker/python/expri_metrics.py").open("a") as helper:
      helper.write("\n# stale generated content\n")
    output = self.root / "github-output"
    result = subprocess.run([sys.executable, "-B", str(self.root / "scripts/release.py"),
                             "check", "--github-output", str(output)],
                            capture_output=True, text=True)
    self.assertNotEqual(result.returncode, 0)
    self.assertIn("Stale embedded Python SDK helpers", result.stderr)
    self.assertFalse(output.exists())


if __name__ == "__main__":
  unittest.main()
