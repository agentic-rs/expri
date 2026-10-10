"""Bounded S3 fixture controls for the private CI network; never used in production."""
import argparse
from datetime import datetime, timezone
import hashlib
import hmac
import http.client
import json
import os
from pathlib import Path
from urllib.parse import quote, urlencode
import xml.etree.ElementTree as ET


def request(method, *, key='', query=None, body=b''):
  host = 's3:9000'
  bucket = 'expri-ci'
  path = '/' + bucket + ('/' + quote(key, safe='/') if key else '')
  query = urlencode(sorted((query or {}).items()), quote_via=quote, safe='')
  timestamp = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')
  day = timestamp[:8]
  digest = hashlib.sha256(body).hexdigest()
  scope = day + '/us-east-1/s3/aws4_request'
  headers = {'host': host, 'x-amz-content-sha256': digest, 'x-amz-date': timestamp}
  names = ';'.join(sorted(headers))
  canonical_headers = ''.join(name + ':' + headers[name] + '\n' for name in sorted(headers))
  canonical = '\n'.join([method, path, query, canonical_headers, names, digest])
  signing = ('AWS4' + os.environ['AWS_SECRET_ACCESS_KEY']).encode()
  for component in [day, 'us-east-1', 's3', 'aws4_request']:
    signing = hmac.new(signing, component.encode(), hashlib.sha256).digest()
  message = '\n'.join(['AWS4-HMAC-SHA256', timestamp, scope, hashlib.sha256(canonical.encode()).hexdigest()])
  signature = hmac.new(signing, message.encode(), hashlib.sha256).hexdigest()
  headers['Authorization'] = 'AWS4-HMAC-SHA256 Credential=' + os.environ['AWS_ACCESS_KEY_ID'] + '/' + scope + ', SignedHeaders=' + names + ', Signature=' + signature
  connection = http.client.HTTPConnection(host, timeout=10)
  try:
    connection.request(method, path + ('?' + query if query else ''), body=body, headers=headers)
    response = connection.getresponse()
    data = response.read(2 * 1024 * 1024 + 1)
    if len(data) > 2 * 1024 * 1024:
      raise RuntimeError('S3 fixture response exceeded its bound')
    if response.status not in [200, 204, 404]:
      raise RuntimeError('S3 fixture request failed: status ' + str(response.status))
    return response.status, data
  finally:
    connection.close()


def versions(prefix):
  status, data = request('GET', query={'versions': '', 'prefix': prefix, 'max-keys': '1000'})
  assert status == 200
  root = ET.fromstring(data)
  for element in root.iter():
    element.tag = element.tag.rsplit('}', 1)[-1]
  assert root.tag == 'ListVersionsResult' and root.findtext('IsTruncated') == 'false'
  return [{
    'key': item.findtext('Key'),
    'version_id': item.findtext('VersionId'),
    'delete_marker': item.tag == 'DeleteMarker',
    'size': int(item.findtext('Size') or '0'),
  } for item in root if item.tag in ['Version', 'DeleteMarker']]


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument('action', choices=['versioning', 'versions', 'uploads', 'put', 'head'])
  parser.add_argument('--key', default='')
  parser.add_argument('--prefix', default='')
  parser.add_argument('--file', type=Path)
  args = parser.parse_args()
  if args.action == 'versioning':
    status, _ = request('PUT', query={'versioning': ''}, body=b'<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>')
    assert status in [200, 204]
    print(json.dumps({'versioning': 'enabled'}))
  elif args.action == 'versions':
    print(json.dumps({'versions': versions(args.prefix)}))
  elif args.action == 'uploads':
    status, data = request('GET', query={'uploads': '', 'prefix': args.prefix})
    assert status == 200
    root = ET.fromstring(data)
    for element in root.iter():
      element.tag = element.tag.rsplit('}', 1)[-1]
    assert root.tag == 'ListMultipartUploadsResult' and root.findtext('IsTruncated') == 'false'
    print(json.dumps({'uploads': [item.findtext('Key') for item in root if item.tag == 'Upload']}))
  elif args.action == 'put':
    assert args.key and args.file and args.file.stat().st_size <= 65536
    status, _ = request('PUT', key=args.key, body=args.file.read_bytes())
    assert status in [200, 204]
    print(json.dumps({'status': status}))
  else:
    assert args.key
    status, _ = request('HEAD', key=args.key)
    print(json.dumps({'status': status}))


if __name__ == '__main__':
  main()
