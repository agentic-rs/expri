"""Drive native hosted forms in isolated Firefox through W3C WebDriver."""
import http.client
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import time
from urllib.parse import urlsplit

ORIGIN = 'https://expri.example.net'
POLICY_OVERRIDE = Path('/tmp/expri-browser-policy')
TRACE = Path('/tmp/expri-browser-requests.jsonl')
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
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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
    self.call('POST', '/element/' + element + '/click', {})

  def keys(self, selector, text, using='css selector'):
    self.call('POST', '/element/' + self.element(selector, using) + '/value', {'text': text})

  def pointer(self, selector, fraction, drag_to=None, using='css selector'):
    element = self.element(selector, using)
    # Position the viewport, then use native input; never synthesize DOM events.
    geometry = self.call('POST', '/execute/sync', {'script': '''const node = arguments[0];
      node.scrollIntoView({block: 'center', inline: 'nearest'});
      const rect = node.getBoundingClientRect();
      return {left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom,
        width: rect.width, viewport_width: innerWidth, viewport_height: innerHeight};''', 'args': [{ELEMENT: element}]})
    left = max(0, geometry['left'])
    right = min(geometry['viewport_width'], geometry['right'])
    top = max(0, geometry['top'])
    bottom = min(geometry['viewport_height'], geometry['bottom'])
    assert left < right and top < bottom, 'chart pointer target is outside the viewport'
    center_x = int((left + right) / 2)
    center_y = int((top + bottom) / 2)
    def move(position, duration):
      target_x = int(max(left + 1, min(right - 1, geometry['left'] + position * geometry['width'])))
      return {'type': 'pointerMove', 'duration': duration, 'origin': {ELEMENT: element},
        'x': target_x - center_x, 'y': int((top + bottom) / 2) - center_y}
    actions = [move(fraction, 100)]
    if drag_to is not None:
      actions += [{'type': 'pointerDown', 'button': 0}, move(drag_to, 300), {'type': 'pointerUp', 'button': 0}]
    self.call('POST', '/actions', {'actions': [{
      'type': 'pointer', 'id': 'chart-pointer', 'parameters': {'pointerType': 'mouse'}, 'actions': actions,
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
    wait_for(lambda: sample('loss', 0, 1.0), 'native hover did not show exact recorded first values')
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
    frame = browser.element('#chart-frame')
    browser.call('POST', '/frame', {'id': {ELEMENT: frame}})
    try:
      wait_for(lambda: evaluate("return document.querySelectorAll('svg').length;") > 0, 'embedded curves did not load')
      assert not evaluate("return !!document.querySelector('h1');"), 'embedded charts duplicated report heading'
      assert evaluate("return !!document.querySelector('details.parameter-comparison:not([open])');"), 'parameter differences are missing or displace the charts'
    finally:
      browser.call('POST', '/frame', {'id': None})
    interactive_charts(browser, [run_id, second_run_id], check_security=True)
    assert evaluate('''const list = document.querySelector('.runs-card').getBoundingClientRect();
      const review = document.querySelector('#review-section').getBoundingClientRect();
      return list.right <= review.left + 1;'''), 'desktop runs and charts are not side by side'
    capture('workspace-desktop')
    remember_chart_document(browser)
    browser.click('#refresh-button')
    wait_for(lambda: not evaluate("return document.querySelector('#refresh-button').disabled;"), 'Refresh did not finish')
    assert check_selection([run_id, second_run_id]), 'Refresh lost the selected comparison'
    assert_chart_document_cleaned(browser)
    wait_for(lambda: evaluate("return document.querySelector('#chart-frame').contentDocument?.querySelectorAll('[data-chart-hit]').length;") == 2, 'Refresh lost or duplicated chart interaction layers')
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
      browser.keys(loss_region, '\ue00c', using='xpath')
      browser.pointer(loss_hit, .001, using='xpath')
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
    catalog_with_wire_check(browser, 200, 1)
    ab_cookie = browser.cookies()[0]
    assert ab_cookie['value'] != main_cookie['value'], 'preview reused the main session'
    assert catalog() == main_catalog, 'preview and main do not share one catalog'
    assert evaluate("return document.querySelector('script[src]').getAttribute('src');") == '/assets/' + 'a' * 40 + '/app.js', 'preview did not serve pinned branch assets'
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 2, 'shared run list is missing in preview')
    for selected in [run_id, second_run_id]:
      browser.click(f'input[aria-label="Select {selected} for comparison"]')
    wait_for(lambda: evaluate("return document.querySelectorAll('#comparison-values tbody tr').length;") == 2, 'preview comparison did not load shared run data')
    interactive_charts(browser, [run_id, second_run_id], prefix='workspace-ab')
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
    print('Firefox previews passed: separate host sessions, shared catalog/runs, branch assets and comparison, cookie replay denied, independent logout.', flush=True)
  finally:
    browser.close()


if __name__ == '__main__':
  if len(sys.argv) == 4 and sys.argv[1] == '--workspace':
    workspace(sys.argv[2], sys.argv[3])
  elif len(sys.argv) == 4 and sys.argv[1] == '--previews':
    previews(sys.argv[2], sys.argv[3])
  else:
    forms()
