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
