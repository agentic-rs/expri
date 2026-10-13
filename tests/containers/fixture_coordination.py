"""Exchange complete phase records between the browser and workflow processes."""
import argparse
import json
import os
from pathlib import Path
import tempfile


def read_phase(path):
  try:
    return json.loads(Path(path).read_text()).get('phase', '')
  except FileNotFoundError:
    return ''


def write_phase(path, phase):
  path = Path(path)
  descriptor, temporary = tempfile.mkstemp(prefix='.' + path.name + '.', dir=path.parent)
  try:
    with os.fdopen(descriptor, 'w', encoding='utf-8') as output:
      json.dump({'phase': phase}, output)
    # Readers retain the previous complete record until the replacement closes.
    # The temporary file shares the destination directory for an atomic rename.
    os.replace(temporary, path)
  finally:
    Path(temporary).unlink(missing_ok=True)


if __name__ == '__main__':
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument('path', type=Path)
  parser.add_argument('phase', nargs='?')
  args = parser.parse_args()
  if args.phase is None:
    print(read_phase(args.path))
  else:
    write_phase(args.path, args.phase)
