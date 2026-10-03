"""Fetch pinned real helper wheels at image build time from the official PyPI API."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import tempfile
from urllib.parse import urlparse
from urllib.request import Request, urlopen


PACKAGES = (("packaging", "25.0"), ("tomli", "2.2.1"))


def read_url(url, limit):
  request = Request(url, headers={"User-Agent": "expri-ci-fixtures/1"})
  with urlopen(request, timeout=60) as response:
    content = response.read(limit + 1)
  if len(content) > limit:
    raise ValueError(f"download exceeds {limit} bytes: {url}")
  return content


def fetch_wheel(directory, name, version):
  filename = f"{name}-{version}-py3-none-any.whl"
  metadata = json.loads(read_url(f"https://pypi.org/pypi/{name}/{version}/json", 4 * 1024 * 1024))
  matches = [artifact for artifact in metadata["urls"] if artifact["filename"] == filename]
  if len(matches) != 1 or matches[0]["packagetype"] != "bdist_wheel":
    raise ValueError(f"expected one universal wheel for {name}=={version}")
  artifact = matches[0]
  location = urlparse(artifact["url"])
  if location.scheme != "https" or location.hostname != "files.pythonhosted.org":
    raise ValueError(f"unexpected PyPI artifact location for {filename}")
  expected = artifact["digests"]["sha256"]
  if not re.fullmatch(r"[a-f0-9]{64}", expected):
    raise ValueError(f"invalid PyPI SHA256 digest for {filename}")
  content = read_url(artifact["url"], 16 * 1024 * 1024)
  if hashlib.sha256(content).hexdigest() != expected:
    raise ValueError(f"PyPI artifact integrity check failed for {filename}")
  if len(content) != artifact["size"]:
    raise ValueError(f"PyPI artifact size differs for {filename}")
  with tempfile.NamedTemporaryFile(dir=directory, delete=False) as temporary:
    temporary.write(content)
    temporary_path = Path(temporary.name)
  temporary_path.chmod(0o644)
  temporary_path.replace(directory / filename)
  print(f"Downloaded {filename} ({len(content)} bytes)")


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("directory", type=Path)
  arguments = parser.parse_args()
  arguments.directory.mkdir(parents=True, exist_ok=True)
  for name, version in PACKAGES:
    fetch_wheel(arguments.directory, name, version)


if __name__ == "__main__":
  main()
