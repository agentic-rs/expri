"""Build tiny real wheel distributions and an optional static PEP 503 index."""

import argparse
import base64
import csv
from email.parser import BytesParser
import hashlib
import html
import io
import os
from pathlib import Path
import re
from urllib.parse import quote
import zipfile


def canonicalize(name):
  return re.sub(r"[-_.]+", "-", name).lower()


def build_wheel(directory, name, version, files, *, requires=(), top_level=None, entry_points=None):
  distribution = canonicalize(name).replace("-", "_")
  metadata_dir = f"{distribution}-{version}.dist-info"
  contents = {path: content.encode("utf-8") for path, content in files.items()}
  metadata = [
    "Metadata-Version: 2.1", f"Name: {name}", f"Version: {version}",
    "Summary: expri CI fixture; no GPU computation", "Requires-Python: >=3.10",
  ]
  metadata.extend(f"Requires-Dist: {requirement}" for requirement in requires)
  contents[f"{metadata_dir}/METADATA"] = ("\n".join(metadata) + "\n").encode()
  contents[f"{metadata_dir}/WHEEL"] = (
    "Wheel-Version: 1.0\nGenerator: expri-ci-fixtures\nRoot-Is-Purelib: true\nTag: py3-none-any\n"
  ).encode()
  if top_level is not None:
    contents[f"{metadata_dir}/top_level.txt"] = (top_level + "\n").encode()
  if entry_points:
    contents[f"{metadata_dir}/entry_points.txt"] = (
      "[console_scripts]\n" + "\n".join(f"{key} = {value}" for key, value in sorted(entry_points.items())) + "\n"
    ).encode()
  record_path = f"{metadata_dir}/RECORD"
  record = io.StringIO(newline="")
  writer = csv.writer(record, lineterminator="\n")
  for path, content in sorted(contents.items()):
    digest = base64.urlsafe_b64encode(hashlib.sha256(content).digest()).rstrip(b"=").decode()
    writer.writerow([path, f"sha256={digest}", str(len(content))])
  writer.writerow([record_path, "", ""])
  contents[record_path] = record.getvalue().encode()
  path = directory / f"{distribution}-{version}-py3-none-any.whl"
  with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as wheel:
    for filename, content in sorted(contents.items()):
      # Stable ZIP timestamps keep fixture images reproducible without new provenance logic.
      info = zipfile.ZipInfo(filename, date_time=(2020, 1, 1, 0, 0, 0))
      info.compress_type = zipfile.ZIP_DEFLATED
      info.external_attr = 0o644 << 16
      wheel.writestr(info, content)
  return path


def build_fixtures(directory):
  directory.mkdir(parents=True, exist_ok=True)
  template = Path(__file__).with_name("fake_torch.py").read_text()
  variants = [("2.9.0+cpu", None), ("2.10.0+cu128", "12.8"), ("2.10.0+cu126", "12.6")]
  for torch_version, cuda_version in variants:
    source = template.replace('__version__ = "0.0.0+expri.template"', f"__version__ = {torch_version!r}")
    source = source.replace("  cuda = None", f"  cuda = {cuda_version!r}")
    requires = []
    if cuda_version:
      upper = "12.9" if cuda_version == "12.8" else "12.7"
      requires = [
        f"nvidia-cuda-runtime-cu12>={cuda_version},<{upper}",
        f"nvidia-cublas-cu12>={cuda_version},<{upper}",
      ]
    build_wheel(directory, "torch", torch_version, {"torch/__init__.py": source},
      requires=requires, top_level="torch", entry_points={"torchrun": "torch:run_cli"})
  for package, module, versions, top_level in [
    ("nvidia-cuda-runtime-cu12", "cuda_runtime", ("12.8.90", "12.8.91", "12.6.77"), "nvidia\\cuda_runtime"),
    ("nvidia-cublas-cu12", "cublas", ("12.8.4.1", "12.6.4.1"), "nvidia/cublas"),
  ]:
    for version in versions:
      build_wheel(directory, package, version, {f"nvidia/{module}/__init__.py": f"__version__ = {version!r}\n"},
        top_level=top_level)
  build_wheel(directory, "fixture-extra", "0.1.0", {"fixture_extra/__init__.py": "value = 42\n"},
    top_level="fixture_extra")


def make_index(directory, root):
  directory = directory.resolve()
  root = root.resolve()
  directory.relative_to(root)
  projects = {}
  for wheel_path in sorted(directory.glob("*.whl")):
    with zipfile.ZipFile(wheel_path) as wheel:
      metadata_files = [name for name in wheel.namelist() if name.endswith(".dist-info/METADATA")]
      if len(metadata_files) != 1:
        raise ValueError(f"expected one distribution metadata file in {wheel_path.name}")
      metadata = BytesParser().parsebytes(wheel.read(metadata_files[0]))
      if not metadata.get("Name"):
        raise ValueError(f"missing distribution name in {wheel_path.name}")
      name = canonicalize(metadata["Name"])
      projects.setdefault(name, []).append(wheel_path)
  simple = root / "simple"
  simple.mkdir(parents=True, exist_ok=True)
  links = []
  for name, wheels in sorted(projects.items()):
    project_dir = simple / name
    project_dir.mkdir(exist_ok=True)
    entries = []
    for wheel_path in wheels:
      href = quote(Path(os.path.relpath(wheel_path, project_dir)).as_posix(), safe="/")
      entries.append(f'<a href="{html.escape(href, quote=True)}">{html.escape(wheel_path.name)}</a>')
    (project_dir / "index.html").write_text("<!doctype html>\n" + "\n".join(entries) + "\n")
    links.append(f'<a href="{name}/">{name}</a>')
  (simple / "index.html").write_text("<!doctype html>\n" + "\n".join(links) + "\n")


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("directory", type=Path, help="output wheelhouse directory")
  parser.add_argument("--index-root", type=Path, help="HTTP root containing the wheelhouse")
  parser.add_argument("--index-only", action="store_true", help="index existing wheels without rebuilding")
  arguments = parser.parse_args()
  if arguments.index_only and arguments.index_root is None:
    parser.error("--index-only requires --index-root")
  if not arguments.index_only:
    build_fixtures(arguments.directory)
  if arguments.index_root:
    make_index(arguments.directory, arguments.index_root)


if __name__ == "__main__":
  main()
