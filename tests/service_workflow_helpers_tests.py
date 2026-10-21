"""Check shared-service identity and exact publication acknowledgments."""
import ast
from contextlib import redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, call, patch


class SharedWorkerServiceReadinessTests(unittest.TestCase):
  def setUp(self):
    self.now = 0
    self.sleeps = []
    self.status = Mock()
    def sleep(seconds):
      self.sleeps.append(seconds)
      self.now += seconds
    clock = SimpleNamespace(monotonic=lambda: self.now, sleep=sleep)
    # The runner owns real Docker startup at module scope. Load its actual wait
    # helper without starting containers, as the publication tests below do.
    path = Path(__file__).parent / 'containers/service_workflow.py'
    tree = ast.parse(path.read_text(), filename=str(path))
    function = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == 'wait_shared_worker_service')
    namespace = {'json': json, 'time': clock, 'worker_service_status': self.status}
    exec(compile(ast.Module(body=[function], type_ignores=[]), str(path), 'exec'), namespace)
    self.wait_shared = namespace['wait_shared_worker_service']

  def report(self, *, pid=357, ready=True, running=True):
    return {'pid': pid, 'ready': ready, 'running': running,
      'collection_ready': ready, 'status': 'running',
      'jobs': [{'scope': {'run_id': 'run-first'}}, {'scope': {'run_id': 'run-second'}}]}

  def test_transient_readiness_returns_the_same_ready_registration_snapshot(self):
    ready = self.report()
    self.status.side_effect = [self.report(ready=False), ready]
    self.assertIs(self.wait_shared('worker', 357), ready)
    self.assertEqual(self.status.call_args_list, [call('worker')] * 2)
    self.assertEqual(self.sleeps, [0.2])

  def test_replacement_fails_immediately_even_when_the_new_service_is_ready(self):
    self.status.return_value = self.report(pid=358)
    with self.assertRaisesRegex(AssertionError, r'was replaced; expected pid 357; observed .*"pid": 358'):
      self.wait_shared('worker', 357)
    self.assertEqual(self.status.call_count, 1)
    self.assertEqual(self.sleeps, [])

  def test_replacement_during_readiness_recovery_is_not_retried(self):
    self.status.side_effect = [self.report(ready=False), self.report(pid=358, ready=False)]
    with self.assertRaisesRegex(AssertionError, 'was replaced'):
      self.wait_shared('worker', 357)
    self.assertEqual(self.status.call_count, 2)
    self.assertEqual(self.sleeps, [0.2])

  def test_persistent_unreadiness_has_a_deadline_and_bounded_public_diagnostics(self):
    self.status.return_value = {**self.report(ready=False), 'jobs': [{'private': 'not-in-diagnostics'}]}
    with self.assertRaises(AssertionError) as caught:
      self.wait_shared('worker', 357, timeout=0.5)
    self.assertIn('did not become ready after 0.5s', str(caught.exception))
    self.assertIn('"ready": false', str(caught.exception))
    self.assertIn('"pid": 357', str(caught.exception))
    self.assertNotIn('not-in-diagnostics', str(caught.exception))
    self.assertEqual(self.now, 0.5)
    self.assertEqual(self.status.call_count, 4)

  def test_cached_ready_state_without_a_running_service_does_not_pass(self):
    self.status.return_value = self.report(running=False)
    with self.assertRaisesRegex(AssertionError, 'did not become ready'):
      self.wait_shared('worker', 357, timeout=0)
    self.assertEqual(self.sleeps, [])


