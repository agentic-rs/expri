"""Drive native hosted forms in isolated Firefox through W3C WebDriver."""
import http.client
import fixture_coordination
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import shlex
import zipfile
from urllib.parse import urlencode, urlsplit

ORIGIN = 'https://expri.example.net'
POLICY_OVERRIDE = Path('/tmp/expri-browser-policy')
TRACE = Path('/tmp/expri-browser-requests.jsonl')
REFRESH_COORDINATION = Path('/tmp/expri-refresh-coordination.json')
PROJECT_COORDINATION = Path('/tmp/expri-project-coordination.json')
DOWNLOADS = Path('/tmp/expri-downloads')
ELEMENT = 'element-6066-11e4-a52e-4f735466cecf'


def wait_for(predicate, message, timeout=30):
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    try:
      value = predicate()
      if value:
        return value
    except (ConnectionError, OSError, AssertionError):
      pass
    time.sleep(0.1)
  raise AssertionError(message)


class Firefox:
  def __init__(self):
    self.session = None
    DOWNLOADS.mkdir(mode=0o700, exist_ok=True)
    self.process = subprocess.Popen([
      'geckodriver', '--host', '127.0.0.1', '--port', '4444', '--log', 'fatal',
      '--allow-system-access',
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
      env={**os.environ, 'TZ': 'Asia/Shanghai'})
    try:
      wait_for(lambda: self.request('GET', '/status'), 'Firefox driver did not start')
      session = self.request('POST', '/session', {'capabilities': {'alwaysMatch': {
        'browserName': 'firefox', 'acceptInsecureCerts': True,
        'moz:firefoxOptions': {'binary': '/usr/bin/firefox-esr', 'args': ['-headless'], 'prefs': {
          'network.proxy.type': 0, 'datareporting.policy.dataSubmissionEnabled': False,
          'toolkit.telemetry.enabled': False, 'browser.shell.checkDefaultBrowser': False,
          'browser.download.folderList': 2, 'browser.download.dir': str(DOWNLOADS),
          'browser.download.useDownloadDir': True, 'browser.download.alwaysOpenPanel': False,
          'browser.helperApps.neverAsk.saveToDisk': 'application/octet-stream,application/zip',
        }},
      }}})
      self.session = session['sessionId']
      self.call('POST', '/timeouts', {'pageLoad': 30000, 'script': 30000, 'implicit': 0})
      print('Firefox native form test: browser ' + session['capabilities']['browserVersion'], flush=True)
    except Exception:
      self.close()
      raise

  def request(self, method, path, body=None):
    connection = http.client.HTTPConnection('127.0.0.1', 4444, timeout=40)
    try:
      connection.request(method, path, None if body is None else json.dumps(body),
        headers={'Content-Type': 'application/json'})
      response = connection.getresponse()
      data = response.read(1024 * 1024 + 1)
      assert len(data) <= 1024 * 1024, 'WebDriver response exceeded its bound'
      value = json.loads(data)['value']
      # WebDriver error messages can contain page contents; report only the code.
      assert response.status < 400, 'WebDriver command failed: ' + value.get('error', 'unknown')
      return value
    finally:
      connection.close()

  def call(self, method, path, body=None):
    return self.request(method, '/session/' + self.session + path, body)

  def navigate(self, path, origin=ORIGIN):
    self.call('POST', '/url', {'url': origin + path})

  def allow_narrow_viewports(self):
    # Firefox's desktop window has a 500px minimum. Remove only that browser
    # window constraint, leaving the tested page and its security policy intact.
    self.call('POST', '/moz/context', {'context': 'chrome'})
    try:
      self.call('POST', '/execute/sync', {'script': '''const root = document.documentElement;
        root.style.setProperty('min-width', '0px', 'important');
        root.getBoundingClientRect();''', 'args': []})
    finally:
      self.call('POST', '/moz/context', {'context': 'content'})

  def set_viewport(self, width, height):
    # Gecko can acknowledge Set Window Rect before the page's layout viewport
    # reaches that width, especially after switching away from a download tab.
    # Measure the actual CSS viewport and retry the native resize if necessary.
    observed = None
    for _ in range(3):
      if width < 500:
        self.allow_narrow_viewports()
      self.call('POST', '/window/rect', {'width': width, 'height': height})
      deadline = time.monotonic() + 3
      matches = 0
      while time.monotonic() < deadline:
        observed = self.call('POST', '/execute/sync', {'script': 'return innerWidth;', 'args': []})
        matches = matches + 1 if observed == width else 0
        if matches == 2:
          return
        time.sleep(0.1)
    raise AssertionError(f'Firefox layout viewport stayed {observed} CSS pixels wide after requesting {width}')

  def element(self, selector, using='css selector'):
    return self.call('POST', '/element', {'using': using, 'value': selector})[ELEMENT]

  def click(self, selector, using='css selector'):
    def displayed_element():
      element = self.element(selector, using)
      return element if self.call('GET', '/element/' + element + '/displayed') else None
    element = wait_for(displayed_element, 'native form button is unavailable')
    # Position the visible control before dispatching native input.
    self.call('POST', '/execute/sync', {'script': "arguments[0].scrollIntoView({block: 'center', inline: 'nearest'});", 'args': [{ELEMENT: element}]})
    self.call('POST', '/element/' + element + '/click', {})

  def keys(self, selector, text, using='css selector'):
    self.call('POST', '/element/' + self.element(selector, using) + '/value', {'text': text})

  def pointer(self, selector, fraction, drag_to=None, using='css selector', hold=False):
    element = self.element(selector, using)
    # Position the viewport, then use native input; never synthesize DOM events.
    geometry = self.call('POST', '/execute/sync', {'script': '''const node = arguments[0];
      node.scrollIntoView({block: 'center', inline: 'nearest'});
      const rect = node.getBoundingClientRect();
      const visible = {left: Math.max(0, rect.left), right: Math.min(innerWidth, rect.right),
        top: Math.max(0, rect.top), bottom: Math.min(innerHeight, rect.bottom)};
      const origin_x = Math.floor((visible.left + visible.right) / 2);
      const origin_y = Math.floor((visible.top + visible.bottom) / 2);
      for (let ancestor = node.parentElement; ancestor; ancestor = ancestor.parentElement) {
        const style = getComputedStyle(ancestor);
        const box = ancestor.getBoundingClientRect();
        if (/^(auto|scroll|hidden|clip)$/.test(style.overflowX)) {
          visible.left = Math.max(visible.left, box.left + ancestor.clientLeft);
          visible.right = Math.min(visible.right, box.left + ancestor.clientLeft + ancestor.clientWidth);
        }
        if (/^(auto|scroll|hidden|clip)$/.test(style.overflowY)) {
          visible.top = Math.max(visible.top, box.top + ancestor.clientTop);
          visible.bottom = Math.min(visible.bottom, box.top + ancestor.clientTop + ancestor.clientHeight);
        }
      }
      return {...visible, rect_left: rect.left, width: rect.width, origin_x, origin_y};''', 'args': [{ELEMENT: element}]})
    left = geometry['left']
    right = geometry['right']
    top = geometry['top']
    bottom = geometry['bottom']
    assert left < right and top < bottom, 'chart pointer target is outside the viewport'
    def move(position, duration):
      target_x = int(max(left + 1, min(right - 1, geometry['rect_left'] + position * geometry['width'])))
      target_y = int((top + bottom) / 2)
      assert self.call('POST', '/execute/sync', {'script': 'return document.elementFromPoint(arguments[1], arguments[2]) === arguments[0];',
        'args': [{ELEMENT: element}, target_x, target_y]}), 'native chart pointer target is clipped or covered'
      return {'type': 'pointerMove', 'duration': duration, 'origin': {ELEMENT: element},
        'x': target_x - geometry['origin_x'], 'y': target_y - geometry['origin_y']}
    actions = [move(fraction, 100)]
    if drag_to is not None:
      actions += [{'type': 'pointerDown', 'button': 0}, move(drag_to, 300), {'type': 'pointerUp', 'button': 0}]
    elif hold:
      actions += [{'type': 'pointerDown', 'button': 0}]
    self.call('POST', '/actions', {'actions': [{
      'type': 'pointer', 'id': 'chart-pointer', 'parameters': {'pointerType': 'mouse'}, 'actions': actions,
    }]})
    if not hold:
      self.call('DELETE', '/actions')

  def release_pointer(self):
    self.call('POST', '/actions', {'actions': [{
      'type': 'pointer', 'id': 'chart-pointer', 'parameters': {'pointerType': 'mouse'},
      'actions': [{'type': 'pointerUp', 'button': 0}],
    }]})
    self.call('DELETE', '/actions')

  def login(self):
    self.call('POST', '/element/' + self.element('#password') + '/value',
      {'text': os.environ['EXPRI_DASHBOARD_PASSWORD']})
    self.click('.login-form button[type="submit"]')

  def catalog(self):
    return self.call('POST', '/execute/async', {
      'script': '''const done = arguments[0]; fetch('/api/catalog').then(async response => {
        done({status: response.status, access_mode: response.ok ? (await response.json()).access_mode : null});
      }).catch(() => done({status: 0}));''', 'args': [],
    })

  def cookies(self):
    # Firefox's enumeration can include ancestor-host cookies that its network
    # stack never sends here. Select the exact host; wire checks prove isolation.
    hostname = urlsplit(self.call('GET', '/url')).hostname
    return [cookie for cookie in self.call('GET', '/cookie')
      if cookie['name'] == '__Host-expri_session' and cookie['domain'] == hostname]

  def restore_cookie(self, cookie):
    # Omit Domain so replay uses a host-only cookie on the active document,
    # preserving __Host- semantics instead of creating a domain cookie.
    host_only = {name: value for name, value in cookie.items() if name != 'domain'}
    self.call('POST', '/cookie', {'cookie': host_only})

  def close(self):
    try:
      if self.session is not None:
        self.call('DELETE', '')
    finally:
      self.process.terminate()
      try:
        self.process.wait(timeout=5)
      except subprocess.TimeoutExpired:
        self.process.kill()
        self.process.wait(timeout=5)


def trace_records():
  return [json.loads(line) for line in TRACE.read_text().splitlines()]


def posted_since(start, path, origin, status):
  result = wait_for(lambda: next((record for record in trace_records()[start:]
    if record['method'] == 'POST' and record['path'] == path), None), 'browser form POST was not observed')
  assert result['origin'] == origin, f'native {path} Origin was {result["origin"]!r}'
  assert result['status'] == status, f'native {path} returned {result["status"]}, expected {status}'
  return result


def catalog_with_wire_check(browser, status, session_cookie_count):
  host = urlsplit(browser.call('GET', '/url')).netloc
  start = len(trace_records())
  result = browser.catalog()
  assert result['status'] == status, f'dashboard catalog returned {result["status"]}, expected {status}'
  request = wait_for(lambda: next((record for record in trace_records()[start:]
    if record['method'] == 'GET' and record['path'] == '/api/catalog' and record['host'] == host), None),
    'browser catalog request was not observed')
  assert request['status'] == status, 'catalog wire response disagrees with Firefox'
  assert request['session_cookie_count'] == session_cookie_count, 'browser sent an unexpected number of host session cookies'
  return result


def chart_context(browser):
  assert browser.call('POST', '/execute/sync', {'script': '''const preview = document.querySelector('#chart-preview');
    return preview?.tagName === 'DIV' && preview.ownerDocument === document && !preview.querySelector('iframe');''', 'args': []}), 'chart preview is not rendered in the dashboard document'


def interactive_charts(browser, run_ids, prefix='workspace', check_security=False):
  def parent(script):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': []})
  chart_context(browser)
  wait_for(lambda: parent("return document.querySelector('#chart-preview')?.querySelectorAll('[data-interactive-chart]').length;") == 2, 'both metric plots were not enhanced')
  if check_security:
    policy = browser.call('POST', '/execute/async', {'script': '''const done = arguments[0];
      fetch(document.querySelector('#open-chart').href).then(response => done(response.headers.get('Content-Security-Policy'))).catch(() => done(null));''', 'args': []})
    assert policy and "default-src 'none'" in policy and 'script-src' not in policy, 'standalone chart response weakened its script-blocking CSP'
    parent_policy = browser.call('POST', '/execute/async', {'script': '''const done = arguments[0];
      fetch(location.href).then(response => done(response.headers.get('Content-Security-Policy'))).catch(() => done(null));''', 'args': []})
    assert parent_policy and "default-src 'none'" in parent_policy and 'script-src' in parent_policy and "'unsafe-inline'" not in parent_policy.split('script-src', 1)[1].split(';', 1)[0], 'dashboard chart permits injected inline scripts'
  interaction_start = len(trace_records())
  chart_context(browser)
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def chart_script(metric, script):
    return evaluate("const card = [...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')].find(node => node.querySelector('h2').textContent === arguments[0]); " + script, metric)
  def selector(metric, suffix):
    return f'//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="{metric}"]{suffix}'
  def pointer(metric, fraction, drag_to=None):
    browser.pointer(selector(metric, '//*[@data-chart-hit]'), fraction, drag_to, using='xpath')
  def keys(metric, text):
    browser.keys(selector(metric, '//*[@class="plot-scroll"]'), text, using='xpath')
  def range_of(metric):
    bounds = chart_script(metric, "const range = card.querySelector('[data-chart-range]'); return [range.getAttribute('data-start-step'), range.getAttribute('data-end-step')];")
    return tuple(int(value) for value in bounds)
  def readout(metric):
    return chart_script(metric, "return [...card.querySelectorAll('[data-chart-readout-run]')].map(row => ({run_id: row.querySelector('code').textContent, step: row.getAttribute('data-chart-step'), value: row.getAttribute('data-chart-value'), text: row.textContent}));")
  def sample(metric, step, value):
    rows = readout(metric)
    return len(rows) == len(run_ids) and {row['run_id'] for row in rows} == set(run_ids) and all(row['step'] == str(step) and float(row['value']) == value for row in rows)
  def capture(name):
    Path('/tmp/' + name + '.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
  assert chart_script('loss', "return card.querySelectorAll('[data-chart-hit]').length;") == 1, 'plot has duplicate interaction layers'
  assert range_of('loss') == (0, 79), 'loss range does not match recorded steps'
  if check_security:
    evaluate('''const script = document.createElement('script');
      script.textContent = "document.documentElement.setAttribute('data-probe-script', 'ran')";
      document.body.append(script);
      const card = [...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')].find(node => node.querySelector('h2').textContent === 'duplicate_probe');
      card.querySelector('[data-chart-hit]').setAttribute('onclick', "document.documentElement.setAttribute('data-probe-handler', 'ran')");''')
    browser.click(selector('duplicate_probe', '//*[@data-chart-hit]'), using='xpath')
    assert evaluate("return document.documentElement.getAttribute('data-probe-script');") is None, 'dashboard chart executed an injected script'
    assert evaluate("return document.documentElement.getAttribute('data-probe-handler');") is None, 'dashboard chart executed an inline event handler'
    chart_script('duplicate_probe', "card.querySelector('[data-chart-hit]').removeAttribute('onclick');")
  pointer('loss', .001)
  try:
    wait_for(lambda: sample('loss', 0, 1.0), 'native hover did not show exact recorded first values')
  except AssertionError as error:
    diagnostics = chart_script('loss', '''const hit = card.querySelector('[data-chart-hit]'); const region = card.querySelector('.plot-scroll');
      const svg = card.querySelector('svg'); const rect = hit.getBoundingClientRect();
      return {rows: [...card.querySelectorAll('[data-chart-readout-run]')].map(row => ({run_id: row.querySelector('code').textContent, step: row.dataset.chartStep, value: row.dataset.chartValue, text: row.textContent})),
        axis: svg.dataset.xAxis, range: [card.querySelector('[data-chart-range]').dataset.startX, card.querySelector('[data-chart-range]').dataset.endX],
        runs: [...card.querySelectorAll('[data-chart-run]')].map(node => ({run_id: node.querySelector('code').textContent, pressed: node.getAttribute('aria-pressed')})),
        hit: {x: rect.x, y: rect.y, width: rect.width, height: rect.height}, viewport: {width: innerWidth, height: innerHeight},
        scroll: {window_x: scrollX, window_y: scrollY, region_x: region.scrollLeft, region_y: region.scrollTop}};''')
    raise AssertionError('native hover did not show exact recorded first values: ' + json.dumps(diagnostics)) from error
  assert chart_script('loss', "return card.querySelector('[data-chart-crosshair]').getAttribute('display');") != 'none', 'hover did not show its crosshair'
  assert chart_script('loss', "return card.querySelector('[data-chart-readout]').getAttribute('aria-live');") == 'off', 'pointer hover creates live announcement noise'
  capture(prefix + '-hover')
  keys('loss', '\ue014')
  wait_for(lambda: sample('loss', 1, .5), 'keyboard did not advance one displayed sample')
  assert chart_script('loss', "return card.querySelector('[data-chart-readout]').getAttribute('aria-live');") == 'polite', 'keyboard samples are not announced accessibly'
  toggle = selector('loss', '//button[@data-chart-run="2"]')
  browser.keys(toggle, ' ', using='xpath')
  assert chart_script('loss', "return card.querySelector('[data-chart-run=\"2\"]').getAttribute('aria-pressed');") == 'false', 'native keyboard legend toggle did not hide its run'
  assert chart_script('loss', "const nodes = [...card.querySelectorAll('[data-chart-series=\"2\"]')]; return nodes.some(node => node.tagName.toLowerCase() === 'polyline') && nodes.some(node => node.tagName.toLowerCase() === 'circle') && nodes.every(node => node.getAttribute('display') === 'none');"), 'legend hid its label but left its curve visible or untagged'
  assert chart_script('loss', "return [...card.querySelectorAll('[data-chart-series=\"1\"]')].some(node => node.getAttribute('display') !== 'none');"), 'legend toggle hid an unrelated run'
  browser.click(toggle, using='xpath')
  assert chart_script('loss', "return card.querySelector('[data-chart-run=\"2\"]').getAttribute('aria-pressed');") == 'true', 'native legend click did not restore its run'
  assert chart_script('loss', "const nodes = [...card.querySelectorAll('[data-chart-series=\"2\"]')]; return nodes.length > 0 && nodes.every(node => node.getAttribute('display') !== 'none');"), 'legend restored its label but left its curve hidden'
  pointer('loss', .2, .65)
  start_step, end_step = range_of('loss')
  assert 0 < start_step < end_step < 79, 'native drag did not zoom into the recorded range'
  capture(prefix + '-zoom')
  browser.click(selector('loss', '//button[@data-chart-zoom="reset"]'), using='xpath')
  assert range_of('loss') == (0, 79), 'Reset zoom did not restore the full range'
  keys('loss', '+')
  assert range_of('loss')[1] - range_of('loss')[0] < 79, 'native keyboard zoom did not narrow the range'
  keys('loss', '\ue00c')
  assert range_of('loss') == (0, 79), 'Escape did not reset zoom'
  keys('duplicate_probe', '\ue00c')
  for expected_sample, expected_step, expected_value in [(1, 0, 7.0), (2, 0, 7.0), (3, 1, 8.0)]:
    keys('duplicate_probe', '\ue014')
    row = readout('duplicate_probe')[0]
    assert f'sample {expected_sample} of 3 displayed' in row['text'], 'keyboard got trapped at a duplicate coordinate'
    assert row['step'] == str(expected_step) and float(row['value']) == expected_value, 'keyboard duplicate traversal changed recorded values'
  assert not any(record['path'] in {'/api/chart-data', '/api/chart', '/api/run', '/api/compare'}
    for record in trace_records()[interaction_start:]), 'chart interactions fetched experiment data again'


def select_chart_axis(browser, axis):
  # Clicking the visible radio tag selects it in one native action. The input
  # remains keyboard-accessible while its label carries the visual treatment.
  browser.click('#x-axis-' + axis.replace('_', '-') + ' + span')
  selected = browser.call('POST', '/execute/sync', {'script': "return document.querySelector('input[name=\"x_axis\"]:checked').value;", 'args': []})
  assert selected == axis, f'native axis tag requested {axis}, actual value {selected}'


def chart_axis_diagnostics(browser, requested):
  return browser.call('POST', '/execute/sync', {'script': '''return {requested: arguments[0],
    selected: document.querySelector('input[name="x_axis"]:checked').value,
    rendered: [...(document.querySelector('#chart-preview')?.querySelectorAll('svg[data-x-axis]') ?? [])].map(node => node.dataset.xAxis),
    error: document.querySelector('#chart-error').textContent,
    error_hidden: document.querySelector('#chart-error').hidden};''', 'args': [requested]})


def chart_x_fraction(value, domain):
  start, end = (int(coordinate) for coordinate in domain)
  coordinate = int(value)
  assert start < end and start <= coordinate <= end, 'recorded chart coordinate is outside its domain'
  # Subtract exact epoch nanoseconds before floating-point conversion. Converting
  # absolute timestamps first would collapse nearby samples at large epochs.
  return (coordinate - start) / (end - start)


def chart_sample_window(coordinates, domain):
  points = sorted(set(int(coordinate) for coordinate in coordinates))
  assert len(points) >= 4, 'native zoom needs several distinct recorded coordinates'
  start = points[max(1, (len(points) - 1) // 5)]
  end = points[min(len(points) - 2, (len(points) - 1) * 13 // 20)]
  assert int(domain[0]) < start < end < int(domain[1]), 'recorded sample zoom is not inside the full domain'
  return chart_x_fraction(start, domain), chart_x_fraction(end, domain)


def visible_chart_coordinates(browser, metric, run_index):
  return browser.call('POST', '/execute/sync', {'script': '''const card = [...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')]
    .find(node => node.dataset.chartMetric === arguments[0]);
    return [...card.querySelectorAll('circle.point')].filter(node => node.dataset.chartSeries === String(arguments[1]) && node.getAttribute('display') !== 'none')
      .map(node => node.dataset.xValue);''', 'args': [metric, run_index]})


def time_axes(browser, run_ids, prefix='workspace'):
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def state():
    return evaluate('''const doc = document.querySelector('#chart-preview');
      const card = [...(doc?.querySelectorAll('[data-interactive-chart]') ?? [])].find(node => node.querySelector('h2').textContent === 'loss');
      const duplicate = [...(doc?.querySelectorAll('[data-interactive-chart]') ?? [])].find(node => node.querySelector('h2').textContent === 'duplicate_probe');
      const range = card?.querySelector('[data-chart-range]'); const svg = card?.querySelector('svg');
      return range && svg ? {axis: svg.dataset.xAxis, range: [range.dataset.startX, range.dataset.endX],
        time_zone: range.dataset.timeZone, range_text: range.textContent,
        domain: [svg.dataset.xMin, svg.dataset.xMax], hidden: [...card.querySelectorAll('[data-chart-run]')].filter(node => node.getAttribute('aria-pressed') === 'false').map(node => node.querySelector('code').textContent),
        duplicate_hidden: duplicate ? [...duplicate.querySelectorAll('[data-chart-run]')].filter(node => node.getAttribute('aria-pressed') === 'false').map(node => node.querySelector('code').textContent) : null} : null;''')
  def choose(axis):
    evaluate("window.__acceptance_axis_preview = document.querySelector('#chart-preview');")
    select_chart_axis(browser, axis)
    try:
      wait_for(lambda: (state() or {}).get('axis') == axis, 'native axis selection did not update the plot')
    except AssertionError as error:
      raise AssertionError('native axis selection did not update the plot: ' + json.dumps(chart_axis_diagnostics(browser, axis))) from error
    assert evaluate("return document.querySelector('#chart-preview') === window.__acceptance_axis_preview;"), 'axis switch replaced the stable chart preview'
    assert evaluate('''const plots = [...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')];
      return plots.length === arguments[0] && plots.every(node => node.querySelectorAll('[data-chart-hit]').length === 1);''', 2 if axis == 'step' else 1), 'axis switch lost or duplicated chart interaction layers'
    assert 'x_axis=' + axis in evaluate("return document.querySelector('#open-chart').href;"), 'Open chart lost the selected axis'
  def sample():
    return evaluate('''const card = [...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')].find(node => node.querySelector('h2').textContent === 'loss');
      const row = [...card.querySelectorAll('[data-chart-readout-run]')].find(node =>
        ['chartStep', 'chartValue', 'chartX', 'chartTimestamp'].every(name => node.dataset[name] !== undefined && node.dataset[name] !== ''));
      if (!row) return null;
      const circle = [...card.querySelectorAll('circle.point')].find(node => node.dataset.chartSeries === row.dataset.chartReadoutRun
        && node.dataset.xValue === row.dataset.chartX && node.dataset.timestamp === row.dataset.chartTimestamp && node.getAttribute('display') !== 'none');
      if (!circle) return null;
      return {run_index: row.dataset.chartReadoutRun, run_id: row.querySelector('code').textContent,
        step: row.dataset.chartStep, value: row.dataset.chartValue, x: row.dataset.chartX, timestamp: row.dataset.chartTimestamp, text: row.textContent,
        recorded: {x: circle.dataset.xValue, timestamp: circle.dataset.timestamp, label: circle.getAttribute('aria-label')}};''')
  def visible_coordinates():
    return visible_chart_coordinates(browser, 'loss', 1)
  def assert_recorded_sample(row, axis):
    assert row['run_index'] == '1' and row['run_id'] == run_ids[0], 'time readout inspected a hidden or unrelated run'
    assert 0 <= int(row['step']) < 80 and float(row['value']) == 1 / (int(row['step']) + 1), 'time readout changed the original step or metric value'
    assert row['timestamp'].endswith('Z'), 'time readout omitted the original UTC timestamp'
    recorded = row['recorded']
    assert recorded is not None and recorded['x'] == row['x'] and recorded['timestamp'] == row['timestamp'], 'time readout did not match a recorded circle coordinate and timestamp'
    prefix = f"Run 1 · step {row['step']} · value {row['value']} · timestamp {row['timestamp']}"
    assert recorded['label'] == prefix or recorded['label'].startswith(prefix + ' · elapsed '), 'time readout changed the exact recorded sample label'
    assert ('Elapsed' if axis == 'elapsed' else 'UTC') in row['text'], 'time readout omitted the chosen axis value'
    if axis == 'wall_clock':
      assert int(row['x']) > 10**18, 'wall-clock readout lost its exact epoch nanosecond coordinate'
  def first_sample():
    row = sample()
    return row if row and row.get('step') == '0' else None
  def timezone_tags():
    before = state()
    start = len(trace_records())
    href = evaluate("return document.querySelector('#open-chart').href;")
    evaluate('''window.__acceptance_zone_preview = document.querySelector('#chart-preview');
      window.__acceptance_zone_plots = [...window.__acceptance_zone_preview.querySelectorAll('[data-interactive-chart]')];''')
    for zone in ['utc', 'local']:
      browser.click('#time-zone-' + zone + ' + span')
      wait_for(lambda: (state() or {}).get('time_zone') == zone, 'timezone tag did not redraw the chart')
      after = state()
      assert after['range'] == before['range'] and after['domain'] == before['domain'], 'timezone change shifted exact zoom coordinates'
      assert after['hidden'] == before['hidden'], 'timezone change lost hidden run IDs'
      assert evaluate("return document.querySelector('#chart-preview') === window.__acceptance_zone_preview;"), 'timezone change replaced the chart preview'
      assert evaluate('''const plots = [...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')];
        return plots.length === window.__acceptance_zone_plots.length && plots.every((node, index) => node === window.__acceptance_zone_plots[index]);'''), 'timezone change rebuilt the plots'
      assert evaluate("return document.querySelector('input[name=\"time_zone\"]:checked').value;") == zone, 'timezone tag lost its selected state'
      assert evaluate("return document.querySelector('#time-zone-label').textContent;") == ('UTC' if zone == 'utc' else 'Asia/Shanghai'), 'timezone tag did not name the displayed timezone'
      assert evaluate("return document.querySelector('#open-chart').href;") == href, 'timezone display change altered the server chart URL'
      assert evaluate("return document.querySelector('#open-chart-label').textContent;") == 'Open UTC chart', 'standalone server chart timezone is ambiguous'
      chart_context(browser)
      coordinates = visible_coordinates()
      assert coordinates, 'timezone zoom contains no visible recorded first-run samples'
      fraction = chart_x_fraction(coordinates[len(coordinates) // 2], after['range'])
      browser.pointer(loss + '//*[@data-chart-hit]', fraction, using='xpath')
      row = wait_for(sample, 'timezone hover lost the exact recorded sample')
      assert_recorded_sample(row, 'wall_clock')
      if zone == 'local':
        assert 'UTC+08:00' in row['text'] or 'UTC+8' in row['text'], 'local readout omitted its offset at the recorded instant'
      else:
        assert 'UTC+08:00' not in row['text'] and 'UTC+8' not in row['text'], 'UTC tag retained the local display offset'
    assert not any(record['path'] in {'/api/chart-data', '/api/chart', '/api/run', '/api/compare'} for record in trace_records()[start:]), 'timezone tags refetched experiment data'
  loss = '//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="loss"]'
  chart_context(browser)
  for metric in ['loss', 'duplicate_probe']:
    browser.click(f'//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="{metric}"]//button[code="' + run_ids[1] + '"]', using='xpath')
  for axis in ['elapsed', 'wall_clock']:
    choose(axis)
    full = state()
    assert evaluate("return document.querySelector('#time-zone-options').hidden;") == (axis != 'wall_clock'), 'timezone tags appeared outside the date & time axis'
    if axis == 'wall_clock':
      assert evaluate("return document.querySelector('input[name=\"time_zone\"]:checked').value;") == 'local', 'date & time did not default to local'
      assert evaluate("return Intl.DateTimeFormat().resolvedOptions().timeZone;") == 'Asia/Shanghai', 'native fixture did not establish its non-UTC timezone'
      assert evaluate("return document.querySelector('#time-zone-label').textContent;") == 'Asia/Shanghai', 'date & time omitted the viewer timezone name'
    assert full['range'] == full['domain'], 'axis switch reused an incompatible zoom range'
    assert full['hidden'] == [run_ids[1]], 'axis switch lost the hidden actual run ID'
    chart_context(browser)
    duplicate = evaluate('''const card = document.querySelector('#chart-preview section[data-chart-metric="duplicate_probe"]');
      return {text: card.textContent, circles: card.querySelectorAll('circle.point').length, svgs: card.querySelectorAll('svg').length};''')
    assert duplicate['text'].count('3 of 3 samples omitted') == 2 and duplicate['circles'] == 0 and duplicate['svgs'] == 0, 'time chart invented a timestamp or plotted an entirely legacy series'
    assert 'No timestamped samples are available to plot' in duplicate['text'], 'empty time chart did not explain its missing samples'
    browser.pointer(loss + '//*[@data-chart-hit]', .001, using='xpath')
    row = wait_for(sample, 'native time hover did not show an exact sample')
    assert_recorded_sample(row, axis)
    # Wall-clock runs may start far apart; one physical pointer pixel can
    # span several samples. Keyboard inspection selects the first sample
    # independently of transfer delays or the total wall-clock range.
    browser.keys(loss + '//*[@class="plot-scroll"]', '\ue011\ue014', using='xpath')
    row = wait_for(first_sample, 'native keyboard did not inspect the first time sample')
    assert_recorded_sample(row, axis)
    assert row['step'] == '0' and float(row['value']) == 1, 'keyboard inspection changed the first time sample'
    if axis == 'elapsed':
      assert row['x'] == '0', 'elapsed axis did not anchor the first metric event at zero'
    # Wall-clock gaps between runs include collection/recovery/transfer delays.
    # Drag within the visible run's recorded samples rather than that empty gap.
    start_fraction, end_fraction = chart_sample_window(visible_coordinates(), full['domain'])
    browser.pointer(loss + '//*[@data-chart-hit]', start_fraction, end_fraction, using='xpath')
    zoomed = state()
    assert int(full['domain'][0]) < int(zoomed['range'][0]) < int(zoomed['range'][1]) < int(full['domain'][1]), 'native drag did not narrow the time domain'
    assert visible_coordinates(), 'native time zoom excluded every visible first-run sample'
    if axis == 'wall_clock':
      timezone_tags()
    Path('/tmp/' + prefix + '-' + axis + '.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
  choose('step')
  assert state()['range'] == ['0', '79'], 'returning to steps did not reset the time range'
  assert state()['hidden'] == [run_ids[1]], 'returning to steps lost legend visibility'
  assert state()['duplicate_hidden'] == [run_ids[1]], 'empty time views forgot the legacy metric hidden run'
  browser.keys('#x-axis-step', '\ue014')
  wait_for(lambda: (state() or {}).get('axis') == 'elapsed', 'native radio ArrowRight did not choose elapsed time')
  assert evaluate("return document.querySelector('#x-axis-elapsed').checked;"), 'keyboard-selected axis tag is not checked'
  browser.keys('#x-axis-elapsed', '\ue014')
  wait_for(lambda: (state() or {}).get('axis') == 'wall_clock', 'native radio ArrowRight did not choose date & time')
  assert state()['time_zone'] == 'local', 'keyboard axis changes forgot the selected timezone'
  choose('step')
  chart_context(browser)
  for metric in ['loss', 'duplicate_probe']:
    browser.click(f'//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="{metric}"]//button[code="' + run_ids[1] + '"]', using='xpath')
  assert state()['hidden'] == [] and state()['duplicate_hidden'] == [], 'returning to Step did not restore the visible comparison runs'


def remember_chart_preview(browser):
  browser.call('POST', '/execute/sync', {'script': "window.__acceptance_chart_preview = document.querySelector('#chart-preview');", 'args': []})


def assert_chart_preview_cleaned(browser):
  wait_for(lambda: browser.call('POST', '/execute/sync', {'script': "return window.__acceptance_chart_preview.querySelectorAll('[data-interactive-chart], [data-chart-hit], [data-chart-run]').length;", 'args': []}) == 0, 'navigation left interactive controls in the previous chart preview')


def login_layout(browser, width):
  geometry = browser.call('POST', '/execute/sync', {'script': '''const bounds = node => {
      const rect = node.getBoundingClientRect();
      return {left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom, width: rect.width};
    };
    const main = document.querySelector('.login-main');
    const card = document.querySelector('.login-card');
    const footer = main.querySelector('.deployment-revision');
    return {viewport_width: innerWidth, page_width: document.documentElement.scrollWidth,
      client_width: document.documentElement.clientWidth, main: bounds(main), card: bounds(card),
      controls: [...card.querySelectorAll('input, button')].map(bounds), footer: footer ? bounds(footer) : null};''', 'args': []})
  assert geometry['viewport_width'] == width, f'login viewport is not {width} CSS pixels wide'
  assert geometry['page_width'] <= geometry['client_width'] + 1, f'{width}px login overflows horizontally'
  if width < 500:
    assert geometry['card']['width'] >= geometry['main']['width'] * 0.75, f'{width}px login form is squeezed by deployment metadata'
  for control in geometry['controls']:
    assert control['left'] >= 0 and control['right'] <= geometry['client_width'] + 1, f'{width}px native login control is clipped'
    assert control['width'] >= geometry['card']['width'] * 0.65, f'{width}px native login control is too narrow'
  if footer := geometry['footer']:
    assert footer['top'] >= geometry['card']['bottom'] - 1, 'preview deployment metadata displaces the login form'
    assert footer['left'] >= 0 and footer['right'] <= geometry['client_width'] + 1, f'{width}px preview login metadata overflows'


def forms():
  browser = None
  try:
    browser = Firefox()
    # Prove this regression test reproduces the previous policy without changing
    # either the browser's request headers or the service's Origin guards.
    POLICY_OVERRIDE.write_text('no-referrer')
    browser.navigate('/login')
    start = len(trace_records())
    browser.login()
    posted_since(start, '/login', 'null', 403)
    assert not browser.cookies(), 'old login policy created a browser session'
    # Firefox renders application/json errors in its JSON viewer. Run API
    # probes from an actual HTML document rather than that viewer's context.
    browser.navigate('/login')
    assert browser.catalog()['status'] == 401, 'old login policy authorized dashboard APIs'

    POLICY_OVERRIDE.unlink()
    browser.navigate('/login')
    assert trace_records()[-1]['referrer_policy'] == 'same-origin', 'login policy does not preserve same-origin forms'
    for width in [1440, 360, 320]:
      browser.set_viewport(width, 1000 if width >= 500 else 800)
      login_layout(browser, width)
    browser.set_viewport(1440, 1000)
    start = len(trace_records())
    browser.login()
    posted_since(start, '/login', ORIGIN, 303)
    wait_for(lambda: browser.call('GET', '/url') == ORIGIN + '/', 'native login did not reach the dashboard')
    assert browser.catalog() == {'status': 200, 'access_mode': 'hosted'}, 'native login did not authenticate dashboard APIs'
    cookies = browser.cookies()
    assert len(cookies) == 1, 'native login did not create one browser session'
    cookie = cookies[0]
    assert cookie['secure'] and cookie['httpOnly'] and cookie['sameSite'] == 'Strict', 'browser session cookie lost security flags'

    POLICY_OVERRIDE.write_text('no-referrer')
    browser.navigate('/')
    start = len(trace_records())
    browser.click('#logout-form button[type="submit"]')
    posted_since(start, '/logout', 'null', 403)
    browser.navigate('/')
    assert browser.catalog()['status'] == 200, 'rejected old-policy logout revoked the session'

    POLICY_OVERRIDE.unlink()
    browser.navigate('/')
    start = len(trace_records())
    browser.click('#logout-form button[type="submit"]')
    posted_since(start, '/logout', ORIGIN, 303)
    wait_for(lambda: browser.call('GET', '/url') == ORIGIN + '/login', 'native logout did not reach the login page')
    assert not browser.cookies() and browser.catalog()['status'] == 401, 'native logout left dashboard access active'
    # Replaying the previous cookie also fails, proving server-side revocation.
    browser.restore_cookie(cookie)
    assert browser.catalog()['status'] == 401, 'native logout did not revoke its session'
    print('Firefox native forms passed: old-policy null Origin rejected; same-origin login/logout succeeded; session revoked.', flush=True)
  finally:
    POLICY_OVERRIDE.unlink(missing_ok=True)
    if browser is not None:
      browser.close()


def deep_link(run_id, second_run_id):
  browser = Firefox()
  query = urlencode({'project_id': 'demo', 'origin': 'worker', 'run_id': run_id})
  link = '/?' + query
  source_id = 'hosted:demo:worker'
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def opened(expected):
    return evaluate('''const link = document.querySelector('#open-chart');
      return document.querySelector('#review-section')?.hidden === false
        && document.querySelector('#source-select')?.value === arguments[0]
        && document.querySelector('#review-title')?.textContent === arguments[1]
        && link?.getAttribute('href')
        && JSON.stringify(new URL(link.href).searchParams.getAll('run_id')) === JSON.stringify([arguments[1]]);''', source_id, expected)
  try:
    browser.set_viewport(1440, 1000)
    browser.navigate(link)
    wait_for(lambda: browser.call('GET', '/url') == ORIGIN + '/login?' + query,
      'logged-out run link did not retain its identity at login')
    assert not browser.cookies() and browser.catalog()['status'] == 401, 'run link authorized a logged-out browser'
    assert evaluate("return document.querySelector('.login-form').getAttribute('action');") == '/login?' + query, 'native login form discarded the linked run'
    start = len(trace_records())
    browser.login()
    posted_since(start, '/login', ORIGIN, 303)
    wait_for(lambda: browser.call('GET', '/url') == ORIGIN + link,
      'native login did not return to the original run link')
    wait_for(lambda: opened(run_id), 'run link did not open the exact hosted source and run')
    wait_for(lambda: (evaluate("return document.querySelector('#chart-preview')?.querySelectorAll('svg').length;") or 0) > 0,
      'linked run charts did not load')
    assert evaluate("return document.querySelector('#review-tab-charts').getAttribute('aria-selected');") == 'true', 'linked run did not open its Charts tab'
    assert evaluate("return document.querySelectorAll('#run-rows input:checked').length;") == 0, 'run link implicitly selected a comparison'
    assert not any(record['path'] == '/api/log' for record in trace_records()[start:]), 'linked chart review eagerly loaded logs'
    browser.click('#auto-refresh-toggle')

    browser.click('//*[@id="run-rows"]//button[@class="run-link" and text()="' + second_run_id + '"]', using='xpath')
    wait_for(lambda: opened(second_run_id), 'native run inspection could not leave the linked run')
    browser.click('#refresh-button')
    wait_for(lambda: not evaluate("return document.querySelector('#refresh-button').disabled;"), 'linked dashboard Refresh did not finish')
    assert opened(second_run_id), 'Refresh reopened the original run after native inspection'
    assert browser.call('GET', '/url') == ORIGIN + link, 'native inspection unexpectedly navigated the public run link'
    assert not any(record['path'] == '/api/log' for record in trace_records()[start:]), 'native run inspection eagerly loaded logs'
    browser.click('#logout-form button[type="submit"]')
    wait_for(lambda: browser.call('GET', '/url') == ORIGIN + '/login', 'run-link logout did not complete')
    assert not browser.cookies(), 'run-link logout left its browser session active'
    print('Firefox run links passed: logged-out identity preserved through native login, exact hosted source/run opened, manual inspection and refresh retained, lazy logs and logout preserved.', flush=True)
  finally:
    browser.close()


def artifact_files(browser, run_id):
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def checkpoint_row():
    return evaluate('''const row = [...document.querySelectorAll('#file-rows tr')]
      .find(node => node.querySelector('th code')?.textContent === 'outputs/checkpoint.pt');
      if (!row) return null;
      const link = row.querySelector('a[download]');
      return {size: row.querySelector('.number').textContent,
        locations: row.querySelector('.file-locations').textContent,
        href: link?.getAttribute('href'), filename: link?.getAttribute('download')};''')
  def capture(name):
    Path('/tmp/' + name + '.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
  def show_files():
    evaluate('''const card = document.querySelector('.files-card');
      const sticky = document.querySelector('.topbar');
      const offset = sticky && ['sticky', 'fixed'].includes(getComputedStyle(sticky).position)
        ? sticky.getBoundingClientRect().height : 0;
      window.scrollTo(0, Math.max(0, scrollY + card.getBoundingClientRect().top - offset - 12));''')
  start = len(trace_records())
  browser.keys('#review-tab-charts', '\ue010')  # End selects the last enabled tab.
  wait_for(lambda: evaluate("return document.querySelector('#review-tab-files').getAttribute('aria-selected') === 'true';"), 'End did not select the last Files tab')
  row = wait_for(checkpoint_row, 'Files did not show the published checkpoint')
  assert row['size'] == '17 MiB', 'checkpoint size did not use the expected binary unit'
  assert 'Cloud' in row['locations'] and 'Worker (reported)' in row['locations'] and 'Local' not in row['locations'], 'hosted availability claimed local browser files or omitted reported worker data'
  assert row['href'].startswith('/api/artifact?') and 'X-Amz-' not in row['href'] and row['filename'] == 'checkpoint.pt', 'checkpoint anchor was not a scoped native attachment link'
  assert 'cannot see files downloaded to your laptop' in evaluate("return document.querySelector('.file-location-note').textContent;"), 'hosted Files omitted its laptop visibility limit'
  assert any(record['path'] == '/api/artifacts' for record in trace_records()[start:]), 'Files did not lazily request the inventory'
  browser.click('input[aria-label="Select outputs/checkpoint.pt for download"]')
  browser.keys('#artifact-config-path', '/tmp/owner.toml')
  command = wait_for(lambda: evaluate("return document.querySelector('#artifact-fetch-command')?.value;"), 'selected checkpoint did not generate its CLI command')
  args = shlex.split(command)
  assert args[:3] == ['expri', 'service', 'fetch'] and args[args.index('--config') + 1] == '/tmp/owner.toml', 'CLI command changed the existing client config path'
  assert args[args.index('--project-id') + 1] == 'demo' and args[args.index('--origin') + 1] == 'worker' and args[args.index('--run-id') + 1] == run_id and args[args.index('--artifact') + 1] == 'outputs/checkpoint.pt', 'CLI command changed the selected run or checkpoint'
  assert 'included automatically' in evaluate("return document.querySelector('.file-cli').textContent;"), 'CLI instructions omitted automatic run metadata'
  browser.click('#download-selected-files')
  assert evaluate("return document.querySelectorAll('#selected-file-downloads a').length;") == 1, 'selected download did not show one explicit native link'
  show_files()
  capture('workspace-files')
  browser.set_viewport(500, 800)
  assert evaluate('return document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1;'), 'narrow Files view overflows horizontally'
  assert evaluate('''return ['#refresh-files', '#files-search', '#download-selected-files', '#artifact-config-path', '#artifact-fetch-command'].every(selector => {
    const box = document.querySelector(selector).getBoundingClientRect();
    return box.left >= 0 && box.right <= document.documentElement.clientWidth + 1;
  });'''), 'narrow Files controls are clipped'
  show_files()
  capture('workspace-files-narrow')
  browser.set_viewport(1440, 1000)
  target = DOWNLOADS / 'checkpoint.pt'
  target.unlink(missing_ok=True)
  (DOWNLOADS / 'checkpoint.pt.part').unlink(missing_ok=True)
  original_window = browser.call('GET', '/window')
  browser.click('#selected-file-downloads a[download]')
  wait_for(lambda: target.exists() and target.stat().st_size == 17 * 1024 * 1024
    and not (DOWNLOADS / 'checkpoint.pt.part').exists(), 'native checkpoint download did not complete', timeout=60)
  expected = hashlib.sha256()
  block = bytes(range(256)) * 4096
  for _ in range(17):
    expected.update(block)
  actual = hashlib.sha256()
  with target.open('rb') as checkpoint:
    while chunk := checkpoint.read(1024 * 1024):
      actual.update(chunk)
  assert actual.digest() == expected.digest(), 'native checkpoint download bytes did not match the published fixture'
  assert any(record['path'] == '/api/artifact' and record['status'] == 303 for record in trace_records()[start:]), 'native download did not follow the protected attachment redirect'
  attachments = [record for record in trace_records()[start:] if record['host'] == 's3.expri.example.net']
  assert attachments and all(record['session_cookie_count'] == 0 and set(record['forwarded_headers']) <= {'Range'} for record in attachments), 'attachment origin received dashboard session or authorization headers'
  assert 'X-Amz-' not in TRACE.read_text(), 'browser trace exposed a signed attachment query'
  # A browser may retain a blank attachment tab. Do not inspect or report its URL.
  for handle in browser.call('GET', '/window/handles'):
    if handle != original_window:
      browser.call('POST', '/window', {'handle': handle})
      browser.call('DELETE', '/window')
  browser.call('POST', '/window', {'handle': original_window})
  browser.click('#review-tab-charts')
  wait_for(lambda: evaluate("return document.querySelector('#result-upload-status')?.textContent;") == 'Result uploaded', 'completed run did not show its independent result upload badge')
  archive_path = DOWNLOADS / 'result.zip'
  archive_path.unlink(missing_ok=True)
  (DOWNLOADS / 'result.zip.part').unlink(missing_ok=True)
  browser.click('#download-result-zip')
  wait_for(lambda: archive_path.exists() and archive_path.stat().st_size > 0 and not (DOWNLOADS / 'result.zip.part').exists(), 'native result archive download did not complete', timeout=60)
  with zipfile.ZipFile(archive_path) as archive:
    assert archive.testzip() is None, 'result archive contains corrupt entries'
    manifest = json.loads(archive.read('manifest.json'))
    assert manifest['scope'] == {'project_id': 'demo', 'origin': 'worker', 'run_id': run_id} and manifest['incomplete'] is False, 'native archive changed its scope or completeness'
    metrics = [json.loads(line) for line in archive.read('outputs/metrics.jsonl').splitlines()]
    assert [row['step'] for row in metrics if 'loss' in row['metrics']] == list(range(80)), 'native ZIP lost or duplicated acknowledged metrics'
    assert 'outputs/checkpoint.pt' not in archive.namelist(), 'tracking archive unexpectedly bundled the separately selected heavy checkpoint'
  for handle in browser.call('GET', '/window/handles'):
    if handle != original_window:
      browser.call('POST', '/window', {'handle': handle})
      browser.call('DELETE', '/window')
  browser.call('POST', '/window', {'handle': original_window})
  print('Firefox Files passed: lazy inventory, last keyboard tab, reported locations, selected CLI, exact 17 MiB checkpoint and acknowledged result ZIP.', flush=True)


def project_storage(run_id=None):
  browser = Firefox()
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def capture(name):
    Path('/tmp/' + name + '.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
  try:
    browser.set_viewport(1440, 1000)
    browser.navigate('/login')
    browser.login()
    wait_for(lambda: evaluate("return document.querySelector('#workspace-view-storage') !== null;"), 'hosted project did not offer its Storage page')
    assert evaluate("return !!document.querySelector('#project-options')?.closest('.topbar');"), 'project switching is not in the header'
    evaluate("window.__storage_history = document.querySelector('#run-rows'); window.__storage_chart = document.querySelector('#chart-preview');")
    start = len(trace_records())
    browser.click('#workspace-view-storage + span')
    wait_for(lambda: evaluate("return document.querySelector('#storage-kind-input')?.checked && document.querySelectorAll('#storage-rows tr').length === 2;"), 'project Storage did not show both completed private inputs')
    assert evaluate("return document.querySelector('#workspace-view-storage').checked;"), 'Storage tag did not select the project page in one click'
    assert evaluate("return document.querySelector('#workspace-page').hidden === false && document.querySelector('.run-history').getClientRects().length > 0 && document.querySelector('#run-rows') === window.__storage_history;"), 'Storage hid or replaced the full run history'
    inputs = evaluate('''return [...document.querySelectorAll('#storage-rows tr')].map(row => ({
      id: row.querySelector('th code')?.textContent,
      size: row.querySelector('.number')?.textContent,
      href: row.querySelector('a[download]')?.getAttribute('href'),
      filename: row.querySelector('a[download]')?.getAttribute('download')
    }));''')
    by_id = {item['id']: item for item in inputs}
    assert set(by_id) == {'dataset-v1', 'empty-file'}, 'Storage input rows changed the published input IDs'
    assert by_id['dataset-v1']['size'] == '21 KiB' and by_id['empty-file']['size'] == '0 B', 'Storage input sizes differ from the published files'
    assert by_id['dataset-v1']['href'].startswith('/api/input?') and by_id['dataset-v1']['filename'] == 'dataset-v1', 'Storage input download lost its scoped native attachment'
    assert 'private.bin' not in evaluate("return document.querySelector('#project-storage').textContent;"), 'Storage exposed the original private input filename'
    assert any(record['path'] == '/api/storage' for record in trace_records()[start:]), 'Storage did not lazily request its input catalog'
    capture('workspace-storage-input-only' if run_id is None else 'workspace-storage-inputs')

    target = DOWNLOADS / 'dataset-v1'
    target.unlink(missing_ok=True)
    (DOWNLOADS / 'dataset-v1.part').unlink(missing_ok=True)
    original_window = browser.call('GET', '/window')
    browser.click('//tbody[@id="storage-rows"]/tr[th/code="dataset-v1"]//a[@download="dataset-v1"]', using='xpath')
    wait_for(lambda: target.exists() and target.stat().st_size == len(b'private-input-fixture') * 1024
      and not (DOWNLOADS / 'dataset-v1.part').exists(), 'native private input download did not complete', timeout=60)
    assert hashlib.sha256(target.read_bytes()).digest() == hashlib.sha256(b'private-input-fixture' * 1024).digest(), 'native private input download changed its bytes'
    assert any(record['path'] == '/api/input' and record['status'] == 303 for record in trace_records()[start:]), 'native private input download bypassed the protected attachment route'
    attachments = [record for record in trace_records()[start:] if record['host'] == 's3.expri.example.net']
    assert attachments and all(record['session_cookie_count'] == 0 and set(record['forwarded_headers']) <= {'Range'} for record in attachments), 'private input attachment sent dashboard credentials to S3'
    for handle in browser.call('GET', '/window/handles'):
      if handle != original_window:
        browser.call('POST', '/window', {'handle': handle})
        browser.call('DELETE', '/window')
    browser.call('POST', '/window', {'handle': original_window})

    browser.click('#storage-kind-output + span')
    assert evaluate("return document.querySelector('#storage-kind-output').checked;"), 'Run outputs tag did not select in one click'
    if run_id is None:
      wait_for(lambda: evaluate("return document.querySelector('#storage-empty')?.textContent.includes('No run outputs') && document.querySelectorAll('#storage-rows tr').length === 0;"), 'input-only project claimed uploaded run outputs')
    else:
      def checkpoint_row():
        return evaluate('''const row = [...document.querySelectorAll('#storage-rows tr')]
          .find(node => node.querySelector('th code')?.textContent === 'outputs/checkpoint.pt' && node.textContent.includes(arguments[0]));
          if (!row) return null;
          return {text: row.textContent, href: row.querySelector('a[download]')?.getAttribute('href')};''', run_id)
      row = wait_for(checkpoint_row, 'project Storage did not show the uploaded checkpoint')
      assert 'worker' in row['text'] and '17 MiB' in row['text'] and row['href'].startswith('/api/artifact?'), 'Storage checkpoint row lost machine, size, or scoped download'
      browser.keys('#storage-search', 'checkpoint.pt')
      wait_for(lambda: evaluate("return document.querySelectorAll('#storage-rows tr').length === 1 && document.querySelector('#storage-rows').textContent.includes('outputs/checkpoint.pt');"), 'Storage search did not narrow uploaded outputs')
      capture('workspace-storage')
      browser.set_viewport(500, 800)
      assert evaluate('return document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1;'), 'narrow Storage page overflows horizontally'
      assert evaluate('''return ['#workspace-view-options', '#storage-kind-options', '#storage-search', '#refresh-storage', '#storage-page-label'].every(selector => {
        const box = document.querySelector(selector).getBoundingClientRect();
        return box.left >= 0 && box.right <= document.documentElement.clientWidth + 1;
      });'''), 'narrow Storage controls are clipped'
      capture('workspace-storage-narrow')
    if run_id:
      browser.click(f'#run-rows tr[data-run-key="worker:{run_id}"] .run-link')
      wait_for(lambda: evaluate("return document.querySelector('#workspace-view-workspace').checked && !document.querySelector('#run-workspace').hidden;"), 'opening history from Storage did not return to inspection')
      assert evaluate("return document.querySelector('#run-rows') === window.__storage_history && document.querySelector('#chart-preview') === window.__storage_chart;"), 'opening history from Storage replaced the run list or chart node'
    else:
      browser.click('#workspace-view-workspace + span')
      assert evaluate("return document.querySelector('#workspace-view-workspace').checked;"), 'Storage could not return to the project workspace'
    print('Firefox Storage passed: project inputs before runs, authenticated input bytes, completed output inventory, native tags, search, and narrow layout.' if run_id else 'Firefox input-only Storage passed: project discovery before runs, private input bytes, and empty run outputs.', flush=True)
  finally:
    browser.close()


def status_bar_layout(browser, width):
  geometry = browser.call('POST', '/execute/sync', {'script': '''const bounds = node => {
      const rect = node.getBoundingClientRect();
      return {left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom};
    };
    const bar = document.querySelector('footer.workspace-status-bar');
    return {viewport_width: innerWidth, viewport_height: innerHeight,
      client_width: document.documentElement.clientWidth, bar: bounds(bar),
      controls: ['#refresh-button', '.auto-refresh', '#updated-at', '.deployment-revision']
        .map(selector => ({selector, node: bar.querySelector(selector)}))
        .filter(item => item.node?.getClientRects().length)
        .map(item => ({selector: item.selector, ...bounds(item.node)}))};''', 'args': []})
  assert geometry['viewport_width'] == width, f'status bar viewport is not {width} CSS pixels wide'
  bar = geometry['bar']
  assert bar['left'] >= 0 and bar['right'] <= geometry['client_width'] + 1, f'{width}px status bar overflows horizontally'
  assert bar['top'] >= 0 and bar['bottom'] <= geometry['viewport_height'] + 1, f'{width}px status bar is outside the viewport'
  for control in geometry['controls']:
    assert control['left'] >= bar['left'] - 1 and control['right'] <= bar['right'] + 1, f'{width}px status bar clips {control["selector"]} horizontally'
    assert control['top'] >= bar['top'] - 1 and control['bottom'] <= bar['bottom'] + 1, f'{width}px status bar clips {control["selector"]} vertically'
  return geometry


def workspace_layout(browser, width, wide=False):
  status_bar_layout(browser, width)
  geometry = browser.call('POST', '/execute/sync', {'script': '''window.scrollTo(0, 0);
    const bounds = selector => {
      const rect = document.querySelector(selector).getBoundingClientRect();
      return {left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom,
        width: rect.width, height: rect.height};
    };
    const grid = document.querySelector('.workspace-grid');
    const table = document.querySelector('#runs-region');
    const headers = [...table.querySelectorAll('thead th')];
    const column_classes = ['selection-column', 'run-column', 'status-column',
      ...(table.querySelector('.machine-column') ? ['machine-column'] : []),
      'task-column', 'started-column'];
    return {viewport_width: innerWidth, viewport_height: innerHeight, page_width: document.documentElement.scrollWidth,
      client_width: document.documentElement.clientWidth, grid: bounds('.workspace-grid'),
      runs: bounds('.runs-card'), review: bounds('.review-workspace'),
      has_review: grid.classList.contains('has-review'), has_wide_table: grid.classList.contains('has-wide-table'),
      gap: parseFloat(getComputedStyle(grid).columnGap),
      columns: {builtins_match: column_classes.every((name, index) => headers[index]?.classList.contains(name)),
        header_count: headers.length, builtin_count: column_classes.length,
        custom_count: Number(table.querySelector('table').dataset.columnCount),
        run: bounds('#run-rows .run-cell'), selection: bounds('#run-rows .selection-column'),
        status: bounds('#run-rows .status-column'), region: bounds('#runs-region'),
        row_counts: [...table.querySelectorAll('tbody tr')].map(row => row.children.length)},
      table: {client_width: table.clientWidth, scroll_width: table.scrollWidth, scroll_left: table.scrollLeft}};''', 'args': []})
  assert geometry['viewport_width'] == width, f'layout viewport is not {width} CSS pixels wide'
  assert geometry['page_width'] <= geometry['client_width'] + 1, f'{width}px workspace overflows horizontally'
  runs, review = geometry['runs'], geometry['review']
  if wide:
    assert geometry['has_wide_table'], f'{width}px project or selected columns lost their table layout'
  if width > 1000:
    assert geometry['grid']['bottom'] <= geometry['viewport_height'] + 1, f'{width}px workspace extends below the viewport and clips pane scrolling'
    assert runs['right'] <= review['left'] + 1, f'{width}px Runs and Review are not side by side'
    assert abs(runs['top'] - review['top']) <= 1, f'{width}px Review is displaced below Runs'
    available = geometry['grid']['width'] - geometry['gap']
    assert abs(runs['width'] + review['width'] - available) <= 1, f'{width}px split panels leave unused workspace'
    assert 280 <= runs['width'] <= 420, f'{width}px history pane is not a readable, compact run browser'
    assert review['width'] > runs['width'], f'{width}px Review does not have room for inspecting charts'
  else:
    assert runs['bottom'] <= review['top'] + 1, f'{width}px Runs and Review are not stacked'
    assert abs(runs['width'] - geometry['grid']['width']) <= 1, f'{width}px Runs does not use the full workspace'
    assert abs(review['width'] - geometry['grid']['width']) <= 1, f'{width}px stacked Review does not use the full workspace'
  columns = geometry['columns']
  assert columns['builtins_match'], 'run table lost Run, adjacent Status, Machine, Task, or Started columns'
  expected_columns = columns['builtin_count'] + columns['custom_count']
  assert columns['header_count'] == expected_columns, 'run table omitted built-in or selected column headings'
  assert all(count == expected_columns for count in columns['row_counts']), 'run rows do not align with their column headings'
  if width > 1000 and geometry['table']['scroll_left'] == 0:
    assert columns['status']['right'] <= columns['region']['left'] + geometry['table']['client_width'] + 1, 'desktop Run status is outside the visible table'
  assert geometry['table']['client_width'] <= runs['width'] + 1, f'{width}px table escapes its Runs panel'
  return geometry


def table_scroll_contained(browser, width):
  before = workspace_layout(browser, width, wide=True)
  assert before['table']['scroll_width'] > before['table']['client_width'], f'{width}px selected columns do not scroll within Runs'
  browser.call('POST', '/execute/sync', {'script': '''const table = document.querySelector('#runs-region');
    table.scrollLeft = table.scrollWidth;''', 'args': []})
  try:
    after = workspace_layout(browser, width, wide=True)
    assert after['table']['scroll_left'] > 0, f'{width}px table could not scroll to its later columns'
    run, selection, region = after['columns']['run'], after['columns']['selection'], after['columns']['region']
    assert run['left'] >= selection['right'] - 1, f'{width}px pinned run identity overlaps row selection'
    assert run['right'] <= region['right'] + 1, f'{width}px pinned run identity escapes its table'
    assert abs(run['left'] - before['columns']['run']['left']) <= 1, f'{width}px horizontal scrolling hides the pinned run identity'
    assert after['page_width'] == before['page_width'], f'{width}px table scrolling widened the page'
    assert all(abs(after['runs'][key] - before['runs'][key]) <= 1 for key in before['runs']), f'{width}px table scrolling moved or resized Runs'
    assert all(abs(after['review'][key] - before['review'][key]) <= 1 for key in before['review']), f'{width}px table scrolling moved or resized Review'
  finally:
    browser.call('POST', '/execute/sync', {'script': "document.querySelector('#runs-region').scrollLeft = 0;", 'args': []})
  return before


def compact_focus_visible(browser, target, width):
  def focused_near_top():
    return browser.call('POST', '/execute/sync', {'script': '''const node = document.getElementById(arguments[0]);
      const rect = node?.getBoundingClientRect();
      return !!rect && node.getClientRects().length > 0 && document.activeElement === node
        && rect.top >= -1 && rect.top <= 80 && rect.bottom <= innerHeight;''', 'args': [target]})
  wait_for(focused_near_top, f'{width}px explicit navigation did not reveal and focus {target} near the viewport top')


def choose_run_table_columns(browser):
  def evaluate(script):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': []})
  browser.click('#run-columns summary')
  wait_for(lambda: evaluate("return !!document.querySelector('input[aria-label=\"Parameter learning_rate\"]:not(:disabled)');"), 'parameter column discovery did not finish')
  for name in ['Parameter learning_rate', 'Metric loss']:
    browser.click(f'input[aria-label="{name}"] + span')
    wait_for(lambda: evaluate("return document.querySelector('#runs-region').getAttribute('aria-busy') === 'false';"), 'choosing a run table column did not finish')
  wait_for(lambda: evaluate("return document.querySelector('.runs-table').dataset.columnCount;") == '2', 'native checkbox tags did not select both columns')


def run_table_columns(browser, run_ids):
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def capture(name):
    Path('/tmp/' + name + '.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
  def settled():
    return evaluate("return document.querySelector('#runs-region').getAttribute('aria-busy') === 'false';")
  def selection():
    return evaluate("return [...document.querySelectorAll('#run-rows input:checked')].map(node => node.getAttribute('aria-label')).sort();")
  def loss_state():
    return evaluate('''const doc = document.querySelector('#chart-preview');
      const card = [...doc.querySelectorAll('[data-interactive-chart]')].find(node => node.querySelector('h2').textContent === 'loss');
      return {range: [card.querySelector('[data-chart-range]').dataset.startX, card.querySelector('[data-chart-range]').dataset.endX],
        hidden: [...card.querySelectorAll('[data-chart-run]')].filter(node => node.getAttribute('aria-pressed') === 'false').map(node => node.querySelector('code').textContent)};''')
  def preserved():
    assert selection() == selected, 'table column or sort change lost selected runs'
    assert evaluate("return document.querySelector('#review-tab-charts').getAttribute('aria-selected');") == 'true', 'table column or sort change lost the active review tab'
    assert evaluate("return document.querySelector('#chart-preview') === window.__table_chart_preview;"), 'table column or sort change replaced the chart preview'
    assert loss_state() == explored, 'table column or sort change lost zoom or hidden curves'
  chart_context(browser)
  loss_prefix = '//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="loss"]'
  browser.click(loss_prefix + '//button[code="' + run_ids[0] + '"]', using='xpath')
  browser.keys(loss_prefix + '//*[@class="plot-scroll"]', '+', using='xpath')
  selected = selection()
  explored = loss_state()
  assert explored['hidden'] == [run_ids[0]], 'table fixture did not establish a hidden curve'
  evaluate("window.__table_chart_preview = document.querySelector('#chart-preview');")
  choose_run_table_columns(browser)
  preserved()
  assert evaluate('''return [...document.querySelectorAll('#run-rows td[data-column-kind="param"][data-column-key="/learning_rate"]')].every(node => Number(node.title) === 0.001)
    && document.querySelectorAll('#run-rows td[data-column-kind="param"]').length === 2;'''), 'parameter table values differ from uploaded parameters'
  for reduction, expected in [('max', 1.0), ('min', 1 / 80), ('last', 1 / 80)]:
    browser.click('#run-reduction-' + reduction + ' + span')
    wait_for(lambda: settled() and evaluate('''const values = [...document.querySelectorAll('#run-rows td[data-column-kind="metric"][data-column-key="loss"]')];
      return values.length === 2 && values.every(node => Number(node.title) === arguments[0]);''', expected), 'table summary tag did not show exact uploaded values')
    assert evaluate("return document.querySelector('input[name=\"run_reduction\"]:checked').value;") == reduction, 'table summary lost its native radio selection'
    assert evaluate("return document.querySelector('input[name=\"reduction\"]:checked').value;") == 'last', 'table summary changed the independent comparison summary'
    preserved()
  for direction, expected in [('ascending', sorted(run_ids)), ('descending', sorted(run_ids, reverse=True))]:
    browser.click(f'button[aria-label="Sort by Run {direction}"]')
    wait_for(lambda: settled() and evaluate("return [...document.querySelectorAll('#run-rows .run-link')].map(node => node.textContent);") == expected, 'native run header did not sort the full listing')
    assert evaluate("return document.querySelector('.run-column').getAttribute('aria-sort');") == direction, 'run header did not announce its sort direction'
    preserved()
  browser.keys('button[aria-label="Sort by loss ascending"]', ' ')
  wait_for(lambda: settled() and 'loss · ascending' in evaluate("return document.querySelector('#run-sort-description').textContent;"), 'native keyboard header sorting did not select the metric')
  preserved()
  for width in [1440, 1024]:
    browser.set_viewport(width, 1000)
    if width > 1000:
      table_scroll_contained(browser, width)
    else:
      workspace_layout(browser, width, wide=True)
    preserved()
    capture('workspace-columns-desktop' if width == 1440 else 'workspace-columns-1024')
  for width in [1001, 1000]:
    browser.set_viewport(width, 1000)
    if width == 1001:
      table_scroll_contained(browser, width)
    else:
      workspace_layout(browser, width, wide=True)
    preserved()
  for width in [320, 360]:
    browser.set_viewport(width, 800)
    table_scroll_contained(browser, width)
    assert evaluate('''const table = document.querySelector('#runs-region');
      return table.scrollWidth > table.clientWidth && ['#run-columns', '#run-column-search', '#run-reduction-options', '#search-input', '#task-input', '#status-options'].every(selector => {
        const rect = document.querySelector(selector).getBoundingClientRect(); return rect.left >= 0 && rect.right <= document.documentElement.clientWidth + 1;
      });'''), f'{width}px custom table does not scroll or its controls are clipped'
    capture(f'workspace-columns-{width}')
    preserved()
  browser.set_viewport(1440, 1000)
  for name in ['Parameter learning_rate', 'Metric loss']:
    browser.click(f'input[aria-label="{name}"] + span')
    wait_for(settled, 'removing a table column did not finish')
  assert evaluate("return document.querySelector('.runs-table').dataset.columnCount;") == '0', 'removing all columns did not restore the compact table'
  preserved()
  if evaluate("return !document.querySelector('#reset-run-sort').disabled;"):
    browser.click('#reset-run-sort')
    wait_for(settled, 'resetting the table sort did not finish')
  browser.click('#run-columns summary')
  chart_context(browser)
  browser.click(loss_prefix + '//button[code="' + run_ids[0] + '"]', using='xpath')
  browser.keys(loss_prefix + '//*[@class="plot-scroll"]', '\ue00c', using='xpath')
  print('Firefox run columns passed: parameter/metric checkbox tags, exact Last/Min/Max values, native header click/keyboard sorting, Run/Status/Task/Started columns, review/selection/zoom/visibility preserved, 1440/1024/1001px persistent history beside inspection, 1000/320/360px stacked panels, and contained table scroll with pinned run identity.', flush=True)


def workspace(run_id, second_run_id):
  browser = Firefox()
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def visible(selector):
    return evaluate('const node = document.querySelector(arguments[0]); return !!node && node.getClientRects().length > 0;', selector)
  def check_selection(expected):
    return evaluate('''const link = document.querySelector('#open-chart');
      if (!link || !link.getAttribute('href')) return [];
      return new URL(link.href).searchParams.getAll('run_id').sort();''') == sorted(expected)
  def capture(name):
    Path('/tmp/' + name + '.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
  try:
    browser.set_viewport(1440, 1000)
    browser.navigate('/login')
    browser.login()
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 2, 'uploaded runs did not appear')
    assert not workspace_layout(browser, 1440)['has_review'], 'initial run browser opened an implicit review'
    browser.click('#auto-refresh-toggle')
    start = len(trace_records())
    browser.click(f'input[aria-label="Select {run_id} for comparison"]')
    wait_for(lambda: check_selection([run_id]), 'selecting one run did not open its charts')
    assert visible('#review-panel-charts') and not visible('#review-panel-overview'), 'inspection did not open Charts first'
    assert not any(record['path'] == '/api/log' for record in trace_records()[start:]), 'chart review eagerly fetched logs'
    assert not any(record['path'] == '/api/artifacts' for record in trace_records()[start:]), 'chart review eagerly fetched files'
    assert not any(record['path'] == '/api/run-system' for record in trace_records()[start:]), 'chart review eagerly fetched system metrics'
    browser.keys('#review-tab-charts', '\ue014')
    wait_for(lambda: visible('#review-panel-system'), 'ArrowRight from Charts did not open System metrics')
    assert evaluate('''const tab = document.querySelector('#review-tab-system');
      const panel = document.querySelector('#review-panel-system');
      return document.activeElement === tab && tab.getAttribute('aria-selected') === 'true'
        && tab.tabIndex === 0 && document.querySelector('#review-tab-charts').tabIndex === -1
        && document.querySelectorAll('.review-tabs [role="tab"][tabindex="0"]').length === 1
        && tab.getAttribute('aria-controls') === panel.id
        && panel.getAttribute('aria-labelledby') === tab.id;'''), 'System metrics keyboard navigation lost focus, selection, or panel association'
    wait_for(lambda: any(record['path'] == '/api/run-system' for record in trace_records()[start:]), 'System metrics did not lazily request the selected run capture')
    assert not visible('#review-panel-charts') and not visible('#review-panel-overview'), 'System metrics left another review panel visible'
    assert not any(record['path'] == '/api/log' for record in trace_records()[start:]), 'System metrics eagerly fetched logs'
    assert not any(record['path'] == '/api/artifacts' for record in trace_records()[start:]), 'System metrics eagerly fetched files'
    browser.keys('#review-tab-system', '\ue014')
    wait_for(lambda: visible('#review-panel-overview'), 'ArrowRight from System metrics did not open Overview')
    assert evaluate("return document.activeElement === document.querySelector('#review-tab-overview');"), 'Overview keyboard navigation lost focus'
    assert 'learning_rate' in evaluate("return document.querySelector('#run-detail').textContent;"), 'Overview omitted parameters'
    assert not any(record['path'] == '/api/log' for record in trace_records()[start:]), 'Overview eagerly fetched logs'
    browser.click('#review-tab-logs')
    wait_for(lambda: 'training complete' in evaluate("return document.querySelector('#run-logs').textContent;"), 'Logs did not load the uploaded stdout')
    browser.click('#review-tab-charts')
    artifact_files(browser, run_id)
    browser.click(f'input[aria-label="Select {second_run_id} for comparison"]')
    wait_for(lambda: check_selection([run_id, second_run_id]), 'selecting a second run did not compare automatically')
    wait_for(lambda: evaluate("return document.querySelectorAll('#comparison-values tbody tr').length;") == 2, 'comparison summary lost selected runs')
    for reduction, expected_step in [('max', 'step 0'), ('min', 'step 79'), ('last', 'step 79')]:
      browser.click('#reduction-' + reduction + ' + span')
      wait_for(lambda: evaluate('''const headings = [...document.querySelectorAll('#comparison-values thead th')].map(node => node.textContent);
        return document.querySelector('#comparison-values').getAttribute('aria-busy') === 'false' && [...document.querySelectorAll('#comparison-values tbody tr')].every(row => row.children[headings.indexOf('loss')]?.querySelector('.cell-step')?.textContent === arguments[0]);''', expected_step), 'native summary tag did not update its reduction')
      assert evaluate("return document.querySelector('input[name=\"reduction\"]:checked').value;") == reduction, 'summary tag lost its selected state'
    chart_context(browser)
    wait_for(lambda: evaluate("return document.querySelector('#chart-preview').querySelectorAll('svg').length;") > 0, 'embedded curves did not load')
    assert not evaluate("return !!document.querySelector('#chart-preview h1');"), 'embedded charts duplicated report heading'
    assert evaluate("return !!document.querySelector('#chart-preview details.parameter-comparison:not([open])');"), 'parameter differences are missing or displace the charts'
    interactive_charts(browser, [run_id, second_run_id], check_security=True)
    time_axes(browser, [run_id, second_run_id])
    workspace_layout(browser, 1440)
    capture('workspace-desktop')
    remember_chart_preview(browser)
    evaluate("window.__acceptance_previous_plots = [...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')];")
    browser.click('#refresh-button')
    wait_for(lambda: not evaluate("return document.querySelector('#refresh-button').disabled;"), 'Refresh did not finish')
    assert check_selection([run_id, second_run_id]), 'Refresh lost the selected comparison'
    assert evaluate('''const plots = [...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')];
      return plots.length === window.__acceptance_previous_plots.length && plots.every((node, index) => node === window.__acceptance_previous_plots[index] && node.isConnected);'''), 'unchanged warm Refresh replaced the live plot nodes'
    assert evaluate("return document.querySelector('#chart-preview') === window.__acceptance_chart_preview;"), 'Refresh replaced the stable chart preview'
    wait_for(lambda: evaluate("return document.querySelector('#chart-preview')?.querySelectorAll('[data-chart-hit]').length;") == 2, 'Refresh lost or duplicated chart interaction layers')
    interactive_charts(browser, [run_id, second_run_id])
    run_table_columns(browser, [run_id, second_run_id])
    browser.set_viewport(500, 800)
    assert evaluate('return innerWidth;') == 500, 'narrow viewport is not 500 CSS pixels wide'
    assert evaluate('return document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1;'), 'narrow dashboard overflows horizontally'
    assert evaluate('''return ['#source-select', '#refresh-button', '#search-input'].every(selector => {
      const box = document.querySelector(selector).getBoundingClientRect();
      return box.left >= 0 && box.right <= document.documentElement.clientWidth + 1;
    });'''), 'narrow controls are clipped'
    evaluate("document.querySelector('#review-section').scrollIntoView();")
    chart_context(browser)
    assert evaluate('''return [...document.querySelector('#chart-preview').querySelectorAll('.chart-explorer-controls button, [data-chart-range], [data-chart-run]')].every(node => {
      const box = node.getBoundingClientRect(); return box.left >= 0 && box.right <= document.documentElement.clientWidth + 1;
    });'''), 'narrow chart controls overflow the preview'
    loss_region = '//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="loss"]//*[@class="plot-scroll"]'
    loss_hit = '//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="loss"]//*[@data-chart-hit]'
    browser.pointer(loss_hit, .001, using='xpath')
    hover_rows = evaluate("const card = [...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')].find(node => node.querySelector('h2').textContent === 'loss'); return [...card.querySelectorAll('[data-chart-readout-run]')].map(row => ({step: row.getAttribute('data-chart-step'), value: row.getAttribute('data-chart-value')}));")
    chart_context(browser)
    capture('workspace-narrow')
    chart_context(browser)
    print('Firefox narrow hover samples: ' + json.dumps(hover_rows), flush=True)
    assert len(hover_rows) == 2 and all(0 <= int(row['step']) <= 79 and float(row['value']) == 1 / (int(row['step']) + 1) for row in hover_rows), 'narrow hover did not report recorded loss samples'
    browser.keys(loss_region, '\ue00c', using='xpath')
    browser.keys(loss_region, '\ue014', using='xpath')
    assert evaluate("return [...document.querySelector('#chart-preview').querySelectorAll('[data-chart-readout-run]')].filter(row => row.getAttribute('data-chart-step') === '0' && Number(row.getAttribute('data-chart-value')) === 1).length;") == 2, 'refreshed keyboard handlers did not start at the first sample'
    browser.keys(loss_region, '\ue014', using='xpath')
    assert evaluate("return [...document.querySelector('#chart-preview').querySelectorAll('[data-chart-readout-run]')].filter(row => row.getAttribute('data-chart-step') === '1' && row.getAttribute('data-chart-value') === '0.5').length;") == 2, 'refreshed/narrow keyboard handlers skipped or duplicated a sample'
    capture('workspace-narrow')
    remember_chart_preview(browser)
    search = browser.element('#search-input')
    browser.call('POST', '/element/' + search + '/value', {'text': run_id})
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 1, 'run search did not apply')
    assert not visible('#review-section'), 'filtering left an unrelated comparison visible'
    assert evaluate("return document.querySelectorAll('#run-rows input:checked').length;") == 0, 'filtering left an implicit selection'
    assert_chart_preview_cleaned(browser)
    browser.click('#clear-filters')
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 2, 'Clear filters did not restore the runs')
    browser.click(f'input[aria-label="Select {run_id} for comparison"]')
    wait_for(lambda: check_selection([run_id]), 'selection did not recover after filtering')
    wait_for(lambda: evaluate("return document.querySelector('#chart-preview')?.querySelectorAll('[data-chart-hit]').length;") == 2, 'single-run selection did not restore chart interactions')
    remember_chart_preview(browser)
    browser.click('#clear-selection')
    assert not visible('#review-section') and visible('#review-empty'), 'Clear selection did not reset the workspace'
    assert_chart_preview_cleaned(browser)
    assert evaluate("return document.querySelectorAll('#status-options input[type=\"radio\"]').length;") == 8, 'fixed status choices are not native single-click radio tags'
    browser.click('#status-completed + span')
    wait_for(lambda: evaluate("return document.querySelector('#status-completed').checked && document.querySelector('#runs-region').getAttribute('aria-busy') === 'false';"), 'native Completed status tag did not finish filtering')
    browser.keys('#status-completed', '\ue014')
    wait_for(lambda: evaluate("return document.querySelector('#status-failed').checked && document.querySelector('#runs-region').getAttribute('aria-busy') === 'false';"), 'native status ArrowRight did not select Failed')
    assert evaluate("return getComputedStyle(document.querySelector('#status-failed + span')).outlineStyle;") == 'solid', 'keyboard status tag has no visible focus'
    assert evaluate("return document.querySelectorAll('#run-rows tr').length;") == 0, 'Failed status filter retained completed runs'
    browser.click('#clear-filters')
    wait_for(lambda: evaluate("return document.querySelector('#status-any').checked && document.querySelectorAll('#run-rows tr').length === 2;"), 'Clear filters did not restore the Any status tag')
    for width in [1000, 390, 320]:
      browser.set_viewport(width, 800)
      browser.click('#run-rows tr:first-child .run-link')
      wait_for(lambda: evaluate("return !document.querySelector('#chart-card').hidden && document.querySelector('#chart-preview svg') !== null && document.querySelector('#chart-card').getAttribute('aria-busy') === 'false';"), 'compact inspection did not finish loading its charts')
      compact_focus_visible(browser, 'review-title', width)
      browser.click('#close-review')
      compact_focus_visible(browser, 'runs-heading', width)
    print('Firefox workspace passed: parent-rendered comparison, exact hover, native legend/zoom/keyboard, System metrics tab focus and lazy capture loading, duplicate coordinates, dashboard/standalone CSP, lazy logs, refresh/selection cleanup, desktop/narrow layout.', flush=True)
  finally:
    browser.close()


def previews(run_id, second_run_id):
  preview_origin = 'https://ab.expri.example.net'
  browser = Firefox()
  def evaluate(script):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': []})
  def catalog():
    return browser.call('POST', '/execute/async', {'script': '''const done = arguments[0];
      fetch('/api/catalog').then(async response => done({status: response.status,
        catalog: response.ok ? await response.json() : null})).catch(() => done({status: 0}));''', 'args': []})
  def shared_catalog(result):
    return {**result, 'catalog': {name: value for name, value in result['catalog'].items()
      if name != 'run_management_enabled'}}
  def pause_dashboard():
    # Cookie replay intentionally presents a revoked/other-host session. Finish
    # initial page requests and pause automatic checks so those checks do not
    # navigate away while WebDriver observes the explicit catalog response.
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length === 2 && document.querySelector('#runs-region').getAttribute('aria-busy') === 'false';"), 'preview authentication page did not finish its initial run request')
    if evaluate("return document.querySelector('#auto-refresh-toggle').checked;"):
      browser.click('#auto-refresh-toggle')
    wait_for(lambda: 'Auto updates off' in evaluate("return document.querySelector('#updated-at').textContent;"), 'preview authentication fixture did not pause automatic checks')
  def assert_provenance():
    for selector in ['header .read-only', 'footer .deployment-revision']:
      assert browser.call('GET', '/element/' + browser.element(selector) + '/displayed'), 'preview deployment provenance is hidden after mounting'
    assert evaluate('''const badge = document.querySelector('header .read-only');
      const revision = document.querySelector('footer .deployment-revision');
      const branch = revision.querySelector('code');
      return {badge: badge.textContent, revision: revision.textContent,
        branch: branch.textContent, branch_title: branch.getAttribute('title')};''') == {
          'badge': 'Read only · AB · aaaaaaaa',
          'revision': 'AB · aaaaaaaa · built from fixture/ab',
          'branch': 'fixture/ab',
          'branch_title': 'Built from fixture/ab',
        }, 'preview deployment provenance does not match its pinned release'
  try:
    browser.set_viewport(1440, 1000)
    browser.navigate('/login')
    browser.login()
    wait_for(lambda: browser.catalog()['status'] == 200, 'main login did not complete')
    catalog_with_wire_check(browser, 200, 1)
    main_cookie = browser.cookies()[0]
    main_catalog = catalog()
    assert main_catalog['status'] == 200 and main_catalog['catalog']['run_management_enabled'] is True, 'primary dashboard did not allow run management'
    assert evaluate("return document.querySelector('script[src]').getAttribute('src');") == '/app.js', 'main did not serve its embedded assets'

    start = len(trace_records())
    browser.navigate('/', preview_origin)
    assert browser.call('GET', '/url') == preview_origin + '/login', 'main login unexpectedly authorized the preview'
    catalog_with_wire_check(browser, 401, 0)
    preview_requests = [record for record in trace_records()[start:]
      if record['host'] == urlsplit(preview_origin).netloc]
    assert preview_requests and all(record['session_cookie_count'] == 0 for record in preview_requests), 'main cookie escaped its host on the wire'
    assert not browser.cookies(), 'preview unexpectedly stored its own session before login'
    for width in [1440, 360, 320]:
      browser.set_viewport(width, 1000 if width >= 500 else 800)
      login_layout(browser, width)
    browser.set_viewport(1440, 1000)
    browser.login()
    wait_for(lambda: browser.catalog()['status'] == 200, 'preview native login did not complete')
    browser.click('#auto-refresh-toggle')
    catalog_with_wire_check(browser, 200, 1)
    ab_cookie = browser.cookies()[0]
    assert ab_cookie['value'] != main_cookie['value'], 'preview reused the main session'
    ab_catalog = catalog()
    assert ab_catalog['status'] == 200 and ab_catalog['catalog']['run_management_enabled'] is False, 'read-only preview unexpectedly allowed run management'
    assert shared_catalog(ab_catalog) == shared_catalog(main_catalog), 'preview and main do not share one catalog'
    assert evaluate("return document.querySelector('script[src]').getAttribute('src');") == '/assets/' + 'a' * 40 + '/app.js', 'preview did not serve pinned branch assets'
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 2, 'shared run list is missing in preview')
    assert_provenance()
    for selected in [run_id, second_run_id]:
      browser.click(f'input[aria-label="Select {selected} for comparison"]')
    wait_for(lambda: evaluate("return document.querySelectorAll('#comparison-values tbody tr').length;") == 2, 'preview comparison did not load shared run data')
    interactive_charts(browser, [run_id, second_run_id], prefix='workspace-ab')
    time_axes(browser, [run_id, second_run_id], prefix='workspace-ab')
    assert_provenance()
    Path('/tmp/workspace-ab.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))

    browser.restore_cookie(main_cookie)
    catalog_with_wire_check(browser, 401, 1)
    browser.restore_cookie(ab_cookie)
    catalog_with_wire_check(browser, 200, 1)
    browser.navigate('/')
    catalog_with_wire_check(browser, 200, 1)
    pause_dashboard()
    # Replay both directions while both server sessions are still valid.
    browser.restore_cookie(ab_cookie)
    catalog_with_wire_check(browser, 401, 1)
    browser.restore_cookie(main_cookie)
    catalog_with_wire_check(browser, 200, 1)
    browser.navigate('/', preview_origin)
    catalog_with_wire_check(browser, 200, 1)
    pause_dashboard()
    browser.click('#logout-form button[type="submit"]')
    catalog_with_wire_check(browser, 401, 0)
    browser.restore_cookie(ab_cookie)
    catalog_with_wire_check(browser, 401, 1)
    browser.navigate('/')
    catalog_with_wire_check(browser, 200, 1)
    pause_dashboard()
    browser.click('#logout-form button[type="submit"]')
    catalog_with_wire_check(browser, 401, 0)
    browser.restore_cookie(main_cookie)
    catalog_with_wire_check(browser, 401, 1)
    print('Firefox previews passed: separate host sessions, shared catalog/runs, branch assets and visible provenance, comparison, cookie replay denied, independent logout.', flush=True)
  finally:
    browser.close()


def project_workspace(run_id):
  browser = Firefox()
  source = 'hosted-project:demo'
  keys = ['worker:' + run_id, 'worker-b:' + run_id]
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def settled():
    return evaluate("return document.querySelector('#runs-region').getAttribute('aria-busy') === 'false';")
  def select(key):
    browser.click(f'#run-rows tr[data-run-key="{key}"] input[type="checkbox"]')
  def table_value(key):
    return evaluate('''const row = [...document.querySelectorAll('#run-rows tr')].find(node => node.dataset.runKey === arguments[0]);
      return row?.querySelector('td[data-column-kind="metric"][data-column-key="loss"]')?.title ?? null;''', key)
  def project_selected():
    return evaluate("return document.querySelector('#project-demo')?.checked === true && document.querySelector('#project-name').textContent === 'demo';")
  def capture(name):
    Path('/tmp/' + name + '.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
  def chart_keys():
    return evaluate("const link=document.querySelector('#open-chart'); return link?.getAttribute('href') ? new URL(link.href).searchParams.getAll('run_id').sort() : [];")
  def selected_keys():
    return evaluate("return [...document.querySelectorAll('#run-rows tr')].filter(row=>row.querySelector('input[type=\"checkbox\"]').checked).map(row=>row.dataset.runKey).sort();")
  def chart_state():
    return evaluate('''const doc=document.querySelector('#chart-preview');
      const card=[...doc.querySelectorAll('[data-interactive-chart]')].find(node=>node.querySelector('h2').textContent==='loss');
      const range=card.querySelector('[data-chart-range]'); return {range:[range.dataset.startX,range.dataset.endX],
        hidden:[...card.querySelectorAll('[data-chart-run]')].filter(node=>node.getAttribute('aria-pressed')==='false').map(node=>node.querySelector('code').textContent)};''')
  try:
    browser.set_viewport(1440, 1000)
    browser.navigate('/login')
    browser.login()
    wait_for(lambda: settled() and project_selected(), 'hosted dashboard did not default to its project')
    assert evaluate("return document.querySelector('#project-options legend').textContent;") == 'Projects', 'hosted aggregate selector is not labeled Projects'
    assert evaluate("return !!document.querySelector('#project-options')?.closest('.topbar');"), 'project switching consumes the full history pane'
    assert evaluate("return document.querySelector('#machine-all').checked;"), 'project did not begin with All machines'
    assert evaluate("return document.querySelectorAll('#machine-options input[type=\"radio\"]').length;") == 3, 'project machine tags do not match its two recorded origins'
    assert evaluate('''return [...document.querySelectorAll('#run-rows .run-link')].filter(node=>node.textContent===arguments[0]).length;''', run_id) == 2, 'project default collapsed duplicate actual run IDs'
    workspace_layout(browser, 1440, wide=True)
    browser.click('#machine-worker-b + span')
    wait_for(lambda: settled() and evaluate("return document.querySelectorAll('#run-rows tr').length;") == 1, 'native machine filter did not isolate worker-b')
    assert evaluate("return document.querySelector('#run-rows tr').dataset.runKey;") == keys[1], 'machine filter resolved the colliding ID to worker'
    browser.click('#machine-all + span')
    wait_for(lambda: settled() and evaluate("return document.querySelectorAll('#run-rows tr').length;") >= 4, 'All machines did not restore project history')
    for key in keys:
      select(key)
    wait_for(lambda: chart_keys() == sorted(keys), 'cross-machine selection did not retain both qualified run identities')
    assert evaluate("return new URL(document.querySelector('#open-chart').href).searchParams.get('source');") == source, 'cross-machine comparison used a legacy machine source'
    wait_for(lambda: evaluate("return document.querySelector('#chart-preview')?.querySelectorAll('[data-chart-hit]').length;") >= 1, 'project comparison chart did not load')
    assert selected_keys() == sorted(keys), 'equal actual run IDs share a selection state'
    assert evaluate('''const doc=document.querySelector('#chart-preview');
      const card=[...doc.querySelectorAll('[data-interactive-chart]')].find(node=>node.querySelector('h2').textContent==='loss');
      const labels=[...card.querySelectorAll('[data-chart-run]')].map(node=>node.textContent);
      return labels.length===2 && labels.some(label=>label.includes('worker-b') && label.includes(arguments[0])) && labels.some(label=>label.includes('worker:') && label.includes(arguments[0]));''', run_id), 'chart legend does not distinguish machines with equal run IDs'
    chart_context(browser)
    loss = '//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="loss"]'
    browser.keys(loss + '//*[@class="plot-scroll"]', '\ue011\ue014', using='xpath')
    values = evaluate("const card=[...document.querySelector('#chart-preview').querySelectorAll('[data-interactive-chart]')].find(node=>node.querySelector('h2').textContent==='loss'); return [...card.querySelectorAll('[data-chart-readout-run]')].map(node=>Number(node.dataset.chartValue)).sort((a,b)=>a-b);")
    assert values == [0.7, 1.0], 'exact comparison samples came from the same machine stream'
    browser.click(loss + '//button[code="' + keys[1] + '"]', using='xpath')
    browser.keys(loss + '//*[@class="plot-scroll"]', '+', using='xpath')
    exploration = chart_state()
    assert exploration['hidden'] == [keys[1]], 'project chart visibility toggle did not target the qualified run identity'
    evaluate("window.__project_chart_preview=document.querySelector('#chart-preview');")
    project_layouts = {}
    for width in [1440, 1024]:
      browser.set_viewport(width, 1000)
      project_layouts[width] = workspace_layout(browser, width, wide=True)
      capture('workspace-project-default-desktop' if width == 1440 else 'workspace-project-default-1024')
    browser.set_viewport(1440, 1000)
    choose_run_table_columns(browser)
    browser.click('button[aria-label="Sort by Machine ascending"]')
    wait_for(settled, 'native Machine header did not finish sorting')
    browser.click('#run-columns summary')
    assert sorted([float(table_value(key)) for key in keys]) == [0.004, 0.07], 'project table columns mixed the two machine metric streams'
    assert selected_keys() == sorted(keys) and chart_keys() == sorted(keys), 'Machine sorting or columns lost the cross-machine comparison'
    assert evaluate("return document.querySelector('#chart-preview')===window.__project_chart_preview;"), 'Machine sorting or columns replaced chart exploration'
    assert chart_state() == exploration, 'Machine sorting or columns changed project zoom or hidden curves'
    def stable_project_layout(width, scroll=False):
      geometry = table_scroll_contained(browser, width) if scroll else workspace_layout(browser, width, wide=True)
      reference = project_layouts[width]
      assert geometry['page_width'] == reference['page_width'], f'{width}px choosing project columns widened the page'
      assert all(abs(geometry['runs'][key] - reference['runs'][key]) <= 1 for key in ['left', 'right', 'width']), f'{width}px choosing project columns resized Runs'
      review_keys = reference['review'] if width > 1000 else ['left', 'right', 'width', 'height']
      assert all(abs(geometry['review'][key] - reference['review'][key]) <= 1 for key in review_keys), f'{width}px choosing project columns moved or resized Review'
    for width in [1440, 1024]:
      browser.set_viewport(width, 1000)
      stable_project_layout(width, scroll=True)
      capture('workspace-project-desktop' if width == 1440 else 'workspace-project-1024')
    for width in [320, 360]:
      browser.set_viewport(width, 800)
      table_scroll_contained(browser, width)
      evaluate("document.querySelector('#runs-region').scrollIntoView({block:'start'});")
      capture(f'workspace-project-{width}')
    browser.set_viewport(1440, 1000)
    browser.click('#run-columns summary')
    for name in ['Parameter learning_rate', 'Metric loss']:
      browser.click(f'input[aria-label="{name}"] + span')
      wait_for(settled, 'removing a project table column did not finish')
    assert evaluate("return document.querySelector('.runs-table').dataset.columnCount;") == '0', 'removing project columns did not restore its built-in table'
    browser.click('#run-columns summary')
    for width in [1440, 1024]:
      browser.set_viewport(width, 1000)
      stable_project_layout(width)
    browser.set_viewport(1440, 1000)
    choose_run_table_columns(browser)
    browser.click('#run-columns summary')
    stable_project_layout(1440, scroll=True)
    assert selected_keys() == sorted(keys) and chart_keys() == sorted(keys), 'layout changes lost the cross-machine comparison'
    assert evaluate("return document.querySelector('#chart-preview')===window.__project_chart_preview;"), 'layout changes replaced chart exploration'
    assert chart_state() == exploration, 'layout changes lost project zoom or hidden curves'
    for origin, key, value in [('worker', keys[0], 0.003), ('worker-b', keys[1], 0.006)]:
      fixture_coordination.write_phase(PROJECT_COORDINATION, 'publish-' + origin)
      wait_for(lambda: fixture_coordination.read_phase(PROJECT_COORDINATION) == 'published-' + origin, 'project publication did not finish for ' + origin, timeout=90)
      wait_for(lambda: table_value(key) is not None and float(table_value(key)) == value, 'project live update did not refresh the correct machine metric cell', timeout=25)
      assert project_selected() and evaluate("return document.querySelector('#machine-all').checked;"), 'live update changed project or machine scope'
      assert selected_keys() == sorted(keys) and chart_keys() == sorted(keys), 'live update collapsed equal-ID selected runs'
      assert evaluate("return document.querySelector('.runs-table').dataset.columnCount;") == '2', 'project live update changed table columns'
      assert 'Machine · ascending' in evaluate("return document.querySelector('#run-sort-description').textContent;"), 'project live update changed table sorting'
      assert evaluate("return document.querySelector('#review-tab-charts').getAttribute('aria-selected');") == 'true', 'project live update changed the review tab'
      assert evaluate("return document.querySelector('#chart-preview')===window.__project_chart_preview;"), 'project live update replaced the chart preview'
      assert chart_state() == exploration, 'project live update changed zoom or hidden machine curves'
    fixture_coordination.write_phase(PROJECT_COORDINATION, 'finish-runs')
    wait_for(lambda: fixture_coordination.read_phase(PROJECT_COORDINATION) == 'finished-runs', 'project runs did not finalize their scoped artifacts', timeout=90)
    for origin, key, log in [('worker', keys[0], 'automatic refresh log fixture'), ('worker-b', keys[1], 'project machine worker-b log')]:
      browser.click('#clear-selection')
      select(key)
      wait_for(lambda: chart_keys() == [key], 'single project run did not retain its machine identity')
      browser.click('#review-tab-logs')
      wait_for(lambda: log in evaluate("return document.querySelector('#run-logs').textContent;"), 'project Logs opened the other machine with the same run ID')
      browser.click('#review-tab-files')
      wait_for(lambda: evaluate("return !!document.querySelector('input[aria-label=\"Select outputs/project-scope.txt for download\"]');"), 'project Files omitted its uploaded machine artifact')
      browser.click('input[aria-label="Select outputs/project-scope.txt for download"]')
      if not evaluate("return document.querySelector('#artifact-config-path').value;"):
        browser.keys('#artifact-config-path', '/tmp/client.toml')
      command = wait_for(lambda: evaluate("return document.querySelector('#artifact-fetch-command')?.value;"), 'project artifact did not generate its actual-scope CLI command')
      args = shlex.split(command)
      assert args[args.index('--origin') + 1] == origin and args[args.index('--run-id') + 1] == run_id, 'project artifact CLI command contains the transport key or wrong origin'
      browser.click('#download-selected-files')
      wait_for(lambda: evaluate("return document.querySelectorAll('#selected-file-downloads a').length;") == 1, 'project selected artifact did not provide one attachment link')
      browser.click('#selected-file-downloads a[download]')
      expected = (origin + ' scope fixture\n').encode()
      wait_for(lambda: any(path.is_file() and path.stat().st_size == len(expected) and path.read_bytes() == expected for path in DOWNLOADS.iterdir()), 'project browser download resolved the artifact from the other machine', timeout=30)
    fixture_coordination.write_phase(PROJECT_COORDINATION, 'complete')
    print('Firefox project workspace passed: default All machines, native machine filter, colliding recorded IDs remain distinct in selection/chart/readout, Machine sorting/columns preserved, live samples from both origins, correctly scoped Logs/Files/CLI/native downloads, stable 1440/1024px split layout through column changes and stacked 320/360px layout with contained table scroll.', flush=True)
  except Exception:
    capture('workspace-project-failure')
    raise
  finally:
    browser.close()


def automatic_refresh(run_id, updated_run_id):
  browser = Firefox()
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def phase(name):
    fixture_coordination.write_phase(REFRESH_COORDINATION, name)
  def probes_since(start):
    return [record for record in trace_records()[start:]
      if record['path'] == '/api/updates' and record['status'] == 200]
  def data_counts():
    return {path: sum(record['path'] == path for record in trace_records())
      for path in ['/api/chart-data', '/api/chart', '/api/run', '/api/compare']}
  def loss_state():
    return evaluate('''const doc = document.querySelector('#chart-preview');
      const card = [...(doc?.querySelectorAll('[data-interactive-chart]') ?? [])].find(node => node.querySelector('h2').textContent === 'loss');
      if (!card) return null;
      const range = card.querySelector('[data-chart-range]');
      const svg = card.querySelector('svg');
      return {axis: svg.dataset.xAxis, time_zone: range.dataset.timeZone, domain: [svg.dataset.xMin, svg.dataset.xMax], range: [range.dataset.startX, range.dataset.endX],
        hidden: [...card.querySelectorAll('[data-chart-run]')].filter(node => node.getAttribute('aria-pressed') === 'false').map(node => node.querySelector('code').textContent),
        samples: [...card.querySelectorAll('[data-chart-run]')].map(node => ({run_id: node.querySelector('code').textContent, text: node.textContent})),
        point_labels: [...card.querySelectorAll('circle.point')].map(node => node.getAttribute('aria-label'))};''')
  def last_comparison_step():
    return evaluate('''const row = [...document.querySelectorAll('#comparison-values tbody tr')].find(node => node.querySelector('th').textContent === arguments[0]);
      const headings = [...document.querySelectorAll('#comparison-values thead th')].map(node => node.textContent);
      return row?.children[headings.indexOf('loss')]?.querySelector('.cell-step')?.textContent;''', updated_run_id)
  def selection():
    return evaluate("return [...document.querySelectorAll('#run-rows input:checked')].map(node => node.getAttribute('aria-label'));")
  def wait_phase(name):
    wait_for(lambda: fixture_coordination.read_phase(REFRESH_COORDINATION) == name,
      'worker publication did not finish: ' + name, timeout=90)
  try:
    REFRESH_COORDINATION.unlink(missing_ok=True)
    browser.set_viewport(1440, 1000)
    browser.navigate('/login')
    browser.login()
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") >= 2, 'auto-refresh fixture runs did not load')
    browser.keys('#task-input', 'train')
    wait_for(lambda: evaluate("return document.querySelector('#task-input').value;") == 'train' and
      evaluate("return document.querySelectorAll('#run-rows tr').length;") >= 2, 'auto-refresh task filter did not apply')
    wait_for(lambda: 'Live updates' in evaluate("return document.querySelector('#updated-at').textContent;"), 'native hosted EventSource did not connect', timeout=20)
    assert any(record['path'] == '/api/events' and record['status'] == 200 for record in trace_records()), 'native EventSource did not open its authenticated endpoint'
    for selected_run in [run_id, updated_run_id]:
      browser.click(f'input[aria-label="Select {selected_run} for comparison"]')
    wait_for(lambda: loss_state() is not None and last_comparison_step() == 'step 79', 'auto-refresh comparison did not load its initial samples')
    choose_run_table_columns(browser)
    browser.click('button[aria-label="Sort by Run ascending"]')
    wait_for(lambda: evaluate("return document.querySelector('#runs-region').getAttribute('aria-busy') === 'false';"), 'automatic update fixture did not finish run sorting')
    browser.click('#run-columns summary')
    select_chart_axis(browser, 'elapsed')
    wait_for(lambda: (loss_state() or {}).get('axis') == 'elapsed', 'auto-refresh fixture did not select elapsed time')
    # Let the initial revision baseline settle before proving a later unchanged
    # poll does not fetch chart/summary snapshots again.
    baseline = len(trace_records())
    wait_for(lambda: len(probes_since(baseline)) >= 2, 'five-second change probes did not run', timeout=25)
    counts = data_counts()
    unchanged = len(trace_records())
    wait_for(lambda: len(probes_since(unchanged)) >= 1, 'unchanged change probe did not run', timeout=15)
    time.sleep(.3)
    assert data_counts() == counts, 'unchanged auto poll refetched chart or scalar summaries'

    chart_context(browser)
    loss_prefix = '//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="loss"]'
    browser.click(loss_prefix + '//button[code="' + run_id + '"]', using='xpath')
    start_fraction, end_fraction = chart_sample_window(visible_chart_coordinates(browser, 'loss', 2), loss_state()['domain'])
    browser.pointer(loss_prefix + '//*[@data-chart-hit]', start_fraction, end_fraction, using='xpath')
    before = loss_state()
    assert before['hidden'] == [run_id], 'fixture did not hide the requested actual run'
    assert 0 < int(before['range'][0]) < int(before['range'][1]) < int(before['domain'][1]), 'fixture did not establish an absolute elapsed-time zoom'
    assert visible_chart_coordinates(browser, 'loss', 2), 'elapsed refresh zoom excluded every visible updated-run sample'
    selected_labels = selection()
    evaluate("window.__refresh_original_preview = document.querySelector('#chart-preview');")
    # Keep one native pointer gesture active while the worker publishes. The
    # chart must defer replacement until pointerup, then catch up automatically.
    chart_context(browser)
    evaluate("window.__acceptance_pointerup = 0; document.addEventListener('pointerup', () => { window.__acceptance_pointerup++; }, {capture: true});")
    browser.pointer(loss_prefix + '//*[@data-chart-hit]', .5, using='xpath', hold=True)
    phase('publish-80')
    wait_phase('published-80')
    held = len(trace_records())
    wait_for(lambda: len(probes_since(held)) >= 1, 'change probe did not inspect the active pointer gesture', timeout=15)
    time.sleep(.3)
    assert not any(item['run_id'] == updated_run_id and '81 samples' in item['text']
      for item in loss_state()['samples']), 'chart replacement interrupted an active pointer gesture'
    # Release the native pointer in the same dashboard document where its
    # gesture began, and verify that the chart received the event.
    chart_context(browser)
    browser.release_pointer()
    wait_for(lambda: evaluate("return window.__acceptance_pointerup;") > 0,
      'native pointer release did not reach the chart preview')
    wait_for(lambda: last_comparison_step() == 'step 80' and any(
      item['run_id'] == updated_run_id and '81 samples' in item['text'] for item in (loss_state() or {}).get('samples', [])),
      'new worker samples did not reach the selected chart and summary automatically', timeout=25)
    after = loss_state()
    assert after['range'] == before['range'], 'automatic replacement shifted the absolute zoom range'
    assert after['axis'] == 'elapsed', 'automatic replacement changed the elapsed-time axis'
    assert after['hidden'] == [run_id], 'automatic replacement forgot the hidden actual run ID'
    assert any('step 80 · value 0.005' in label for label in after['point_labels']), 'replacement omitted the new exact metric sample'
    assert selection() == selected_labels, 'automatic update changed selected runs'
    assert evaluate("return document.querySelector('#task-input').value;") == 'train', 'automatic update changed the task filter'
    assert evaluate("return document.querySelector('.runs-table').dataset.columnCount;") == '2', 'automatic update changed selected run table columns'
    assert evaluate("return document.querySelector('.run-column').getAttribute('aria-sort');") == 'ascending', 'automatic update changed run table sorting'
    wait_for(lambda: evaluate('''const row = [...document.querySelectorAll('#run-rows tr')].find(node => node.querySelector('.run-link').textContent === arguments[0]);
      return Number(row?.querySelector('td[data-column-kind="metric"][data-column-key="loss"]')?.title) === 0.005;''', updated_run_id), 'new metric sample did not update its selected run table column', timeout=15)
    assert evaluate("return document.querySelector('#review-tab-charts').getAttribute('aria-selected');") == 'true', 'automatic update changed the active tab'
    assert evaluate("return document.querySelector('#chart-preview') === window.__refresh_original_preview;"), 'automatic replacement changed the stable chart preview'
    chart_context(browser)

    # Parent-rendered controls continue working, while injected scripts and inline
    # event handlers remain blocked after the server snapshot is replaced.
    chart_context(browser)
    evaluate('''const script = document.createElement('script'); script.textContent = "document.documentElement.dataset.refreshScript = 'ran'"; document.body.append(script);
      document.querySelector('#chart-preview [data-chart-hit]').setAttribute('onclick', "document.documentElement.dataset.refreshHandler = 'ran'");''')
    browser.click('[data-chart-hit]')
    assert evaluate("return document.documentElement.dataset.refreshScript ?? null;") is None, 'replaced chart executed injected script content'
    assert evaluate("return document.documentElement.dataset.refreshHandler ?? null;") is None, 'replaced chart executed an injected inline handler'
    evaluate("document.querySelector('#chart-preview [data-chart-hit]').removeAttribute('onclick');")
    Path('/tmp/workspace-auto-refresh.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))

    # A second real publication proves large absolute UTC coordinates retain
    # their own locked range and hidden runs across pause/resume.
    select_chart_axis(browser, 'wall_clock')
    wait_for(lambda: (loss_state() or {}).get('axis') == 'wall_clock', 'pause/resume fixture did not select wall-clock time')
    browser.click('#time-zone-utc + span')
    wait_for(lambda: (loss_state() or {}).get('time_zone') == 'utc', 'pause/resume fixture did not choose UTC')
    assert loss_state()['hidden'] == [run_id], 'time-axis switch forgot the hidden run'
    chart_context(browser)
    start_fraction, end_fraction = chart_sample_window(visible_chart_coordinates(browser, 'loss', 2), loss_state()['domain'])
    browser.pointer(loss_prefix + '//*[@data-chart-hit]', start_fraction, end_fraction, using='xpath')
    before = loss_state()
    assert int(before['domain'][0]) < int(before['range'][0]) < int(before['range'][1]) < int(before['domain'][1]), 'pause/resume fixture did not lock a wall-clock range'
    assert visible_chart_coordinates(browser, 'loss', 2), 'wall-clock refresh zoom excluded every visible updated-run sample'

    browser.click('#auto-refresh-toggle')
    wait_for(lambda: 'Auto updates off' in evaluate("return document.querySelector('#updated-at').textContent;"), 'pause control did not report its state')
    paused = len(trace_records())
    phase('publish-81')
    wait_phase('published-81')
    time.sleep(6)
    assert not probes_since(paused), 'paused automatic refresh continued polling'
    assert last_comparison_step() == 'step 80', 'paused automatic refresh changed the visible summary'
    browser.click('#auto-refresh-toggle')
    wait_for(lambda: last_comparison_step() == 'step 81' and any(
      item['run_id'] == updated_run_id and '82 samples' in item['text'] for item in (loss_state() or {}).get('samples', [])),
      'resuming automatic refresh did not catch up with the worker publication', timeout=25)
    assert loss_state()['range'] == before['range'] and loss_state()['hidden'] == [run_id], 'pause/resume lost chart exploration state'
    assert loss_state()['axis'] == 'wall_clock', 'pause/resume changed the selected wall-clock axis'
    assert loss_state()['time_zone'] == 'utc' and evaluate("return document.querySelector('#time-zone-utc').checked;"), 'pause/resume changed the selected timezone'
    assert evaluate("return document.querySelector('.runs-table').dataset.columnCount;") == '2' and evaluate("return document.querySelector('.run-column').getAttribute('aria-sort');") == 'ascending', 'pause/resume lost run table columns or sorting'

    # Inspect the updated run's logs; another real publication must update this
    # tab alone and leave its selection and filter intact.
    browser.click('#clear-selection')
    browser.click(f'input[aria-label="Select {updated_run_id} for comparison"]')
    wait_for(lambda: evaluate("return document.querySelector('#review-title').textContent;") == updated_run_id, 'updated run did not open individually')
    browser.click('#review-tab-logs')
    wait_for(lambda: 'training complete' in evaluate("return document.querySelector('#run-logs').textContent;"), 'updated run log tail did not load')
    phase('publish-log')
    wait_phase('published-log')
    wait_for(lambda: 'automatic refresh log fixture' in evaluate("return document.querySelector('#run-logs').textContent;"), 'new worker log bytes did not refresh the active Logs tab', timeout=25)
    assert evaluate("return document.querySelector('#review-tab-logs').getAttribute('aria-selected');") == 'true', 'log refresh changed the active tab'
    assert evaluate("return document.querySelector('#task-input').value;") == 'train', 'log refresh changed the task filter'
    phase('complete')
    print('Firefox auto refresh passed: acknowledged tracking publications, live notifications and recovery probes, unchanged JSON snapshots skipped, active drag deferred, exact samples/summary, elapsed and wall-clock zoom and hidden run preserved, pause/resume, live Logs tab, dashboard CSP retained.', flush=True)
  except Exception:
    try:
      chart_context(browser)
      state = loss_state()
      if state:
        state['point_count'] = len(state['point_labels'])
        state['point_labels'] = state['point_labels'][-4:]
      print('Firefox auto refresh failure state: ' + json.dumps({
        'last_comparison_step': last_comparison_step(), 'loss_state': state,
        'freshness': evaluate("return document.querySelector('#updated-at')?.textContent;"),
        'request_counts': data_counts(),
      }), flush=True)
      Path('/tmp/workspace-auto-refresh.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
    except Exception:
      print('Firefox auto refresh failure state was unavailable.', flush=True)
    raise
  finally:
    browser.close()


def chart_data_fixture(project_id, run_id, unsafe_metric, base_step):
  browser = Firefox()
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  try:
    browser.set_viewport(1440, 1000)
    start = len(trace_records())
    browser.navigate('/?' + urlencode({'project_id': project_id, 'origin': 'fixture', 'run_id': run_id}))
    browser.login()
    wait_for(lambda: evaluate("return document.querySelector('#chart-preview')?.querySelectorAll('[data-interactive-chart]').length;") == 2,
      'long-run React chart preview did not render both metrics')
    if evaluate("return document.querySelector('#auto-refresh-toggle').checked;"):
      browser.click('#auto-refresh-toggle')
    chart_context(browser)
    requests = trace_records()[start:]
    assert any(record['path'] == '/api/chart-data' and record['status'] == 200 for record in requests), 'long-run preview did not request bounded chart JSON'
    assert not any(record['path'] == '/api/chart' for record in requests), 'long-run dashboard downloaded a static HTML report'
    assert evaluate('''return [...document.querySelectorAll('#chart-preview h2')].some(node => node.textContent === arguments[0]);''', unsafe_metric), 'React chart changed the literal metric name'
    assert evaluate("return document.querySelectorAll('#chart-preview img, #chart-preview script').length;") == 0, 'React chart parsed a metric name as HTML'
    assert evaluate("return document.documentElement.dataset.metric_injected ?? null;") is None, 'React chart executed metric text'
    browser.click('#review-tab-overview')
    unsafe_parameter = '<script>document.documentElement.dataset.parameter_injected="ran"</script>'
    wait_for(lambda: evaluate("return document.querySelector('#run-detail').textContent.includes(arguments[0]);", unsafe_parameter),
      'React overview changed the literal parameter text')
    assert evaluate("return document.querySelectorAll('#run-detail script').length;") == 0, 'React overview parsed a parameter as HTML'
    assert evaluate("return document.documentElement.dataset.parameter_injected ?? null;") is None, 'React overview executed parameter text'
    browser.click('#review-tab-charts')
    points = evaluate('''return [...document.querySelectorAll('#chart-preview [data-interactive-chart]')].map(card => ({
      name: card.querySelector('h2').textContent, point_count: card.querySelectorAll('circle.point').length,
      x_min: card.querySelector('svg').dataset.xMin, x_max: card.querySelector('svg').dataset.xMax}));''')
    assert len(points) == 2 and all(0 < item['point_count'] <= 600 and item['x_min'] == base_step for item in points), 'long-run browser rendered an unbounded or rounded plot'
    loss = '//*[@id="chart-preview"]//section[@data-interactive-chart][@data-chart-metric="loss"]'
    browser.keys(loss + '//*[@class="plot-scroll"]', '\ue011\ue014', using='xpath')
    first = evaluate('''const card = [...document.querySelectorAll('#chart-preview [data-interactive-chart]')].find(node => node.querySelector('h2').textContent === 'loss');
      const row = card.querySelector('[data-chart-readout-run]'); return row ? {step: row.dataset.chartStep, value: row.dataset.chartValue} : null;''')
    assert first and first['step'] == base_step and float(first['value']) == 1.0, 'long-run keyboard inspection rounded a step beyond Number.MAX_SAFE_INTEGER'
    before = len(trace_records())
    browser.pointer(loss + '//*[@data-chart-hit]', .2, .65, using='xpath')
    assert evaluate('''const card = [...document.querySelectorAll('#chart-preview [data-interactive-chart]')].find(node => node.querySelector('h2').textContent === 'loss');
      const range = card.querySelector('[data-chart-range]'); return BigInt(range.dataset.startX) > BigInt(arguments[0]) && BigInt(range.dataset.endX) > BigInt(range.dataset.startX);''', base_step), 'long-run native zoom lost exact large coordinates'
    assert not any(record['path'] in {'/api/chart-data', '/api/chart', '/api/run', '/api/compare'} for record in trace_records()[before:]), 'long-run exploration fetched experiment data again'
    Path('/tmp/workspace-chart-data.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
    print('Firefox long-run chart data passed: ' + json.dumps({'plots': points,
      'retained_points': sum(item['point_count'] for item in points), 'first_step': first['step'],
      'json_requests': sum(record['path'] == '/api/chart-data' for record in requests)}), flush=True)
  except Exception:
    Path('/tmp/workspace-chart-data-failure.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
    raise
  finally:
    browser.close()


def storage_management(project_id):
  browser = Firefox()
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def assert_storage_usage(dashboard):
    usage = evaluate('''const panel = document.querySelector('#storage-usage');
      const cards = Array.from(panel?.querySelectorAll('.storage-usage-grid > div') ?? []);
      return {
        card_count: cards.length,
        labels: cards.map(card => card.querySelector('dt')?.textContent.trim()),
        reference_totals: /File totals|Shared references/.test(panel?.textContent ?? ''),
      };''')
    assert usage['card_count'] == 2, dashboard + ' did not show exactly two storage usage cards'
    assert usage['labels'] == ['S3 storage', 'Local storage'], dashboard + ' changed storage usage labels'
    assert not usage['reference_totals'], dashboard + ' showed reference-expanded file totals'
  try:
    browser.set_viewport(1440, 1000)
    browser.navigate('/login')
    browser.login()
    selector = '#project-options input[value="hosted-project:' + project_id + '"] + span'
    wait_for(lambda: evaluate('return document.querySelector(arguments[0]) !== null;', selector), 'management fixture project was not discovered')
    browser.click(selector)
    browser.click('#workspace-view-storage + span')
    wait_for(lambda: evaluate("return document.querySelector('#delete-project-button') !== null && !document.querySelector('#delete-project-button').disabled;"), 'primary dashboard did not enable project deletion')
    assert_storage_usage('Primary dashboard')
    start = len(trace_records())
    browser.click('#delete-project-button')
    wait_for(lambda: evaluate("return document.querySelector('#delete-project-preview') !== null;"), 'project deletion preview did not load')
    assert project_id in evaluate("return document.querySelector('#delete-project-dialog').textContent;"), 'delete preview changed its project'
    assert evaluate("return document.querySelector('#delete-project-submit').disabled;"), 'project deletion did not require confirmation and password'
    browser.keys('#delete-project-confirmation', project_id)
    assert evaluate("return document.querySelector('#delete-project-submit').disabled;"), 'project name alone authorized deletion'
    browser.set_viewport(360, 800)
    assert evaluate('return document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1;'), 'narrow deletion preview overflowed the page'
    assert evaluate('''return ['#delete-project-confirmation', '#delete-project-password', '#delete-project-cancel'].every(selector => {
      const box = document.querySelector(selector).getBoundingClientRect();
      return box.left >= 0 && box.right <= document.documentElement.clientWidth + 1;
    });'''), 'narrow deletion controls were clipped'
    Path('/tmp/workspace-storage-management.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
    browser.click('#delete-project-cancel')
    wait_for(lambda: evaluate("return !document.querySelector('#delete-project-dialog')?.open;"), 'cancel did not close deletion preview')
    assert not any(record['path'] == '/api/projects/delete' and record['method'] == 'POST'
      for record in trace_records()[start:]), 'cancel submitted a destructive request'
    browser.set_viewport(1440, 1000)
    browser.navigate('/login', origin='https://ab.expri.example.net')
    browser.login()
    wait_for(lambda: evaluate('return document.querySelector(arguments[0]) !== null;', selector), 'AB did not share the management fixture catalog')
    browser.click(selector)
    browser.click('#workspace-view-storage + span')
    wait_for(lambda: evaluate("return document.querySelector('#storage-usage .storage-usage-grid') !== null;"), 'AB did not show shared storage statistics')
    assert_storage_usage('AB dashboard')
    assert evaluate("return document.querySelector('#delete-project-button') === null || document.querySelector('#delete-project-button').disabled;"), 'AB enabled destructive project controls'
    print('Firefox storage management passed: S3/local usage, scoped preview, name/password requirement, narrow layout, cancel and read-only AB.', flush=True)
  finally:
    browser.close()


if __name__ == '__main__':
  if len(sys.argv) == 4 and sys.argv[1] == '--deep-link':
    deep_link(sys.argv[2], sys.argv[3])
  elif len(sys.argv) == 4 and sys.argv[1] == '--workspace':
    workspace(sys.argv[2], sys.argv[3])
  elif len(sys.argv) == 4 and sys.argv[1] == '--previews':
    previews(sys.argv[2], sys.argv[3])
  elif len(sys.argv) == 4 and sys.argv[1] == '--auto-refresh':
    automatic_refresh(sys.argv[2], sys.argv[3])
  elif len(sys.argv) == 3 and sys.argv[1] == '--project':
    project_workspace(sys.argv[2])
  elif len(sys.argv) == 2 and sys.argv[1] == '--storage-input-only':
    project_storage()
  elif len(sys.argv) == 3 and sys.argv[1] == '--storage':
    project_storage(sys.argv[2])
  elif len(sys.argv) == 3 and sys.argv[1] == '--storage-management':
    storage_management(sys.argv[2])
  elif len(sys.argv) == 6 and sys.argv[1] == '--chart-data':
    chart_data_fixture(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5])
  else:
    forms()
