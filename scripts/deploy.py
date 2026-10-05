"""Manually deploy main's binary or a named feature branch's AB dashboard."""
import argparse
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile

from deploy_install import WEB_FILES, digest

SCRIPTS = Path(__file__).resolve().parent


def command(args, *, cwd=None, timeout=60):
  result = subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)
  if result.returncode:
    raise RuntimeError(f'{args[0]} failed ({result.returncode}): ' + result.stderr[-2000:])
  return result.stdout.strip()


def source(repo, ref, *, fetch=False):
  def git(*args):
    return command(['git', '-C', str(repo), *args])
  if git('status', '--porcelain'):
    raise ValueError('deployment requires a clean Git checkout')
  branch = ref or git('branch', '--show-current')
  if not branch or len(branch.encode()) > 256:
    raise ValueError('provide a named branch, not a detached commit')
  git('check-ref-format', '--branch', branch)
  if branch == 'main':
    if fetch:
      git('fetch', '--no-tags', 'origin', '+refs/heads/main:refs/remotes/origin/main')
    revision = 'refs/remotes/origin/main'
  else:
    revision = 'refs/heads/' + branch
  commit = git('rev-parse', '--verify', revision + '^{commit}')
  if not re.fullmatch('[0-9a-f]{40}', commit):
    raise ValueError('Git did not resolve a full commit SHA')
  return branch, commit, 'main' if branch == 'main' else 'ab'


def bundle(repo, branch, commit, target, directory):
  archive = directory / 'source.tar'
  files = [f'dashboard_web/{name}' for name in WEB_FILES] if target == 'ab' else []
  command(['git', '-C', str(repo), 'archive', '--format=tar', '--output', str(archive), commit, *files])
  if target == 'ab':
    with tarfile.open(archive) as source_archive:
      payloads = {}
      for name in WEB_FILES:
        member = source_archive.getmember('dashboard_web/' + name)
        if not member.isfile() or not 0 < member.size <= 2 * 1024 * 1024:
          raise ValueError('dashboard bundle contains an invalid asset')
        payloads[name] = source_archive.extractfile(member).read()
        payloads[name].decode('utf-8')
    if b'<!-- LOGIN_ERROR -->' not in payloads['login.html']:
      raise ValueError('login page is missing its error marker')
  else:
    with tarfile.open(archive) as source_archive:
      source_archive.extractall(directory / 'source', filter='data')
    image = 'expri-deploy:' + commit
    command(['docker', 'build', '--file', str(SCRIPTS / 'deploy.Dockerfile'), '--tag', image,
      str(directory)], timeout=1200)
    container = command(['docker', 'create', image, '/not-executed'])
    try:
      command(['docker', 'cp', container + ':/artifacts/expri', str(directory / 'expri')])
    finally:
      command(['docker', 'rm', '--force', container])
    payloads = {'expri': (directory / 'expri').read_bytes()}
  payloads['deployment.json'] = (json.dumps({'commit': commit, 'branch': branch}) + '\n').encode()
  output = directory / 'release.tar.gz'
  with tarfile.open(output, 'w:gz') as release:
    for name, content in payloads.items():
      member = tarfile.TarInfo(name)
      member.size = len(content)
      member.mode = 0o755 if name == 'expri' else 0o644
      release.addfile(member, io.BytesIO(content))
  return output


def deploy(args, branch, commit, target):
  ctl = str(args.ctl)
  def execute(*command_args):
    return command([ctl, '-H', args.host, 'exec', '--', *command_args], timeout=120)
  with tempfile.TemporaryDirectory(prefix='expri-deploy-') as temporary:
    release = bundle(args.repo, branch, commit, target, Path(temporary))
    remote = execute('python3', '-c', "import tempfile; print(tempfile.mkdtemp(prefix='expri-deploy-', dir='/tmp'))")
    if not re.fullmatch('/tmp/expri-deploy-[A-Za-z0-9_-]+', remote):
      raise ValueError('remote did not return a private staging directory')
    try:
      command([ctl, 'scp', str(release), args.host + ':' + remote + '/release.tar.gz'], timeout=120)
      command([ctl, 'scp', str(SCRIPTS / 'deploy_install.py'), args.host + ':' + remote + '/install.py'], timeout=120)
      return json.loads(execute('python3', remote + '/install.py', '--upload', remote + '/release.tar.gz',
        '--target', target, '--commit', commit, '--branch', branch, '--sha256', digest(release)))
    finally:
      try:
        execute('rm', '-rf', remote)
      except (OSError, RuntimeError, subprocess.SubprocessError):
        print('Remote staging cleanup failed; deployment result is unchanged.', file=sys.stderr)


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument('--ref', help='named local feature branch, or main (fetches origin/main before deployment)')
  parser.add_argument('--repo', type=Path, default=SCRIPTS.parent)
  parser.add_argument('--host', default='vultr-2', help='ctl host alias')
  parser.add_argument('--ctl', type=Path, default=Path('ctl'), help='local ctl executable, including a development build')
  parser.add_argument('--dry-run', action='store_true', help='show the plan without fetching, building or contacting a server')
  args = parser.parse_args()
  if not re.fullmatch('[A-Za-z0-9][A-Za-z0-9_.-]{0,127}', args.host):
    parser.error('host must be a ctl alias without shell or path syntax')
  try:
    branch, commit, target = source(args.repo, args.ref, fetch=not args.dry_run)
    if args.dry_run:
      print(json.dumps({'host': args.host, 'branch': branch, 'commit': commit, 'target': target,
        'fetch_origin_main_before_build': target == 'main', 'commit_is_cached': target == 'main',
        'release_directory': ('/opt/expri/releases/' if target == 'main' else '/opt/expri/dashboard/ab/releases/') + commit,
        'restart_backend': target == 'main'}, indent=2))
    else:
      print(json.dumps(deploy(args, branch, commit, target), indent=2))
  except (OSError, ValueError, RuntimeError, KeyError, tarfile.TarError, subprocess.SubprocessError) as error:
    parser.exit(1, 'Deployment failed: ' + str(error)[:2500] + '\n')


if __name__ == '__main__':
  main()
