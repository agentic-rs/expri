"""Test-only HTTPS proxy: preserve Firefox request headers and log no secrets."""
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import ssl
import subprocess
import tempfile
import threading
from urllib.parse import urlsplit, urlunsplit

POLICY_OVERRIDE = Path('/tmp/expri-browser-policy')
TRACE = Path('/tmp/expri-browser-requests.jsonl')
TRACE_LOCK = threading.Lock()
HOP_HEADERS = {'connection', 'transfer-encoding', 'keep-alive', 'proxy-connection'}
S3_HOST = 's3.expri.example.net'


def attachment_location(value):
  target = urlsplit(value)
  if target.scheme == 'http' and target.netloc == 's3:9000':
    return urlunsplit(('https', S3_HOST, target.path, target.query, ''))
  return value


def record_trace(record):
  with TRACE_LOCK:
    with TRACE.open('a') as trace:
      trace.write(json.dumps(record) + '\n')


class Proxy(BaseHTTPRequestHandler):
  def log_message(self, *_):
    pass

  def do_GET(self):
    self.forward()

  def do_POST(self):
    self.forward()

  def do_HEAD(self):
    self.forward()

  def forward(self):
    self.connection.settimeout(20)
    if self.headers.get('Host') == S3_HOST:
      self.forward_s3()
      return
    size = int(self.headers.get('Content-Length', '0'))
    if size > 1024:
      self.send_error(413)
      return
    body = self.rfile.read(size) if size else None
    # Host, Origin, Sec-Fetch-Site and Cookie come directly from Firefox.
    headers = {name: value for name, value in self.headers.items() if name.lower() not in HOP_HEADERS}
    upstream = http.client.HTTPConnection('service', 8787, timeout=20)
    started = False
    try:
      upstream.request(self.command, self.path, body=body, headers=headers)
      response = upstream.getresponse()
      events = response.getheader('Content-Type', '').startswith('text/event-stream')
      data = b'' if events else response.read(2 * 1024 * 1024 + 1)
      if len(data) > 2 * 1024 * 1024:
        raise ValueError('bounded proxy response exceeded')
      policy = response.getheader('Referrer-Policy')
      if POLICY_OVERRIDE.exists():
        policy = POLICY_OVERRIDE.read_text().strip()
      record = {
        'method': self.command, 'path': urlsplit(self.path).path,
        'host': self.headers.get('Host'),
        'origin': self.headers.get('Origin'), 'status': response.status,
        'referrer_policy': policy,
        'session_cookie_count': sum(
          pair.strip().split('=', 1)[0] == '__Host-expri_session'
          for header in self.headers.get_all('Cookie', []) for pair in header.split(';')
        ),
      }
      record_trace(record)
      self.send_response(response.status)
      for name, value in response.getheaders():
        if name.lower() not in HOP_HEADERS | {'content-length', 'referrer-policy'}:
          if name.lower() == 'location':
            value = attachment_location(value)
          self.send_header(name, value)
      if policy is not None:
        self.send_header('Referrer-Policy', policy)
      if not events:
        self.send_header('Content-Length', response.getheader('Content-Length', '0') if self.command == 'HEAD' else str(len(data)))
      self.end_headers()
      started = True
      if events:
        # read1 forwards each available event/heartbeat without waiting for a
        # full chunk or buffering the lifetime of a browser connection.
        while chunk := response.read1(64 * 1024):
          self.wfile.write(chunk)
          self.wfile.flush()
      else:
        self.wfile.write(data)
    except (OSError, ValueError, http.client.HTTPException):
      if not started:
        self.send_error(502, 'fixture upstream unavailable')
      else:
        self.close_connection = True
    finally:
      upstream.close()

  def forward_s3(self):
    # Only this fixed fixture origin can reach S3. Signed query strings are
    # preserved on the wire and are never included in logs or errors.
    target = urlsplit(self.path)
    if self.command not in {'GET', 'HEAD'} or target.scheme or target.netloc or target.fragment or not self.path.startswith('/'):
      self.send_error(400, 'invalid fixture attachment request')
      return
    headers = {}
    if self.headers.get('Range') is not None:
      headers['Range'] = self.headers['Range']
    upstream = http.client.HTTPConnection('s3', 9000, timeout=20)
    started = False
    try:
      # HTTPConnection supplies the original s3:9000 Host for signed requests.
      # No browser Cookie, Authorization or other dashboard headers cross here.
      upstream.request(self.command, self.path, headers=headers)
      response = upstream.getresponse()
      record_trace({
        'method': self.command, 'path': target.path, 'host': S3_HOST,
        'status': response.status,
        'session_cookie_count': sum(
          pair.strip().split('=', 1)[0] == '__Host-expri_session'
          for header in self.headers.get_all('Cookie', []) for pair in header.split(';')
        ),
        'forwarded_headers': list(headers),
      })
      self.send_response(response.status)
      for name, value in response.getheaders():
        if name.lower() not in HOP_HEADERS:
          self.send_header(name, value)
      self.end_headers()
      started = True
      if self.command != 'HEAD':
        while chunk := response.read(64 * 1024):
          self.wfile.write(chunk)
    except (OSError, ValueError, http.client.HTTPException):
      if not started:
        self.send_error(502, 'fixture attachment upstream unavailable')
      else:
        self.close_connection = True
    finally:
      upstream.close()


if __name__ == '__main__':
  TRACE.touch(mode=0o644)
  with tempfile.TemporaryDirectory(prefix='expri-browser-tls-') as directory:
    cert = Path(directory) / 'cert.pem'
    key = Path(directory) / 'key.pem'
    subprocess.run([
      'openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
      '-subj', '/CN=expri.example.net', '-addext', 'subjectAltName=DNS:expri.example.net,DNS:ab.expri.example.net,DNS:s3.expri.example.net',
      '-keyout', str(key), '-out', str(cert),
    ], check=True, timeout=20, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.load_cert_chain(cert, key)
    server = ThreadingHTTPServer(('0.0.0.0', 443), Proxy)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    print('Firefox HTTPS fixture ready.', flush=True)
    server.serve_forever()
