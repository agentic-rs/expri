"""Keep published worker crates self-contained with canonical Python SDK helpers."""

import argparse
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parents[1]
HELPERS = ("expri_metrics.py", "expri_run.py")


def sync(check=False):
  stale = []
  for name in HELPERS:
    source = ROOT / "sdk/python" / name
    destination = ROOT / "crates/worker/python" / name
    header = (f"# Generated from sdk/python/{name}; edit that source instead.\n"
              "# Regenerate with python3 scripts/sync_python_sdk.py.\n").encode()
    expected = header + source.read_bytes()
    if destination.is_file() and destination.read_bytes() == expected:
      continue
    if check:
      stale.append(str(destination.relative_to(ROOT)))
    else:
      destination.parent.mkdir(parents=True, exist_ok=True)
      destination.write_bytes(expected)
  if stale:
    print("Stale embedded Python SDK helpers: " + ", ".join(stale), file=sys.stderr)
    print("Run python3 scripts/sync_python_sdk.py", file=sys.stderr)
    return 1
  return 0


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("--check", action="store_true", help="Check generated helpers without writing them")
  return sync(parser.parse_args().check)


if __name__ == "__main__":
  raise SystemExit(main())
