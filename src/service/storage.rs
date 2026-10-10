use std::{collections::HashMap, time::Duration};

use quick_xml::{Reader, events::Event};
use s3::{Bucket, BucketConfiguration, creds::Credentials, error::S3Error, region::Region};
use serde::Deserialize;

use crate::error::{ExpriError, Result};

mod project_deletion;

pub use super::types::CompletedPart;

pub const MAX_PRESIGN_EXPIRY_SECS: u32 = 3600;

#[derive(Clone, Debug, Deserialize)]
pub struct S3Config {
  #[serde(default)]
  pub endpoint: Option<String>,
  pub bucket: String,
  pub region: String,
  #[serde(default)]
  pub path_style: bool,
  #[serde(default)]
  pub prefix: String,
}

#[derive(Clone, Debug)]
pub struct ObjectMetadata {
  pub size: u64,
}

pub trait ObjectStorage: Send + Sync {
  fn begin_upload(&self, key: &str, content_type: &str) -> Result<String>;
  fn presign_part(
    &self,
    key: &str,
    upload_id: &str,
    part_number: u32,
    expires_secs: u32,
  ) -> Result<String>;
  fn complete_upload(
    &self,
    key: &str,
    upload_id: &str,
    parts: &[CompletedPart],
  ) -> Result<ObjectMetadata>;
  fn head(&self, key: &str) -> Result<Option<ObjectMetadata>>;
  fn presign_get(&self, key: &str, expires_secs: u32) -> Result<String>;
  fn presign_get_attachment(
    &self,
    key: &str,
    expires_secs: u32,
    _disposition: &str,
  ) -> Result<String> {
    self.presign_get(key, expires_secs)
  }
  fn delete_object(&self, _key: &str) -> Result<()> {
    Err(invalid(
      "object deletion is not supported by this storage backend",
    ))
  }
  fn abort_upload(&self, _key: &str, _upload_id: &str) -> Result<()> {
    Err(invalid(
      "multipart cancellation is not supported by this storage backend",
    ))
  }
}

pub struct S3Storage {
  bucket: Box<Bucket>,
  prefix: String,
}

/// Select a provider explicitly because the two HTTP clients enable different rustls providers.
pub(super) fn init_tls() {
  // Another caller may already have selected a provider. Both maintained providers support our clients.
  let _ = rustls::crypto::ring::default_provider().install_default();
}

impl S3Storage {
  pub fn new(config: S3Config) -> Result<Self> {
    init_tls();
    let credentials = Credentials::from_env()
      .map_err(|_| invalid("set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY for S3 storage"))?;
    if credentials.access_key.as_deref().is_none_or(str::is_empty)
      || credentials.secret_key.as_deref().is_none_or(str::is_empty)
    {
      return Err(invalid("S3 credentials must not be empty"));
    }
    Self::with_credentials(config, credentials)
  }

  fn with_credentials(config: S3Config, credentials: Credentials) -> Result<Self> {
    init_tls();
    validate_config(&config)?;
    let region = match config.endpoint {
      Some(endpoint) => {
        // Transport URL parsing removes default ports. Sign the same normalized authority.
        let endpoint =
          reqwest::Url::parse(&endpoint).map_err(|_| invalid("invalid S3 endpoint"))?;
        Region::Custom {
          region: config.region,
          endpoint: endpoint.as_str().trim_end_matches('/').into(),
        }
      }
      None => config
        .region
        .parse()
        .map_err(|_| invalid("invalid AWS S3 region"))?,
    };
    let mut bucket = Bucket::new(&config.bucket, region, credentials)
      .map_err(|error| backend_error("configuration", error))?;
    if config.path_style {
      bucket = bucket.with_path_style();
    }
    bucket.set_request_timeout(Some(Duration::from_secs(30)));
    Ok(Self {
      bucket,
      prefix: config.prefix.trim_matches('/').into(),
    })
  }

  /// Bucket creation is explicit: normal service startup never changes bucket configuration.
  pub fn create_bucket(&self) -> Result<()> {
    let credentials = self
      .bucket
      .credentials()
      .map_err(|error| backend_error("credentials", error))?;
    let create = if self.bucket.is_path_style() {
      Bucket::create_with_path_style
    } else {
      Bucket::create
    };
    let response = create(
      &self.bucket.name(),
      self.bucket.region(),
      credentials,
      BucketConfiguration::default(),
    )
    .map_err(|error| backend_error("create bucket", error))?;
    if (200..300).contains(&response.response_code) {
      return Ok(());
    }
    if response.response_code == 409 {
      #[derive(Deserialize)]
      struct ErrorCode {
        #[serde(rename = "Code")]
        code: String,
      }
      if quick_xml::de::from_str::<ErrorCode>(&response.response_text)
        .is_ok_and(|error| error.code == "BucketAlreadyOwnedByYou")
      {
        return Ok(());
      }
    }
    Err(status_error("create bucket", response.response_code))
  }

