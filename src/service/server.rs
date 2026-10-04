use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use http::{HeaderName, HeaderValue, Request as HttpRequest, Response as HttpResponse};
use serde_json::json;
use subtle::ConstantTimeEq;

use super::browser_auth::BrowserAuth;
use super::dashboard_data::HostedDashboard;
use super::storage::{ObjectStorage, S3Storage};
use super::store::{ApiError, ApiResult, Store};
use super::types::{self, FileTarget, Request, ServerConfig, WorkerAuth};
use crate::error::{ExpriError, Result};

const HEADER_LIMIT: usize = 16 * 1024;
const WORKERS: usize = 4;
const QUEUE: usize = 16;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

struct Auth {
  owner: Vec<u8>,
  workers: Vec<(Vec<u8>, WorkerAuth)>,
  browser: Option<BrowserAuth>,
}

impl Auth {
  fn new(config: &ServerConfig) -> Result<Self> {
    let owner = token(&config.owner_token_env)?;
    let mut workers = Vec::new();
    for worker in &config.workers {
      types::validate_component(&worker.project_id)?;
      types::validate_component(&worker.origin)?;
      let value = token(&worker.token_env)?;
      if constant_eq(&value, &owner)
        || workers
          .iter()
          .any(|(previous, _): &(Vec<u8>, WorkerAuth)| constant_eq(&value, previous))
      {
        return Err(ExpriError::Message(
          "service tokens must be distinct for each role and worker".into(),
        ));
      }
      workers.push((value, worker.clone()));
    }
    let browser = config
      .dashboard
      .as_ref()
      .map(|dashboard| {
        let password = std::env::var(&dashboard.password_env).map_err(|_| {
          ExpriError::Message(format!(
            "dashboard password environment variable is missing: {}",
            dashboard.password_env
          ))
        })?;
        if !(16..=256).contains(&password.len())
          || !password.bytes().all(|byte| (32..=126).contains(&byte))
        {
          return Err(ExpriError::Message(
            "dashboard password must contain 16 to 256 printable ASCII bytes".into(),
          ));
        }
        if constant_eq(password.as_bytes(), &owner)
          || workers
            .iter()
            .any(|(token, _)| constant_eq(password.as_bytes(), token))
        {
          return Err(ExpriError::Message(
            "dashboard password must be distinct from service bearer tokens".into(),
          ));
        }
        BrowserAuth::new(&dashboard.public_url, password.as_bytes())
      })
      .transpose()?;
    Ok(Self {
      owner,
      workers,
      browser,
    })
  }

  fn role(&self, request: &HttpRequest<Vec<u8>>) -> ApiResult<Option<&WorkerAuth>> {
    let mut headers = request.headers().get_all("authorization").iter();
    let header = headers
      .next()
      .and_then(|value| value.to_str().ok())
      .and_then(|value| value.strip_prefix("Bearer "));
    if headers.next().is_some() {
      return Err(ApiError::new(401, "bearer authentication required"));
    }
    let supplied = header
      .ok_or_else(|| ApiError::new(401, "bearer authentication required"))?
      .as_bytes();
    if constant_eq(supplied, &self.owner) {
      return Ok(None);
    }
    self
      .workers
      .iter()
      .find(|(token, _)| constant_eq(supplied, token))
      .map(|(_, worker)| Some(worker))
      .ok_or_else(|| ApiError::new(401, "bearer authentication required"))
  }
}

fn token(name: &str) -> Result<Vec<u8>> {
  let value = std::env::var(name).map_err(|_| {
    ExpriError::Message(format!(
      "service token environment variable is missing: {name}"
    ))
  })?;
  if !(24..=256).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
    return Err(ExpriError::Message(
      "service tokens must contain 24 to 256 printable ASCII bytes without spaces".into(),
    ));
  }
  Ok(value.into_bytes())
}

fn constant_eq(left: &[u8], right: &[u8]) -> bool {
  bool::from(left.ct_eq(right))
}

