"""Drive native hosted forms in isolated Firefox through W3C WebDriver."""
import http.client
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import time

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

  def navigate(self, path):
    self.call('POST', '/url', {'url': ORIGIN + path})

  def element(self, selector):
    return self.call('POST', '/element', {'using': 'css selector', 'value': selector})[ELEMENT]

  def click(self, selector):
    def displayed_element():
      element = self.element(selector)
      return element if self.call('GET', '/element/' + element + '/displayed') else None
    element = wait_for(displayed_element, 'native form button is unavailable')
    self.call('POST', '/element/' + element + '/click', {})

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
    return [cookie for cookie in self.call('GET', '/cookie') if cookie['name'] == '__Host-expri_session']

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
    browser.call('POST', '/cookie', {'cookie': cookie})
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
    assert evaluate('''const list = document.querySelector('.runs-card').getBoundingClientRect();
      const review = document.querySelector('#review-section').getBoundingClientRect();
      return list.right <= review.left + 1;'''), 'desktop runs and charts are not side by side'
    capture('workspace-desktop')
    browser.click('#refresh-button')
    wait_for(lambda: not evaluate("return document.querySelector('#refresh-button').disabled;"), 'Refresh did not finish')
    assert check_selection([run_id, second_run_id]), 'Refresh lost the selected comparison'
    browser.call('POST', '/window/rect', {'width': 500, 'height': 800})
    assert evaluate('return innerWidth;') == 500, 'narrow viewport is not 500 CSS pixels wide'
    assert evaluate('return document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1;'), 'narrow dashboard overflows horizontally'
    assert evaluate('''return ['#source-select', '#refresh-button', '#search-input'].every(selector => {
      const box = document.querySelector(selector).getBoundingClientRect();
      return box.left >= 0 && box.right <= document.documentElement.clientWidth + 1;
    });'''), 'narrow controls are clipped'
    evaluate("document.querySelector('#review-section').scrollIntoView();")
    capture('workspace-narrow')
    search = browser.element('#search-input')
    browser.call('POST', '/element/' + search + '/value', {'text': run_id})
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 1, 'run search did not apply')
    assert not visible('#review-section'), 'filtering left an unrelated comparison visible'
    assert evaluate("return document.querySelectorAll('#run-rows input:checked').length;") == 0, 'filtering left an implicit selection'
    browser.click('#clear-filters')
    wait_for(lambda: evaluate("return document.querySelectorAll('#run-rows tr').length;") == 2, 'Clear filters did not restore the runs')
    browser.click(f'input[aria-label="Select {run_id} for comparison"]')
    wait_for(lambda: check_selection([run_id]), 'selection did not recover after filtering')
    browser.click('#clear-selection')
    assert not visible('#review-section') and visible('#review-empty'), 'Clear selection did not reset the workspace'
    print('Firefox workspace passed: direct comparison, chart-first tabs, lazy logs, refresh, filters, desktop/narrow layout.', flush=True)
  finally:
    browser.close()


if __name__ == '__main__':
  if len(sys.argv) == 4 and sys.argv[1] == '--workspace':
    workspace(sys.argv[2], sys.argv[3])
  else:
    forms()