  fn object_key(&self, key: &str) -> Result<String> {
    validate_key(key)?;
    let key = if self.prefix.is_empty() {
      key.into()
    } else {
      format!("{}/{key}", self.prefix)
    };
    if key.len() > 1024 {
      return Err(invalid("S3 object key exceeds 1024 bytes"));
    }
    Ok(key)
  }
}

impl ObjectStorage for S3Storage {
  fn delete_object(&self, key: &str) -> Result<()> {
    self.purge_object(key)
  }

  fn abort_upload(&self, key: &str, upload_id: &str) -> Result<()> {
    validate_upload_id(upload_id)?;
    let key = self.object_key(key)?;
    // Like completion, the sync client inserts the opaque ID before signing.
    let upload_id = s3::signing::uri_encode(upload_id, true);
    match self.bucket.abort_upload(&key, &upload_id) {
      Ok(()) | Err(S3Error::HttpFailWithBody(404, _)) => Ok(()),
      Err(error) => Err(backend_error("abort upload", error)),
    }
  }

  fn begin_upload(&self, key: &str, content_type: &str) -> Result<String> {
    validate_content_type(content_type)?;
    let key = self.object_key(key)?;
    let response = self
      .bucket
      .initiate_multipart_upload(&key, content_type)
      .map_err(|error| backend_error("begin upload", error))?;
    validate_upload_id(&response.upload_id)?;
    Ok(response.upload_id)
  }

  fn presign_part(
    &self,
    key: &str,
    upload_id: &str,
    part_number: u32,
    expires_secs: u32,
  ) -> Result<String> {
    validate_upload_id(upload_id)?;
    validate_part_number(part_number)?;
    validate_expiry(expires_secs)?;
    let key = self.object_key(key)?;
    let queries = HashMap::from([
      ("uploadId".into(), upload_id.into()),
      ("partNumber".into(), part_number.to_string()),
    ]);
    let mut url = self
      .bucket
      .presign_put(key, expires_secs, None, Some(queries.clone()))
      .map_err(|error| backend_error("presign upload part", error))?;
    // Version 0.37.2's sync API signs custom queries but omits them from its returned URL.
    // Use its own encoding to restore exactly those already-signed fields.
    url.push_str(
      &s3::signing::flatten_queries(Some(&queries))
        .map_err(|error| backend_error("presign upload part", error))?,
    );
    Ok(url)
  }

  fn complete_upload(
    &self,
    key: &str,
    upload_id: &str,
    parts: &[CompletedPart],
  ) -> Result<ObjectMetadata> {
    validate_upload_id(upload_id)?;
    let object_key = self.object_key(key)?;
    let parts = completion_parts(parts)?;
    // The client inserts this value directly into the URL before signing the request.
    // Preserve opaque IDs by using its query-component encoding first.
    let encoded_upload_id = s3::signing::uri_encode(upload_id, true);
    let response = self
      .bucket
      .complete_multipart_upload(&object_key, &encoded_upload_id, parts)
      .map_err(|error| backend_error("complete upload", error))?;
    check_status("complete upload", response.status_code())?;
    // S3 may return an XML Error with HTTP 200 after accepting the completion request.
    validate_completion(response.as_slice())?;
    self
      .head(key)?
      .ok_or_else(|| invalid("completed S3 object is missing"))
  }

  fn head(&self, key: &str) -> Result<Option<ObjectMetadata>> {
    let key = self.object_key(key)?;
    let (metadata, status) = self
      .bucket
      .head_object(key)
      .map_err(|error| backend_error("head object", error))?;
    if status == 404 {
      return Ok(None);
    }
    check_status("head object", status)?;
    let size = metadata
      .content_length
      .and_then(|size| u64::try_from(size).ok())
      .ok_or_else(|| invalid("S3 object response has no valid content length"))?;
    Ok(Some(ObjectMetadata { size }))
  }

  fn presign_get(&self, key: &str, expires_secs: u32) -> Result<String> {
    validate_expiry(expires_secs)?;
    let key = self.object_key(key)?;
    self
      .bucket
      .presign_get(key, expires_secs, None)
      .map_err(|error| backend_error("presign download", error))
  }