fn authorize<S: ObjectStorage>(
  store: &Store<S>,
  worker: Option<&WorkerAuth>,
  request: &Request,
) -> ApiResult<()> {
  let Some(worker) = worker else {
    return Ok(());
  };
  let allowed = match request {
    Request::BeginUpload { target, .. } => allowed_target(worker, target, false),
    Request::PartUrl { upload_id, .. }
    | Request::RecordPart { upload_id, .. }
    | Request::CompleteUpload { upload_id } => {
      allowed_target(worker, &store.upload_target(upload_id)?, false)
    }
    Request::ListFiles { scope }
    | Request::AppendStream { scope, .. }
    | Request::ReadStream { scope, .. } => {
      worker.project_id == scope.project_id && worker.origin == scope.origin
    }
    Request::ListRuns { project_id, origin } => {
      worker.project_id == *project_id && worker.origin == *origin
    }
    Request::GetFile { target } | Request::DownloadUrl { target } => {
      allowed_target(worker, target, true)
    }
  };
  if allowed {
    Ok(())
  } else {
    Err(ApiError::new(
      403,
      "worker token is outside this project or origin",
    ))
  }
}

fn allowed_target(worker: &WorkerAuth, target: &FileTarget, read_input: bool) -> bool {
  match target {
    FileTarget::Run { scope, .. } => {
      worker.project_id == scope.project_id && worker.origin == scope.origin
    }
    FileTarget::Input { project_id, .. } => read_input && worker.project_id == *project_id,
  }
}

pub fn serve(
  config: ServerConfig,
  listen: SocketAddr,
  data_dir: PathBuf,
  create_bucket: bool,
) -> Result<()> {
  let auth = Auth::new(&config)?;
  let storage = S3Storage::new(config.storage)?;
  if create_bucket {
    storage.create_bucket()?;
  }
  let store = Store::open(&data_dir, storage)?;
  let dashboard = auth
    .browser
    .as_ref()
    .map(|_| HostedDashboard::new(&store))
    .transpose()?;
  let listener = TcpListener::bind(listen)?;
  let address = listener.local_addr()?;
  println!("Service: http://{address}");
  std::io::stdout().flush()?;
  let (sender, receiver) = mpsc::sync_channel::<TcpStream>(QUEUE);
  let receiver = Mutex::new(receiver);
  std::thread::scope(|scope| {
    for _ in 0..WORKERS {
      let receiver = &receiver;
      let auth = &auth;
      let store = &store;
      let dashboard = dashboard.as_ref();
      scope.spawn(move || {
        loop {
          let stream = receiver
            .lock()
            .expect("service connection queue lock")
            .recv();
          match stream {
            Ok(stream) => respond(store, auth, dashboard, stream),
            Err(_) => break,
          }
        }
      });
    }
    let result = loop {
      match listener.accept() {
        Ok((stream, _)) => match sender.try_send(stream) {
          Ok(()) => {}
          Err(mpsc::TrySendError::Full(stream)) => {
            let _ = stream.shutdown(Shutdown::Both);
          }
          Err(mpsc::TrySendError::Disconnected(_)) => {
            break Err(ExpriError::Message("service workers stopped".into()));
          }
        },
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
        Err(error) => break Err(error.into()),
      }
    };
    drop(sender);
    result
  })
}

fn respond<S: ObjectStorage>(
  store: &Store<S>,
  auth: &Auth,
  dashboard: Option<&HostedDashboard<'_, S>>,
  mut stream: TcpStream,
) {
  let mut head = false;
  let reply = match read_request(&mut stream) {
    Ok(request) => {
      head = request.method() == "HEAD";
      if let Some(browser) = &auth.browser
        && !matches!(request.uri().path(), "/health" | "/v1/request")
      {
        super::browser::handle(
          dashboard.expect("configured browser dashboard"),
          browser,
          &request,
        )
      } else {
        api_reply(route(store, auth, request))
      }
    }
    Err(error) => api_reply(Err(error)),
  };
  let _ = write_response(&mut stream, reply, head);
  let _ = stream.shutdown(Shutdown::Both);
}

fn api_reply(reply: ApiResult<Vec<u8>>) -> HttpResponse<Vec<u8>> {
  let (status, body) = match reply {
    Ok(body) => (200, body),
    Err(error) => (
      error.status,
      serde_json::to_vec(&json!({"error": error.message})).unwrap_or_default(),
    ),
  };
  super::browser::response(status, "application/json", body)
}

