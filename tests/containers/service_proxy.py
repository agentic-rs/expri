"""CI-only proxy losing successful metric and checkpoint-part acknowledgements."""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.request import Request, urlopen

state = {'armed': False, 'lost_ack': False, 'lost_stream_ack': False, 'part_urls': {}, 'stream_batches': 0}
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
    with lock:
      if self.path == '/test/arm':
        state['armed'] = True
      self.reply(200, state)

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
      if action == 'append_stream' and status == 200:
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
    self.reply(status, json.loads(data))

ThreadingHTTPServer(('0.0.0.0', 8001), Proxy).serve_forever()
