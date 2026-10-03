"""CI acceptance: real worker -> service/S3 -> local cache, with transfer faults."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--no-build', action='store_true')
options = parser.parse_args()
logs = Path(os.environ['EXPRI_CONTAINER_ARTIFACTS']) if 'EXPRI_CONTAINER_ARTIFACTS' in os.environ else Path(tempfile.mkdtemp(prefix='expri-service-logs-'))
logs.mkdir(parents=True, exist_ok=True)
print(f'Service workflow logs: {logs}', flush=True)
state = Path(tempfile.mkdtemp(prefix='expri-service-state-'))
network = f'expri-service-{os.getpid()}'
containers = []
created_network = False
watch = None
fixture_env = dict(os.environ)
fixture_env.update({
  'EXPRI_OWNER_TOKEN': secrets.token_hex(24), 'EXPRI_WORKER_TOKEN': secrets.token_hex(24),
  'AWS_ACCESS_KEY_ID': 'expri-ci', 'AWS_SECRET_ACCESS_KEY': secrets.token_hex(24),
})
fixture_env['MINIO_ROOT_USER'] = fixture_env['AWS_ACCESS_KEY_ID']
fixture_env['MINIO_ROOT_PASSWORD'] = fixture_env['AWS_SECRET_ACCESS_KEY']

def redact(text):
  for key in ['EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'AWS_SECRET_ACCESS_KEY']:
    text = text.replace(fixture_env[key], '[redacted]')
  return re.sub(r'(https?://[^\s"<>?]+)\?[^\s"<>]+', r'\1?[redacted]', text)

def command(args, *, timeout=90, check=True):
  try:
    result = subprocess.run(args, cwd=ROOT, env=fixture_env, capture_output=True, timeout=timeout)
  except subprocess.TimeoutExpired as error:
    raise AssertionError(f'{args[0]} {args[1]} timed out after {timeout}s') from error
  if check and result.returncode:
    raise AssertionError(f'{args[0]} {args[1]} failed ({result.returncode}): ' + redact(result.stderr.decode(errors='replace'))[-4096:])
  return result

def docker(*args, **kwargs):
  return command(['docker', *args], **kwargs)

def logged(name, args, timeout=600):
  with (logs / name).open('wb') as output:
    try:
      result = subprocess.run(args, cwd=ROOT, env=fixture_env, stdout=output, stderr=subprocess.STDOUT, timeout=timeout)
    except subprocess.TimeoutExpired as error:
      raise AssertionError(f'{name} timed out after {timeout}s') from error
  if result.returncode:
    raise AssertionError(f'{name} failed: ' + redact((logs / name).read_text(errors='replace'))[-4096:])

def create(name, image, args=(), env=(), alias=None, entrypoint=None):
  full = f'{network}-{name}'
  flags = ['create', '--name', full, '--network', network, '--network-alias', alias or name]
  for item in env:
    flags += ['--env', item]
  if entrypoint:
    flags += ['--entrypoint', entrypoint]
  docker(*flags, image, *args)
  containers.append(full)
  return full

def copy(source, destination, path):
  docker('cp', str(source), f'{destination}:{path}')

def execute(container, *args, check=True, timeout=90):
  return docker('exec', '--user', 'tester', container, *args, check=check, timeout=timeout)

def python(container, code):
  return execute(container, 'python3', '-c', code, timeout=10).stdout.decode().strip()

def api_proxy(path):
  return json.loads(python(host, f"import json;from urllib.request import urlopen;print(urlopen('http://proxy:8001{path}',timeout=2).read().decode())"))

def wait_for(predicate, message, timeout=60):
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    try:
      if predicate():
        return
    except AssertionError:
      pass
    time.sleep(0.2)
  raise AssertionError(message)

def client(container, action, *args, check=True):
  config = '/tmp/worker.toml' if container == worker else '/tmp/owner.toml'
  return execute(container, 'expri', 'service', *action.split(), '--config', config, *args, check=check)

try:
  if not options.no_build:
    for target in ['worker', 'host', 'service']:
      logged(f'build-{target}.log', ['docker', 'build', '--target', target, '--tag', f'expri-ci-{target}',
        '--file', 'tests/containers/Dockerfile', '.'], timeout=900)
  s3_image = os.environ.get('EXPRI_TEST_S3_IMAGE', 'expri-ci-s3')
  if 'EXPRI_TEST_S3_IMAGE' in os.environ:
    logged('s3-pull.log', ['docker', 'pull', s3_image])
  elif not options.no_build:
    logged('build-s3.log', ['docker', 'build', '--tag', s3_image,
      '--file', 'tests/containers/S3.Dockerfile', '.'], timeout=1200)
  docker('network', 'create', '--internal', network)
  created_network = True
  subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(state / 'key')], check=True)
  s3 = create('s3', s3_image, ['server', '/data'], env=['MINIO_ROOT_USER', 'MINIO_ROOT_PASSWORD'])
  docker('start', s3)
  server_config = state / 'server.toml'
  server_config.write_text('''owner_token_env = "EXPRI_OWNER_TOKEN"
[[workers]]
project_id = "demo"
origin = "worker"
token_env = "EXPRI_WORKER_TOKEN"
[storage]
endpoint = "http://s3:9000"
bucket = "expri-ci"
region = "us-east-1"
path_style = true
prefix = "acceptance"
''')
  (state / 'owner.toml').write_text('url = "http://proxy:8001"\ntoken_env = "EXPRI_OWNER_TOKEN"\n')
  (state / 'worker.toml').write_text('url = "http://proxy:8001"\ntoken_env = "EXPRI_WORKER_TOKEN"\n')
  service = create('service', 'expri-ci-service', ['--config', '/tmp/server.toml', '--listen', '0.0.0.0:8787',
    '--data-dir', '/home/tester/state', '--create-bucket'], env=['AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY', 'EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN'])
  copy(server_config, service, '/tmp/server.toml')
  host = create('host', 'expri-ci-host', ['infinity'], env=['EXPRI_OWNER_TOKEN'], entrypoint='sleep')
  worker = create('worker', 'expri-ci-worker', env=['EXPRI_WORKER_TOKEN'])
  proxy = create('proxy', 'expri-ci-host', ['/tmp/proxy.py'], entrypoint='python3')
  copy(state / 'key.pub', worker, '/run/expri-ssh/id_ed25519.pub')
  copy(ROOT / 'tests/containers/service_proxy.py', proxy, '/tmp/proxy.py')
  copy(state / 'owner.toml', host, '/tmp/owner.toml')
  copy(state / 'worker.toml', worker, '/tmp/worker.toml')
  docker('start', host)
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://s3:9000/minio/health/ready',timeout=1).status)") == '200', 'S3 fixture not ready')
  docker('start', service)
  docker('start', proxy)
  docker('start', worker)
  wait_for(lambda: docker('exec', worker, 'test', '-f', '/tmp/expri-worker.ready', check=False).returncode == 0, 'worker not ready')
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://service:8787/health',timeout=1).status)") == '200', 'service not ready')
  wait_for(lambda: api_proxy('/test/state') is not None, 'fault proxy not ready')
  python(host, "from pathlib import Path;Path('/home/tester/private.bin').write_bytes(b'private-input-fixture'*1024)")
  client(host, 'input put', '--project-id', 'demo', '--input-id', 'dataset-v1', '--file', '/home/tester/private.bin', '--queue-dir', '/home/tester/queue')
  client(worker, 'input get', '--project-id', 'demo', '--input-id', 'dataset-v1', '--destination', '/home/tester/private.bin')
  python(host, "from pathlib import Path;Path('/home/tester/empty.bin').write_bytes(b'')")
  client(host, 'input put', '--project-id', 'demo', '--input-id', 'empty-file', '--file', '/home/tester/empty.bin', '--queue-dir', '/home/tester/queue')
  client(worker, 'input get', '--project-id', 'demo', '--input-id', 'empty-file', '--destination', '/home/tester/empty.bin')
  assert python(worker, "from pathlib import Path;print(Path('/home/tester/empty.bin').stat().st_size)") == '0', 'empty input did not round-trip'
  forbidden = client(worker, 'input put', '--project-id', 'demo', '--input-id', 'forbidden', '--file', '/home/tester/private.bin', '--queue-dir', '/home/tester/input-queue', check=False)
  assert forbidden.returncode != 0, 'worker unexpectedly uploaded a private input'
  python(host, "from pathlib import Path;Path('/home/tester/different.bin').write_bytes(b'different dataset')")
  immutable = client(host, 'input put', '--project-id', 'demo', '--input-id', 'dataset-v1', '--file', '/home/tester/different.bin', '--queue-dir', '/home/tester/queue', check=False)
  assert immutable.returncode != 0, 'input reference allowed replacement with different bytes'
  execute(worker, 'mkdir', '-p', '/home/tester/experiment')
  copy(ROOT / 'tests/containers/service_seed.py', worker, '/tmp/seed.py')
  copy(ROOT / 'tests/containers/service_train.py', worker, '/home/tester/experiment/train.py')
  copy(ROOT / 'python/expri_metrics.py', worker, '/home/tester/experiment/expri_metrics.py')
  execute(worker, 'python3', '/tmp/seed.py')
  receipt = json.loads(execute(worker, 'sh', '-c', 'cd /home/tester/experiment && expri run --detach train /home/tester/private.bin').stdout)
  run_id, run_dir = receipt['run_id'], receipt['run_dir']
  push_args = ['--run-dir', run_dir, '--project-id', 'demo', '--origin', 'worker', '--queue-dir', '/home/tester/queue']
  watch_log = (logs / 'watch.log').open('wb')
  watch = subprocess.Popen(['docker', 'exec', '--user', 'tester', worker, 'expri', 'service', 'push', '--config', '/tmp/worker.toml', *push_args, '--watch'],
    cwd=ROOT, env=fixture_env, stdout=watch_log, stderr=subprocess.STDOUT)
  wait_for(lambda: api_proxy('/test/state')['lost_stream_ack'], 'live metrics did not arrive or the lost stream acknowledgement was not injected')
  assert json.loads(python(worker, f"from pathlib import Path;print(Path({run_dir!r}+'/run-state.json').read_text())"))['status'] == 'running', 'training finished before the service outage was injected'
  docker('stop', '--time', '1', service)
  wait_for(lambda: json.loads(python(worker, f"from pathlib import Path;print(Path({run_dir!r}+'/run-state.json').read_text())"))['status'] == 'completed', 'training did not complete while service was offline')
  docker('start', service)
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://service:8787/health',timeout=1).status)") == '200', 'service failed to restart')
  assert watch.wait(timeout=120) == 0, 'watch failed to drain after service recovery'
  watch_log.close()
  api_proxy('/test/arm')
  first = client(worker, 'push', *push_args, '--artifact', 'outputs/checkpoint.pt', check=False)
  assert first.returncode != 0 and api_proxy('/test/state')['lost_ack'], 'lost part acknowledgement was not injected'
  docker('restart', '--time', '1', service)
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://service:8787/health',timeout=1).status)") == '200', 'service failed to recover multipart state')
  client(worker, 'push', *push_args, '--artifact', 'outputs/checkpoint.pt')
  counts = api_proxy('/test/state')['part_urls']
  assert counts.get('1') == 1 and counts.get('2') == 1 and counts.get('3') == 1, counts
  python(host, "from pathlib import Path;p=Path('/home/tester/review');p.mkdir();(p/'expri.toml').write_text('[project]\\nname=\"Offline service review\"\\n[download]\\nresults_dir=\"results\"\\n')")
  pull_args = ['--project-id', 'demo', '--origin', 'worker', '--run-id', run_id, '--repo', '/home/tester/review', '--source', 'service']
  client(host, 'pull', *pull_args)
  local = f'/home/tester/review/results/service/runs/{run_id}'
  assert python(host, f"from pathlib import Path;print(Path({local!r}+'/outputs/checkpoint.pt').exists())") == 'False'
  metrics = json.loads(execute(host, 'expri', '-T', 'service', 'runs', 'metrics', run_id, '--cached', '--config', '/home/tester/review/expri.toml', '--repo', '/home/tester/review', '--json').stdout)
  assert metrics['metrics']['loss']['summary']['last']['step'] == 79
  steps = json.loads(python(host, f"import json;from pathlib import Path;print(json.dumps([json.loads(line)['step'] for line in Path({local!r}+'/outputs/metrics.jsonl').read_text().splitlines()]))"))
  assert steps == list(range(80)), 'metrics were lost or duplicated during recovery'
  assert python(host, f"from pathlib import Path;print(('STDOUT_BURST:'+'x'*(256*1024)+'\\n').encode() in Path({local!r}+'/logs/stdout.log').read_bytes())") == 'True', 'large log stream was truncated'
  client(host, 'pull', *pull_args, '--artifact', 'outputs/checkpoint.pt')
  digest = python(host, f"import hashlib;from pathlib import Path;print(hashlib.sha256(Path({local!r}+'/outputs/checkpoint.pt').read_bytes()).hexdigest())")
  assert digest == hashlib.sha256(bytes(range(256)) * (4096 * 17)).hexdigest()
  docker('stop', '--time', '1', service, s3)
  execute(host, 'expri', '-T', 'service', 'runs', 'metrics', run_id, '--cached', '--config', '/home/tester/review/expri.toml', '--repo', '/home/tester/review', '--json')
  python(host, "import subprocess;from pathlib import Path;f=Path('/tmp/dashboard.log').open('wb');subprocess.Popen(['expri','-T','service','dashboard','--config','/home/tester/review/expri.toml','--repo','/home/tester/review','--port','0'],stdout=f,stderr=f,start_new_session=True)")
  def review():
    return python(host, "import json;from pathlib import Path;from urllib.request import urlopen;url=Path('/tmp/dashboard.log').read_text().strip().split('Dashboard: ')[1];catalog=json.load(urlopen(url+'/api/catalog',timeout=2));print(catalog['initial_source'])") == 'cached:service'
  wait_for(review, 'offline dashboard did not recognize service cache')
  print('Service workflow passed: inputs, offline metrics, restart, multipart resume, selective pulls, offline review.', flush=True)
finally:
  if watch is not None and watch.poll() is None:
    watch.terminate()
    try:
      watch.wait(timeout=5)
    except subprocess.TimeoutExpired:
      watch.kill()
      watch.wait()
  for container in containers:
    try:
      with (logs / (container.rsplit('-', 1)[-1] + '.log')).open('wb') as output:
        subprocess.run(['docker', 'logs', container], stdout=output, stderr=subprocess.STDOUT, timeout=15)
    except (OSError, subprocess.TimeoutExpired):
      print('Could not collect a container log before cleanup.', flush=True)
    finally:
      try:
        docker('rm', '--force', container, check=False, timeout=20)
      except (OSError, AssertionError):
        print('Container cleanup did not finish; check the local container engine.', flush=True)
  if created_network:
    try:
      docker('network', 'rm', network, check=False, timeout=20)
    except (OSError, AssertionError):
      print('Network cleanup did not finish; check the local container engine.', flush=True)
  for path in logs.glob('*.log'):
    text = path.read_text(errors='replace')
    path.write_text(redact(text))
  shutil.rmtree(state)
