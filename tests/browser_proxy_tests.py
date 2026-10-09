"""Bounded HTTPS object fixture forwarding keeps browser credentials isolated."""
from email.message import Message
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('browser_proxy', Path(__file__).parent / 'containers/browser_proxy.py')
proxy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proxy)


class BrowserProxyTests(unittest.TestCase):
  def test_legacy_catalog_fixture_overrides_only_authenticated_project_catalog(self):
    for path, upstream_status, expected in [
      ('/api/projects', 200, 404), ('/api/projects?unused=1', 200, 404),
      ('/api/projects', 401, 401), ('/api/catalog', 200, 200),
      ('/api/projects-extra', 200, 200),
    ]:
      with self.subTest(path=path, upstream_status=upstream_status):
        handler = object.__new__(proxy.Proxy)
        handler.command, handler.path = 'GET', path
        handler.headers = Message()
        handler.headers['Host'] = 'expri.example.net'
        handler.connection, handler.rfile, handler.wfile = Mock(), Mock(), Mock()
        handler.send_response, handler.send_header, handler.end_headers, handler.send_error = Mock(), Mock(), Mock(), Mock()
        response = Mock(status=upstream_status)
        response.getheader.side_effect = lambda name, default=None: {'Content-Type': 'application/json', 'Referrer-Policy': 'same-origin'}.get(name, default)
        response.getheaders.return_value = [('Content-Type', 'application/json'), ('Content-Security-Policy', "default-src 'self'")]
        response.read.return_value = b'{"sources":[]}'
        upstream = Mock()
        upstream.getresponse.return_value = response
        with patch.object(proxy.http.client, 'HTTPConnection', return_value=upstream), patch.object(proxy, 'record_trace'), patch.object(proxy, 'POLICY_OVERRIDE', Mock(exists=lambda: False)), patch.object(proxy, 'LEGACY_CATALOG', Mock(exists=lambda: True)):
          handler.forward()
        handler.send_response.assert_called_once_with(expected)
        handler.send_header.assert_any_call('Referrer-Policy', 'same-origin')
        handler.send_header.assert_any_call('Content-Security-Policy', "default-src 'self'")
        expected_body = b'{"error":"unknown dashboard route"}' if expected == 404 else b'{"sources":[]}'
        handler.wfile.write.assert_called_once_with(expected_body)
        handler.send_error.assert_not_called()

  def test_event_stream_forwards_available_hints_without_buffering_until_disconnect(self):
    handler = object.__new__(proxy.Proxy)
    handler.command = 'GET'
    handler.path = '/api/events?source=hosted%3Ademo%3Aworker'
    handler.headers = Message()
    handler.headers['Host'] = 'expri.example.net'
    handler.headers['Cookie'] = '__Host-expri_session=private'
    handler.connection = Mock()
    handler.rfile = Mock()
    handler.wfile = Mock()
    handler.send_response = Mock()
    handler.send_header = Mock()
    handler.end_headers = Mock()
    handler.send_error = Mock()
    response = Mock(status=200)
    response.getheader.side_effect = lambda name, default=None: {'Content-Type': 'text/event-stream', 'Referrer-Policy': 'no-referrer'}.get(name, default)
    response.getheaders.return_value = [('Content-Type', 'text/event-stream')]
    response.read1.side_effect = [b'event: updates\ndata: {"catalog_revision":"7"}\n\n', b': heartbeat\n\n', b'']
    upstream = Mock()
    upstream.getresponse.return_value = response
    with patch.object(proxy.http.client, 'HTTPConnection', return_value=upstream), patch.object(proxy, 'record_trace'), patch.object(proxy, 'POLICY_OVERRIDE', Mock(exists=lambda: False)):
      handler.forward()
    response.read.assert_not_called()
    self.assertEqual(response.read1.call_count, 3)
    self.assertTrue(all(call.args == (64 * 1024,) for call in response.read1.call_args_list))
    self.assertEqual(handler.wfile.flush.call_count, 2)
    self.assertFalse(any(call.args[0] == 'Content-Length' for call in handler.send_header.call_args_list))
    handler.send_error.assert_not_called()

  def test_attachment_redirect_rewrites_only_the_exact_fixture_origin(self):
    path = '/bucket/model.pt?X-Amz-Signature=fixture%2B%2F&response-content-disposition=attachment'
    self.assertEqual(proxy.attachment_location('http://s3:9000' + path), 'https://s3.expri.example.net' + path)
    for origin in ['https://s3:9000', 'http://s3:9001', 'http://s3:9000.outside.invalid', 'http://user@s3:9000']:
      self.assertEqual(proxy.attachment_location(origin + path), origin + path)

  def test_attachment_stream_is_bounded_and_never_forwards_dashboard_credentials(self):
    handler = object.__new__(proxy.Proxy)
    handler.command = 'GET'
    handler.path = '/bucket/model.pt?X-Amz-Signature=fixture-secret'
    handler.headers = Message()
    for name, value in {'Host': proxy.S3_HOST, 'Range': 'bytes=0-', 'Cookie': '__Host-expri_session=private', 'Authorization': 'Bearer private'}.items():
      handler.headers[name] = value
    handler.connection = Mock()
    handler.wfile = Mock()
    handler.send_response = Mock()
    handler.send_header = Mock()
    handler.end_headers = Mock()
    handler.send_error = Mock()
    remaining = 128 * 1024 + 3
    read_sizes = []
    def read(size):
      nonlocal remaining
      read_sizes.append(size)
      count = min(size, remaining)
      remaining -= count
      return b'x' * count
    response = Mock(status=200)
    response.getheaders.return_value = [('Content-Length', '131075'), ('Content-Disposition', 'attachment; filename="model.pt"'), ('Content-Type', 'application/octet-stream')]
    response.read.side_effect = read
    upstream = Mock()
    upstream.getresponse.return_value = response
    records = []
    with patch.object(proxy.http.client, 'HTTPConnection', return_value=upstream) as connect, patch.object(proxy, 'record_trace', side_effect=records.append):
      handler.forward()
    connect.assert_called_once_with('s3', 9000, timeout=20)
    upstream.request.assert_called_once_with('GET', handler.path, headers={'Range': 'bytes=0-'})
    self.assertTrue(read_sizes and all(size == 64 * 1024 for size in read_sizes))
    self.assertEqual([len(call.args[0]) for call in handler.wfile.write.call_args_list], [65536, 65536, 3])
    handler.send_header.assert_any_call('Content-Length', '131075')
    handler.send_header.assert_any_call('Content-Disposition', 'attachment; filename="model.pt"')
    self.assertEqual(records[0]['forwarded_headers'], ['Range'])
    self.assertNotIn('fixture-secret', str(records))
    self.assertNotIn('X-Amz-', str(records))
    handler.send_error.assert_not_called()
    upstream.close.assert_called_once()


if __name__ == '__main__':
  unittest.main()
