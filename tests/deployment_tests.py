"""Hermetic deployment tests: local Git refs and installer filesystem fixtures."""
import contextlib
import io
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import deploy
import deploy_install as installer

OLD = 'a' * 40
NEW = 'b' * 40


def web_files(marker='new'):
  return {'index.html': ('<p>' + marker + '</p>').encode(),
    'login.html': b'<form><!-- LOGIN_ERROR --></form>',
    'app.js': ('console.log(' + repr(marker) + ')').encode(), 'styles.css': b'body { color: black; }'}


def archive(path, files, commit=NEW, branch='feature/demo'):
  files = dict(files, **{'deployment.json': json.dumps({'commit': commit, 'branch': branch}).encode()})
  with tarfile.open(path, 'w:gz') as output:
    for name, content in files.items():
      member = tarfile.TarInfo(name)
      member.size = len(content)
      output.addfile(member, io.BytesIO(content))
  return path


class SourceTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory()
    self.addCleanup(self.temporary.cleanup)
    self.repo = Path(self.temporary.name) / 'repo'
    self.repo.mkdir()
    self.git('init', '--initial-branch=main')
    self.git('config', 'user.name', 'Deployment fixture')
    self.git('config', 'user.email', 'deployment@example.invalid')
    self.git('config', 'commit.gpgsign', 'false')
    self.git('config', 'core.hooksPath', '/dev/null')
    (self.repo / 'dashboard_web').mkdir()
    for name, content in web_files('main').items():
      (self.repo / 'dashboard_web' / name).write_bytes(content)
    self.git('add', '.')
    self.git('commit', '-m', 'main fixture')
    self.main = self.git('rev-parse', 'HEAD')
    self.git('update-ref', 'refs/remotes/origin/main', self.main)
    self.git('checkout', '-b', 'feature/demo')
    (self.repo / 'dashboard_web/index.html').write_bytes(b'<p>feature</p>')
    self.git('commit', '-am', 'feature fixture')
    self.feature = self.git('rev-parse', 'HEAD')

  def git(self, *args):
    return subprocess.check_output(['git', '-C', str(self.repo), *args], text=True, stderr=subprocess.DEVNULL).strip()

  def test_branch_mapping_uses_remote_main_and_named_feature_commit(self):
    self.assertEqual(deploy.source(self.repo, 'main'), ('main', self.main, 'main'))
    self.assertEqual(deploy.source(self.repo, 'feature/demo'), ('feature/demo', self.feature, 'ab'))
    self.assertEqual(deploy.source(self.repo, None), ('feature/demo', self.feature, 'ab'))
    with self.assertRaises(RuntimeError):
      deploy.source(self.repo, self.feature)

  def test_dirty_checkout_is_rejected_before_fetch(self):
    (self.repo / 'untracked').write_text('not part of the release')
    with patch.object(deploy, 'command', wraps=deploy.command) as command:
      with self.assertRaisesRegex(ValueError, 'clean Git'):
        deploy.source(self.repo, 'main', fetch=True)
      self.assertFalse(any('fetch' in call.args[0] for call in command.call_args_list))

  def test_main_execution_fetches_explicit_remote_ref(self):
    original = deploy.command
    def command(args, **kwargs):
      if 'fetch' in args:
        self.assertEqual(args[-3:], ['--no-tags', 'origin', '+refs/heads/main:refs/remotes/origin/main'])
        self.git('update-ref', 'refs/remotes/origin/main', self.feature)
        return ''
      return original(args, **kwargs)
    with patch.object(deploy, 'command', side_effect=command):
      self.assertEqual(deploy.source(self.repo, 'main', fetch=True)[1], self.feature)

  def test_dry_run_never_fetches_builds_or_contacts_ctl(self):
    with patch.object(sys, 'argv', ['deploy.py', '--repo', str(self.repo), '--ref', 'main', '--dry-run']), \
        patch.object(deploy, 'command', wraps=deploy.command) as command, \
        patch.object(deploy, 'deploy') as activate, contextlib.redirect_stdout(io.StringIO()) as output:
      deploy.main()
    plan = json.loads(output.getvalue())
    self.assertEqual(plan['commit'], self.main)
    self.assertTrue(plan['commit_is_cached'] and plan['fetch_origin_main_before_build'])
    self.assertFalse(activate.called)
    self.assertTrue(all(call.args[0][0] == 'git' and 'fetch' not in call.args[0] for call in command.call_args_list))

  def test_bundle_reads_committed_assets_from_exact_archive(self):
    directory = Path(self.temporary.name) / 'bundle'
    directory.mkdir()
    output = deploy.bundle(self.repo, 'feature/demo', self.feature, 'ab', directory)
    with tarfile.open(output) as release:
      self.assertEqual(set(release.getnames()), set(installer.WEB_FILES) | {'deployment.json'})
      self.assertEqual(release.extractfile('index.html').read(), b'<p>feature</p>')
      self.assertEqual(json.load(release.extractfile('deployment.json')), {'commit': self.feature, 'branch': 'feature/demo'})


class InstallerTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory()
    self.addCleanup(self.temporary.cleanup)
    self.root = Path(self.temporary.name).resolve()
    self.config = self.root / 'etc/expri/server.toml'
    self.config.parent.mkdir(parents=True)
    self.configuration = '[dashboard]\npublic_url = "https://expri.clouds56.top"\n'
    self.config.write_text(self.configuration)
    self.database = self.root / 'var/lib/expri/metadata.sqlite3'
    self.database.parent.mkdir(parents=True)
    with sqlite3.connect(self.database) as connection:
      connection.execute('CREATE TABLE received_data (value TEXT NOT NULL)')
      connection.execute("INSERT INTO received_data VALUES ('initial data')")
      connection.execute('PRAGMA user_version=1')
    self.initial_database = self.database.read_bytes()
    self.commands = []

  def update_database(self, value, *, schema=1):
    with sqlite3.connect(self.database) as connection:
      connection.execute('UPDATE received_data SET value=?', (value,))
      connection.execute(f'PRAGMA user_version={schema}')

  def received_data(self):
    with sqlite3.connect(self.database) as connection:
      return connection.execute('SELECT value FROM received_data').fetchone()[0]

  def existing(self, kind):
    base = self.root / ('opt/expri/dashboard/ab' if kind == 'ab' else 'opt/expri')
    release = base / 'releases' / OLD
    release.mkdir(parents=True)
    files = web_files('old') if kind == 'ab' else {'expri': b'old binary'}
    for name, content in files.items():
      (release / name).write_bytes(content)
    (release / 'deployment.json').write_text(json.dumps({'commit': OLD, 'branch': 'feature/old' if kind == 'ab' else 'main'}))
    link = base / 'current' if kind == 'ab' else self.root / 'usr/local/bin/expri'
    link.parent.mkdir(parents=True, exist_ok=True)
    link.symlink_to('releases/' + OLD if kind == 'ab' else release / 'expri')
    return link, release

  def install(self, kind='ab', files=None, branch=None, health=lambda *_: None, sha=None, execute=None):
    branch = branch or ('main' if kind == 'main' else 'feature/demo')
    files = files or (web_files() if kind == 'ab' else {'expri': b'new binary'})
    upload = archive(self.root / 'upload.tar.gz', files, branch=branch)
    return installer.install(upload, kind, NEW, branch, sha or installer.digest(upload), root=self.root,
      check_health=health, execute=execute or (lambda *args: self.commands.append(args)))

  def test_ab_activation_keeps_backend_data_and_old_assets(self):
    link, old = self.existing('ab')
    observed = []
    result = self.install(health=lambda kind, release: observed.append((kind, link.resolve() == release)))
    self.assertEqual(os.readlink(link), 'releases/' + NEW)
    self.assertEqual(observed, [('ab', True)])
    self.assertTrue(old.is_dir())
    self.assertEqual(self.commands, [])
    self.assertEqual(result['built_from_branch'], 'feature/demo')
    self.assertEqual(self.database.read_bytes(), self.initial_database)
    self.assertEqual(self.config.read_text(), self.configuration)

  def test_failed_ab_health_restores_previous_link_without_restart(self):
    link, old = self.existing('ab')
    def health(kind, release):
      if release.name == NEW:
        raise RuntimeError('asset response mismatch')
      self.assertEqual(release, old)
    with self.assertRaisesRegex(RuntimeError, 'asset response mismatch'):
      self.install(health=health)
    self.assertEqual(link.resolve(), old)
    self.assertEqual(self.commands, [])

  def test_main_failure_restarts_old_binary_without_reverting_new_data(self):
    link, old = self.existing('main')
    def health(kind, release):
      if release.name == NEW:
        self.update_database('upload accepted during deployment')
        raise RuntimeError('new process unhealthy')
      self.assertEqual(release, old)
    with self.assertRaisesRegex(RuntimeError, 'new process unhealthy'):
      self.install('main', health=health)
    self.assertEqual(link.resolve(), old / 'expri')
    self.assertEqual(self.commands[-3:], [('systemctl', 'restart', 'expri'),
      ('systemctl', 'stop', 'expri'), ('systemctl', 'restart', 'expri')])
    self.assertEqual(self.received_data(), 'upload accepted during deployment')
    self.assertEqual(installer.metadata_schema(self.root), 1)
    self.assertEqual(self.config.read_text(), self.configuration)

  def test_migration_health_failure_keeps_new_binary_and_received_data(self):
    link, old = self.existing('main')
    def health(kind, release):
      self.assertEqual(release.name, NEW)
      self.update_database('upload accepted after migration', schema=2)
      raise RuntimeError('new process unhealthy')
    with self.assertRaisesRegex(RuntimeError, 'schema changed from 1 to 2.*Automatic rollback was skipped'):
      self.install('main', health=health)
    self.assertEqual(link.resolve().parent.name, NEW)
    self.assertEqual(self.commands[-1:], [('systemctl', 'stop', 'expri')])
    self.assertEqual(sum(command == ('systemctl', 'restart', 'expri') for command in self.commands), 1)
    self.assertEqual(installer.metadata_schema(self.root), 2)
    self.assertEqual(self.received_data(), 'upload accepted after migration')
    self.assertEqual(self.config.read_text(), self.configuration)
    self.assertTrue((old / 'expri').is_file())

  def test_failure_stops_new_process_before_checking_for_late_migration(self):
    link, _ = self.existing('main')
    def execute(*args):
      self.commands.append(args)
      if args == ('systemctl', 'stop', 'expri'):
        self.update_database('migration committed before stop finished', schema=2)
    def health(*_):
      raise RuntimeError('health check failed before migration completed')
    with self.assertRaisesRegex(RuntimeError, 'schema changed from 1 to 2.*service stopped'):
      self.install('main', health=health, execute=execute)
    self.assertEqual(link.resolve().parent.name, NEW)
    self.assertEqual(self.commands[-1], ('systemctl', 'stop', 'expri'))
    self.assertEqual(sum(command == ('systemctl', 'restart', 'expri') for command in self.commands), 1)
    self.assertEqual(self.received_data(), 'migration committed before stop finished')

  def test_failed_stop_forbids_rollback_even_when_schema_is_unchanged(self):
    link, _ = self.existing('main')
    def execute(*args):
      self.commands.append(args)
      if args == ('systemctl', 'stop', 'expri'):
        raise RuntimeError('stop request failed')
    def health(*_):
      raise RuntimeError('new process unhealthy')
    with self.assertRaisesRegex(RuntimeError, 'could not be stopped.*Automatic rollback was skipped'):
      self.install('main', health=health, execute=execute)
    self.assertEqual(link.resolve().parent.name, NEW)
    self.assertEqual(sum(command == ('systemctl', 'restart', 'expri') for command in self.commands), 1)
    self.assertEqual(installer.metadata_schema(self.root), 1)
    self.assertEqual(self.received_data(), 'initial data')

  def test_unreadable_post_startup_schema_keeps_new_binary_without_rollback(self):
    link, _ = self.existing('main')
    def health(*_):
      self.database.write_bytes(b'unreadable upgraded database')
      raise RuntimeError('new process unhealthy')
    with self.assertRaisesRegex(RuntimeError, 'metadata schema cannot be read.*Operator recovery is required'):
      self.install('main', health=health)
    self.assertEqual(link.resolve().parent.name, NEW)
    self.assertEqual(sum(command == ('systemctl', 'restart', 'expri') for command in self.commands), 1)
    self.assertEqual(self.database.read_bytes(), b'unreadable upgraded database')

  def test_unreadable_schema_is_rejected_before_activation(self):
    link, old = self.existing('main')
    self.database.write_bytes(b'unreadable existing database')
    with self.assertRaisesRegex(RuntimeError, 'cannot read service metadata schema'):
      self.install('main', health=lambda *_: self.fail('must not activate an unreadable store'))
    self.assertEqual(link.resolve(), old / 'expri')
    self.assertEqual(self.commands, [])

  def test_successful_migration_reports_both_schema_versions(self):
    link, _ = self.existing('main')
    result = self.install('main', health=lambda *_: self.update_database('migrated data', schema=2))
    self.assertEqual(link.resolve().parent.name, NEW)
    self.assertEqual((result['metadata_schema_before'], result['metadata_schema_after']), (1, 2))
    self.assertEqual(self.received_data(), 'migrated data')

  def test_schema_inspection_reads_committed_wal_without_modifying_the_database(self):
    with sqlite3.connect(self.database) as connection:
      connection.execute('PRAGMA journal_mode=WAL')
      connection.execute('PRAGMA user_version=2')
      connection.commit()
      before = self.database.read_bytes()
      wal_before = self.database.with_name(self.database.name + '-wal').read_bytes()
      self.assertEqual(installer.metadata_schema(self.root), 2)
      self.assertEqual(self.database.read_bytes(), before)
      self.assertEqual(self.database.with_name(self.database.name + '-wal').read_bytes(), wal_before)

  def test_main_initial_rollout_without_previews_does_not_probe_ab(self):
    link, _ = self.existing('main')
    with patch.object(installer, 'urlopen') as request:
      result = self.install('main')
    self.assertEqual(result['target'], 'main')
    self.assertEqual(link.resolve().parent.name, NEW)
    self.assertFalse(request.called)
    self.assertEqual(self.commands[-1], ('systemctl', 'restart', 'expri'))

  def test_main_rolls_back_when_it_breaks_configured_ab_dashboard(self):
    link, old = self.existing('main')
    preview, preview_release = self.existing('ab')
    self.config.write_text(self.configuration + '''[[dashboard.previews]]
public_url = "https://ab.expri.clouds56.top"
assets_dir = "/opt/expri/dashboard/ab"
''')
    class Reply:
      status = 200
      headers = {'X-Expri-Revision': OLD}
      def __enter__(self):
        return self
      def __exit__(self, *_):
        pass
    observed = []
    def request(value, **_):
      self.assertEqual(value.get_header('Host'), 'ab.expri.clouds56.top')
      current = link.resolve().parent.name
      observed.append(current)
      if current == NEW:
        raise RuntimeError('new backend rejected AB host')
      return Reply()
    with patch.object(installer, 'urlopen', side_effect=request):
      with self.assertRaisesRegex(RuntimeError, 'new backend rejected AB host'):
        self.install('main')
    self.assertEqual(observed, [OLD, NEW, OLD])
    self.assertEqual(link.resolve(), old / 'expri')
    self.assertEqual(preview.resolve(), preview_release)
    self.assertEqual((preview_release / 'app.js').read_bytes(), web_files('old')['app.js'])

  def test_same_commit_branch_alias_reuses_original_provenance(self):
    self.existing('ab')
    self.install(branch='feature/original')
    result = self.install(branch='feature/alias')
    self.assertTrue(result['reused_release'])
    self.assertEqual(result['built_from_branch'], 'feature/original')
    self.assertEqual(result['requested_branch'], 'feature/alias')

  def test_digest_mismatch_and_archive_paths_fail_before_activation(self):
    link, old = self.existing('ab')
    with self.assertRaisesRegex(ValueError, 'digest'):
      self.install(sha='0' * 64)
    with self.assertRaisesRegex(ValueError, 'invalid file'):
      self.install(files={**web_files(), '../outside': b'forbidden'})
    self.assertEqual(link.resolve(), old)
    self.assertFalse((self.root / 'opt/expri/dashboard/ab/releases/outside').exists())


if __name__ == '__main__':
  unittest.main()