fn route<S: ObjectStorage>(
  store: &Store<S>,
  auth: &Auth,
  request: HttpRequest<Vec<u8>>,
) -> ApiResult<Vec<u8>> {
  if request.uri().path() == "/health"
    && request.method() == "GET"
    && request.uri().query().is_none()
    && request.body().is_empty()
  {
    return Ok(br#"{"ok":true}"#.to_vec());
  }
  if request.uri().path() != "/v1/request" || request.uri().query().is_some() {
    return Err(ApiError::new(404, "service endpoint is missing"));
  }
  if request.method() != "POST" {
    return Err(ApiError::new(405, "service requests require POST"));
  }
  let worker = auth.role(&request)?;
  let mut content_types = request.headers().get_all("content-type").iter();
  if !content_types
    .next()
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| {
      value
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    })
    || content_types.next().is_some()
  {
    return Err(ApiError::new(
      415,
      "service requests require application/json",
    ));
  }
  let operation: Request = serde_json::from_slice(request.body())
    .map_err(|_| ApiError::new(400, "invalid service request"))?;
  authorize(store, worker, &operation)?;
  let response = store.execute(operation)?;
  let mut body = LimitedBytes(Vec::new());
  serde_json::to_writer(&mut body, &response).map_err(|_| {
    ApiError::new(
      413,
      "service response exceeds 1 MiB; use a smaller selection",
    )
  })?;
  Ok(body.0)
}

struct LimitedBytes(Vec<u8>);
impl Write for LimitedBytes {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    if bytes.len() > types::MAX_REQUEST.saturating_sub(self.0.len()) {
      return Err(std::io::Error::other("response limit"));
    }
    self.0.extend_from_slice(bytes);
    Ok(bytes.len())
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

fn read_request(stream: &mut TcpStream) -> ApiResult<HttpRequest<Vec<u8>>> {
  let deadline = Instant::now() + IO_TIMEOUT;
  let mut bytes = Vec::with_capacity(2048);
  let (mut request, header_length) = loop {
    if let Some(request) = parse_headers(&bytes)? {
      break request;
    }
    if bytes.len() == HEADER_LIMIT {
      return Err(ApiError::new(431, "request headers exceed 16 KiB"));
    }
    let mut buffer = [0_u8; 2048];
    let length = buffer.len().min(HEADER_LIMIT - bytes.len());
    let count = read_deadline(stream, &mut buffer[..length], deadline)?;
    if count == 0 {
      return Err(ApiError::new(400, "incomplete request headers"));
    }
    bytes.extend_from_slice(&buffer[..count]);
  };
  let mut lengths = request.headers().get_all("content-length").iter();
  let length = lengths.next();
  if lengths.next().is_some() || request.headers().contains_key("transfer-encoding") {
    return Err(ApiError::new(400, "ambiguous request body framing"));
  }
  if request.headers().contains_key("expect") {
    return Err(ApiError::new(417, "Expect requests are unsupported"));
  }
  let body_length = match length {
    None => 0,
    Some(value) => {
      let value = value
        .to_str()
        .map_err(|_| ApiError::new(400, "invalid content length"))?;
      if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ApiError::new(400, "invalid content length"));
      }
      value
        .parse::<usize>()
        .map_err(|_| ApiError::new(413, "request body exceeds 1 MiB"))?
    }
  };
  if body_length > types::MAX_REQUEST {
    return Err(ApiError::new(413, "request body exceeds 1 MiB"));
  }
  let mut body = bytes.split_off(header_length);
  if body.len() > body_length {
    return Err(ApiError::new(400, "unexpected bytes after request body"));
  }
  while body.len() < body_length {
    let mut buffer = [0_u8; 8192];
    let length = buffer.len().min(body_length - body.len());
    let count = read_deadline(stream, &mut buffer[..length], deadline)?;
    if count == 0 {
      return Err(ApiError::new(400, "incomplete request body"));
    }
    body.extend_from_slice(&buffer[..count]);
  }
  *request.body_mut() = body;
  Ok(request)
}

fn read_deadline(stream: &mut TcpStream, bytes: &mut [u8], deadline: Instant) -> ApiResult<usize> {
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      return Err(ApiError::new(408, "request read timed out"));
    }
    stream
      .set_read_timeout(Some(remaining))
      .map_err(|_| ApiError::new(400, "could not read request"))?;
    match stream.read(bytes) {
      Ok(count) => return Ok(count),
      Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
      Err(error)
        if matches!(
          error.kind(),
          std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) =>
      {
        return Err(ApiError::new(408, "request read timed out"));
      }
      Err(_) => return Err(ApiError::new(400, "could not read request")),
    }
  }
}

