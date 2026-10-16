"""Check coordinated release metadata before building or publishing packages."""

import argparse
import ast
from pathlib import Path
import re
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]


def read_toml(path):
  with path.open("rb") as source:
    return tomllib.load(source)


def require_equal(actual, expected, label):
  if actual != expected:
    raise ValueError(f"{label}: expected {expected!r}, found {actual!r}")


def check_versions(root):
  manifest = read_toml(root / "Cargo.toml")
  workspace = manifest["workspace"]
  version = workspace["package"]["version"]
  if not re.fullmatch(r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)", version):
    raise ValueError("Release version must be a stable major.minor.patch version")

  manifests = [(root / "Cargo.toml", manifest)]
  for member in workspace["members"]:
    path = root / member / "Cargo.toml"
    manifests.append((path, read_toml(path)))
  packages = {}
  for path, metadata in manifests:
    package = metadata["package"]
    name = package["name"]
    if name in packages:
      raise ValueError(f"Duplicate workspace package: {name}")
    packages[name] = path.parent
    package_version = package["version"]
    if package_version == {"workspace": True}:
      package_version = version
    require_equal(package_version, version, str(path.relative_to(root)))

  for name, dependency in workspace["dependencies"].items():
    if name not in packages:
      continue
    require_equal(dependency.get("version"), f"={version}", f"{name} dependency pin")
    require_equal((root / dependency["path"]).resolve(), packages[name].resolve(),
                  f"{name} dependency path")

  locked = read_toml(root / "Cargo.lock")["package"]
  for name in packages:
    versions = [package["version"] for package in locked if package["name"] == name]
    require_equal(versions, [version], f"Cargo.lock {name}")

  cli_project = read_toml(root / "pyproject.toml")
  require_equal(cli_project["project"]["name"], "expri", "CLI Python package name")
  require_equal(cli_project["tool"]["maturin"]["bindings"], "bin", "CLI wheel bindings")
  if "version" not in cli_project["project"].get("dynamic", []):
    require_equal(cli_project["project"].get("version"), version, "CLI Python version")

  sdk_project = read_toml(root / "sdk/python/pyproject.toml")["project"]
  require_equal(sdk_project["name"], "expri-sdk", "Python SDK package name")
  require_equal(sdk_project["version"], version, "Python SDK version")
  module = ast.parse((root / "sdk/python/expri/__init__.py").read_text())
  sdk_versions = [ast.literal_eval(node.value) for node in module.body
                  if isinstance(node, ast.Assign)
                  and any(isinstance(target, ast.Name) and target.id == "__version__"
                          for target in node.targets)]
  require_equal(sdk_versions, [version], "Python SDK __version__")
  return version


def eligible_release_event(event_name, ref, version):
  expected = f"refs/tags/v{version}"
  if ref.startswith("refs/tags/") and ref != expected:
    raise ValueError(f"Release tag must be {expected}, found {ref}")
  return event_name == "push" and ref == expected


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("command", choices=["check"])
  parser.add_argument("--github-event", default="")
  parser.add_argument("--github-ref", default="")
  parser.add_argument("--github-output", type=Path)
  args = parser.parse_args()
  try:
    version = check_versions(ROOT)
    eligible = eligible_release_event(args.github_event, args.github_ref, version)
    subprocess.run([sys.executable, "-B", str(ROOT / "scripts/sync_python_sdk.py"), "--check"],
                   check=True)
  except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
    parser.exit(1, f"Release check failed: {error}\n")
  if args.github_output:
    with args.github_output.open("a") as output:
      output.write(f"version={version}\neligible_release_event={str(eligible).lower()}\n")
  print(f"Release metadata aligned at {version}; eligible release event: {str(eligible).lower()}")
  return 0


if __name__ == "__main__":
  raise SystemExit(main())
