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
from urllib.parse import parse_qs, urlencode, urlsplit

ROOT = Path(__file__).resolve().parents[2]
WORKER_QUEUE = '/home/tester/experiment/.expri/service-sync'
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
refresh_browser = None
fixture_env = dict(os.environ)
fixture_env.update({
  'EXPRI_OWNER_TOKEN': secrets.token_hex(24), 'EXPRI_WORKER_TOKEN': secrets.token_hex(24),
  'EXPRI_MACHINE_TOKEN': secrets.token_hex(24),
  'EXPRI_DASHBOARD_PASSWORD': secrets.token_hex(24),
  'AWS_ACCESS_KEY_ID': 'expri-ci', 'AWS_SECRET_ACCESS_KEY': secrets.token_hex(24),
})
fixture_env['MINIO_ROOT_USER'] = fixture_env['AWS_ACCESS_KEY_ID']
fixture_env['MINIO_ROOT_PASSWORD'] = fixture_env['AWS_SECRET_ACCESS_KEY']
browser_secrets = []

def redact(text):
  for key in ['EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'EXPRI_MACHINE_TOKEN', 'EXPRI_DASHBOARD_PASSWORD', 'AWS_SECRET_ACCESS_KEY']:
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

def client(container, action, *args, check=True, config=None):
  config = config or ('/tmp/worker.toml' if container == worker else '/tmp/owner.toml')
  return execute(container, 'expri', 'service', *action.split(), '--config', config, *args, check=check)

def browser(path, *, method='GET', cookie=None, bearer_env=None, password_env=None,
    json_password_env=None, origin='https://expri.example.net', payload=None, limit=512 * 1024):
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
  if {json_password_env!r} is not None:
    payload['password'] = os.environ[{json_password_env!r}]
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

def service_record(action, run_id, origin='worker'):
  result = browser('/v1/request', method='POST', bearer_env='EXPRI_OWNER_TOKEN', payload={
    'action': action, 'scope': {'project_id': 'demo', 'origin': origin, 'run_id': run_id},
  })
  assert result['status'] == 200, 'owner could not inspect the tracking catalog'
  return json.loads(result['body'])

def tracking_files(run_id):
  return {file['target']['path']: file for file in service_record('list_files', run_id)['files']
    if isinstance(file['storage'], dict) and 'tracking' in file['storage']}

def wait_archive(run_id):
  wait_for(lambda: service_record('archive_status', run_id)['archive']['status'] == 'archived',
    'server did not finish the independent result archive', timeout=90)
  return service_record('archive_status', run_id)['archive']

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
  assert browser('/api/projects')['status'] == 401, 'unauthenticated project dashboard exposed data'
  assert browser('/api/run-columns?source=service:demo:worker')['status'] == 401, 'unauthenticated run columns exposed data'
  assert browser('/api/artifacts?source=service:demo:worker&run_id=run-a')['status'] == 401, 'unauthenticated artifact catalog exposed data'
  assert browser('/api/artifact?source=service:demo:worker&run_id=run-a&path=outputs/checkpoint.pt')['status'] == 401, 'unauthenticated artifact download exposed data'
  assert browser('/api/storage?project_id=demo&kind=input')['status'] == 401, 'unauthenticated project storage catalog exposed data'
  assert browser('/api/input?project_id=demo&input_id=dataset-v1')['status'] == 401, 'unauthenticated private input download exposed data'
  assert browser('/api/archive?source=service:demo:worker&run_id=run-a')['status'] == 401, 'unauthenticated archive download exposed data'
  assert browser('/api/events')['status'] == 401, 'unauthenticated live updates exposed data'
  login = browser('/login')
  assert login['status'] == 200 and 'autocomplete="current-password"' in login['body'], 'public login form is unavailable'
  for key in ['EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'EXPRI_MACHINE_TOKEN', 'EXPRI_DASHBOARD_PASSWORD']:
    assert fixture_env[key] not in login['body'], 'public login page exposed a fixture credential'
  assert browser('/login', method='POST', password_env='EXPRI_OWNER_TOKEN')['status'] == 401, 'owner API token replaced the dedicated dashboard password'
  for token in ['EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'EXPRI_MACHINE_TOKEN']:
    assert browser('/api/catalog', bearer_env=token)['status'] == 401, 'service bearer token authorized the browser dashboard'
  cookie = dashboard_login()
  catalog = browser_json('/api/catalog', cookie)
  assert catalog['access_mode'] == 'hosted' and catalog['sources'] == [], 'new hosted catalog did not begin empty'
  assert browser('/v1/request', method='POST', cookie=cookie, payload={
    'action': 'list_runs', 'project_id': 'demo', 'origin': 'worker',
  })['status'] == 401, 'browser session authorized the writable CLI API'
  return cookie

def project_storage_checks(cookie, run_id=None, second_run_id=None):
  def catalog(kind, **query):
    return browser_json('/api/storage?' + urlencode({'project_id': 'demo', 'kind': kind, **query}), cookie)

  inputs = catalog('input', limit=1)
  assert inputs['project_id'] == 'demo' and inputs['kind'] == 'input' and inputs['total_count'] == 2, 'project storage did not count both completed private inputs'
  assert len(inputs['items']) == 1 and inputs['next_offset'] == 1, 'project input catalog did not bound its first page'
  next_page = catalog('input', limit=1, offset=inputs['next_offset'])
  assert len(next_page['items']) == 1 and next_page['next_offset'] is None, 'project input catalog did not finish its second page'
  input_rows = {item['input_id']: item for item in inputs['items'] + next_page['items']}
  assert set(input_rows) == {'dataset-v1', 'empty-file'}, 'project input catalog changed immutable input IDs'
  assert input_rows['dataset-v1']['size'] == len(b'private-input-fixture') * 1024 and input_rows['empty-file']['size'] == 0, 'project input catalog reported wrong sizes'
  assert [item['input_id'] for item in catalog('input', search='dataset-v1')['items']] == ['dataset-v1'], 'project input search did not filter by input ID'
  assert not catalog('input', search='private.bin')['items'], 'project input search exposed the original local filename'
  serialized = json.dumps(input_rows)
  assert '/home/tester/private.bin' not in serialized and 'X-Amz-' not in serialized, 'project input catalog exposed a local filename or signed URL'
  input_url = input_rows['dataset-v1']['download_url']
  input_link = urlsplit(input_url)
  assert input_link.path == '/api/input' and not input_link.scheme and not input_link.netloc and parse_qs(input_link.query) == {'project_id': ['demo'], 'input_id': ['dataset-v1']}, 'project input download link changed its project or ID'
  attachment = browser(input_url, cookie=cookie)
  assert attachment['status'] == 303 and attachment['headers']['referrer-policy'] == 'no-referrer', 'input download did not use a protected no-referrer redirect'
  head = browser(input_url, method='HEAD', cookie=cookie)
  assert head['status'] == 200 and int(head['headers']['content-length']) == input_rows['dataset-v1']['size'] and 'location' not in head['headers'], 'input HEAD did not describe its finalized object safely'
  assert browser(input_url, cookie=cookie, origin='https://outside.invalid')['status'] == 403, 'cross-origin input download was accepted'
  assert browser('/api/input?project_id=other&input_id=dataset-v1', cookie=cookie)['status'] == 404, 'private input download crossed project scope'
  assert browser('/api/input?project_id=demo&input_id=unknown', cookie=cookie)['status'] == 404, 'unknown private input acquired a download URL'
  foreign = browser('/api/storage?project_id=other&kind=input', cookie=cookie)
  assert foreign['status'] in {200, 404} and (foreign['status'] == 404 or json.loads(foreign['body'])['items'] == []), 'project storage exposed another project\'s inputs'
  assert browser('/api/storage?project_id=demo&kind=unknown', cookie=cookie)['status'] == 400, 'unknown storage kind was accepted'

  if run_id is not None:
    outputs = catalog('output', search='checkpoint.pt')
    assert outputs['project_id'] == 'demo' and outputs['kind'] == 'output', 'project output catalog changed its scope'
    checkpoint = next((item for item in outputs['items'] if item['origin'] == 'worker' and item['run_id'] == run_id and item['path'] == 'outputs/checkpoint.pt'), None)
    assert checkpoint is not None and checkpoint['size'] == 17 * 1024 * 1024, 'project Storage omitted the uploaded checkpoint'
    assert not any(item['run_id'] == second_run_id and item['path'] == 'outputs/checkpoint.pt' for item in outputs['items']), 'worker-only checkpoint appeared as a completed cloud object'
    assert checkpoint['download_url'].startswith('/api/artifact?') and 'X-Amz-' not in json.dumps(outputs), 'project output catalog exposed a signed URL or changed the artifact route'
    output_link = urlsplit(checkpoint['download_url'])
    assert not output_link.scheme and not output_link.netloc and parse_qs(output_link.query) == {'source': ['hosted-project:demo'], 'run_id': [f'worker:{run_id}'], 'path': ['outputs/checkpoint.pt']}, 'project output download link changed its machine or run'
    assert browser(checkpoint['download_url'], method='HEAD', cookie=cookie)['status'] == 200, 'project output link did not resolve the completed checkpoint'
    assert not catalog('output', search='dataset-v1')['items'], 'private inputs leaked into project output results'

def dashboard_uploaded_checks(run_id, second_run_id, previous_cookie):
  assert browser('/api/catalog', cookie=previous_cookie)['status'] == 401, 'service restart preserved an old browser session'
  cookie = dashboard_login()
  project_storage_checks(cookie, run_id, second_run_id)
  page = browser('/', cookie=cookie)
  assert page['status'] == 200 and 'id="logout-form"' in page['body'], 'authenticated dashboard HTML is unavailable'
  assert "frame-ancestors 'none'" in page['headers']['content-security-policy'], 'private dashboard can be framed'
  catalog = browser_json('/api/catalog', cookie)
  assert catalog['access_mode'] == 'hosted', 'dashboard catalog is missing hosted access mode'
  matching = [source for source in catalog['sources'] if source['project_id'] == 'demo' and source['origin'] == 'worker']
  assert len(matching) == 1, 'uploaded worker source is missing from the hosted catalog'
  assert matching[0]['kind'] == 'service', 'hosted source kind must match the service frontend contract'
  source_id = matching[0]['source_id']
  columns = browser_json('/api/run-columns?' + urlencode({'source': source_id}), cookie)
  assert any(column['key'] == '/learning_rate' for column in columns['available_columns']['params']), 'hosted run column discovery omitted the uploaded parameter'
  assert any(column['key'] == 'loss' for column in columns['available_columns']['metrics']), 'hosted run column discovery omitted the uploaded metric'
  for reduction, expected in [('max', 1.0), ('min', 1 / 80), ('last', 1 / 80)]:
    values = browser_json('/api/runs?' + urlencode([
      ('source', source_id), ('param', '/learning_rate'), ('metric', 'loss'),
      ('reduction', reduction), ('sort', 'run_id'), ('direction', 'asc'),
    ]), cookie)['runs']
    assert [run['run_id'] for run in values] == sorted([run_id, second_run_id]), 'hosted table sorting changed the full uploaded run listing'
    assert all(run['table_values']['params']['/learning_rate'] == 0.001 and run['table_values']['metrics']['loss'] == expected for run in values), 'hosted table column values differ from the uploaded parameters or metric reduction'
  listing = browser_json('/api/runs?' + urlencode({
    'source': source_id, 'status': 'completed', 'task': 'train', 'search': run_id, 'limit': 20, 'offset': 0,
  }), cookie)
  assert listing['source']['source_id'] == source_id and isinstance(listing['warnings'], list), 'hosted listing changed its UI shape'
  assert any(run['run_id'] == run_id and run['status'] == 'completed' for run in listing['runs']), 'hosted filters did not find the uploaded completed run'
  detail = browser_json('/api/run?' + urlencode({'source': source_id, 'run_id': run_id}), cookie)
  assert detail['run']['run_id'] == run_id and detail['metrics_error'] is None, 'hosted detail could not review the uploaded run'
  assert detail['params']['learning_rate'] == 0.001 and detail['params']['input_id'] == 'dataset-v1', 'hosted parameters differ from uploaded data'
  assert detail['metrics']['loss']['count'] == 80 and detail['metrics']['loss']['last']['step'] == 79, 'hosted metric summaries differ from uploaded data'
  assert detail['archive']['status'] == 'archived' and detail['archive']['incomplete'] is False, 'terminal training did not retain a distinct completed archive status'
  archive_url = '/api/archive?' + urlencode({'source': source_id, 'run_id': run_id})
  archived = browser(archive_url, cookie=cookie)
  assert archived['status'] == 303 and archived['headers']['referrer-policy'] == 'no-referrer', 'archive download did not use a protected attachment redirect'
  archive_head = browser(archive_url, method='HEAD', cookie=cookie)
  assert archive_head['status'] == 200 and int(archive_head['headers']['content-length']) == detail['archive']['file']['size'], 'archive HEAD did not describe its stored snapshot'
  assert browser(archive_url, cookie=cookie, origin='https://outside.invalid')['status'] == 403, 'cross-origin archive download was accepted'
  artifacts = browser_json('/api/artifacts?' + urlencode({'source': source_id, 'run_id': run_id}), cookie)
  checkpoint = next(file for file in artifacts['files'] if file['path'] == 'outputs/checkpoint.pt')
  assert checkpoint['size'] == 17 * 1024 * 1024 and checkpoint['cloud'] is True and checkpoint['worker'] is True and checkpoint['local'] is None, 'artifact location or size differs from the published checkpoint'
  assert checkpoint['download_url'].startswith('/api/artifact?') and 'X-Amz-' not in json.dumps(artifacts), 'artifact catalog exposed a signed URL'
  assert artifacts['pull_scope'] == {'project_id': 'demo', 'origin': 'worker', 'run_id': run_id}, 'artifact pull command changed run scope'
  other_files = browser_json('/api/artifacts?' + urlencode({'source': source_id, 'run_id': second_run_id}), cookie)
  other_checkpoint = next(file for file in other_files['files'] if file['path'] == 'outputs/checkpoint.pt')
  assert other_checkpoint['worker'] is True and other_checkpoint['cloud'] is False and other_checkpoint['download_url'] is None, 'worker-only checkpoint was advertised as a cloud download'
  attachment = browser(checkpoint['download_url'], cookie=cookie)
  assert attachment['status'] == 303 and attachment['headers']['referrer-policy'] == 'no-referrer', 'artifact download did not use a protected no-referrer redirect'
  head = browser(checkpoint['download_url'], method='HEAD', cookie=cookie)
  assert head['status'] == 200 and int(head['headers']['content-length']) == checkpoint['size'] and 'location' not in head['headers'], 'artifact HEAD did not describe its stored size safely'
  assert browser(checkpoint['download_url'], cookie=cookie, origin='https://outside.invalid')['status'] == 403, 'cross-origin artifact download was accepted'
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
    metrics.write(json.dumps({{'schema_version': 1, 'step': step, 'timestamp': datetime.now(timezone.utc).isoformat(timespec='microseconds').replace('+00:00', 'Z'), 'metrics': {{'loss': 0.005 if step == 80 else 0.004}}}}) + chr(10))
  if step == 81:
    state = json.loads((root / 'run-state.json').read_text())
    state['refresh_probe'] = 'metadata-replacement'
    (root / 'run-state.json').write_text(json.dumps(state))
if {log!r}:
  with (root / 'logs/stdout.log').open('a') as output:
    output.write('automatic refresh log fixture' + chr(10))
'''
  python(worker, code)
  client(worker, 'push', '--run-dir', run_dir, '--project-id', 'demo', '--origin', 'worker', '--queue-dir', WORKER_QUEUE)

def running_refresh_fixture(original):
  run_id = 'refresh-' + original['run_id']
  run_dir = '/home/tester/refresh-run'
  python(worker, f'''import json, shutil
from pathlib import Path
root = Path({run_dir!r})
shutil.copytree({original['run_dir']!r}, root)
record = json.loads((root / 'run-state.json').read_text())
record.update(run_id={run_id!r}, status='running', finished_at=None, exit_code=None)
(root / 'run-state.json').write_text(json.dumps(record))
''')
  client(worker, 'push', '--run-dir', run_dir, '--project-id', 'demo', '--origin', 'worker', '--queue-dir', WORKER_QUEUE)
  return {'run_id': run_id, 'run_dir': run_dir}

def acknowledged_prefix_checks(updated):
  records = tracking_files(updated['run_id'])
  metrics = records['outputs/metrics.jsonl']
  assert metrics['storage']['tracking']['sealed'] is False, 'active metrics were sealed before completion'
  size = metrics['size']
  expected = python(worker, f"import hashlib;from pathlib import Path;print(hashlib.sha256(Path({updated['run_dir']!r}+'/outputs/metrics.jsonl').read_bytes()[:{size}]).hexdigest())")
  python(worker, f"from pathlib import Path;p=Path({updated['run_dir']!r}+'/outputs/metrics.jsonl');p.open('ab').write(b'{{\"step\":999,\"metrics\":{{\"loss\":99}}}}'+bytes([10]));print('unsent tail')")
  docker('stop', '--time', '1', worker)
  docker('restart', '--time', '1', service)
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://service:8787/health',timeout=1).status)") == '200', 'service did not recover its acknowledged raw tracking files')
  prefix_args = ['--project-id', 'demo', '--origin', 'worker', '--run-id', updated['run_id'], '--repo', '/home/tester/review', '--source', 'service']
  client(host, 'pull', *prefix_args)
  local = f"/home/tester/review/results/service/runs/{updated['run_id']}"
  result = json.loads(python(host, f"import hashlib,json;from pathlib import Path;p=Path({local!r}+'/outputs/metrics.jsonl');print(json.dumps({{'size':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()}}))"))
  assert result == {'size': size, 'sha256': expected}, 'offline worker pull exposed an unacknowledged tail or lost the acknowledged prefix'
  partial = json.loads(client(host, 'archive', '--project-id', 'demo', '--origin', 'worker', '--run-id', updated['run_id'], '--partial').stdout)
  assert partial['incomplete'] is True and partial['status'] in ['pending', 'uploading', 'archived', 'failed'], 'owner partial archive did not capture the acknowledged active prefix'
  archive = wait_archive(updated['run_id'])
  assert archive['incomplete'] is True, 'interrupted run archive lost its partial marker'
  client(host, 'pull', *prefix_args, '--artifact', 'result.zip')
  manifest = json.loads(python(host, f"import json,zipfile;from pathlib import Path;z=zipfile.ZipFile(Path({local!r})/'result.zip');m=json.loads(z.read('manifest.json'));assert len(z.read('outputs/metrics.jsonl'))=={size};print(json.dumps(m))"))
  assert manifest['incomplete'] is True and manifest['scope']['run_id'] == updated['run_id'], 'partial ZIP changed its run identity or completeness'
  docker('start', worker)
  wait_for(lambda: execute(worker, 'true', check=False).returncode == 0, 'worker did not resume after the recovery fixture')
  python(worker, f"from pathlib import Path;p=Path({updated['run_dir']!r}+'/outputs/metrics.jsonl');p.open('r+b').truncate({size});print('restored acknowledged fixture')")

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
      assert 'outputs/metrics.jsonl' in tracking_files(updated['run_id']), 'live worker metrics lost their acknowledged tracking storage'
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

def project_machine_fixture(original):
  run_dir = '/home/tester/project-worker-b'
  staging = state / 'project-machine-run'
  docker('cp', f"{worker}:{original['run_dir']}", str(staging))
  copy(staging, host, run_dir)
  docker('exec', '--user', '0', host, 'chown', '-R', 'tester:tester', run_dir)
  python(host, f'''import json
from pathlib import Path
root = Path({run_dir!r})
rows = [json.loads(line) for line in (root / 'outputs/metrics.jsonl').read_text().splitlines()]
timed = [row for row in rows if 'timestamp' in row]
for row, value in [(timed[0], 0.7), (timed[-1], 0.07)]:
  row['metrics'] = {{'loss': value}}
(root / 'outputs/metrics.jsonl').write_text(''.join(json.dumps(row) + chr(10) for row in [timed[0], timed[-1]]))
(root / 'outputs/params.json').write_text(json.dumps({{'learning_rate': 0.002, 'machine_probe': 'worker-b'}}))
(root / 'logs/stdout.log').write_text('project machine worker-b log' + chr(10))
(root / 'outputs/project-scope.txt').write_text('worker-b scope fixture' + chr(10))
''')
  client(host, 'push', '--run-dir', run_dir, '--project-id', 'demo', '--origin', 'worker-b',
    '--queue-dir', '/home/tester/project-machine-queue', config='/tmp/machine-worker.toml')
  python(worker, f"from pathlib import Path;Path({original['run_dir']!r}+'/outputs/project-scope.txt').write_text('worker scope fixture'+chr(10))")
  client(worker, 'push', '--run-dir', original['run_dir'], '--project-id', 'demo', '--origin', 'worker',
    '--queue-dir', WORKER_QUEUE)
  assert json.loads(python(host, f"from pathlib import Path;print(Path({run_dir!r}+'/run-state.json').read_text())"))['run_id'] == original['run_id'], 'second recorded machine did not preserve the colliding run ID'
  return {'run_id': original['run_id'], 'run_dir': run_dir}

def project_api_checks(first, second):
  cookie = dashboard_login()
  source = 'hosted-project:demo'
  projects = browser_json('/api/projects', cookie)
  matching = [project for project in projects['sources'] if project['source_id'] == source]
  assert len(matching) == 1 and matching[0]['kind'] == 'hosted_project', 'project catalog did not aggregate the recorded machines'
  keys = ['worker:' + first['run_id'], 'worker-b:' + second['run_id']]
  listing = browser_json('/api/runs?' + urlencode({'source': source}), cookie)['runs']
  collisions = [run for run in listing if run['run_id'] == first['run_id']]
  assert {run['run_key'] for run in collisions} == set(keys), 'project listing collapsed equal run IDs from different machines'
  assert {run['origin'] for run in collisions} == {'worker', 'worker-b'}, 'project listing omitted machine provenance'
  filtered = browser_json('/api/runs?' + urlencode({'source': source, 'origin': 'worker-b', 'sort': 'origin', 'direction': 'asc'}), cookie)['runs']
  assert len(filtered) == 1 and filtered[0]['run_key'] == keys[1], 'machine filter selected the wrong colliding run'
  comparison = browser_json('/api/compare?' + urlencode([('source', source), *[('run_id', key) for key in keys], ('metric', 'loss')]), cookie)['comparison']
  assert {run['run_id'] for run in comparison['runs']} == set(keys), 'cross-machine comparison collapsed transport identities'
  for origin, key, value in [('worker', keys[0], 0.004), ('worker-b', keys[1], 0.07)]:
    row = next(run for run in comparison['runs'] if run['run_id'] == key)
    assert row['run']['run_id'] == first['run_id'] and row['run']['origin'] == origin, 'comparison replaced the recorded run ID or machine'
    assert row['values']['loss']['value'] == value, 'cross-machine comparison read a different machine metric stream'
    detail = browser_json('/api/run?' + urlencode({'source': source, 'run_id': key}), cookie)
    assert detail['run']['run_id'] == first['run_id'] and detail['run']['origin'] == origin, 'project detail resolved the wrong machine'
    files = browser_json('/api/artifacts?' + urlencode({'source': source, 'run_id': key}), cookie)
    assert files['pull_scope'] == {'project_id': 'demo', 'origin': origin, 'run_id': first['run_id']}, 'project artifact pull command changed the actual scope'
    artifact = next(file for file in files['files'] if file['path'] == 'outputs/project-scope.txt')
    assert artifact['worker'] is True and artifact['cloud'] is False and artifact['download_url'] is None, 'active project artifact inventory did not preserve its reported machine location'
  rejected = browser('/v1/request', method='POST', bearer_env='EXPRI_MACHINE_TOKEN', payload={
    'action': 'list_runs', 'project_id': 'demo', 'origin': 'worker',
  })
  assert rejected['status'] == 403, 'second machine token authorized the first recorded origin'
  browser('/logout', method='POST', cookie=cookie)

def project_dashboard_checks(first, second):
  global refresh_browser
  coordination = '/tmp/expri-project-coordination.json'
  python(firefox, f"from pathlib import Path;Path({coordination!r}).unlink(missing_ok=True)")
  def phase():
    if refresh_browser.poll() is not None:
      raise RuntimeError('Firefox project test exited before publication: ' + redact((logs / 'browser-project.log').read_text(errors='replace'))[-4096:])
    return python(firefox, f"import json;from pathlib import Path;p=Path({coordination!r});print(json.loads(p.read_text()).get('phase','') if p.exists() else '')")
  with (logs / 'browser-project.log').open('wb') as output:
    refresh_browser = subprocess.Popen(['docker', 'exec', '--user', 'tester', firefox,
      'python3', '/opt/expri-browser/browser_forms.py', '--project', first['run_id']],
      cwd=ROOT, env=fixture_env, stdout=output, stderr=subprocess.STDOUT)
    for origin, container, fixture, step, value, config, queue in [
      ('worker', worker, first, 82, 0.003, '/tmp/worker.toml', WORKER_QUEUE),
      ('worker-b', host, second, 83, 0.006, '/tmp/machine-worker.toml', '/home/tester/project-machine-queue'),
    ]:
      wait_for(lambda: phase() == 'publish-' + origin, 'Firefox did not request project publication from ' + origin, timeout=90)
      python(container, f'''import json
from datetime import datetime, timezone
from pathlib import Path
root = Path({fixture['run_dir']!r})
with (root / 'outputs/metrics.jsonl').open('a') as metrics:
  metrics.write(json.dumps({{'schema_version': 1, 'step': {step}, 'timestamp': datetime.now(timezone.utc).isoformat(timespec='microseconds').replace('+00:00', 'Z'), 'metrics': {{'loss': {value}}}}}) + chr(10))
with (root / 'logs/stdout.log').open('a') as output:
  output.write('project live update from {origin}' + chr(10))
''')
      client(container, 'push', '--run-dir', fixture['run_dir'], '--project-id', 'demo', '--origin', origin,
        '--queue-dir', queue, config=config)
      python(firefox, f"import json;from pathlib import Path;Path({coordination!r}).write_text(json.dumps({{'phase': 'published-{origin}'}}))")
    wait_for(lambda: phase() == 'finish-runs', 'Firefox did not request terminal project artifacts', timeout=90)
    for origin, container, fixture, config, queue in [
      ('worker', worker, first, '/tmp/worker.toml', WORKER_QUEUE),
      ('worker-b', host, second, '/tmp/machine-worker.toml', '/home/tester/project-machine-queue'),
    ]:
      python(container, f'''import json
from datetime import datetime, timezone
from pathlib import Path
path=Path({fixture['run_dir']!r})/'run-state.json'
record=json.loads(path.read_text())
record.update(status='completed', exit_code=0, finished_at=datetime.now(timezone.utc).isoformat().replace('+00:00','Z'))
path.write_text(json.dumps(record))
''')
      client(container, 'push', '--run-dir', fixture['run_dir'], '--project-id', 'demo', '--origin', origin,
        '--queue-dir', queue, '--artifact', 'outputs/project-scope.txt', config=config)
      python(container, f'''import json
from pathlib import Path
saved=json.loads((Path({queue!r})/'runs/demo'/{origin!r}/{fixture['run_id']!r}/'queue.json').read_text())
assert saved['protocol']=='tracking_v1' and saved['archive']['incomplete'] is False
assert all(document['complete'] for document in saved['documents'].values())
assert saved['files']['outputs/project-scope.txt']['upload']['complete'] is True
record=json.loads((Path({fixture['run_dir']!r})/'run-state.json').read_text())
assert record['status']=='completed' and record['exit_code']==0 and record['finished_at']
print('project terminal tracking queue verified')
''')
    project_artifact_checks(first, second)
    python(firefox, f"import json;from pathlib import Path;Path({coordination!r}).write_text(json.dumps({{'phase': 'finished-runs'}}))")
    wait_for(lambda: refresh_browser.poll() is not None, 'Firefox project workflow did not finish', timeout=90)
    if refresh_browser.returncode:
      raise RuntimeError('Firefox project workflow failed: ' + redact((logs / 'browser-project.log').read_text(errors='replace'))[-4096:])
    refresh_browser = None

def project_artifact_checks(first, second):
  cookie=dashboard_login()
  for origin, fixture in [('worker', first), ('worker-b', second)]:
    key=origin+':'+fixture['run_id']
    query={'source':'hosted-project:demo','run_id':key}
    detail=browser_json('/api/run?'+urlencode(query),cookie)
    assert detail['run']['status']=='completed' and detail['run']['exit_code']==0 and detail['run']['finished_at'], 'project terminal fields did not reach their machine scope'
    files=browser_json('/api/artifacts?'+urlencode(query),cookie)
    assert files['pull_scope']=={'project_id':'demo','origin':origin,'run_id':fixture['run_id']}, 'terminal project artifact command changed its recorded scope'
    artifact=next(file for file in files['files'] if file['path']=='outputs/project-scope.txt')
    expected_size=len((origin+' scope fixture\n').encode())
    assert artifact['worker'] is True and artifact['cloud'] is True and artifact['size']==expected_size, 'terminal project artifact inventory did not report the finalized machine file'
    head=browser(artifact['download_url'],method='HEAD',cookie=cookie)
    assert head['status']==200 and int(head['headers']['content-length'])==expected_size, 'project artifact HEAD resolved a different machine file'
    assert browser(artifact['download_url'],cookie=cookie,origin='https://outside.invalid')['status']==403, 'cross-origin project artifact request was accepted'
  browser('/logout',method='POST',cookie=cookie)


def storage_management_checks():
  """Delete only an isolated fixture project; verify real versioned S3 cleanup."""
  import base64

  project_id = 'delete-me'
  scope = {'project_id': project_id, 'origin': 'fixture', 'run_id': 'release'}
  source = {'kind': 'input', 'project_id': project_id, 'input_id': 'blob'}
  shared = {'kind': 'run', 'scope': scope, 'path': 'outputs/shared.bin'}
  alias = {'kind': 'input', 'project_id': project_id, 'input_id': 'alias'}
  old = {'kind': 'run', 'scope': scope, 'path': 'outputs/old.bin'}
  contents = b'shared-dataset-fixture'
  digest = hashlib.sha256(contents).hexdigest()
  path = '/home/tester/management.bin'
  python(host, f'from pathlib import Path;Path({path!r}).write_bytes({contents!r})')
  client(host, 'input put', '--project-id', project_id, '--input-id', 'blob', '--file', path,
    '--queue-dir', '/home/tester/management-queue')
  for target in [shared, alias]:
    result = browser('/v1/request', method='POST', bearer_env='EXPRI_OWNER_TOKEN', payload={
      'action': 'reference_file', 'source': source, 'target': target, 'size': len(contents), 'sha256': digest,
    })
    assert result['status'] == 200, 'owner could not create the shared-object fixture'
  for value in [b'old', b'next']:
    python(host, f'from pathlib import Path;Path("/home/tester/old.bin").write_bytes({value!r})')
    client(host, 'file-put', '--target', json.dumps(old), '--file', '/home/tester/old.bin',
      '--queue-dir', '/home/tester/management-queue')
  document = json.dumps({'run_id': 'release', 'task': 'fixture', 'status': 'completed'}).encode()
  metrics = b'{"schema_version":1,"step":0,"metrics":{"loss":1}}\n'
  for action, remote_path, value, extra in [
    ('put_document', 'run-state.json', document, {'revision': 1, 'total_size': len(document)}),
    ('append_tracking', 'outputs/metrics.jsonl', metrics, {}),
  ]:
    result = browser('/v1/request', method='POST', bearer_env='EXPRI_OWNER_TOKEN', payload={
      'action': action, 'scope': scope, 'path': remote_path, 'offset': 0,
      'data_base64': base64.b64encode(value).decode(), **extra,
    })
    assert result['status'] == 200, 'tracking cleanup fixture was not acknowledged'
  pending = browser('/v1/request', method='POST', bearer_env='EXPRI_OWNER_TOKEN', payload={
    'action': 'begin_upload', 'upload_id': 'management-pending',
    'target': {'kind': 'run', 'scope': scope, 'path': 'outputs/pending.bin'},
    'size': 1024, 'sha256': hashlib.sha256(b'x' * 1024).hexdigest(),
  })
  assert pending['status'] == 200
  part_status = python(host, '''import json, os
from urllib.request import Request, urlopen
request = Request('http://service:8787/v1/request',
  data=json.dumps({'action':'part_url','upload_id':'management-pending','part_number':1}).encode(),
  headers={'Authorization':'Bearer '+os.environ['EXPRI_OWNER_TOKEN'],'Content-Type':'application/json'})
with urlopen(request,timeout=10) as response:
  url = json.load(response)['url']
with urlopen(Request(url,data=b'x'*1024,method='PUT'),timeout=10) as response:
  print(response.status)
''')
  assert part_status == '200', 'fixture multipart part was not stored'
  prefix = 'acceptance/projects/' + project_id + '/'
  pending_key = prefix + 'runs/fixture/release/objects/management-pending'
  def s3_state(action, object_prefix=prefix):
    return json.loads(execute(host, 'python3', '/tmp/s3_fixture.py', action, '--prefix', object_prefix).stdout)
  surviving_prefix = 'acceptance/projects/demo/inputs/dataset-v1/'
  surviving_versions = s3_state('versions', surviving_prefix)['versions']
  assert surviving_versions, 'another project had no S3 data to preserve'
  originals = s3_state('versions')['versions']
  original_key = next(item['key'] for item in originals if '/inputs/blob/' in item['key'])
  execute(host, 'python3', '/tmp/s3_fixture.py', 'put', '--key', original_key, '--file', path)
  assert len(s3_state('versions')['versions']) == len(originals) + 1, 'fixture bucket did not retain a historical version'
  # MinIO's multipart listing supports exact object keys rather than directory prefixes.
  assert s3_state('uploads', pending_key)['uploads'] == [pending_key], 'fixture multipart session was not created'
  cookie = dashboard_login()
  stats_path = '/api/storage/stats?' + urlencode({'project_id': project_id})
  stats = browser_json(stats_path, cookie)
  assert stats['delete_enabled'] is True
  stats = stats['stats']
  assert stats['file_count'] == 4 and stats['logical_bytes'] == len(contents) * 3 + 4
  assert stats['object_count'] == 2 and stats['object_bytes'] == len(contents) + 4, 'shared references counted as extra physical objects'
  assert stats['shared_reference_count'] == 2
  assert stats['retained_object_count'] == 1 and stats['retained_object_bytes'] == 3, 'superseded upload was absent from retained-object stats'
  assert stats['pending_upload_count'] == 1 and stats['pending_upload_bytes'] == 1024
  assert stats['tracking_bytes'] == len(document) + len(metrics)
  assert stats['reclaimable_object_count'] == 3 and stats['reclaimable_object_bytes'] == len(contents) + 7
  assert browser(stats_path)['status'] == 401
  forbidden = client(worker, 'project stats', '--project-id', project_id, check=False)
  assert forbidden.returncode != 0, 'worker acquired project-management authority'
  preview_path = '/api/projects/delete-preview?' + urlencode({'project_id': project_id})
  preview = browser_json(preview_path, cookie)
  assert preview['run_count'] == 1 and preview['stats'] == stats
  changed = browser('/v1/request', method='POST', bearer_env='EXPRI_OWNER_TOKEN', payload={
    'action': 'append_tracking', 'scope': scope, 'path': 'logs/stdout.log', 'offset': 0,
    'data_base64': base64.b64encode(b'new log\n').decode(),
  })
  assert changed['status'] == 200
  payload = {'project_id': project_id, 'revision': preview['revision'], 'confirmation': project_id}
  stale = browser('/api/projects/delete', method='POST', cookie=cookie, payload=payload,
    json_password_env='EXPRI_DASHBOARD_PASSWORD')
  assert stale['status'] == 409, 'changed project was deleted from a stale preview'
  preview = browser_json(preview_path, cookie)
  payload['revision'] = preview['revision']
  assert browser('/api/projects/delete', method='POST', cookie=cookie,
    payload={**payload, 'password': 'incorrect-password'})['status'] == 403
  assert browser('/api/projects/delete', method='POST', cookie=cookie,
    payload={**payload, 'confirmation': 'another-project'}, json_password_env='EXPRI_DASHBOARD_PASSWORD')['status'] == 400
  assert browser('/api/projects/delete', method='POST', cookie=cookie,
    payload=payload, json_password_env='EXPRI_DASHBOARD_PASSWORD', origin='https://outside.invalid')['status'] == 403
  assert browser_json(stats_path, cookie)['stats']['file_count'] == 4, 'rejected deletions changed project data'
  logged('browser-storage-management.log', ['docker', 'exec', '--user', 'tester', firefox,
    'python3', '/opt/expri-browser/browser_forms.py', '--storage-management', project_id], timeout=90)
  docker('stop', '--time', '1', s3)
  accepted = browser('/api/projects/delete', method='POST', cookie=cookie, payload=payload,
    json_password_env='EXPRI_DASHBOARD_PASSWORD')
  assert accepted['status'] == 202 and json.loads(accepted['body'])['status'] == 'pending'
  status_path = '/api/projects/deletion?' + urlencode({'project_id': project_id})
  wait_for(lambda: browser_json(status_path, cookie)['last_error'] is not None,
    'S3 outage did not leave a visible retryable deletion', timeout=45)
  assert project_id not in {item['project_id'] for item in browser_json('/api/projects', cookie)['sources']}
  blocked = browser('/v1/request', method='POST', bearer_env='EXPRI_OWNER_TOKEN', payload={
    'action': 'append_tracking', 'scope': scope, 'path': 'logs/stdout.log', 'offset': 0,
    'data_base64': base64.b64encode(b'recreated\n').decode(),
  })
  assert blocked['status'] == 410, 'a publisher resurrected the deleted project'
  docker('stop', '--time', '1', service)
  docker('start', s3)
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://s3:9000/minio/health/ready',timeout=1).status)") == '200', 'S3 fixture did not restart')
  docker('start', service)
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://service:8787/health',timeout=1).status)") == '200', 'service did not resume durable project deletion')
  cookie = dashboard_login()
  wait_for(lambda: browser_json(status_path, cookie)['status'] == 'deleted',
    'project cleanup did not resume after restart', timeout=90)
  assert s3_state('versions')['versions'] == [], 'project deletion retained S3 versions or delete markers'
  assert s3_state('uploads', pending_key)['uploads'] == [], 'project deletion retained a multipart upload'
  assert python(service, "from pathlib import Path;print(Path('/home/tester/state/tracking/delete-me').exists())") == 'False', 'project tracking files remained on the server'
  assert 'demo' in {item['project_id'] for item in browser_json('/api/projects', cookie)['sources']}, 'cleanup deleted another project'
  assert s3_state('versions', surviving_prefix)['versions'] == surviving_versions, 'cleanup changed another project\'s S3 versions'
  survivor = json.loads(execute(host, 'python3', '/tmp/s3_fixture.py', 'head', '--key', surviving_versions[0]['key']).stdout)
  assert survivor['status'] == 200, 'another project\'s object became unavailable in S3'
  assert browser('/api/input?project_id=demo&input_id=dataset-v1', method='HEAD', cookie=cookie)['status'] == 200
  browser('/logout', method='POST', cookie=cookie)

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
[[workers]]
project_id = "demo"
origin = "worker-b"
token_env = "EXPRI_MACHINE_TOKEN"
[dashboard]
public_url = "https://expri.example.net"
password_env = "EXPRI_DASHBOARD_PASSWORD"
allow_project_deletion = true
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
  (state / 'machine-worker.toml').write_text('url = "http://proxy:8001"\ntoken_env = "EXPRI_MACHINE_TOKEN"\n')
  service = create('service', 'expri-ci-service', ['--config', '/tmp/server.toml', '--listen', '0.0.0.0:8787',
    '--data-dir', '/home/tester/state', '--create-bucket'], env=['AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY', 'EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'EXPRI_MACHINE_TOKEN', 'EXPRI_DASHBOARD_PASSWORD'])
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
  host = create('host', 'expri-ci-host', ['infinity'], env=['EXPRI_OWNER_TOKEN', 'EXPRI_WORKER_TOKEN', 'EXPRI_MACHINE_TOKEN', 'EXPRI_DASHBOARD_PASSWORD', 'AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY'], entrypoint='sleep')
  worker = create('worker', 'expri-ci-worker', env=['EXPRI_WORKER_TOKEN'])
  proxy = create('proxy', 'expri-ci-host', ['/tmp/proxy.py'], entrypoint='python3')
  firefox = create('browser', 'expri-ci-browser', alias=['expri.example.net', 'ab.expri.example.net', 's3.expri.example.net'], env=['EXPRI_DASHBOARD_PASSWORD'])
  copy(state / 'key.pub', worker, '/run/expri-ssh/id_ed25519.pub')
  copy(ROOT / 'tests/containers/service_proxy.py', proxy, '/tmp/proxy.py')
  copy(state / 'owner.toml', host, '/tmp/owner.toml')
  copy(state / 'machine-worker.toml', host, '/tmp/machine-worker.toml')
  copy(state / 'worker.toml', worker, '/tmp/worker.toml')
  copy(ROOT / 'tests/containers/s3_fixture.py', host, '/tmp/s3_fixture.py')
  docker('start', host)
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://s3:9000/minio/health/ready',timeout=1).status)") == '200', 'S3 fixture not ready')
  docker('start', service)
  docker('start', proxy)
  docker('start', worker)
  wait_for(lambda: docker('exec', worker, 'test', '-f', '/tmp/expri-worker.ready', check=False).returncode == 0, 'worker not ready')
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://service:8787/health',timeout=1).status)") == '200', 'service not ready')
  execute(host, 'python3', '/tmp/s3_fixture.py', 'versioning')
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
  assert python(worker, "from pathlib import Path;print(Path('/home/tester/private.bin').read_bytes() == b'private-input-fixture'*1024)") == 'True', 'private input bytes changed during worker download'
  input_only = browser_json('/api/projects', initial_session)
  project = next((source for source in input_only['sources'] if source['source_id'] == 'hosted-project:demo'), None)
  assert project is not None and project['machines'] == [], 'input-only project is absent from hosted project discovery'
  assert browser_json('/api/catalog', initial_session)['sources'] == [], 'private inputs unexpectedly became run sources'
  project_storage_checks(initial_session)
  logged('browser-storage-input-only.log', ['docker', 'exec', '--user', 'tester', firefox,
    'python3', '/opt/expri-browser/browser_forms.py', '--storage-input-only'], timeout=90)
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
  assert receipt['dashboard_url'] == 'https://expri.example.net/?' + urlencode({'project_id': 'demo', 'origin': 'worker', 'run_id': run_id}), 'automatic run link has the wrong identity'
  push_args = ['--run-dir', run_dir, '--project-id', 'demo', '--origin', 'worker', '--queue-dir', WORKER_QUEUE]
  def publishing(directory):
    return json.loads(python(worker, f"from pathlib import Path;print(Path({directory!r}+'/publishing-state.json').read_text())"))
  wait_for(lambda: api_proxy('/test/state')['lost_stream_ack'], 'live metrics did not arrive or the lost stream acknowledgement was not injected')
  assert json.loads(python(worker, f"from pathlib import Path;print(Path({run_dir!r}+'/run-state.json').read_text())"))['status'] == 'running', 'training finished before the service outage was injected'
  docker('stop', '--time', '1', service)
  wait_for(lambda: json.loads(python(worker, f"from pathlib import Path;print(Path({run_dir!r}+'/run-state.json').read_text())"))['status'] == 'completed', 'training did not complete while service was offline')
  assert publishing(run_dir)['status'] != 'synced', 'publisher claimed synced during the outage'
  # Kill only the independent publisher after training exits, then resume its
  # saved intent and queue while the service is still unavailable.
  killed = python(worker, f'''import os, signal
from pathlib import Path
count = 0
for process in Path('/proc').iterdir():
  if not process.name.isdigit():
    continue
  try:
    argv = (process / 'cmdline').read_bytes().split(b'\\0')
    if b'publish-worker' in argv and {run_dir.encode()!r} in argv:
      os.kill(int(process.name), signal.SIGTERM)
      count += 1
  except (FileNotFoundError, ProcessLookupError, PermissionError):
    pass
print(count)''')
  assert killed == '1', 'did not find exactly one independent publisher'
  wait_for(lambda: json.loads(execute(worker, 'expri', 'runs', 'status', run_id, '--config', '/home/tester/experiment/expri.toml', '--repo', '/home/tester/experiment', '--json').stdout)['service_sync']['worker_active'] is False, 'stopped publisher retained its lease')
  resumed = json.loads(execute(worker, 'expri', 'service', 'resume', '--run-dir', run_dir).stdout)
  assert resumed['run_id'] == run_id, 'resume started a different run'
  docker('start', service)
  wait_for(lambda: python(host, "from urllib.request import urlopen;print(urlopen('http://service:8787/health',timeout=1).status)") == '200', 'service failed to restart')
  def drained(directory):
    report = json.loads(execute(worker, 'expri', 'runs', 'status', Path(directory).name, '--config', '/home/tester/experiment/expri.toml', '--repo', '/home/tester/experiment', '--json').stdout)
    return report['service_sync']['status'] == 'synced' and not report['service_sync']['worker_active']
  wait_for(lambda: drained(run_dir), 'automatic publisher failed to drain after recovery', timeout=120)
  queue_state = json.loads(python(worker, f"from pathlib import Path;print(Path({WORKER_QUEUE!r}+'/runs/demo/worker/'+{run_id!r}+'/queue.json').read_text())"))
  assert queue_state['protocol'] == 'tracking_v1' and queue_state['files'] == {}, 'fresh publishing queue used terminal per-file multipart instead of tracking-v1'
  tracked = tracking_files(run_id)
  assert all(path in tracked for path in ['run-state.json', 'snapshot.json', 'outputs/params.json', 'outputs/metrics.jsonl', 'logs/stdout.log', 'logs/stderr.log']), 'terminal tracking catalog omitted run records or raw streams'
  assert tracked['outputs/metrics.jsonl']['storage']['tracking']['sealed'] is True, 'terminal metrics were not sealed at their acknowledged extent'
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
  wait_for(lambda: drained(second['run_dir']), 'second automatic publisher did not drain', timeout=120)
  for archived_run in [run_id, second['run_id']]:
    archive = wait_archive(archived_run)
    assert archive['incomplete'] is False and archive['file']['target']['path'] == 'result.zip', 'terminal archive did not finish independently as a complete result ZIP'
  assert set(api_proxy('/test/state')['multipart_paths']) <= {'outputs/checkpoint.pt'}, 'worker finalized tracking documents or streams through per-file S3 multipart'
  intent = python(worker, f"from pathlib import Path;print(Path({run_dir!r}+'/publishing-request.json').read_text())")
  assert fixture_env['EXPRI_WORKER_TOKEN'] not in intent, 'publishing intent exposed its token'
  assert 'X-Amz-' not in intent, 'publishing intent exposed a signed URL'
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
  # Interrupt an actual pull after one durable range, then restart the same CLI.
  # Previous cached metadata and unselected files must remain intact throughout.
  previous_state = python(host, f"import hashlib;from pathlib import Path;print(hashlib.sha256(Path({local!r}+'/run-state.json').read_bytes()).hexdigest())")
  api_proxy('/test/arm-download')
  download_command = ['expri', 'service', 'pull', '--config', '/tmp/owner.toml', *pull_args,
    '--artifact', 'outputs/checkpoint.pt']
  pull_pid = int(python(host, f'''import subprocess
from pathlib import Path
with open('/tmp/checkpoint-pull.stdout', 'wb') as out, open('/tmp/checkpoint-pull.stderr', 'wb') as err:
  process = subprocess.Popen({download_command!r}, stdin=subprocess.DEVNULL, stdout=out, stderr=err, start_new_session=True)
print(process.pid)
'''))
  progress = f'/home/tester/review/results/service/.service-pull/{run_id}/state.json'
  wait_for(lambda: api_proxy('/test/state')['download_blocked'], 'checkpoint pull did not reach its second range')
  assert json.loads(python(host, f"from pathlib import Path;print(Path({progress!r}).read_text())"))['files']['outputs/checkpoint.pt']['offset'] == 8 * 1024 * 1024, 'first range progress was not durable'
  python(host, f'import os,signal;os.kill({pull_pid},signal.SIGKILL);print("interrupted")')
  assert python(host, f"from pathlib import Path;print(Path({local!r}+'/outputs/checkpoint.pt').exists())") == 'False', 'partial checkpoint was published into the review cache'
  assert python(host, f"import hashlib;from pathlib import Path;print(hashlib.sha256(Path({local!r}+'/run-state.json').read_bytes()).hexdigest())") == previous_state, 'interrupted pull changed previous metadata'
  saved_progress = python(host, f"from pathlib import Path;print(Path({progress!r}).read_text())")
  assert 'X-Amz-' not in saved_progress and fixture_env['EXPRI_OWNER_TOKEN'] not in saved_progress, 'download progress exposed a credential'
  api_proxy('/test/release-download')
  download_report = json.loads(client(host, 'pull', *pull_args, '--artifact', 'outputs/checkpoint.pt').stdout)
  assert download_report['resumed_bytes'] >= 8 * 1024 * 1024 and download_report['resumed_files'] >= 1, 'pull did not resume its saved checkpoint range'
  assert api_proxy('/test/state')['download_ranges'].get('bytes=0-8388607') == 1, 'restart downloaded the acknowledged checkpoint prefix again'
  digest = python(host, f"import hashlib;from pathlib import Path;print(hashlib.sha256(Path({local!r}+'/outputs/checkpoint.pt').read_bytes()).hexdigest())")
  assert digest == hashlib.sha256(bytes(range(256)) * (4096 * 17)).hexdigest()
  dashboard_uploaded_checks(run_id, second['run_id'], initial_session)
  logged('browser-storage.log', ['docker', 'exec', '--user', 'tester', firefox,
    'python3', '/opt/expri-browser/browser_forms.py', '--storage', run_id], timeout=90)
  python(firefox, "from pathlib import Path;Path('/tmp/expri-browser-legacy-catalog').touch()")
  logged('browser-workspace.log', ['docker', 'exec', '--user', 'tester', firefox,
    'python3', '/opt/expri-browser/browser_forms.py', '--workspace', run_id, second['run_id']], timeout=180)
  logged('browser-previews.log', ['docker', 'exec', '--user', 'tester', firefox,
    'python3', '/opt/expri-browser/browser_forms.py', '--previews', run_id, second['run_id']], timeout=180)
  logged('browser-deep-link.log', ['docker', 'exec', '--user', 'tester', firefox,
    'python3', '/opt/expri-browser/browser_forms.py', '--deep-link', run_id, second['run_id']], timeout=180)
  refresh_run = running_refresh_fixture(second)
  acknowledged_prefix_checks(refresh_run)
  automatic_dashboard_checks(run_id, refresh_run)
  python(firefox, "from pathlib import Path;Path('/tmp/expri-browser-legacy-catalog').unlink()")
  machine_run = project_machine_fixture(refresh_run)
  project_api_checks(refresh_run, machine_run)
  project_dashboard_checks(refresh_run, machine_run)
  # Failed and cancelled runs must also finish publication, without a selected
  # final checkpoint preventing their metadata/logs from draining.
  failed = json.loads(execute(worker, 'sh', '-c', 'cd /home/tester/experiment && expri run --detach fail').stdout)
  wait_for(lambda: drained(failed['run_dir']), 'failed run publisher did not drain', timeout=120)
  failed_state = json.loads(python(worker, f"from pathlib import Path;print(Path({failed['run_dir']!r}+'/run-state.json').read_text())"))
  assert failed_state['status'] == 'failed' and failed_state['exit_code'] == 7, 'publishing changed the failed task result'
  cancelled = json.loads(execute(worker, 'sh', '-c', 'cd /home/tester/experiment && expri run --detach wait').stdout)
  wait_for(lambda: json.loads(python(worker, f"from pathlib import Path;print(Path({cancelled['run_dir']!r}+'/run-state.json').read_text())"))['status'] == 'running', 'cancellation fixture did not start')
  execute(worker, 'expri', 'runs', 'cancel', cancelled['run_id'], '--config', '/home/tester/experiment/expri.toml', '--repo', '/home/tester/experiment', '--json')
  wait_for(lambda: drained(cancelled['run_dir']), 'cancelled run publisher did not drain', timeout=120)
  cancelled_state = json.loads(python(worker, f"from pathlib import Path;print(Path({cancelled['run_dir']!r}+'/run-state.json').read_text())"))
  assert cancelled_state['status'] == 'cancelled', 'publishing changed cancellation status'
  terminal_session = dashboard_login()
  terminal_catalog = browser_json('/api/catalog', terminal_session)
  terminal_source = next(source['source_id'] for source in terminal_catalog['sources'] if source['project_id'] == 'demo' and source['origin'] == 'worker')
  for result, expected in [(failed, 'failed'), (cancelled, 'cancelled')]:
    detail = browser_json('/api/run?' + urlencode({'source': terminal_source, 'run_id': result['run_id']}), terminal_session)
    assert detail['run']['status'] == expected, 'hosted result does not show the terminal training status'
  browser('/logout', method='POST', cookie=terminal_session)
  storage_management_checks()
  docker('stop', '--time', '1', service, s3)
  execute(host, 'expri', '-T', 'service', 'runs', 'metrics', run_id, '--cached', '--config', '/home/tester/review/expri.toml', '--repo', '/home/tester/review', '--json')
  python(host, "import subprocess;from pathlib import Path;f=Path('/tmp/dashboard.log').open('wb');subprocess.Popen(['expri','-T','service','dashboard','--config','/home/tester/review/expri.toml','--repo','/home/tester/review','--port','0'],stdout=f,stderr=f,start_new_session=True)")
  def review():
    return python(host, "import json;from pathlib import Path;from urllib.request import urlopen;url=Path('/tmp/dashboard.log').read_text().strip().split('Dashboard: ')[1];catalog=json.load(urlopen(url+'/api/catalog',timeout=2));print(catalog['initial_source'])") == 'cached:service'
  wait_for(review, 'offline dashboard did not recognize service cache')
  offline_artifact = json.loads(python(host, f'''import hashlib, json
from pathlib import Path
from urllib.parse import urlencode
from urllib.request import urlopen
url = Path('/tmp/dashboard.log').read_text().strip().split('Dashboard: ')[1]
query = urlencode({{'source': 'cached:service', 'run_id': {run_id!r}}})
catalog = json.load(urlopen(url + '/api/artifacts?' + query, timeout=2))
checkpoint = next(file for file in catalog['files'] if file['path'] == 'outputs/checkpoint.pt')
digest = hashlib.sha256()
with urlopen(url + checkpoint['download_url'], timeout=5) as response:
  assert response.headers['Content-Disposition'].startswith('attachment;')
  for chunk in iter(lambda: response.read(64 * 1024), b''):
    digest.update(chunk)
print(json.dumps({{'file': checkpoint, 'sha256': digest.hexdigest()}}))
'''))
  assert offline_artifact['file']['local'] is True and offline_artifact['file']['cloud'] is True, 'offline Files lost cached availability'
  assert offline_artifact['sha256'] == digest, 'offline dashboard download changed the cached checkpoint'
  print('Service workflow passed: tracking-v1 publishing, acknowledged-prefix recovery, complete/partial archives, Firefox native downloads/SSE/chart review, versioned project storage cleanup/restart, checkpoint multipart/range recovery, terminal statuses and offline review.', flush=True)
finally:
  if refresh_browser is not None and refresh_browser.poll() is None:
    refresh_browser.terminate()
    try:
      refresh_browser.wait(timeout=5)
    except subprocess.TimeoutExpired:
      refresh_browser.kill()
      refresh_browser.wait()
  for container in containers:
    try:
      if container.endswith('-browser'):
        docker('cp', f'{container}:/tmp/expri-browser-requests.jsonl', str(logs / 'browser-requests.log'), check=False, timeout=10)
        for name in [
          'workspace-desktop', 'workspace-narrow', 'workspace-ab', 'workspace-files', 'workspace-files-narrow',
          'workspace-storage-input-only', 'workspace-storage-inputs', 'workspace-storage', 'workspace-storage-narrow',
          'workspace-storage-management',
          'workspace-columns-desktop', 'workspace-columns-1024', 'workspace-columns-320', 'workspace-columns-360',
          'workspace-project-default-desktop', 'workspace-project-default-1024',
          'workspace-project-desktop', 'workspace-project-1024',
          'workspace-project-320', 'workspace-project-360', 'workspace-project-failure',
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