fn parse_headers(bytes: &[u8]) -> ApiResult<Option<(HttpRequest<Vec<u8>>, usize)>> {
  let mut headers = [httparse::EMPTY_HEADER; 64];
  let mut parsed = httparse::Request::new(&mut headers);
  let length = match parsed.parse(bytes) {
    Ok(httparse::Status::Partial) => return Ok(None),
    Ok(httparse::Status::Complete(length)) => length,
    Err(httparse::Error::TooManyHeaders) => {
      return Err(ApiError::new(431, "request exceeds 64 headers"));
    }
    Err(_) => return Err(ApiError::new(400, "malformed request headers")),
  };
  if length > HEADER_LIMIT {
    return Err(ApiError::new(431, "request headers exceed 16 KiB"));
  }
  let invalid = || ApiError::new(400, "malformed request headers");
  let path = parsed.path.ok_or_else(invalid)?;
  if !path.starts_with('/') || path.starts_with("//") || path.len() > 8192 {
    return Err(invalid());
  }
  let mut request = HttpRequest::builder()
    .method(parsed.method.ok_or_else(invalid)?)
    .uri(path)
    .body(Vec::new())
    .map_err(|_| invalid())?;
  for header in parsed.headers {
    let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| invalid())?;
    let value = HeaderValue::from_bytes(header.value).map_err(|_| invalid())?;
    value.to_str().map_err(|_| invalid())?;
    request.headers_mut().append(name, value);
  }
  let mut hosts = request.headers().get_all("host").iter();
  if hosts.next().is_none_or(|value| value.is_empty()) || hosts.next().is_some() {
    return Err(invalid());
  }
  Ok(Some((request, length)))
}