class PublicationReadinessTests(unittest.TestCase):
  def setUp(self):
    temporary = tempfile.TemporaryDirectory(prefix='expri-publication-readiness-')
    self.addCleanup(temporary.cleanup)
    self.root = Path(temporary.name)
    self.run = self.root / 'run'
    (self.run / 'outputs').mkdir(parents=True)
    (self.run / 'environment').mkdir()
    self.queue = self.root / 'queue'
    self.saved_path = self.queue / 'runs/demo/worker/run-1/queue.json'
    self.saved_path.parent.mkdir(parents=True)
    self.files = {
      'run-state.json': b'{"run_id":"run-1","status":"running"}',
      'snapshot.json': b'{}',
      'environment/environment-state.json': b'{"status":"ready"}',
      'outputs/params.json': b'{}',
      'outputs/metrics.jsonl': b'{"step":0,"metrics":{"loss":1}}\n',
      'outputs/.expri-artifacts.json': b'{"schema_version":1,"files":[]}',
    }
    for name, data in self.files.items():
      (self.run / name).write_bytes(data)
    self.saved = {'protocol': 'tracking_v1', 'streams': {'outputs/metrics.jsonl': len(self.files['outputs/metrics.jsonl'])},
      'documents': {name: {'complete': True, 'sha256': hashlib.sha256(data).hexdigest()}
        for name, data in self.files.items() if name != 'outputs/metrics.jsonl'}}
    self.save_queue()
    # Import only the predicate function; the runner's module-level code owns
    # real Docker startup and CLI parsing, which this filesystem test must avoid.
    path = Path(__file__).parent / 'containers/service_workflow.py'
    tree = ast.parse(path.read_text(), filename=str(path))
    function = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == 'wait_run_acknowledged')
    def python(_container, script):
      output = io.StringIO()
      with redirect_stdout(output):
        exec(script, {})
      return output.getvalue().strip()
    def wait_for(predicate, *_args, **_kwargs):
      self.assertTrue(predicate(), 'publication has not acknowledged the requested inventory')
    namespace = {'python': python, 'wait_for': wait_for}
    exec(compile(ast.Module(body=[function], type_ignores=[]), str(path), 'exec'), namespace)
    self.wait_ack = namespace['wait_run_acknowledged']

  def save_queue(self):
    self.saved_path.write_text(json.dumps(self.saved))

  def check(self, **kwargs):
    self.wait_ack('fixture', str(self.run), str(self.queue), **kwargs)

  def test_new_artifact_requires_regenerated_and_acknowledged_inventory(self):
    artifact = self.run / 'outputs/project-scope.txt'
    artifact.write_text('worker scope fixture\n')
    expected = {'artifact_paths': ['outputs/project-scope.txt']}
    with self.assertRaises(AssertionError):
      self.check(**expected)
    inventory = self.run / 'outputs/.expri-artifacts.json'
    inventory.write_text(json.dumps({'schema_version': 1, 'files': [{'path': 'outputs/project-scope.txt', 'size': artifact.stat().st_size}]}))
    with self.assertRaises(AssertionError):
      self.check(**expected)
    self.saved['documents']['outputs/.expri-artifacts.json']['sha256'] = hashlib.sha256(inventory.read_bytes()).hexdigest()
    self.save_queue()
    self.check(**expected)
    artifact.write_text('changed worker artifact\n')
    with self.assertRaises(AssertionError):
      self.check(**expected)

  def test_environment_and_inventory_require_their_exact_completed_revisions(self):
    self.check()
    environment = self.run / 'environment/environment-state.json'
    environment.write_text('{"status":"changed"}')
    with self.assertRaises(AssertionError):
      self.check()
    self.saved['documents']['environment/environment-state.json']['sha256'] = hashlib.sha256(environment.read_bytes()).hexdigest()
    self.saved['documents']['outputs/.expri-artifacts.json']['complete'] = False
    self.save_queue()
    with self.assertRaises(AssertionError):
      self.check()
    self.saved['documents']['outputs/.expri-artifacts.json']['complete'] = True
    self.save_queue()
    self.check()
    (self.run / 'outputs/.expri-artifacts.json').unlink()
    with self.assertRaises(AssertionError):
      self.check()

  def test_inventory_replacement_during_check_cannot_adopt_unacknowledged_entries(self):
    artifact = self.run / 'outputs/project-scope.txt'
    artifact.write_text('worker scope fixture\n')
    inventory = self.run / 'outputs/.expri-artifacts.json'
    original_read = Path.read_bytes
    replaced = False
    def read_bytes(path):
      nonlocal replaced
      content = original_read(path)
      if path == inventory and not replaced:
        replaced = True
        inventory.write_text(json.dumps({'schema_version': 1, 'files': [{'path': 'outputs/project-scope.txt', 'size': artifact.stat().st_size}]}))
      return content
    with patch.object(Path, 'read_bytes', read_bytes), self.assertRaises(AssertionError):
      self.check(artifact_paths=['outputs/project-scope.txt'])
    self.assertTrue(replaced)

  def test_one_origin_queue_cannot_acknowledge_another_origin(self):
    self.check(origin='worker')
    with self.assertRaises(AssertionError):
      self.check(origin='worker-b')


if __name__ == '__main__':
  unittest.main()
