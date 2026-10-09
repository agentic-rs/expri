"""CI-only proxy injecting acknowledgement loss and interrupted checkpoint reads."""
import json
import http.client
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.request import Request, urlopen
from urllib.parse import urlsplit

state = {'armed': False, 'lost_ack': False, 'lost_stream_ack': False, 'part_urls': {}, 'stream_batches': 0,
  'download_armed': False, 'download_blocked': False, 'download_ranges': {}, 'multipart_paths': []}
uploads = {}
lock = threading.Lock()

class Proxy(BaseHTTPRequestHandler):
  def log_message(self, *args):
    pass

  def reply(self, status, value):
    body = json.dumps(value).encode()
    self.send_response(status)
    self.send_header('Content-Type', 'application/json')
    self.send_header('Content-Length', str(len(body)))
    self.end_headers()
    self.wfile.write(body)

  def do_GET(self):
    if self.path.startswith('/objects/'):
      self.object_download()
      return
    with lock:
      if self.path == '/test/arm':
        state['armed'] = True
      elif self.path == '/test/arm-download':
        state['download_armed'] = True
        state['download_blocked'] = False
        state['download_ranges'] = {}
      elif self.path == '/test/release-download':
        state['download_armed'] = False
      self.reply(200, state)

  def object_download(self):
    requested = self.headers.get('Range', '')
    with lock:
      state['download_ranges'][requested] = state['download_ranges'].get(requested, 0) + 1
      blocked = state['download_armed'] and requested.startswith('bytes=8388608-')
      if blocked:
        state['download_blocked'] = True
    if blocked:
      deadline = time.monotonic() + 45
      while time.monotonic() < deadline:
        with lock:
          if not state['download_armed']:
            break
        time.sleep(0.1)
    connection = http.client.HTTPConnection('s3', 9000, timeout=30)
    try:
      connection.request('GET', self.path.removeprefix('/objects'), headers={'Range': requested})
      response = connection.getresponse()
      self.send_response(response.status)
      for name in ['Content-Length', 'Content-Range', 'Content-Type']:
        value = response.getheader(name)
        if value is not None:
          self.send_header(name, value)
      self.end_headers()
      while chunk := response.read(64 * 1024):
        self.wfile.write(chunk)
    except (BrokenPipeError, ConnectionResetError):
      pass
    finally:
      connection.close()

  def do_POST(self):
    length = int(self.headers.get('Content-Length', '0'))
    if not 0 < length <= 1024 * 1024:
      self.reply(413, {'error': 'fixture request too large'})
      return
    body = self.rfile.read(length)
    query = json.loads(body)
    request = Request('http://service:8787/v1/request', data=body,
      headers={'Content-Type': 'application/json', 'Authorization': self.headers.get('Authorization', '')})
    try:
      with urlopen(request, timeout=45) as response:
        status, data = response.status, response.read(1024 * 1024)
    except HTTPError as error:
      status, data = error.code, error.read(1024 * 1024)
    except OSError:
      self.reply(503, {'error': 'fixture service unavailable'})
      return
    with lock:
      action = query['action']
      if action == 'begin_upload':
        uploads[query['upload_id']] = query['target'].get('path')
        path = query['target'].get('path')
        if query['target'].get('kind') == 'run' and path not in state['multipart_paths']:
          state['multipart_paths'].append(path)
      if action in {'append_stream', 'append_tracking'} and status == 200:
        state['stream_batches'] += 1
        if query['path'] == 'outputs/metrics.jsonl' and not state['lost_stream_ack']:
          state['lost_stream_ack'] = True
          self.reply(502, {'error': 'injected lost acknowledgement after durable stream commit'})
          return
      checkpoint = uploads.get(query.get('upload_id')) == 'outputs/checkpoint.pt'
      if action == 'part_url' and checkpoint:
        key = str(query['part_number'])
        state['part_urls'][key] = state['part_urls'].get(key, 0) + 1
      if action == 'record_part' and checkpoint and state['armed'] and status == 200:
        state['armed'] = False
        state['lost_ack'] = True
        self.reply(502, {'error': 'injected lost acknowledgement after durable part commit'})
        return
    result = json.loads(data)
    if status == 200 and query['action'] == 'download_url' and query['target'].get('path') == 'outputs/checkpoint.pt':
      # Keep MinIO's signed Host/path/query when forwarding. Never record the URL.
      parsed = urlsplit(result['url'])
      result['url'] = 'http://proxy:8001/objects' + parsed.path + '?' + parsed.query
    self.reply(status, result)

ThreadingHTTPServer(('0.0.0.0', 8001), Proxy).serve_forever()