fn write_response(
  stream: &mut TcpStream,
  reply: HttpResponse<Vec<u8>>,
  head: bool,
) -> std::io::Result<()> {
  let status = reply.status();
  let mut headers = format!(
    "HTTP/1.1 {} {}\r\n",
    status.as_u16(),
    status.canonical_reason().unwrap_or("Error")
  );
  for (name, value) in reply.headers() {
    headers.push_str(name.as_str());
    headers.push_str(": ");
    headers.push_str(value.to_str().expect("validated response header"));
    headers.push_str("\r\n");
  }
  headers.push_str("\r\n");
  let deadline = Instant::now() + IO_TIMEOUT;
  let body = if head {
    &[][..]
  } else {
    reply.body().as_slice()
  };
  for mut bytes in [headers.as_bytes(), body] {
    while !bytes.is_empty() {
      let remaining = deadline.saturating_duration_since(Instant::now());
      if remaining.is_zero() {
        return Err(std::io::Error::new(
          std::io::ErrorKind::TimedOut,
          "response write timed out",
        ));
      }
      stream.set_write_timeout(Some(remaining))?;
      match stream.write(bytes) {
        Ok(0) => {
          return Err(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "response write stopped",
          ));
        }
        Ok(count) => bytes = &bytes[count..],
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
        Err(error) => return Err(error),
      }
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::service::store::tests::{MockStorage, scope};
  use crate::service::types::{FileTarget, Response, RunScope, STREAM_BATCH};

  fn auth() -> Auth {
    Auth {
      owner: b"owner-token-with-at-least-24-characters".to_vec(),
      browser: None,
      workers: vec![(
        b"worker-token-with-at-least-24-characters".to_vec(),
        WorkerAuth {
          project_id: "project".into(),
          origin: "worker".into(),
          token_env: "unused".into(),
        },
      )],
    }
  }

  fn request(token: Option<&str>, operation: &Request) -> HttpRequest<Vec<u8>> {
    let mut request = HttpRequest::builder()
      .method("POST")
      .uri("/v1/request")
      .header("content-type", "application/json");
    if let Some(token) = token {
      request = request.header("authorization", format!("Bearer {token}"));
    }
    request
      .body(serde_json::to_vec(operation).unwrap())
      .unwrap()
  }

  #[test]
  fn bearer_roles_restrict_workers_to_origin_and_read_only_project_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let auth = auth();
    let list = Request::ListFiles { scope: scope() };
    assert_eq!(
      route(&store, &auth, request(None, &list))
        .unwrap_err()
        .status,
      401
    );
    assert_eq!(
      route(&store, &auth, request(Some("wrong"), &list))
        .unwrap_err()
        .status,
      401
    );
    let token = "worker-token-with-at-least-24-characters";
    let response: Response =
      serde_json::from_slice(&route(&store, &auth, request(Some(token), &list)).unwrap()).unwrap();
    assert!(matches!(response, Response::Files { files } if files.is_empty()));
    let foreign = RunScope {
      origin: "other-worker".into(),
      ..scope()
    };
    assert_eq!(
      route(
        &store,
        &auth,
        request(Some(token), &Request::ListFiles { scope: foreign })
      )
      .unwrap_err()
      .status,
      403
    );
    let input = FileTarget::Input {
      project_id: "project".into(),
      input_id: "data-v1".into(),
    };
    assert!(
      authorize(
        &store,
        Some(&auth.workers[0].1),
        &Request::GetFile {
          target: input.clone()
        }
      )
      .is_ok()
    );
    assert!(
      authorize(
        &store,
        Some(&auth.workers[0].1),
        &Request::DownloadUrl {
          target: input.clone()
        }
      )
      .is_ok()
    );
    let upload = Request::BeginUpload {
      upload_id: "input-upload".into(),
      target: input,
      size: 1,
      sha256: "a".repeat(64),
    };
    assert_eq!(
      authorize(&store, Some(&auth.workers[0].1), &upload)
        .unwrap_err()
        .status,
      403
    );
    assert!(authorize(&store, None, &upload).is_ok());
    let foreign_input = Request::GetFile {
      target: FileTarget::Input {
        project_id: "other".into(),
        input_id: "data-v1".into(),
      },
    };
    assert_eq!(
      authorize(&store, Some(&auth.workers[0].1), &foreign_input)
        .unwrap_err()
        .status,
      403
    );
    let mut duplicate = request(Some(token), &list);
    duplicate.headers_mut().append(
      "authorization",
      HeaderValue::from_static("Bearer owner-token-with-at-least-24-characters"),
    );
    assert_eq!(route(&store, &auth, duplicate).unwrap_err().status, 401);
  }

  #[test]
  fn dashboard_session_cannot_authorize_service_api_uploads() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let mut auth = auth();
    auth.browser = Some(
      BrowserAuth::new(
        "https://expri.example.com",
        b"a-dedicated-dashboard-password",
      )
      .unwrap(),
    );
    let browser = auth.browser.as_ref().unwrap();
    let login = HttpRequest::builder()
      .method("POST")
      .uri("/login")
      .header("Host", "expri.example.com")
      .header("Origin", "https://expri.example.com")
      .body(Vec::<u8>::new())
      .unwrap();
    let issued = browser
      .login(&login, b"a-dedicated-dashboard-password")
      .unwrap();
    let cookie = issued.split(';').next().unwrap();
    let upload = Request::BeginUpload {
      upload_id: "browser-upload".into(),
      target: FileTarget::Input {
        project_id: "project".into(),
        input_id: "private-input".into(),
      },
      size: 1,
      sha256: "a".repeat(64),
    };
    for bearer in [None, Some("a-dedicated-dashboard-password")] {
      let mut write = request(bearer, &upload);
      write
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());
      write
        .headers_mut()
        .insert("Host", "expri.example.com".parse().unwrap());
      write
        .headers_mut()
        .insert("Origin", "https://expri.example.com".parse().unwrap());
      assert_eq!(route(&store, &auth, write).unwrap_err().status, 401);
      assert_eq!(
        store.upload_target("browser-upload").unwrap_err().status,
        404
      );
    }
    let mut owner = request(
      Some("owner-token-with-at-least-24-characters"),
      &Request::ListFiles { scope: scope() },
    );
    owner
      .headers_mut()
      .insert("Cookie", cookie.parse().unwrap());
    assert!(route(&store, &auth, owner).is_ok());
  }

  #[test]
  fn bounded_http_rejects_huge_and_ambiguous_bodies_without_draining() {
    for framing in [
      "Content-Length: 1073741824\r\n",
      "Content-Length: 18446744073709551615\r\n",
      "Content-Length: 0\r\nContent-Length: 0\r\n",
      "Transfer-Encoding: chunked\r\n",
    ] {
      let listener = TcpListener::bind("127.0.0.1:0").unwrap();
      let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
      let (mut socket, _) = listener.accept().unwrap();
      write!(
        client,
        "POST /v1/request HTTP/1.1\r\nHost: localhost\r\n{framing}\r\n"
      )
      .unwrap();
      client.shutdown(Shutdown::Write).unwrap();
      let error = read_request(&mut socket).unwrap_err();
      assert!(matches!(error.status, 400 | 413));
    }
    assert_eq!(
      parse_headers(b"GET /health HTTP/1.1\r\nHost: one\r\nHost: two\r\n\r\n")
        .unwrap_err()
        .status,
      400
    );
    let mut output = LimitedBytes(Vec::new());
    assert!(output.write_all(&vec![0; types::MAX_REQUEST + 1]).is_err());
    assert!(output.0.is_empty());
    assert_eq!(STREAM_BATCH, 64 * 1024);
  }
}
