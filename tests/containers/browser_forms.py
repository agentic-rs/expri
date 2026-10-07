"""Drive native hosted forms in isolated Firefox through W3C WebDriver."""
import http.client
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import time
from urllib.parse import urlencode, urlsplit

ORIGIN = 'https://expri.example.net'
POLICY_OVERRIDE = Path('/tmp/expri-browser-policy')
TRACE = Path('/tmp/expri-browser-requests.jsonl')
REFRESH_COORDINATION = Path('/tmp/expri-refresh-coordination.json')
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
    self.process = subprocess.Popen([
      'geckodriver', '--host', '127.0.0.1', '--port', '4444', '--log', 'fatal',
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
      env={**os.environ, 'TZ': 'Asia/Shanghai'})
    try:
      wait_for(lambda: self.request('GET', '/status'), 'Firefox driver did not start')
      session = self.request('POST', '/session', {'capabilities': {'alwaysMatch': {
        'browserName': 'firefox', 'acceptInsecureCerts': True,
        'moz:firefoxOptions': {'binary': '/usr/bin/firefox-esr', 'args': ['-headless'], 'prefs': {
          'network.proxy.type': 0, 'datareporting.policy.dataSubmissionEnabled': False,
          'toolkit.telemetry.enabled': False, 'browser.shell.checkDefaultBrowser': False,
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

  def element(self, selector, using='css selector'):
    return self.call('POST', '/element', {'using': using, 'value': selector})[ELEMENT]

  def click(self, selector, using='css selector'):
    def displayed_element():
      element = self.element(selector, using)
      return element if self.call('GET', '/element/' + element + '/displayed') else None
    element = wait_for(displayed_element, 'native form button is unavailable')
    # Gecko can find a visible child element while its iframe is clipped by the
    # parent viewport. Position both surfaces before dispatching native input.
    self.call('POST', '/execute/sync', {'script': '''arguments[0].scrollIntoView({block: 'center', inline: 'nearest'});
      window.frameElement?.scrollIntoView({block: 'center', inline: 'nearest'});''', 'args': [{ELEMENT: element}]})
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


def interactive_charts(browser, run_ids, prefix='workspace', check_security=False):
  def parent(script):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': []})
  assert parent("return document.querySelector('#chart-frame').getAttribute('sandbox');").split() == ['allow-same-origin'], 'chart sandbox allows unexpected capabilities'
  wait_for(lambda: parent("return document.querySelector('#chart-frame').contentDocument?.querySelectorAll('[data-interactive-chart]').length;") == 2, 'both metric plots were not enhanced')
  if check_security:
    policy = browser.call('POST', '/execute/async', {'script': '''const done = arguments[0];
      fetch(document.querySelector('#chart-frame').src).then(response => done(response.headers.get('Content-Security-Policy'))).catch(() => done(null));''', 'args': []})
    assert policy and "default-src 'none'" in policy and 'script-src' not in policy, 'chart response weakened its script-blocking CSP'
  interaction_start = len(trace_records())
  frame = browser.element('#chart-frame')
  browser.call('POST', '/frame', {'id': {ELEMENT: frame}})
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def chart_script(metric, script):
    return evaluate("const card = [...document.querySelectorAll('[data-interactive-chart]')].find(node => node.querySelector('h2').textContent === arguments[0]); " + script, metric)
  def selector(metric, suffix):
    return f'//section[@data-interactive-chart][h2="{metric}"]{suffix}'
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
    browser.call('POST', '/frame', {'id': None})
    Path('/tmp/' + name + '.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
    browser.call('POST', '/frame', {'id': {ELEMENT: frame}})
  try:
    assert chart_script('loss', "return card.querySelectorAll('[data-chart-hit]').length;") == 1, 'plot has duplicate interaction layers'
    assert range_of('loss') == (0, 79), 'loss range does not match recorded steps'
    if check_security:
      evaluate('''const script = document.createElement('script');
        script.textContent = "document.documentElement.setAttribute('data-probe-script', 'ran')";
        document.body.append(script);
        const card = [...document.querySelectorAll('[data-interactive-chart]')].find(node => node.querySelector('h2').textContent === 'duplicate_probe');
        card.querySelector('[data-chart-hit]').setAttribute('onclick', "document.documentElement.setAttribute('data-probe-handler', 'ran')");''')
      browser.click(selector('duplicate_probe', '//*[@data-chart-hit]'), using='xpath')
      assert evaluate("return document.documentElement.getAttribute('data-probe-script');") is None, 'sandboxed chart executed an injected script'
      assert evaluate("return document.documentElement.getAttribute('data-probe-handler');") is None, 'sandboxed chart executed an inline event handler'
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
    assert not any(record['path'] in {'/api/chart', '/api/run', '/api/compare'}
      for record in trace_records()[interaction_start:]), 'chart interactions fetched experiment data again'
  finally:
    browser.call('POST', '/frame', {'id': None})


def select_chart_axis(browser, axis):
  # Clicking the visible radio tag selects it in one native action. The input
  # remains keyboard-accessible while its label carries the visual treatment.
  browser.click('#x-axis-' + axis.replace('_', '-') + ' + span')
  selected = browser.call('POST', '/execute/sync', {'script': "return document.querySelector('input[name=\"x_axis\"]:checked').value;", 'args': []})
  assert selected == axis, f'native axis tag requested {axis}, actual value {selected}'


def chart_axis_diagnostics(browser, requested):
  return browser.call('POST', '/execute/sync', {'script': '''return {requested: arguments[0],
    selected: document.querySelector('input[name="x_axis"]:checked').value,
    rendered: [...(document.querySelector('#chart-frame').contentDocument?.querySelectorAll('svg[data-x-axis]') ?? [])].map(node => node.dataset.xAxis),
    error: document.querySelector('#chart-error').textContent,
    error_hidden: document.querySelector('#chart-error').hidden};''', 'args': [requested]})


def time_axes(browser, run_ids, prefix='workspace'):
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def state():
    return evaluate('''const doc = document.querySelector('#chart-frame').contentDocument;
      const card = [...(doc?.querySelectorAll('[data-interactive-chart]') ?? [])].find(node => node.querySelector('h2').textContent === 'loss');
      const duplicate = [...(doc?.querySelectorAll('[data-interactive-chart]') ?? [])].find(node => node.querySelector('h2').textContent === 'duplicate_probe');
      const range = card?.querySelector('[data-chart-range]'); const svg = card?.querySelector('svg');
      return range && svg ? {axis: svg.dataset.xAxis, range: [range.dataset.startX, range.dataset.endX],
        time_zone: range.dataset.timeZone, range_text: range.textContent,
        domain: [svg.dataset.xMin, svg.dataset.xMax], hidden: [...card.querySelectorAll('[data-chart-run]')].filter(node => node.getAttribute('aria-pressed') === 'false').map(node => node.querySelector('code').textContent),
        duplicate_hidden: duplicate ? [...duplicate.querySelectorAll('[data-chart-run]')].filter(node => node.getAttribute('aria-pressed') === 'false').map(node => node.querySelector('code').textContent) : null} : null;''')
  def choose(axis):
    previous_plot_count = 2 if state()['axis'] == 'step' else 1
    evaluate('''window.__acceptance_axis_document = document.querySelector('#chart-frame').contentDocument;
      window.__acceptance_axis_plots = [...window.__acceptance_axis_document.querySelectorAll('[data-interactive-chart]')];''')
    select_chart_axis(browser, axis)
    try:
      wait_for(lambda: (state() or {}).get('axis') == axis, 'native axis selection did not update the plot')
    except AssertionError as error:
      raise AssertionError('native axis selection did not update the plot: ' + json.dumps(chart_axis_diagnostics(browser, axis))) from error
    assert evaluate("return window.__acceptance_axis_plots.length === arguments[0] && window.__acceptance_axis_plots.every(node => !node.isConnected);", previous_plot_count), 'changed axis preview did not dispose the previous plot nodes'
    assert evaluate("return document.querySelector('#chart-frame').contentDocument === window.__acceptance_axis_document;"), 'axis switch replaced the chart document and its HTTP security policy'
    assert evaluate('''const plots = [...document.querySelector('#chart-frame').contentDocument.querySelectorAll('[data-interactive-chart]')];
      return plots.length === arguments[0] && plots.every(node => node.querySelectorAll('[data-chart-hit]').length === 1);''', 2 if axis == 'step' else 1), 'axis switch lost or duplicated chart interaction layers'
    assert 'x_axis=' + axis in evaluate("return document.querySelector('#open-chart').href;"), 'Open chart lost the selected axis'
  def child():
    browser.call('POST', '/frame', {'id': {ELEMENT: browser.element('#chart-frame')}})
  def sample():
    return evaluate('''const card = [...document.querySelectorAll('[data-interactive-chart]')].find(node => node.querySelector('h2').textContent === 'loss');
      const row = card.querySelector('[data-chart-readout-run]'); if (!row) return null;
      const circle = [...card.querySelectorAll('circle.point')].find(node => node.dataset.chartSeries === row.dataset.chartReadoutRun
        && node.dataset.xValue === row.dataset.chartX && node.dataset.timestamp === row.dataset.chartTimestamp);
      return {run_index: row.dataset.chartReadoutRun, run_id: row.querySelector('code').textContent,
        step: row.dataset.chartStep, value: row.dataset.chartValue, x: row.dataset.chartX, timestamp: row.dataset.chartTimestamp, text: row.textContent,
        recorded: circle ? {x: circle.dataset.xValue, timestamp: circle.dataset.timestamp, label: circle.getAttribute('aria-label')} : null};''')
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
    evaluate('''window.__acceptance_zone_document = document.querySelector('#chart-frame').contentDocument;
      window.__acceptance_zone_plots = [...window.__acceptance_zone_document.querySelectorAll('[data-interactive-chart]')];''')
    for zone in ['utc', 'local']:
      browser.click('#time-zone-' + zone + ' + span')
      wait_for(lambda: (state() or {}).get('time_zone') == zone, 'timezone tag did not redraw the chart')
      after = state()
      assert after['range'] == before['range'] and after['domain'] == before['domain'], 'timezone change shifted exact zoom coordinates'
      assert after['hidden'] == before['hidden'], 'timezone change lost hidden run IDs'
      assert evaluate("return document.querySelector('#chart-frame').contentDocument === window.__acceptance_zone_document;"), 'timezone change replaced the chart document'
      assert evaluate('''const plots = [...document.querySelector('#chart-frame').contentDocument.querySelectorAll('[data-interactive-chart]')];
        return plots.length === window.__acceptance_zone_plots.length && plots.every((node, index) => node === window.__acceptance_zone_plots[index]);'''), 'timezone change rebuilt the plots'
      assert evaluate("return document.querySelector('input[name=\"time_zone\"]:checked').value;") == zone, 'timezone tag lost its selected state'
      assert evaluate("return document.querySelector('#time-zone-label').textContent;") == ('UTC' if zone == 'utc' else 'Asia/Shanghai'), 'timezone tag did not name the displayed timezone'
      assert evaluate("return document.querySelector('#open-chart').href;") == href, 'timezone display change altered the server chart URL'
      assert evaluate("return document.querySelector('#open-chart-label').textContent;") == 'Open UTC chart', 'standalone server chart timezone is ambiguous'
      child()
      try:
        browser.pointer(loss + '//*[@data-chart-hit]', .35, using='xpath')
        row = wait_for(sample, 'timezone hover lost the exact recorded sample')
        assert_recorded_sample(row, 'wall_clock')
        if zone == 'local':
          assert 'UTC+08:00' in row['text'] or 'UTC+8' in row['text'], 'local readout omitted its offset at the recorded instant'
        else:
          assert 'UTC+08:00' not in row['text'] and 'UTC+8' not in row['text'], 'UTC tag retained the local display offset'
      finally:
        browser.call('POST', '/frame', {'id': None})
    assert not any(record['path'] in {'/api/chart', '/api/run', '/api/compare'} for record in trace_records()[start:]), 'timezone tags refetched experiment data'
  loss = '//section[@data-interactive-chart][h2="loss"]'
  child()
  try:
    for metric in ['loss', 'duplicate_probe']:
      browser.click(f'//section[@data-interactive-chart][h2="{metric}"]//button[code="' + run_ids[1] + '"]', using='xpath')
  finally:
    browser.call('POST', '/frame', {'id': None})
  try:
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
      child()
      try:
        duplicate = evaluate('''const card = [...document.querySelectorAll('section.card')].find(node => node.querySelector('h2')?.textContent === 'duplicate_probe');
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
        browser.pointer(loss + '//*[@data-chart-hit]', .2, .65, using='xpath')
      finally:
        browser.call('POST', '/frame', {'id': None})
      zoomed = state()
      assert int(full['domain'][0]) < int(zoomed['range'][0]) < int(zoomed['range'][1]) < int(full['domain'][1]), 'native drag did not narrow the time domain'
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
    child()
    try:
      for metric in ['loss', 'duplicate_probe']:
        browser.click(f'//section[@data-interactive-chart][h2="{metric}"]//button[code="' + run_ids[1] + '"]', using='xpath')
    finally:
      browser.call('POST', '/frame', {'id': None})
    assert state()['hidden'] == [] and state()['duplicate_hidden'] == [], 'returning to Step did not restore the visible comparison runs'
  finally:
    browser.call('POST', '/frame', {'id': None})


def remember_chart_document(browser):
  browser.call('POST', '/execute/sync', {'script': "window.__acceptance_chart_document = document.querySelector('#chart-frame').contentDocument;", 'args': []})


def assert_chart_document_cleaned(browser):
  wait_for(lambda: browser.call('POST', '/execute/sync', {'script': "return window.__acceptance_chart_document.querySelectorAll('[data-interactive-chart], [data-chart-hit], [data-chart-run]').length;", 'args': []}) == 0, 'navigation left interactive controls in the previous chart document')


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
    return evaluate('''const frame = document.querySelector('#chart-frame');
      return document.querySelector('#review-section')?.hidden === false
        && document.querySelector('#source-select')?.value === arguments[0]
        && document.querySelector('#review-title')?.textContent === arguments[1]
        && frame?.getAttribute('src')
        && JSON.stringify(new URL(frame.src).searchParams.getAll('run_id')) === JSON.stringify([arguments[1]]);''', source_id, expected)
  try:
    browser.call('POST', '/window/rect', {'width': 1440, 'height': 1000})
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
    wait_for(lambda: (evaluate("return document.querySelector('#chart-frame').contentDocument?.querySelectorAll('svg').length;") or 0) > 0,
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


def workspace(run_id, second_run_id):
  browser = Firefox()
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def visible(selector):
    return evaluate('const node = document.querySelector(arguments[0]); return !!node && node.getClientRects().length > 0;', selector)
  def check_selection(expected):
    return evaluate('''const frame = document.querySelector('#chart-frame');
      if (!frame || !frame.getAttribute('src')) return [];
      return new URL(frame.src).searchParams.getAll('run_id').sort();''') == sorted(expected)
  def capture(name):
    Path('/tmp/' + name + '.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))
  try:
    browser.call('POST', '/window/rect', {'width': 1440, 'height': 1000})
    browser.navigate('/login')
    browser.login()
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 2, 'uploaded runs did not appear')
    browser.click('#auto-refresh-toggle')
    start = len(trace_records())
    browser.click(f'input[aria-label="Select {run_id} for comparison"]')
    wait_for(lambda: check_selection([run_id]), 'selecting one run did not open its charts')
    assert visible('#review-panel-charts') and not visible('#review-panel-overview'), 'inspection did not open Charts first'
    assert not any(record['path'] == '/api/log' for record in trace_records()[start:]), 'chart review eagerly fetched logs'
    browser.call('POST', '/element/' + browser.element('#review-tab-charts') + '/value', {'text': '\ue014'})
    wait_for(lambda: visible('#review-panel-overview'), 'review tabs do not support ArrowRight')
    assert 'learning_rate' in evaluate("return document.querySelector('#run-detail').textContent;"), 'Overview omitted parameters'
    assert not any(record['path'] == '/api/log' for record in trace_records()[start:]), 'Overview eagerly fetched logs'
    browser.click('#review-tab-logs')
    wait_for(lambda: 'training complete' in evaluate("return document.querySelector('#run-logs').textContent;"), 'Logs did not load the uploaded stdout')
    browser.click('#review-tab-charts')
    browser.click(f'input[aria-label="Select {second_run_id} for comparison"]')
    wait_for(lambda: check_selection([run_id, second_run_id]), 'selecting a second run did not compare automatically')
    wait_for(lambda: evaluate("return document.querySelectorAll('#comparison-values tbody tr').length;") == 2, 'comparison summary lost selected runs')
    for reduction, expected_step in [('max', 'step 0'), ('min', 'step 79'), ('last', 'step 79')]:
      browser.click('#reduction-' + reduction + ' + span')
      wait_for(lambda: evaluate('''const headings = [...document.querySelectorAll('#comparison-values thead th')].map(node => node.textContent);
        return document.querySelector('#comparison-values').getAttribute('aria-busy') === 'false' && [...document.querySelectorAll('#comparison-values tbody tr')].every(row => row.children[headings.indexOf('loss')]?.querySelector('.cell-step')?.textContent === arguments[0]);''', expected_step), 'native summary tag did not update its reduction')
      assert evaluate("return document.querySelector('input[name=\"reduction\"]:checked').value;") == reduction, 'summary tag lost its selected state'
    frame = browser.element('#chart-frame')
    browser.call('POST', '/frame', {'id': {ELEMENT: frame}})
    try:
      wait_for(lambda: evaluate("return document.querySelectorAll('svg').length;") > 0, 'embedded curves did not load')
      assert not evaluate("return !!document.querySelector('h1');"), 'embedded charts duplicated report heading'
      assert evaluate("return !!document.querySelector('details.parameter-comparison:not([open])');"), 'parameter differences are missing or displace the charts'
    finally:
      browser.call('POST', '/frame', {'id': None})
    interactive_charts(browser, [run_id, second_run_id], check_security=True)
    time_axes(browser, [run_id, second_run_id])
    assert evaluate('''const list = document.querySelector('.runs-card').getBoundingClientRect();
      const review = document.querySelector('#review-section').getBoundingClientRect();
      return list.right <= review.left + 1;'''), 'desktop runs and charts are not side by side'
    capture('workspace-desktop')
    remember_chart_document(browser)
    evaluate("window.__acceptance_previous_plots = [...document.querySelector('#chart-frame').contentDocument.querySelectorAll('[data-interactive-chart]')];")
    browser.click('#refresh-button')
    wait_for(lambda: not evaluate("return document.querySelector('#refresh-button').disabled;"), 'Refresh did not finish')
    assert check_selection([run_id, second_run_id]), 'Refresh lost the selected comparison'
    assert evaluate('''const plots = [...document.querySelector('#chart-frame').contentDocument.querySelectorAll('[data-interactive-chart]')];
      return plots.length === window.__acceptance_previous_plots.length && plots.every((node, index) => node === window.__acceptance_previous_plots[index] && node.isConnected);'''), 'unchanged warm Refresh replaced the live plot nodes'
    assert evaluate("return document.querySelector('#chart-frame').contentDocument === window.__acceptance_chart_document;"), 'Refresh replaced the chart document and its HTTP security policy'
    wait_for(lambda: evaluate("return document.querySelector('#chart-frame').contentDocument?.querySelectorAll('[data-chart-hit]').length;") == 2, 'Refresh lost or duplicated chart interaction layers')
    interactive_charts(browser, [run_id, second_run_id])
    browser.call('POST', '/window/rect', {'width': 500, 'height': 800})
    assert evaluate('return innerWidth;') == 500, 'narrow viewport is not 500 CSS pixels wide'
    assert evaluate('return document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1;'), 'narrow dashboard overflows horizontally'
    assert evaluate('''return ['#source-select', '#refresh-button', '#search-input'].every(selector => {
      const box = document.querySelector(selector).getBoundingClientRect();
      return box.left >= 0 && box.right <= document.documentElement.clientWidth + 1;
    });'''), 'narrow controls are clipped'
    evaluate("document.querySelector('#review-section').scrollIntoView();")
    frame = browser.element('#chart-frame')
    browser.call('POST', '/frame', {'id': {ELEMENT: frame}})
    try:
      assert evaluate('''return [...document.querySelectorAll('.chart-explorer-controls button, [data-chart-range], [data-chart-run]')].every(node => {
        const box = node.getBoundingClientRect(); return box.left >= 0 && box.right <= document.documentElement.clientWidth + 1;
      });'''), 'narrow chart controls overflow their iframe'
      loss_region = '//section[@data-interactive-chart][h2="loss"]//*[@class="plot-scroll"]'
      loss_hit = '//section[@data-interactive-chart][h2="loss"]//*[@data-chart-hit]'
      browser.pointer(loss_hit, .001, using='xpath')
      hover_rows = evaluate("const card = [...document.querySelectorAll('[data-interactive-chart]')].find(node => node.querySelector('h2').textContent === 'loss'); return [...card.querySelectorAll('[data-chart-readout-run]')].map(row => ({step: row.getAttribute('data-chart-step'), value: row.getAttribute('data-chart-value')}));")
      browser.call('POST', '/frame', {'id': None})
      capture('workspace-narrow')
      browser.call('POST', '/frame', {'id': {ELEMENT: frame}})
      print('Firefox narrow hover samples: ' + json.dumps(hover_rows), flush=True)
      assert len(hover_rows) == 2 and all(0 <= int(row['step']) <= 79 and float(row['value']) == 1 / (int(row['step']) + 1) for row in hover_rows), 'narrow hover did not report recorded loss samples'
      browser.keys(loss_region, '\ue00c', using='xpath')
      browser.keys(loss_region, '\ue014', using='xpath')
      assert evaluate("return [...document.querySelectorAll('[data-chart-readout-run]')].filter(row => row.getAttribute('data-chart-step') === '0' && Number(row.getAttribute('data-chart-value')) === 1).length;") == 2, 'refreshed keyboard handlers did not start at the first sample'
      browser.keys(loss_region, '\ue014', using='xpath')
      assert evaluate("return [...document.querySelectorAll('[data-chart-readout-run]')].filter(row => row.getAttribute('data-chart-step') === '1' && row.getAttribute('data-chart-value') === '0.5').length;") == 2, 'refreshed/narrow keyboard handlers skipped or duplicated a sample'
    finally:
      browser.call('POST', '/frame', {'id': None})
    capture('workspace-narrow')
    remember_chart_document(browser)
    search = browser.element('#search-input')
    browser.call('POST', '/element/' + search + '/value', {'text': run_id})
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 1, 'run search did not apply')
    assert not visible('#review-section'), 'filtering left an unrelated comparison visible'
    assert evaluate("return document.querySelectorAll('#run-rows input:checked').length;") == 0, 'filtering left an implicit selection'
    assert_chart_document_cleaned(browser)
    browser.click('#clear-filters')
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 2, 'Clear filters did not restore the runs')
    browser.click(f'input[aria-label="Select {run_id} for comparison"]')
    wait_for(lambda: check_selection([run_id]), 'selection did not recover after filtering')
    wait_for(lambda: evaluate("return document.querySelector('#chart-frame').contentDocument?.querySelectorAll('[data-chart-hit]').length;") == 2, 'single-run selection did not restore chart interactions')
    remember_chart_document(browser)
    browser.click('#clear-selection')
    assert not visible('#review-section') and visible('#review-empty'), 'Clear selection did not reset the workspace'
    assert_chart_document_cleaned(browser)
    print('Firefox workspace passed: direct comparison, exact hover, native legend/zoom/keyboard, duplicate coordinates, sandbox/CSP, lazy logs, refresh/selection cleanup, desktop/narrow layout.', flush=True)
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
    browser.call('POST', '/window/rect', {'width': 1440, 'height': 1000})
    browser.navigate('/login')
    browser.login()
    wait_for(lambda: browser.catalog()['status'] == 200, 'main login did not complete')
    catalog_with_wire_check(browser, 200, 1)
    main_cookie = browser.cookies()[0]
    main_catalog = catalog()
    assert evaluate("return document.querySelector('script[src]').getAttribute('src');") == '/app.js', 'main did not serve its embedded assets'

    start = len(trace_records())
    browser.navigate('/', preview_origin)
    assert browser.call('GET', '/url') == preview_origin + '/login', 'main login unexpectedly authorized the preview'
    catalog_with_wire_check(browser, 401, 0)
    preview_requests = [record for record in trace_records()[start:]
      if record['host'] == urlsplit(preview_origin).netloc]
    assert preview_requests and all(record['session_cookie_count'] == 0 for record in preview_requests), 'main cookie escaped its host on the wire'
    assert not browser.cookies(), 'preview unexpectedly stored its own session before login'
    browser.login()
    wait_for(lambda: browser.catalog()['status'] == 200, 'preview native login did not complete')
    browser.click('#auto-refresh-toggle')
    catalog_with_wire_check(browser, 200, 1)
    ab_cookie = browser.cookies()[0]
    assert ab_cookie['value'] != main_cookie['value'], 'preview reused the main session'
    assert catalog() == main_catalog, 'preview and main do not share one catalog'
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
    # Replay both directions while both server sessions are still valid.
    browser.restore_cookie(ab_cookie)
    catalog_with_wire_check(browser, 401, 1)
    browser.restore_cookie(main_cookie)
    catalog_with_wire_check(browser, 200, 1)
    browser.navigate('/', preview_origin)
    catalog_with_wire_check(browser, 200, 1)
    browser.click('#logout-form button[type="submit"]')
    catalog_with_wire_check(browser, 401, 0)
    browser.restore_cookie(ab_cookie)
    catalog_with_wire_check(browser, 401, 1)
    browser.navigate('/')
    catalog_with_wire_check(browser, 200, 1)
    browser.click('#logout-form button[type="submit"]')
    catalog_with_wire_check(browser, 401, 0)
    browser.restore_cookie(main_cookie)
    catalog_with_wire_check(browser, 401, 1)
    print('Firefox previews passed: separate host sessions, shared catalog/runs, branch assets and visible provenance, comparison, cookie replay denied, independent logout.', flush=True)
  finally:
    browser.close()


def automatic_refresh(run_id, updated_run_id):
  browser = Firefox()
  def evaluate(script, *args):
    return browser.call('POST', '/execute/sync', {'script': script, 'args': list(args)})
  def phase(name):
    REFRESH_COORDINATION.write_text(json.dumps({'phase': name}))
  def probes_since(start):
    return [record for record in trace_records()[start:]
      if record['path'] == '/api/updates' and record['status'] == 200]
  def data_counts():
    return {path: sum(record['path'] == path for record in trace_records())
      for path in ['/api/chart', '/api/run', '/api/compare']}
  def loss_state():
    return evaluate('''const doc = document.querySelector('#chart-frame').contentDocument;
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
    wait_for(lambda: json.loads(REFRESH_COORDINATION.read_text()).get('phase') == name,
      'worker publication did not finish: ' + name, timeout=90)
  try:
    REFRESH_COORDINATION.unlink(missing_ok=True)
    browser.call('POST', '/window/rect', {'width': 1440, 'height': 1000})
    browser.navigate('/login')
    browser.login()
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 2, 'auto-refresh fixture runs did not load')
    browser.keys('#task-input', 'train')
    wait_for(lambda: evaluate("return document.querySelector('#task-input').value;") == 'train' and
      evaluate("return document.querySelectorAll('#run-rows tr').length;") == 2, 'auto-refresh task filter did not apply')
    for selected_run in [run_id, updated_run_id]:
      browser.click(f'input[aria-label="Select {selected_run} for comparison"]')
    wait_for(lambda: loss_state() is not None and last_comparison_step() == 'step 79', 'auto-refresh comparison did not load its initial samples')
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

    frame = browser.element('#chart-frame')
    browser.call('POST', '/frame', {'id': {ELEMENT: frame}})
    loss_prefix = '//section[@data-interactive-chart][h2="loss"]'
    try:
      browser.click(loss_prefix + '//button[code="' + run_id + '"]', using='xpath')
      browser.pointer(loss_prefix + '//*[@data-chart-hit]', .2, .65, using='xpath')
    finally:
      browser.call('POST', '/frame', {'id': None})
    before = loss_state()
    assert before['hidden'] == [run_id], 'fixture did not hide the requested actual run'
    assert 0 < int(before['range'][0]) < int(before['range'][1]) < int(before['domain'][1]), 'fixture did not establish an absolute elapsed-time zoom'
    selected_labels = selection()
    evaluate("window.__refresh_original_document = document.querySelector('#chart-frame').contentDocument;")
    # Keep one native pointer gesture active while the worker publishes. The
    # chart must defer replacement until pointerup, then catch up automatically.
    browser.call('POST', '/frame', {'id': {ELEMENT: browser.element('#chart-frame')}})
    try:
      evaluate("window.__acceptance_pointerup = 0; document.addEventListener('pointerup', () => { window.__acceptance_pointerup++; }, {capture: true});")
      browser.pointer(loss_prefix + '//*[@data-chart-hit]', .5, using='xpath', hold=True)
    finally:
      browser.call('POST', '/frame', {'id': None})
    phase('publish-80')
    wait_phase('published-80')
    held = len(trace_records())
    wait_for(lambda: len(probes_since(held)) >= 1, 'change probe did not inspect the active pointer gesture', timeout=15)
    time.sleep(.3)
    assert not any(item['run_id'] == updated_run_id and '81 samples' in item['text']
      for item in loss_state()['samples']), 'chart replacement interrupted an active pointer gesture'
    # WebDriver dispatches native actions in its current browsing context.
    # Release inside the child where the gesture began; a parent-context
    # pointerup leaves Gecko's child pointer capture active.
    browser.call('POST', '/frame', {'id': {ELEMENT: browser.element('#chart-frame')}})
    try:
      browser.release_pointer()
      wait_for(lambda: evaluate("return window.__acceptance_pointerup;") > 0,
        'native pointer release did not reach the chart iframe')
    finally:
      browser.call('POST', '/frame', {'id': None})
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
    assert evaluate("return document.querySelector('#review-tab-charts').getAttribute('aria-selected');") == 'true', 'automatic update changed the active tab'
    assert evaluate("return document.querySelector('#chart-frame').contentDocument === window.__refresh_original_document;"), 'automatic replacement changed the HTTP chart document'
    assert evaluate("return document.querySelector('#chart-frame').getAttribute('sandbox');").split() == ['allow-same-origin'], 'automatic replacement weakened the iframe sandbox'

    # Parent-owned controls continue working, while content scripts and inline
    # event handlers remain blocked after the server snapshot is replaced.
    browser.call('POST', '/frame', {'id': {ELEMENT: browser.element('#chart-frame')}})
    try:
      evaluate('''const script = document.createElement('script'); script.textContent = "document.documentElement.dataset.refreshScript = 'ran'"; document.body.append(script);
        document.querySelector('[data-chart-hit]').setAttribute('onclick', "document.documentElement.dataset.refreshHandler = 'ran'");''')
      browser.click('[data-chart-hit]')
      assert evaluate("return document.documentElement.dataset.refreshScript ?? null;") is None, 'replaced chart executed injected script content'
      assert evaluate("return document.documentElement.dataset.refreshHandler ?? null;") is None, 'replaced chart executed an injected inline handler'
      evaluate("document.querySelector('[data-chart-hit]').removeAttribute('onclick');")
    finally:
      browser.call('POST', '/frame', {'id': None})
    Path('/tmp/workspace-auto-refresh.png').write_bytes(base64.b64decode(browser.call('GET', '/screenshot')))

    # A second real publication proves large absolute UTC coordinates retain
    # their own locked range and hidden runs across pause/resume.
    select_chart_axis(browser, 'wall_clock')
    wait_for(lambda: (loss_state() or {}).get('axis') == 'wall_clock', 'pause/resume fixture did not select wall-clock time')
    browser.click('#time-zone-utc + span')
    wait_for(lambda: (loss_state() or {}).get('time_zone') == 'utc', 'pause/resume fixture did not choose UTC')
    assert loss_state()['hidden'] == [run_id], 'time-axis switch forgot the hidden run'
    browser.call('POST', '/frame', {'id': {ELEMENT: browser.element('#chart-frame')}})
    try:
      browser.pointer(loss_prefix + '//*[@data-chart-hit]', .2, .65, using='xpath')
    finally:
      browser.call('POST', '/frame', {'id': None})
    before = loss_state()
    assert int(before['domain'][0]) < int(before['range'][0]) < int(before['range'][1]) < int(before['domain'][1]), 'pause/resume fixture did not lock a wall-clock range'

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
    print('Firefox auto refresh passed: real finalized worker publications, five-second probes, unchanged snapshots skipped, active drag deferred, exact samples/summary, elapsed and wall-clock zoom and hidden run preserved, pause/resume, live Logs tab, sandbox/CSP retained.', flush=True)
  except Exception:
    try:
      browser.call('POST', '/frame', {'id': None})
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


if __name__ == '__main__':
  if len(sys.argv) == 4 and sys.argv[1] == '--deep-link':
    deep_link(sys.argv[2], sys.argv[3])
  elif len(sys.argv) == 4 and sys.argv[1] == '--workspace':
    workspace(sys.argv[2], sys.argv[3])
  elif len(sys.argv) == 4 and sys.argv[1] == '--previews':
    previews(sys.argv[2], sys.argv[3])
  elif len(sys.argv) == 4 and sys.argv[1] == '--auto-refresh':
    automatic_refresh(sys.argv[2], sys.argv[3])
  else:
    forms()
