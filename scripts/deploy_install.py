"""Remote installer used by deploy.py; never restores or edits service data."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import time
import tomllib
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

WEB_FILES = ('index.html', 'login.html', 'app.js', 'styles.css')
HOSTS = {'ab': 'ab.expri.clouds56.top', 'main': 'expri.clouds56.top'}
PROTECTED = ('etc/expri/server.toml', 'etc/expri/service.env', 'etc/expri/s3.env',
  'etc/expri/dashboard.env', 'etc/nginx/conf.d/expri.conf', 'etc/nginx/conf.d/expri-ab.conf',
  'etc/nginx/snippets/expri-proxy.conf', 'etc/nginx/snippets/expri-ab-proxy.conf',
  'etc/systemd/system/expri.service')


def digest(path):
  with path.open('rb') as source:
    return hashlib.file_digest(source, 'sha256').hexdigest()


def run(*args):
  return subprocess.run(args, check=True, capture_output=True, text=True, timeout=30).stdout.strip()


def manifest(path, commit):
  if path.stat().st_size > 16 * 1024:
    raise ValueError('release manifest exceeds its size limit')
  value = json.loads(path.read_text())
  if set(value) != {'commit', 'branch'} or value['commit'] != commit:
    raise ValueError('release manifest does not match its commit')
  branch = value['branch']
  if not isinstance(branch, str) or not branch or len(branch.encode()) > 256 or any(ord(char) < 32 or 127 <= ord(char) <= 159 for char in branch):
    raise ValueError('release branch is invalid')
  return value


def unpack(upload, stage, kind, commit, branch):
  files = WEB_FILES if kind == 'ab' else ('expri',)
  limits = {name: 2 * 1024 * 1024 for name in files}
  if kind == 'main':
    limits['expri'] = 128 * 1024 * 1024
  limits['deployment.json'] = 16 * 1024
  seen = set()
  with tarfile.open(upload, 'r:gz') as archive:
    for member in archive:
      if member.name not in limits or member.name in seen or not member.isfile() or not 0 < member.size <= limits[member.name]:
        raise ValueError('deployment archive contains an invalid file')
      seen.add(member.name)
      with archive.extractfile(member) as source, (stage / member.name).open('xb') as destination:
        shutil.copyfileobj(source, destination)
        destination.flush()
        os.fsync(destination.fileno())
      (stage / member.name).chmod(0o755 if member.name == 'expri' else 0o644)
  if seen != set(limits) or manifest(stage / 'deployment.json', commit)['branch'] != branch:
    raise ValueError('deployment archive is incomplete or has a different branch')
  if kind == 'ab':
    for name in WEB_FILES:
      (stage / name).read_text(encoding='utf-8')
    if '<!-- LOGIN_ERROR -->' not in (stage / 'login.html').read_text():
      raise ValueError('login page is missing its error marker')
  return files


def switch(link, target):
  temporary = link.with_name(f'.{link.name}.{os.getpid()}.next')
  try:
    temporary.symlink_to(target)
    os.replace(temporary, link)
  finally:
    temporary.unlink(missing_ok=True)


def healthy(kind, release):
  deadline = time.monotonic() + 20
  while time.monotonic() < deadline:
    try:
      headers = {'Host': HOSTS[kind]}
      with urlopen(Request('http://127.0.0.1:8787/login', headers=headers), timeout=2) as reply:
        if reply.status != 200 or reply.headers.get('Referrer-Policy') != 'same-origin':
          raise ValueError('login unavailable or native form policy changed')
        if kind == 'ab' and reply.headers.get('X-Expri-Revision') != release.name:
          raise ValueError('preview revision mismatch')
      if kind == 'ab':
        with urlopen(Request('http://127.0.0.1:8787/app.js', headers=headers), timeout=2) as reply:
          content = reply.read(2 * 1024 * 1024 + 1)
          if reply.status != 200 or reply.headers.get('X-Expri-Revision') != release.name or hashlib.sha256(content).hexdigest() != digest(release / 'app.js'):
            raise ValueError('preview asset mismatch')
      else:
        with urlopen('http://127.0.0.1:8787/health', timeout=2) as reply:
          if reply.status != 200 or json.loads(reply.read(128)) != {'ok': True}:
            raise ValueError('backend unavailable')
        pid = run('systemctl', 'show', 'expri', '--property=MainPID', '--value')
        if not pid.isdecimal() or pid == '0' or Path('/proc', pid, 'exe').resolve() != release / 'expri':
          raise ValueError('running binary mismatch')
      return
    except (OSError, ValueError, subprocess.SubprocessError):
      time.sleep(0.2)
  raise RuntimeError('deployed release did not become healthy')


def preview_revisions(root):
  config = root / 'etc/expri/server.toml'
  if not config.exists():
    return {}
  previews = tomllib.loads(config.read_text()).get('dashboard', {}).get('previews', [])
  revisions = {}
  for site in previews:
    url = urlsplit(site['public_url'])
    host = url.hostname
    if ':' in host:
      host = '[' + host + ']'
    if url.port not in (None, 443):
      host += ':' + str(url.port)
    with urlopen(Request('http://127.0.0.1:8787/login', headers={'Host': host}), timeout=2) as reply:
      revision = reply.headers.get('X-Expri-Revision', '')
      if reply.status != 200 or not re.fullmatch('[0-9a-f]{40}', revision):
        raise RuntimeError('configured preview is unavailable or missing its revision')
      revisions[host] = revision
  return revisions


def install(upload, kind, commit, branch, expected_digest, *, root=Path('/'), check_health=healthy, execute=run):
  if kind not in HOSTS or not re.fullmatch('[0-9a-f]{40}', commit) or not re.fullmatch('[0-9a-f]{64}', expected_digest):
    raise ValueError('invalid deployment identity')
  if (branch == 'main') != (kind == 'main'):
    raise ValueError('main must deploy to primary; feature branches must deploy to ab')
  if not stat.S_ISREG(upload.lstat().st_mode) or upload.stat().st_size > 129 * 1024 * 1024 or digest(upload) != expected_digest:
    raise ValueError('uploaded archive digest or size mismatch')
  base = root / ('opt/expri/dashboard/ab' if kind == 'ab' else 'opt/expri')
  releases = base / 'releases'
  releases.mkdir(parents=True, exist_ok=True)
  link = base / 'current' if kind == 'ab' else root / 'usr/local/bin/expri'
  link.parent.mkdir(parents=True, exist_ok=True)
  lock = root / 'opt/expri/deploy.lock'
  with lock.open('a') as lease:
    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
    if link.exists() and not link.is_symlink():
      raise ValueError('active release must be a symlink')
    previous = os.readlink(link) if link.is_symlink() else None
    if kind == 'main' and previous is None:
      raise ValueError('primary bootstrap must install an initial release first')
    protected = {path: digest(root / path) for path in PROTECTED if (root / path).is_file()}
    release = releases / commit
    reused = release.exists()
    with tempfile.TemporaryDirectory(prefix='.stage-', dir=releases) as directory:
      stage = Path(directory)
      files = unpack(upload, stage, kind, commit, branch)
      if reused:
        if release.is_symlink() or not release.is_dir() or any(digest(stage / name) != digest(release / name) for name in files):
          raise ValueError('immutable release already contains different files')
        provenance = manifest(release / 'deployment.json', commit)
      else:
        provenance = manifest(stage / 'deployment.json', commit)
        stage.chmod(0o755)
        os.rename(stage, release)
    target = 'releases/' + commit if kind == 'ab' else str(release / 'expri')
    if kind == 'main':
      execute(str(release / 'expri'), '--version')
    previews = preview_revisions(root) if kind == 'main' else {}
    try:
      switch(link, target)
      if kind == 'main':
        execute('systemctl', 'restart', 'expri')
      check_health(kind, release)
      if kind == 'main' and preview_revisions(root) != previews:
        raise RuntimeError('primary deployment changed a configured preview revision')
      if protected != {path: digest(root / path) for path in protected}:
        raise RuntimeError('service configuration changed during deployment')
    except Exception:
      if previous is None:
        link.unlink(missing_ok=True)
      else:
        switch(link, previous)
        if kind == 'main':
          execute('systemctl', 'restart', 'expri')
        check_health(kind, link.resolve().parent if kind == 'main' else link.resolve())
        if kind == 'main' and preview_revisions(root) != previews:
          raise RuntimeError('rollback did not restore configured preview access')
      raise
    return {'status': 'deployed', 'target': kind, 'commit': commit, 'requested_branch': branch,
      'built_from_branch': provenance['branch'], 'reused_release': reused, 'previous': previous}


if __name__ == '__main__':
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument('--upload', type=Path, required=True)
  parser.add_argument('--target', choices=HOSTS, required=True)
  parser.add_argument('--commit', required=True)
  parser.add_argument('--branch', required=True)
  parser.add_argument('--sha256', required=True)
  args = parser.parse_args()
  if os.geteuid() != 0:
    parser.error('installer requires root')
  print(json.dumps(install(args.upload, args.target, args.commit, args.branch, args.sha256)))
