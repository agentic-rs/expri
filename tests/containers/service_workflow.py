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
from urllib.parse import urlencode

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
refresh_browser = None
fixture_env = dict(os.environ)
fixture_env.update({
  'EXPRI_OWNER_TOKEN': secrets.token_hex(24), 'EXPRI_WORKER_TOKEN': secrets.token_hex(24),
  'EXPRI_DASHBOARD_PASSWORD': secrets.token_hex(24),
  'AWS_ACCESS_KEY_ID': 'expri-ci', 'AWS_SECRET_ACCESS_KEY': secrets.token_hex(24),
})
fixture_env['MINIO_ROOT_USER'] = fixture_env['AWS_ACCESS_KEY_ID']
fixture_env['MINIO_ROOT_PASSWORD'] = fixture_env['AWS_SECRET_ACCESS_KEY']
browser_secrets = []

def redact(text):
  for key in ['EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'EXPRI_DASHBOARD_PASSWORD', 'AWS_SECRET_ACCESS_KEY']:
    text = text.replace(fixture_env[key], '[redacted]')
  for cookie in browser_secrets:
    text = text.replace(cookie, '[redacted]')
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
  flags = ['create', '--name', full, '--network', network]
  aliases = alias if isinstance(alias, list) else [alias or name]
  for item in aliases:
    flags += ['--network-alias', item]
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

def browser(path, *, method='GET', cookie=None, bearer_env=None, password_env=None,
    origin='https://expri.example.net', payload=None, limit=512 * 1024):
  # Simulate the TLS reverse proxy inside the private Docker network. The Secure
  # cookie is sent explicitly, without an HTTP cookie jar or published host port.
  code = f'''
import http.client
import json
import os
from urllib.parse import urlencode
headers = {{'Host': 'expri.example.net', 'Origin': {origin!r}, 'Sec-Fetch-Site': 'same-origin'}}
cookie = {cookie!r}
bearer_env = {bearer_env!r}
password_env = {password_env!r}
payload = {payload!r}
body = None
if cookie is not None:
  headers['Cookie'] = cookie
if bearer_env is not None:
  headers['Authorization'] = 'Bearer ' + os.environ[bearer_env]
if password_env is not None:
  headers['Content-Type'] = 'application/x-www-form-urlencoded'
  body = urlencode({{'password': os.environ[password_env]}}).encode()
elif payload is not None:
  headers['Content-Type'] = 'application/json'
  body = json.dumps(payload).encode()
connection = http.client.HTTPConnection('service', 8787, timeout=40)
try:
  connection.request({method!r}, {path!r}, body=body, headers=headers)
  response = connection.getresponse()
  data = response.read({limit} + 1)
  if len(data) > {limit}:
    raise RuntimeError('hosted dashboard response exceeded its size limit')
  print(json.dumps({{'status': response.status, 'headers': dict(response.getheaders()), 'body': data.decode('utf-8')}}))
finally:
  connection.close()
'''
  result = json.loads(execute(host, 'python3', '-c', code, timeout=45).stdout)
  result['headers'] = {name.lower(): value for name, value in result['headers'].items()}
  return result

def browser_json(path, cookie):
  result = browser(path, cookie=cookie)
  assert result['status'] == 200, f'hosted dashboard request failed ({result["status"]})'
  assert result['headers']['content-type'].startswith('application/json')
  return json.loads(result['body'])

def dashboard_login():
  result = browser('/login', method='POST', password_env='EXPRI_DASHBOARD_PASSWORD')
  assert result['status'] == 303 and result['headers'].get('location') == '/', 'dashboard password login failed'
  header = result['headers']['set-cookie']
  cookie = header.split(';', 1)[0]
  browser_secrets.extend([cookie, cookie.split('=', 1)[1]])
  attributes = [attribute.strip().lower() for attribute in header.split(';')[1:]]
  assert cookie.startswith('__Host-expri_session='), 'dashboard session is not host scoped'
  assert {'secure', 'httponly', 'samesite=strict', 'path=/'} <= set(attributes), 'dashboard session cookie flags are incomplete'
  assert not any(attribute.startswith('domain=') for attribute in attributes), 'dashboard cookie unexpectedly sets Domain'
  return cookie

def dashboard_public_checks():
  result = browser('/')
  assert result['status'] == 303 and result['headers'].get('location') == '/login', 'public dashboard root did not redirect to login'
  assert browser('/api/catalog')['status'] == 401, 'unauthenticated dashboard API exposed data'
  login = browser('/login')
  assert login['status'] == 200 and 'autocomplete="current-password"' in login['body'], 'public login form is unavailable'
  for key in ['EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'EXPRI_DASHBOARD_PASSWORD']:
    assert fixture_env[key] not in login['body'], 'public login page exposed a fixture credential'
  assert browser('/login', method='POST', password_env='EXPRI_OWNER_TOKEN')['status'] == 401, 'owner API token replaced the dedicated dashboard password'
  for token in ['EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN']:
    assert browser('/api/catalog', bearer_env=token)['status'] == 401, 'service bearer token authorized the browser dashboard'
  cookie = dashboard_login()
  catalog = browser_json('/api/catalog', cookie)
  assert catalog['access_mode'] == 'hosted' and catalog['sources'] == [], 'new hosted catalog did not begin empty'
  assert browser('/v1/request', method='POST', cookie=cookie, payload={
    'action': 'list_runs', 'project_id': 'demo', 'origin': 'worker',
  })['status'] == 401, 'browser session authorized the writable CLI API'
  return cookie

def dashboard_uploaded_checks(run_id, second_run_id, previous_cookie):
  assert browser('/api/catalog', cookie=previous_cookie)['status'] == 401, 'service restart preserved an old browser session'
  cookie = dashboard_login()
  page = browser('/', cookie=cookie)
  assert page['status'] == 200 and 'id="logout-form"' in page['body'], 'authenticated dashboard HTML is unavailable'
  assert "frame-ancestors 'none'" in page['headers']['content-security-policy'], 'private dashboard can be framed'
  catalog = browser_json('/api/catalog', cookie)
  assert catalog['access_mode'] == 'hosted', 'dashboard catalog is missing hosted access mode'
  matching = [source for source in catalog['sources'] if source['project_id'] == 'demo' and source['origin'] == 'worker']
  assert len(matching) == 1, 'uploaded worker source is missing from the hosted catalog'
  assert matching[0]['kind'] == 'service', 'hosted source kind must match the service frontend contract'
  source_id = matching[0]['source_id']
  listing = browser_json('/api/runs?' + urlencode({
    'source': source_id, 'status': 'completed', 'task': 'train', 'search': run_id, 'limit': 20, 'offset': 0,
  }), cookie)
  assert listing['source']['source_id'] == source_id and isinstance(listing['warnings'], list), 'hosted listing changed its UI shape'
  assert any(run['run_id'] == run_id and run['status'] == 'completed' for run in listing['runs']), 'hosted filters did not find the uploaded completed run'
  detail = browser_json('/api/run?' + urlencode({'source': source_id, 'run_id': run_id}), cookie)
  assert detail['run']['run_id'] == run_id and detail['metrics_error'] is None, 'hosted detail could not review the uploaded run'
  assert detail['params']['learning_rate'] == 0.001 and detail['params']['input_id'] == 'dataset-v1', 'hosted parameters differ from uploaded data'
  assert detail['metrics']['loss']['count'] == 80 and detail['metrics']['loss']['last']['step'] == 79, 'hosted metric summaries differ from uploaded data'
  stdout = browser_json('/api/log?' + urlencode({'source': source_id, 'run_id': run_id, 'stream': 'stdout', 'tail': 100}), cookie)
  assert stdout['stream'] == 'stdout' and not stdout['missing'], 'hosted stdout log is unavailable'
  assert 'step=79' in stdout['content'] and 'training complete' in stdout['content'], 'hosted stdout does not contain the uploaded task output'
  assert stdout['truncated'] and len(stdout['content'].encode()) <= 64 * 1024, 'hosted huge-log preview is unbounded'
  stderr = browser_json('/api/log?' + urlencode({'source': source_id, 'run_id': run_id, 'stream': 'stderr', 'tail': 100}), cookie)
  assert stderr['stream'] == 'stderr' and not stderr['missing'] and len(stderr['content'].encode()) <= 64 * 1024, 'hosted stderr preview is unavailable or unbounded'
  chart = browser('/api/chart?' + urlencode({'source': source_id, 'run_id': run_id, 'metric': 'loss'}), cookie=cookie, limit=2 * 1024 * 1024)
  assert chart['status'] == 200 and chart['headers']['content-type'].startswith('text/html'), 'hosted metric chart is unavailable'
  assert '<svg' in chart['body'] and 'loss' in chart['body'] and run_id in chart['body'], 'hosted chart does not render uploaded metric data'
  assert 'X-Amz-Signature' not in chart['body'], 'hosted chart exposes an object-store credential'
  all_runs = browser_json('/api/runs?' + urlencode({'source': source_id, 'limit': 20}), cookie)['runs']
  compared_ids = [run_id, second_run_id]
  assert set(compared_ids) <= {run['run_id'] for run in all_runs}, 'hosted catalog did not retain both uploaded experiments'
  comparison = browser_json('/api/compare?' + urlencode([
    ('source', source_id), *[('run_id', value) for value in compared_ids], ('metric', 'loss'), ('reduction', 'last'),
  ]), cookie)['comparison']
  assert {run['run_id'] for run in comparison['runs']} == set(compared_ids), 'hosted comparison changed the selected runs'
  assert 'loss' in comparison['metric_names'], 'hosted comparison omitted uploaded metrics'
  assert all(run['values']['loss']['step'] == 79 and run['values']['loss']['value'] == 1 / 80
    for run in comparison['runs']), 'hosted comparison values differ from uploaded metrics'
  assert browser('/logout', method='POST', cookie=cookie, origin='https://outside.invalid')['status'] == 403, 'cross-origin logout was accepted'
  browser_json('/api/catalog', cookie)
  logout = browser('/logout', method='POST', cookie=cookie)
  assert logout['status'] == 303 and logout['headers'].get('location') == '/login', 'dashboard logout did not redirect to login'
  assert 'Max-Age=0' in logout['headers']['set-cookie'], 'dashboard logout did not clear its cookie'
  assert browser('/api/catalog', cookie=cookie)['status'] == 401, 'dashboard logout did not revoke its session'

def publish_refresh_fixture(run_dir, step=None, log=False):
  code = f'''
import json
from datetime import datetime, timezone
from pathlib import Path
root = Path({run_dir!r})
step = {step!r}
if step is not None:
  with (root / 'outputs/metrics.jsonl').open('a') as metrics:
    metrics.write(json.dumps({{'schema_version': 1, 'step': step, 'timestamp': datetime.now(timezone.utc).isoformat(timespec='microseconds').replace('+00:00', 'Z'), 'metrics': {{'loss': 0.005 if step == 80 else 0.004}}}}) + '\\n')
  if step == 81:
    state = json.loads((root / 'run-state.json').read_text())
    state['refresh_probe'] = 'metadata-replacement'
    (root / 'run-state.json').write_text(json.dumps(state))
if {log!r}:
  with (root / 'logs/stdout.log').open('a') as output:
    output.write('automatic refresh log fixture\\n')
'''
  python(worker, code)
  client(worker, 'push', '--run-dir', run_dir, '--project-id', 'demo', '--origin', 'worker', '--queue-dir', '/home/tester/queue')

def automatic_dashboard_checks(run_id, updated):
  global refresh_browser
  coordination = '/tmp/expri-refresh-coordination.json'
  cookie = dashboard_login()
  source_id = 'hosted:demo:worker'
  def phase():
    if refresh_browser.poll() is not None:
      raise RuntimeError('Firefox auto-refresh test exited before fixture coordination finished: ' +
        redact((logs / 'browser-auto-refresh.log').read_text(errors='replace'))[-4096:])
    return python(firefox, f"import json;from pathlib import Path;p=Path({coordination!r});print(json.loads(p.read_text()).get('phase','') if p.exists() else '')")
  def set_phase(value):
    python(firefox, f"import json;from pathlib import Path;Path({coordination!r}).write_text(json.dumps({{'phase':{value!r}}}))")
  python(firefox, f"from pathlib import Path;Path({coordination!r}).unlink(missing_ok=True)")
  with (logs / 'browser-auto-refresh.log').open('wb') as output:
    refresh_browser = subprocess.Popen(['docker', 'exec', '--user', 'tester', firefox,
      'python3', '/opt/expri-browser/browser_forms.py', '--auto-refresh', run_id, updated['run_id']],
      cwd=ROOT, env=fixture_env, stdout=output, stderr=subprocess.STDOUT)
    for step in [80, 81]:
      wait_for(lambda: phase() == f'publish-{step}', f'Firefox did not request sample {step}', timeout=90)
      before = browser_json('/api/updates?' + urlencode({'source': source_id, 'run_id': updated['run_id']}), cookie)
      publish_refresh_fixture(updated['run_dir'], step)
      after = browser_json('/api/updates?' + urlencode({'source': source_id, 'run_id': updated['run_id']}), cookie)
      assert after['runs'][0]['metrics_revision'] != before['runs'][0]['metrics_revision'], 'worker republication did not change the saved metric revision'
      assert after['runs'][0]['metrics_revision'].startswith('object:'), 'finalized worker metrics were not republished as an object'
      if step == 81:
        assert after['runs'][0]['metadata_revision'] != before['runs'][0]['metadata_revision'], 'worker state replacement did not change its metadata revision'
      detail = browser_json('/api/run?' + urlencode({'source': source_id, 'run_id': updated['run_id']}), cookie)
      assert detail['metrics']['loss']['count'] == step + 1 and detail['metrics']['loss']['last']['step'] == step, 'service did not observe the finalized worker publication'
      set_phase(f'published-{step}')
    wait_for(lambda: phase() == 'publish-log', 'Firefox did not request a log publication', timeout=90)
    publish_refresh_fixture(updated['run_dir'], log=True)
    stdout = browser_json('/api/log?' + urlencode({'source': source_id, 'run_id': updated['run_id'], 'stream': 'stdout'}), cookie)
    assert 'automatic refresh log fixture' in stdout['content'], 'service did not observe new worker log bytes'
    set_phase('published-log')
    assert refresh_browser.wait(timeout=90) == 0, ('Firefox auto-refresh acceptance failed: ' +
      redact((logs / 'browser-auto-refresh.log').read_text(errors='replace'))[-4096:])
  assert browser('/logout', method='POST', cookie=cookie)['status'] == 303, 'auto-refresh fixture session did not close'

try:
  if not options.no_build:
    for target in ['worker', 'host', 'service', 'browser']:
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
[dashboard]
public_url = "https://expri.example.net"
password_env = "EXPRI_DASHBOARD_PASSWORD"
[[dashboard.previews]]
public_url = "https://ab.expri.example.net"
assets_dir = "/home/tester/preview"
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
    '--data-dir', '/home/tester/state', '--create-bucket'], env=['AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY', 'EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'EXPRI_DASHBOARD_PASSWORD'])
  copy(server_config, service, '/tmp/server.toml')
  preview = state / 'preview'
  revision = 'a' * 40
  release = preview / 'releases' / revision
  release.mkdir(parents=True)
  for name in ['index.html', 'login.html', 'app.js', 'styles.css']:
    shutil.copyfile(ROOT / 'dashboard_web' / name, release / name)
  (release / 'deployment.json').write_text(json.dumps({'commit': revision, 'branch': 'fixture/ab'}))
  (preview / 'current').symlink_to('releases/' + revision)
  copy(preview, service, '/home/tester/preview')
  host = create('host', 'expri-ci-host', ['infinity'], env=['EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'EXPRI_DASHBOARD_PASSWORD'], entrypoint='sleep')
  worker = create('worker', 'expri-ci-worker', env=['EXPRI_WORKER_TOKEN'])
  proxy = create('proxy', 'expri-ci-host', ['/tmp/proxy.py'], entrypoint='python3')
  firefox = create('browser', 'expri-ci-browser', alias=['expri.example.net', 'ab.expri.example.net'], env=['EXPRI_DASHBOARD_PASSWORD'])
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
  docker('start', firefox)
  wait_for(lambda: python(firefox, "import ssl;from urllib.request import urlopen;print(urlopen('https://expri.example.net/login',context=ssl._create_unverified_context(),timeout=2).status)") == '200', 'Firefox HTTPS proxy not ready')
  logged('browser-forms.log', ['docker', 'exec', '--user', 'tester', firefox,
    'python3', '/opt/expri-browser/browser_forms.py'], timeout=180)
  initial_session = dashboard_public_checks()
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
  second = json.loads(execute(worker, 'sh', '-c', 'cd /home/tester/experiment && expri run --detach train /home/tester/private.bin').stdout)
  assert second['run_id'] != run_id, 'second experiment reused the first run identity'
  wait_for(lambda: json.loads(python(worker, f"from pathlib import Path;print(Path({second['run_dir']!r}+'/run-state.json').read_text())"))['status'] == 'completed', 'second experiment did not complete')
  client(worker, 'push', '--run-dir', second['run_dir'], '--project-id', 'demo', '--origin', 'worker', '--queue-dir', '/home/tester/queue')
  python(host, "from pathlib import Path;p=Path('/home/tester/review');p.mkdir();(p/'expri.toml').write_text('[project]\\nname=\"Offline service review\"\\n[download]\\nresults_dir=\"results\"\\n')")
  pull_args = ['--project-id', 'demo', '--origin', 'worker', '--run-id', run_id, '--repo', '/home/tester/review', '--source', 'service']
  client(host, 'pull', *pull_args)
  local = f'/home/tester/review/results/service/runs/{run_id}'
  assert python(host, f"from pathlib import Path;print(Path({local!r}+'/outputs/checkpoint.pt').exists())") == 'False'
  metrics = json.loads(execute(host, 'expri', '-T', 'service', 'runs', 'metrics', run_id, '--cached', '--config', '/home/tester/review/expri.toml', '--repo', '/home/tester/review', '--json').stdout)
  assert metrics['metrics']['loss']['summary']['last']['step'] == 79
  steps = json.loads(python(host, f"import json;from pathlib import Path;rows=[json.loads(line) for line in Path({local!r}+'/outputs/metrics.jsonl').read_text().splitlines()];print(json.dumps([row['step'] for row in rows if 'loss' in row['metrics']]))"))
  assert steps == list(range(80)), 'metrics were lost or duplicated during recovery'
  duplicate_samples = json.loads(python(host, f"import json;from pathlib import Path;rows=[json.loads(line) for line in Path({local!r}+'/outputs/metrics.jsonl').read_text().splitlines()];print(json.dumps([[row['step'], row['metrics']['duplicate_probe']] for row in rows if 'duplicate_probe' in row['metrics']]))"))
  assert duplicate_samples == [[0, 7.0], [0, 7.0], [1, 8.0]], 'recovery lost repeated-coordinate samples'
  assert python(host, f"from pathlib import Path;print(('STDOUT_BURST:'+'x'*(256*1024)+'\\n').encode() in Path({local!r}+'/logs/stdout.log').read_bytes())") == 'True', 'large log stream was truncated'
  client(host, 'pull', *pull_args, '--artifact', 'outputs/checkpoint.pt')
  digest = python(host, f"import hashlib;from pathlib import Path;print(hashlib.sha256(Path({local!r}+'/outputs/checkpoint.pt').read_bytes()).hexdigest())")
  assert digest == hashlib.sha256(bytes(range(256)) * (4096 * 17)).hexdigest()
  dashboard_uploaded_checks(run_id, second['run_id'], initial_session)
  logged('browser-workspace.log', ['docker', 'exec', '--user', 'tester', firefox,
    'python3', '/opt/expri-browser/browser_forms.py', '--workspace', run_id, second['run_id']], timeout=180)
  logged('browser-previews.log', ['docker', 'exec', '--user', 'tester', firefox,
    'python3', '/opt/expri-browser/browser_forms.py', '--previews', run_id, second['run_id']], timeout=180)
  automatic_dashboard_checks(run_id, second)
  docker('stop', '--time', '1', service, s3)
  execute(host, 'expri', '-T', 'service', 'runs', 'metrics', run_id, '--cached', '--config', '/home/tester/review/expri.toml', '--repo', '/home/tester/review', '--json')
  python(host, "import subprocess;from pathlib import Path;f=Path('/tmp/dashboard.log').open('wb');subprocess.Popen(['expri','-T','service','dashboard','--config','/home/tester/review/expri.toml','--repo','/home/tester/review','--port','0'],stdout=f,stderr=f,start_new_session=True)")
  def review():
    return python(host, "import json;from pathlib import Path;from urllib.request import urlopen;url=Path('/tmp/dashboard.log').read_text().strip().split('Dashboard: ')[1];catalog=json.load(urlopen(url+'/api/catalog',timeout=2));print(catalog['initial_source'])") == 'cached:service'
  wait_for(review, 'offline dashboard did not recognize service cache')
  print('Service workflow passed: Firefox native forms, inputs, offline metrics, restart, multipart resume, selective pulls, authenticated hosted dashboard, offline review.', flush=True)
finally:
  if refresh_browser is not None and refresh_browser.poll() is None:
    refresh_browser.terminate()
    try:
      refresh_browser.wait(timeout=5)
    except subprocess.TimeoutExpired:
      refresh_browser.kill()
      refresh_browser.wait()
  if watch is not None and watch.poll() is None:
    watch.terminate()
    try:
      watch.wait(timeout=5)
    except subprocess.TimeoutExpired:
      watch.kill()
      watch.wait()
  for container in containers:
    try:
      if container.endswith('-browser'):
        docker('cp', f'{container}:/tmp/expri-browser-requests.jsonl', str(logs / 'browser-requests.log'), check=False, timeout=10)
        for name in [
          'workspace-desktop', 'workspace-narrow', 'workspace-ab',
          'workspace-hover', 'workspace-zoom', 'workspace-ab-hover', 'workspace-ab-zoom',
          'workspace-elapsed', 'workspace-wall_clock',
          'workspace-ab-elapsed', 'workspace-ab-wall_clock', 'workspace-auto-refresh',
        ]:
          docker('cp', f'{container}:/tmp/{name}.png', str(logs / (name + '.png')), check=False, timeout=10)
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
