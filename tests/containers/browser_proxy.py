"""Test-only HTTPS proxy: preserve Firefox request headers and log no secrets."""
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import ssl
import subprocess
import tempfile
import threading
from urllib.parse import urlsplit

POLICY_OVERRIDE = Path('/tmp/expri-browser-policy')
TRACE = Path('/tmp/expri-browser-requests.jsonl')
TRACE_LOCK = threading.Lock()
HOP_HEADERS = {'connection', 'transfer-encoding', 'keep-alive', 'proxy-connection'}


class Proxy(BaseHTTPRequestHandler):
  def log_message(self, *_):
    pass

  def do_GET(self):
    self.forward()

  def do_POST(self):
    self.forward()

  def forward(self):
    self.connection.settimeout(20)
    size = int(self.headers.get('Content-Length', '0'))
    if size > 1024:
      self.send_error(413)
      return
    body = self.rfile.read(size) if size else None
    # Host, Origin, Sec-Fetch-Site and Cookie come directly from Firefox.
    headers = {name: value for name, value in self.headers.items() if name.lower() not in HOP_HEADERS}
    upstream = http.client.HTTPConnection('service', 8787, timeout=20)
    try:
      upstream.request(self.command, self.path, body=body, headers=headers)
      response = upstream.getresponse()
      data = response.read(2 * 1024 * 1024 + 1)
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
      with TRACE_LOCK:
        with TRACE.open('a') as trace:
          trace.write(json.dumps(record) + '\n')
      self.send_response(response.status)
      for name, value in response.getheaders():
        if name.lower() not in HOP_HEADERS | {'content-length', 'referrer-policy'}:
          self.send_header(name, value)
      if policy is not None:
        self.send_header('Referrer-Policy', policy)
      self.send_header('Content-Length', str(len(data)))
      self.end_headers()
      self.wfile.write(data)
    except (OSError, ValueError, http.client.HTTPException):
      self.send_error(502, 'fixture upstream unavailable')
    finally:
      upstream.close()


if __name__ == '__main__':
  TRACE.touch(mode=0o644)
  with tempfile.TemporaryDirectory(prefix='expri-browser-tls-') as directory:
    cert = Path(directory) / 'cert.pem'
    key = Path(directory) / 'key.pem'
    subprocess.run([
      'openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
      '-subj', '/CN=expri.example.net', '-addext', 'subjectAltName=DNS:expri.example.net,DNS:ab.expri.example.net',
      '-keyout', str(key), '-out', str(cert),
    ], check=True, timeout=20, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.load_cert_chain(cert, key)
    server = ThreadingHTTPServer(('0.0.0.0', 443), Proxy)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    print('Firefox HTTPS fixture ready.', flush=True)
    server.serve_forever()