  fn presign_get_attachment(
    &self,
    key: &str,
    expires_secs: u32,
    disposition: &str,
  ) -> Result<String> {
    validate_expiry(expires_secs)?;
    if disposition.len() > 8192 || disposition.chars().any(char::is_control) {
      return Err(invalid("invalid attachment disposition"));
    }
    let key = self.object_key(key)?;
    let queries = std::collections::HashMap::from([
      ("response-content-disposition".into(), disposition.into()),
      (
        "response-content-type".into(),
        "application/octet-stream".into(),
      ),
    ]);
    self
      .bucket
      .presign_get(key, expires_secs, Some(queries))
      .map_err(|error| backend_error("presign download", error))
  }
}

fn validate_config(config: &S3Config) -> Result<()> {
  let bucket = config.bucket.as_bytes();
  if !(3..=63).contains(&bucket.len())
    || !bucket[0].is_ascii_alphanumeric()
    || !bucket[bucket.len() - 1].is_ascii_alphanumeric()
    || !bucket
      .iter()
      .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(byte))
    || config.bucket.contains("..")
  {
    return Err(invalid("invalid S3 bucket name"));
  }
  if config.region.is_empty()
    || config.region.len() > 64
    || !config
      .region
      .bytes()
      .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
  {
    return Err(invalid("invalid S3 region"));
  }
  if let Some(endpoint) = &config.endpoint {
    let uri: http::Uri = endpoint
      .parse()
      .map_err(|_| invalid("invalid S3 endpoint"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
      || uri.authority().is_none()
      || uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
      || !matches!(uri.path(), "" | "/")
      || uri.query().is_some()
      || endpoint.contains('#')
    {
      return Err(invalid(
        "S3 endpoint must be an HTTP(S) origin without credentials, path, query, or fragment",
      ));
    }
  }
  let prefix = config.prefix.trim_matches('/');
  if prefix.len() > 512 {
    return Err(invalid("S3 prefix exceeds 512 bytes"));
  }
  if !prefix.is_empty() {
    validate_key(prefix)?;
  }
  Ok(())
}

fn validate_key(key: &str) -> Result<()> {
  if key.is_empty()
    || key.len() > 1024
    || key.contains('\\')
    || key.chars().any(char::is_control)
    || key
      .split('/')
      .any(|part| part.is_empty() || part == "." || part == "..")
  {
    return Err(invalid("invalid S3 object key"));
  }
  Ok(())
}

fn validate_content_type(content_type: &str) -> Result<()> {
  if content_type.is_empty()
    || content_type.len() > 256
    || http::HeaderValue::from_str(content_type).is_err()
  {
    return Err(invalid("invalid S3 content type"));
  }
  Ok(())
}

fn validate_upload_id(upload_id: &str) -> Result<()> {
  if upload_id.is_empty() || upload_id.len() > 4096 || upload_id.chars().any(char::is_control) {
    return Err(invalid("invalid S3 upload ID"));
  }
  Ok(())
}

fn validate_part_number(part_number: u32) -> Result<()> {
  if !(1..=10_000).contains(&part_number) {
    return Err(invalid("S3 part number must be between 1 and 10000"));
  }
  Ok(())
}

fn validate_expiry(expires_secs: u32) -> Result<()> {
  if !(1..=MAX_PRESIGN_EXPIRY_SECS).contains(&expires_secs) {
    return Err(invalid(
      "S3 presigned URL expiry must be between 1 and 3600 seconds",
    ));
  }
  Ok(())
}

fn completion_parts(parts: &[CompletedPart]) -> Result<Vec<s3::serde_types::Part>> {
  if parts.is_empty() || parts.len() > 10_000 {
    return Err(invalid("S3 completion requires between 1 and 10000 parts"));
  }
  let mut result = Vec::with_capacity(parts.len());
  for part in parts {
    validate_part_number(part.part_number)?;
    if part.etag.is_empty() || part.etag.len() > 1024 || part.etag.chars().any(char::is_control) {
      return Err(invalid("invalid S3 part ETag"));
    }
    // The library formats this value as XML text. Escaping preserves its opaque value at S3.
    let etag = part
      .etag
      .replace('&', "&amp;")
      .replace('<', "&lt;")
      .replace('>', "&gt;");
    result.push(s3::serde_types::Part {
      part_number: part.part_number,
      etag,
    });
  }
  result.sort_by_key(|part| part.part_number);
  if result
    .windows(2)
    .any(|pair| pair[0].part_number == pair[1].part_number)
  {
    return Err(invalid("duplicate S3 part number"));
  }
  Ok(result)
}

fn validate_completion(content: &[u8]) -> Result<()> {
  let failure = || invalid("S3 completion did not return a valid success response");
  if content.len() > 64 * 1024 {
    return Err(failure());
  }
  let mut reader = Reader::from_reader(content);
  reader.config_mut().trim_text(true);
  let mut depth = 0usize;
  let mut root_seen = false;
  loop {
    match reader.read_event().map_err(|_| failure())? {
      Event::Start(element) => {
        if depth == 0 {
          if root_seen || element.local_name().as_ref() != b"CompleteMultipartUploadResult" {
            return Err(failure());
          }
          root_seen = true;
        }
        depth += 1;
        if depth > 16 {
          return Err(failure());
        }
      }
      Event::End(_) => depth = depth.checked_sub(1).ok_or_else(failure)?,
      Event::Eof => break,
      Event::DocType(_) => return Err(failure()),
      Event::Empty(_) | Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if depth == 0 => {
        return Err(failure());
      }
      _ => {}
    }
  }
  if !root_seen || depth != 0 {
    return Err(failure());
  }
  Ok(())
}

fn check_status(operation: &str, status: u16) -> Result<()> {
  if (200..300).contains(&status) {
    Ok(())
  } else {
    Err(status_error(operation, status))
  }
}

fn status_error(operation: &str, status: u16) -> ExpriError {
  invalid(format!("S3 {operation} failed (HTTP {status})"))
}

fn backend_error(operation: &str, error: S3Error) -> ExpriError {
  // Library errors may contain signed URLs, credentials, or backend response bodies.
  match error {
    S3Error::HttpFailWithBody(status, _) => status_error(operation, status),
    _ => invalid(format!("S3 {operation} request failed")),
  }
}

fn invalid(message: impl Into<String>) -> ExpriError {
  ExpriError::Message(message.into())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::BTreeMap;

  fn accept_peer(listener: &std::net::TcpListener) -> std::net::TcpStream {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let (stream, _) = loop {
      match listener.accept() {
        Ok(connection) => break connection,
        Err(error)
          if error.kind() == std::io::ErrorKind::WouldBlock
            && std::time::Instant::now() < deadline =>
        {
          std::thread::sleep(Duration::from_millis(10))
        }
        Err(error) => panic!("test peer did not receive a connection: {error}"),
      }
    };
    stream.set_nonblocking(false).unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();
    stream
      .set_write_timeout(Some(Duration::from_secs(5)))
      .unwrap();
    stream
  }

  fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    loop {
      assert!(bytes.len() < 16 * 1024, "fixture request exceeds limit");
      let mut chunk = [0; 1024];
      let count = stream.read(&mut chunk).unwrap();
      assert_ne!(count, 0, "fixture request ended early");
      bytes.extend_from_slice(&chunk[..count]);
      if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
        let header = std::str::from_utf8(&bytes[..end]).unwrap();
        let length: usize = header
          .lines()
          .skip(1)
          .filter_map(|line| line.split_once(':'))
          .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
          .map(|(_, length)| length.trim().parse().unwrap())
          .unwrap_or(0);
        assert!(length < 8 * 1024);
        if bytes.len() >= end + 4 + length {
          return String::from_utf8(bytes).unwrap();
        }
      }
    }
  }

  fn config() -> S3Config {
    S3Config {
      endpoint: Some("http://127.0.0.1:9000/".into()),
      bucket: "test-bucket".into(),
      region: "us-east-1".into(),
      path_style: true,
      prefix: "/expri/".into(),
    }
  }

  fn storage(config: S3Config) -> S3Storage {
    let credentials = Credentials::new(
      Some("test-access"),
      Some("test-secret"),
      None,
      Some("test-session"),
      None,
    )
    .unwrap();
    S3Storage::with_credentials(config, credentials).unwrap()
  }

  #[test]
  fn presigned_parts_bind_object_upload_and_part_without_contacting_s3() {
    let storage = storage(config());
    let url = storage
      .presign_part("runs/a/checkpoint step+1.pt", "id+/=token", 7, 60)
      .unwrap();
    let uri: http::Uri = url.parse().unwrap();
    assert_eq!(uri.scheme_str(), Some("http"));
    assert_eq!(uri.authority().unwrap().as_str(), "127.0.0.1:9000");
    assert!(
      uri
        .path()
        .starts_with("/test-bucket/expri/runs/a/checkpoint")
    );
    assert!(uri.path().contains("%20"));
    let pairs: Vec<_> = form_urlencoded::parse(uri.query().unwrap().as_bytes())
      .into_owned()
      .collect();
    for name in ["uploadId", "partNumber", "X-Amz-Signature"] {
      assert_eq!(pairs.iter().filter(|(key, _)| key == name).count(), 1);
    }
    let query: BTreeMap<_, _> = pairs.into_iter().collect();
    assert_eq!(query["uploadId"], "id+/=token");
    assert_eq!(query["partNumber"], "7");
    assert_eq!(query["X-Amz-Expires"], "60");
    assert_eq!(query["X-Amz-Algorithm"], "AWS4-HMAC-SHA256");
    assert_eq!(query["X-Amz-Security-Token"], "test-session");
    assert_eq!(query["X-Amz-Signature"].len(), 64);
    assert!(!url.contains("test-secret"));
  }

  #[test]
  fn aws_endpoints_and_virtual_hosting_work_without_custom_endpoint() {
    let mut config = config();
    config.endpoint = None;
    config.path_style = false;
    let storage = storage(config);
    let url = storage.presign_get("runs/a/model.pt", 60).unwrap();
    let uri: http::Uri = url.parse().unwrap();
    assert_eq!(uri.scheme_str(), Some("https"));
    assert!(
      uri
        .authority()
        .unwrap()
        .host()
        .starts_with("test-bucket.s3")
    );
    assert_eq!(uri.path(), "/expri/runs/a/model.pt");
  }

  #[test]
  fn attachment_presigns_include_header_overrides_in_the_signature() {
    let storage = storage(config());
    let disposition = crate::dashboard::artifacts::disposition("model \"é.pt");
    let url = storage
      .presign_get_attachment("runs/a/model.pt", 60, &disposition)
      .unwrap();
    let parsed = reqwest::Url::parse(&url).unwrap();
    let query: BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(query["response-content-disposition"], disposition);
    assert_eq!(query["response-content-type"], "application/octet-stream");
    assert_eq!(query["X-Amz-Expires"], "60");
    assert_eq!(query["X-Amz-Signature"].len(), 64);
    assert!(!disposition.contains('é'));
    assert!(!disposition.contains("filename=\"model \""));
    assert!(disposition.contains("%22%C3%A9.pt"));
    assert!(
      storage
        .presign_get_attachment("runs/a/model.pt", 60, "attachment\r\nX-Test: unsafe")
        .is_err()
    );
  }

  #[test]
  fn custom_endpoints_normalize_default_ports_before_signing() {
    for (endpoint, authority) in [
      ("http://localhost:80", "localhost"),
      ("https://localhost:443/", "localhost"),
      ("http://localhost:9000", "localhost:9000"),
      ("https://[::1]:443", "[::1]"),
      ("http://[::1]:9000", "[::1]:9000"),
    ] {
      let mut config = config();
      config.endpoint = Some(endpoint.into());
      let storage = storage(config);
      assert_eq!(storage.bucket.host(), authority);
      let url = storage.presign_get("runs/a/key", 60).unwrap();
      let uri: http::Uri = url.parse().unwrap();
      assert_eq!(uri.authority().unwrap().as_str(), authority);
    }
  }

  #[test]
  fn unsafe_endpoints_and_keys_are_rejected_without_echoing_secrets() {
    for endpoint in [
      "ftp://host",
      "http://user:secret@host",
      "http://host/path",
      "http://host?token=secret",
      "http://host#secret",
    ] {
      let mut config = config();
      config.endpoint = Some(endpoint.into());
      let error = validate_config(&config).unwrap_err().to_string();
      assert!(!error.contains("secret"));
    }
    let storage = storage(config());
    for key in [
      "",
      "/run",
      "run/../key",
      "run//key",
      "run\\key",
      "run/\0key",
    ] {
      assert!(storage.presign_get(key, 60).is_err(), "{key:?}");
    }
    assert_eq!(
      storage.object_key("runs/a/模型.pt").unwrap(),
      "expri/runs/a/模型.pt"
    );
    assert!(storage.object_key(&"a".repeat(1024)).is_err());
  }

  #[test]
  fn prefix_limit_leaves_room_for_generated_object_keys() {
    let mut config = config();
    config.prefix = format!("/{}/", "a".repeat(512));
    let storage = storage(config.clone());
    assert!(storage.object_key(&"a".repeat(480)).is_ok());
    config.prefix = "a".repeat(513);
    assert_eq!(
      validate_config(&config).unwrap_err().to_string(),
      "S3 prefix exceeds 512 bytes"
    );
    config.prefix = "模".repeat(171);
    assert!(validate_config(&config).is_err());
  }

  #[test]
  fn expiry_and_part_bounds_are_enforced_before_signing() {
    let storage = storage(config());
    for expiry in [0, MAX_PRESIGN_EXPIRY_SECS + 1] {
      assert!(storage.presign_get("run/key", expiry).is_err());
    }
    for part in [0, 10_001] {
      assert!(storage.presign_part("run/key", "upload", part, 60).is_err());
    }
    assert!(storage.presign_part("run/key", "", 1, 60).is_err());
    assert!(storage.presign_part("run/key", "id\n", 1, 60).is_err());
    assert!(
      storage
        .presign_get("run/key", MAX_PRESIGN_EXPIRY_SECS)
        .is_ok()
    );
  }

  #[test]
  fn opaque_etags_are_preserved_when_encoded_for_xml_and_parts_are_ordered() {
    let etag = "\"opaque<&>value\"";
    let parts = [
      CompletedPart {
        part_number: 2,
        etag: etag.into(),
      },
      CompletedPart {
        part_number: 1,
        etag: "first".into(),
      },
    ];
    let encoded = completion_parts(&parts).unwrap();
    assert_eq!(encoded[0].part_number, 1);
    assert_eq!(encoded[1].etag, "\"opaque&lt;&amp;&gt;value\"");
    assert_eq!(parts[0].etag, etag);
    assert!(completion_parts(&[]).is_err());
    assert!(completion_parts(&[parts[0].clone(), parts[0].clone()]).is_err());
  }

  #[test]
  fn completion_requires_success_xml_even_when_http_succeeded() {
    for response in [
      b"<Error><Code>InvalidPart</Code></Error>".as_slice(),
      b"<CompleteMultipartUploadResult><Key>x</CompleteMultipartUploadResult>",
      b"<CompleteMultipartUploadResult></CompleteMultipartUploadResult><Error/>",
      b"<!DOCTYPE root><CompleteMultipartUploadResult></CompleteMultipartUploadResult>",
      b"",
      b"not xml",
    ] {
      assert!(validate_completion(response).is_err());
    }
    assert!(validate_completion(b" \n<CompleteMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><ETag>\"opaque\"</ETag></CompleteMultipartUploadResult>\n ").is_ok());
    assert!(validate_completion(b"<s3:CompleteMultipartUploadResult xmlns:s3=\"http://s3.amazonaws.com/doc/2006-03-01/\"><s3:ETag>opaque</s3:ETag></s3:CompleteMultipartUploadResult>").is_ok());
  }

  #[test]
  fn backend_errors_hide_body_and_url_secrets() {
    let body = "secret http://s3/key?X-Amz-Signature=private".into();
    let error = backend_error("complete upload", S3Error::HttpFailWithBody(403, body)).to_string();
    assert_eq!(error, "S3 complete upload failed (HTTP 403)");
    let error = backend_error(
      "download",
      S3Error::Io(std::io::Error::other("signed secret URL")),
    )
    .to_string();
    assert_eq!(error, "S3 download request failed");
  }

  #[test]
  fn cleanup_requests_sign_exact_object_versions_and_opaque_multipart_ids() {
    use std::{io::Write, net::TcpListener, thread};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
      let responses = [
        (
          200,
          "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
        ),
        (
          200,
          "<ListVersionsResult><Prefix>expri/runs/a/key</Prefix><IsTruncated>false</IsTruncated><Version><Key>expri/runs/a/key</Key><VersionId>v+/=&amp;?#</VersionId></Version><DeleteMarker><Key>expri/runs/a/key</Key><VersionId>marker</VersionId></DeleteMarker><Version><Key>expri/runs/a/key-neighbor</Key><VersionId>neighbor</VersionId></Version></ListVersionsResult>",
        ),
        (204, ""),
        (
          404,
          "<Error><Code>NoSuchVersion</Code><Message>secret</Message></Error>",
        ),
        (
          404,
          "<Error><Code>NoSuchUpload</Code><Message>secret</Message></Error>",
        ),
      ];
      for (index, (status, body)) in responses.into_iter().enumerate() {
        let mut stream = accept_peer(&listener);
        let request = read_http_request(&mut stream);
        let fields: Vec<_> = request.lines().next().unwrap().split_whitespace().collect();
        let uri: http::Uri = fields[1].parse().unwrap();
        let query: BTreeMap<_, _> = form_urlencoded::parse(uri.query().unwrap().as_bytes())
          .into_owned()
          .collect();
        assert!(
          request
            .to_ascii_lowercase()
            .contains("authorization: aws4-hmac-sha256")
        );
        assert!(
          request
            .to_ascii_lowercase()
            .contains("x-amz-security-token: test-session")
        );
        assert!(!request.contains("test-secret"));
        match index {
          0 => {
            assert_eq!(fields[0], "GET");
            assert!(query.contains_key("versioning"));
          }
          1 => {
            assert_eq!(fields[0], "GET");
            assert_eq!(query["prefix"], "expri/runs/a/key");
            assert_eq!(query["max-keys"], "16");
            assert!(query.contains_key("versions"));
          }
          2 | 3 => {
            assert_eq!(fields[0], "DELETE");
            assert_eq!(uri.path(), "/test-bucket/expri/runs/a/key");
            assert_eq!(
              query["versionId"],
              if index == 2 { "v+/=&?#" } else { "marker" }
            );
          }
          _ => {
            assert_eq!(fields[0], "DELETE");
            assert_eq!(uri.path(), "/test-bucket/expri/runs/a/key");
            assert_eq!(query["uploadId"], "id+/=token&next?#");
          }
        }
        write!(
          stream,
          "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
          body.len()
        )
        .unwrap();
      }
    });
    let mut config = config();
    config.endpoint = Some(format!("http://{address}"));
    let storage = storage(config);
    storage.delete_object("runs/a/key").unwrap();
    storage
      .abort_upload("runs/a/key", "id+/=token&next?#")
      .unwrap();
    peer.join().unwrap();
  }

  #[test]
  fn cleanup_does_not_fallback_when_versioning_or_version_listing_is_unavailable() {
    use std::{io::Write, net::TcpListener, thread};
    for fail_listing in [false, true] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      listener.set_nonblocking(true).unwrap();
      let address = listener.local_addr().unwrap();
      let peer = thread::spawn(move || {
        if fail_listing {
          let mut stream = accept_peer(&listener);
          assert!(read_http_request(&mut stream).starts_with("GET "));
          let body =
            "<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>";
          write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
          )
          .unwrap();
        }
        let mut stream = accept_peer(&listener);
        assert!(read_http_request(&mut stream).starts_with("GET "));
        let body = "<Error><Code>AccessDenied</Code><Message>signed-secret</Message></Error>";
        write!(
          stream,
          "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
          body.len()
        )
        .unwrap();
      });
      let mut config = config();
      config.endpoint = Some(format!("http://{address}"));
      let result = storage(config)
        .delete_object("runs/a/key")
        .unwrap_err()
        .to_string();
      assert!(result.contains("403"));
      assert!(!result.contains("signed-secret"));
      peer.join().unwrap();
    }
  }

  #[test]
  fn version_cleanup_leaves_truncated_work_pending_and_resumes_remaining_versions() {
    use std::{io::Write, net::TcpListener, thread};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
      for version in ["first", "second"] {
        let mut stream = accept_peer(&listener);
        let request = read_http_request(&mut stream);
        assert!(request.starts_with("GET ") && request.contains("versioning"));
        let body = "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>";
        write!(
          stream,
          "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
          body.len()
        )
        .unwrap();
        drop(stream);
        let mut stream = accept_peer(&listener);
        assert!(read_http_request(&mut stream).starts_with("GET "));
        let markers = if version == "first" {
          "<NextKeyMarker>expri/runs/a/key</NextKeyMarker><NextVersionIdMarker>first</NextVersionIdMarker>"
        } else {
          ""
        };
        let body = format!(
          "<ListVersionsResult><Prefix>expri/runs/a/key</Prefix><IsTruncated>{}</IsTruncated>{markers}<Version><Key>expri/runs/a/key</Key><VersionId>{version}</VersionId></Version></ListVersionsResult>",
          version == "first"
        );
        write!(
          stream,
          "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
          body.len()
        )
        .unwrap();
        drop(stream);
        let mut stream = accept_peer(&listener);
        let request = read_http_request(&mut stream);
        assert!(request.starts_with("DELETE "));
        let uri: http::Uri = request
          .lines()
          .next()
          .unwrap()
          .split_whitespace()
          .nth(1)
          .unwrap()
          .parse()
          .unwrap();
        let query: BTreeMap<_, _> = form_urlencoded::parse(uri.query().unwrap().as_bytes())
          .into_owned()
          .collect();
        assert_eq!(query["versionId"], version);
        stream
          .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
          .unwrap();
      }
    });
    let mut config = config();
    config.endpoint = Some(format!("http://{address}"));
    let storage = storage(config);
    assert!(
      storage
        .delete_object("runs/a/key")
        .unwrap_err()
        .to_string()
        .contains("cleanup will retry")
    );
    storage.delete_object("runs/a/key").unwrap();
    peer.join().unwrap();
  }

  #[test]
  fn version_cleanup_ignores_prefix_neighbors_and_fails_closed_on_invalid_metadata() {
    use std::{io::Write, net::TcpListener, thread};
    for case in [
      "neighbor",
      "wrong-prefix",
      "foreign-key",
      "unknown-status",
      "malformed",
    ] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      listener.set_nonblocking(true).unwrap();
      let address = listener.local_addr().unwrap();
      let peer = thread::spawn(move || {
        let body = match case {
          "unknown-status" => {
            "<VersioningConfiguration><Status>Unknown</Status></VersioningConfiguration>"
          }
          "malformed" => "<Error><Message>signed-secret</Message></Error>",
          _ => "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
        };
        let mut stream = accept_peer(&listener);
        assert!(read_http_request(&mut stream).starts_with("GET "));
        write!(
          stream,
          "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
          body.len()
        )
        .unwrap();
        drop(stream);
        if !matches!(case, "unknown-status" | "malformed") {
          let mut stream = accept_peer(&listener);
          assert!(read_http_request(&mut stream).starts_with("GET "));
          let body = match case {
            "neighbor" => {
              "<ListVersionsResult><Prefix>expri/runs/a/key</Prefix><IsTruncated>true</IsTruncated><NextKeyMarker>expri/runs/a/key-neighbor</NextKeyMarker><NextVersionIdMarker>v1</NextVersionIdMarker><Version><Key>expri/runs/a/key-neighbor</Key><VersionId>v1</VersionId></Version></ListVersionsResult>"
            }
            "wrong-prefix" => {
              "<ListVersionsResult><Prefix>foreign/</Prefix><IsTruncated>false</IsTruncated><Version><Key>foreign/key</Key><VersionId>v1</VersionId></Version></ListVersionsResult>"
            }
            _ => {
              "<ListVersionsResult><Prefix>expri/runs/a/key</Prefix><IsTruncated>false</IsTruncated><Version><Key>foreign/key</Key><VersionId>v1</VersionId></Version></ListVersionsResult>"
            }
          };
          write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
          )
          .unwrap();
        }
      });
      let mut config = config();
      config.endpoint = Some(format!("http://{address}"));
      let result = storage(config).delete_object("runs/a/key");
      if case == "neighbor" {
        assert!(result.is_ok());
      } else {
        assert!(!result.unwrap_err().to_string().contains("signed-secret"));
      }
      peer.join().unwrap();
    }
  }

  #[test]
  fn unversioned_cleanup_and_missing_uploads_are_idempotent() {
    use std::{io::Write, net::TcpListener, thread};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
      for (index, (status, body)) in [
        (200, "<VersioningConfiguration/>"),
        (404, ""),
        (403, "secret"),
      ]
      .into_iter()
      .enumerate()
      {
        let mut stream = accept_peer(&listener);
        let request = read_http_request(&mut stream);
        assert!(request.starts_with(if index == 0 { "GET " } else { "DELETE " }));
        write!(
          stream,
          "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
          body.len()
        )
        .unwrap();
      }
    });
    let mut config = config();
    config.endpoint = Some(format!("http://{address}"));
    let storage = storage(config);
    storage.delete_object("runs/a/key").unwrap();
    let error = storage
      .abort_upload("runs/a/key", "upload")
      .unwrap_err()
      .to_string();
    assert_eq!(error, "S3 abort upload failed (HTTP 403)");
    peer.join().unwrap();
  }

  #[test]
  fn native_completion_preserves_opaque_upload_ids_in_the_http_request() {
    use std::{io::Write, net::TcpListener, thread};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
      let mut stream = accept_peer(&listener);
      let request = read_http_request(&mut stream);
      let line = request.lines().next().unwrap();
      let mut parts = line.split_whitespace();
      assert_eq!(parts.next(), Some("POST"));
      let uri: http::Uri = parts.next().unwrap().parse().unwrap();
      let queries: BTreeMap<_, _> = form_urlencoded::parse(uri.query().unwrap().as_bytes())
        .into_owned()
        .collect();
      assert_eq!(queries["uploadId"], "id+/=token&next?#");
      let body =
        "<CompleteMultipartUploadResult><ETag>opaque</ETag></CompleteMultipartUploadResult>";
      write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
      )
      .unwrap();
      drop(stream);
      let mut stream = accept_peer(&listener);
      assert!(read_http_request(&mut stream).starts_with("HEAD "));
      stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\nConnection: close\r\n\r\n")
        .unwrap();
    });
    let mut config = config();
    config.endpoint = Some(format!("http://{address}"));
    let storage = storage(config);
    let result = storage.complete_upload(
      "runs/a/key",
      "id+/=token&next?#",
      &[CompletedPart {
        part_number: 1,
        etag: "opaque".into(),
      }],
    );
    peer.join().unwrap();
    assert_eq!(result.unwrap().size, 17);
  }

  #[test]
  fn https_transport_sends_a_tls_client_hello_without_provider_panics() {
    use std::{io::Read, net::TcpListener, thread};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let peer = thread::spawn(move || {
      let mut stream = accept_peer(&listener);
      let mut record = [0; 3];
      stream.read_exact(&mut record).unwrap();
      assert_eq!(record[0], 0x16, "TLS handshake record");
      assert_eq!(record[1], 0x03, "TLS record version");
      // Closing the peer avoids certificates or a public network dependency.
    });
    let mut config = config();
    config.endpoint = Some(format!("https://{address}"));
    let storage = storage(config);
    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| storage.head("runs/a/key")));
    peer.join().unwrap();
    assert!(result.is_ok(), "TLS provider setup must not panic");
    assert!(
      result.unwrap().is_err(),
      "peer deliberately closes before completing TLS"
    );
  }
}
