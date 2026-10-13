"""Publication readiness waits for the inventory revision, not registration."""
import ast
from contextlib import redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


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
