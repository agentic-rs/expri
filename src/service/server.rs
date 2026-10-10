use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use http::{HeaderName, HeaderValue, Request as HttpRequest, Response as HttpResponse};
use serde_json::json;
use subtle::ConstantTimeEq;

use super::browser_assets::DashboardAssets;
use super::browser_auth::BrowserAuth;
use super::dashboard_data::HostedDashboard;
use super::storage::{ObjectStorage, S3Storage};
use super::store::{ApiError, ApiResult, Store};
use super::types::{self, DashboardConfig, FileTarget, Request, ServerConfig, WorkerAuth};
use crate::error::{ExpriError, Result};

const HEADER_LIMIT: usize = 16 * 1024;
const WORKERS: usize = 4;
const QUEUE: usize = 16;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const PREVIEW_LIMIT: usize = 8;

struct DashboardSite {
  auth: BrowserAuth,
  assets: DashboardAssets,
  preview: bool,
  allow_project_deletion: bool,
}

struct Auth {
  owner: Vec<u8>,
  workers: Vec<(Vec<u8>, WorkerAuth)>,
  sites: Vec<DashboardSite>,
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
    let sites = config
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
        dashboard_sites(dashboard, password.as_bytes())
      })
      .transpose()?
      .unwrap_or_default();
    Ok(Self {
      owner,
      workers,
      sites,
    })
  }

  fn site<T>(&self, request: &HttpRequest<T>) -> ApiResult<Option<&DashboardSite>> {
    let host = request_host(request)?;
    Ok(
      self
        .sites
        .iter()
        .find(|site| Some(site.auth.authority()) == host),
    )
  }

  fn reject_preview_api<T>(&self, request: &HttpRequest<T>) -> ApiResult<()> {
    if self.sites.is_empty() {
      return Ok(());
    }
    let Some(host) = request_host(request)? else {
      return Ok(());
    };
    let Ok(url) = reqwest::Url::parse(&format!("https://{host}")) else {
      return Ok(());
    };
    if !url.username().is_empty()
      || url.password().is_some()
      || url.path() != "/"
      || url.query().is_some()
      || url.fragment().is_some()
    {
      return Ok(());
    }
    let origin = url.origin().ascii_serialization();
    let authority = origin.strip_prefix("https://");
    if self
      .sites
      .iter()
      .any(|site| site.preview && Some(site.auth.authority()) == authority)
    {
      return Err(ApiError::new(
        403,
        "preview dashboards cannot access the service API",
      ));
    }
    Ok(())
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

fn dashboard_sites(config: &DashboardConfig, credential: &[u8]) -> Result<Vec<DashboardSite>> {
  if config.previews.len() > PREVIEW_LIMIT {
    return Err(ExpriError::Message(
      "dashboard supports at most 8 preview sites".into(),
    ));
  }
  let main = BrowserAuth::new(&config.public_url, credential)?;
  let mut previews = Vec::with_capacity(config.previews.len());
  for preview in &config.previews {
    let auth = BrowserAuth::new(&preview.public_url, credential)?;
    if main.authority() == auth.authority()
      || previews
        .iter()
        .any(|previous: &BrowserAuth| previous.authority() == auth.authority())
    {
      return Err(ExpriError::Message(
        "dashboard public_url authorities must be distinct".into(),
      ));
    }
    previews.push(auth);
  }
  let mut sites = Vec::with_capacity(previews.len() + 1);
  sites.push(DashboardSite {
    auth: main,
    assets: DashboardAssets::embedded(),
    preview: false,
    allow_project_deletion: config.allow_project_deletion,
  });
  for (auth, config) in previews.into_iter().zip(&config.previews) {
    sites.push(DashboardSite {
      auth,
      assets: DashboardAssets::external(config.assets_dir.clone())?,
      preview: true,
      allow_project_deletion: false,
    });
  }
  Ok(sites)
}

fn request_host<T>(request: &HttpRequest<T>) -> ApiResult<Option<&str>> {
  let mut hosts = request.headers().get_all("host").iter();
  let host = hosts
    .next()
    .map(|host| host.to_str())
    .transpose()
    .map_err(|_| ApiError::new(403, "dashboard request header is invalid"))?;
  if hosts.next().is_some() {
    return Err(ApiError::new(403, "dashboard request header is ambiguous"));
  }
  Ok(host)
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
  let server_archive = match request {
    Request::BeginUpload {
      upload_id, target, ..
    } => {
      upload_id.starts_with("result-archive-")
        || matches!(target, FileTarget::Run { path, .. } if path == "result.zip")
    }
    Request::PartUrl { upload_id, .. }
    | Request::RecordPart { upload_id, .. }
    | Request::CompleteUpload { upload_id } => {
      upload_id.starts_with("result-archive-")
        || matches!(store.upload_target(upload_id)?, FileTarget::Run { path, .. } if path == "result.zip")
    }
    _ => false,
  };
  if server_archive {
    return Err(ApiError::new(
      403,
      "result.zip uploads are managed by the server",
    ));
  }
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
    | Request::ReadStream { scope, .. }
    | Request::PutDocument { scope, .. }
    | Request::AppendTracking { scope, .. }
    | Request::ArchiveStatus { scope }
    | Request::SealRun {
      scope,
      incomplete: false,
      ..
    } => worker.project_id == scope.project_id && worker.origin == scope.origin,
    Request::ReferenceFile { .. }
    | Request::ProjectStorage { .. }
    | Request::PreviewProjectDelete { .. }
    | Request::DeleteProject { .. }
    | Request::ProjectDeletion { .. }
    | Request::ArchiveRun { .. }
    | Request::RestoreRun { .. }
    | Request::RunArchival { .. } => false,
    Request::Capabilities => true,
    Request::SealRun {
      incomplete: true, ..
    } => false,
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
  let dashboard = if auth.sites.is_empty() {
    None
  } else {
    Some(HostedDashboard::new(&store)?)
  };
  let listener = TcpListener::bind(listen)?;
  let address = listener.local_addr()?;
  println!("Service: http://{address}");
  std::io::stdout().flush()?;
  let (sender, receiver) = mpsc::sync_channel::<TcpStream>(QUEUE);
  let receiver = Mutex::new(receiver);
  let (notification_sender, notification_receiver) =
    mpsc::sync_channel::<super::notifications::Connection>(super::notifications::CONNECTION_LIMIT);
  let notification_receiver = Mutex::new(notification_receiver);
  let notification_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let stop = std::sync::atomic::AtomicBool::new(false);
  std::thread::scope(|scope| {
    for _ in 0..super::notifications::CONNECTION_LIMIT {
      let receiver = &notification_receiver;
      let auth = &auth;
      let dashboard = dashboard.as_ref();
      let stop = &stop;
      scope.spawn(move || {
        loop {
          let connection = receiver
            .lock()
            .expect("notification connection queue lock")
            .recv();
          let Ok(connection) = connection else {
            break;
          };
          if let (Ok(Some(site)), Some(dashboard)) = (auth.site(&connection.request), dashboard) {
            let _ = super::notifications::serve(connection, &site.auth, dashboard, stop);
          }
        }
      });
    }
    let archive_store = &store;
    let archive_stop = &stop;
    scope.spawn(move || {
      while !archive_stop.load(std::sync::atomic::Ordering::Acquire) {
        // Errors are persisted with a bounded retry delay; they cannot stop ingestion.
        let _ = archive_store.archive_cycle();
        for _ in 0..10 {
          if archive_stop.load(std::sync::atomic::Ordering::Acquire) {
            break;
          }
          std::thread::sleep(Duration::from_millis(500));
        }
      }
    });
    let cleanup_store = &store;
    let cleanup_stop = &stop;
    scope.spawn(move || {
      while !cleanup_stop.load(std::sync::atomic::Ordering::Acquire) {
        // Each durable cleanup cycle is bounded. S3 failures retry independently
        // of ingestion and archive production, including after a restart.
        let _ = cleanup_store.project_deletion_cycle();
        for _ in 0..10 {
          if cleanup_stop.load(std::sync::atomic::Ordering::Acquire) {
            break;
          }
          std::thread::sleep(Duration::from_millis(500));
        }
      }
    });
    let retention_store = &store;
    let retention_stop = &stop;
    scope.spawn(move || {
      while !retention_stop.load(std::sync::atomic::Ordering::Acquire) {
        // Bounded durable cleanup runs independently of ingestion and ZIP production.
        let _ = retention_store.run_retention_cycle();
        for _ in 0..10 {
          if retention_stop.load(std::sync::atomic::Ordering::Acquire) {
            break;
          }
          std::thread::sleep(Duration::from_millis(500));
        }
      }
    });
    for _ in 0..WORKERS {
      let receiver = &receiver;
      let auth = &auth;
      let store = &store;
      let dashboard = dashboard.as_ref();
      let notification_sender = notification_sender.clone();
      let notification_count = std::sync::Arc::clone(&notification_count);
      scope.spawn(move || {
        loop {
          let stream = receiver
            .lock()
            .expect("service connection queue lock")
            .recv();
          match stream {
            Ok(stream) => respond_with_notifications(
              store,
              auth,
              dashboard,
              stream,
              Some((&notification_sender, &notification_count)),
            ),
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
    stop.store(true, std::sync::atomic::Ordering::Release);
    drop(sender);
    drop(notification_sender);
    result
  })
}

type NotificationSender = mpsc::SyncSender<super::notifications::Connection>;
type NotificationCount = std::sync::Arc<std::sync::atomic::AtomicUsize>;

fn respond_with_notifications<S: ObjectStorage>(
  store: &Store<S>,
  auth: &Auth,
  dashboard: Option<&HostedDashboard<'_, S>>,
  mut stream: TcpStream,
  notifications: Option<(&NotificationSender, &NotificationCount)>,
) {
  let mut head = false;
  let reply = match read_request(&mut stream) {
    Ok(request) => {
      head = request.method() == "HEAD";
      if request.uri().path() == "/api/events"
        && let Some((sender, count)) = notifications
      {
        let prepared = (|| {
          let site = auth
            .site(&request)?
            .ok_or_else(|| ApiError::new(404, "hosted live updates are unavailable"))?;
          let dashboard =
            dashboard.ok_or_else(|| ApiError::new(404, "hosted live updates are unavailable"))?;
          let selection = super::notifications::validate(&request, &site.auth, dashboard)?;
          let permit = super::notifications::acquire(count)?;
          Ok::<_, ApiError>((selection, permit))
        })();
        let error = match prepared {
          Ok((selection, permit)) => {
            let connection = super::notifications::Connection {
              stream,
              request,
              selection,
              _permit: permit,
            };
            match sender.try_send(connection) {
              Ok(()) => return,
              Err(
                mpsc::TrySendError::Full(connection) | mpsc::TrySendError::Disconnected(connection),
              ) => {
                stream = connection.stream;
                ApiError::new(
                  503,
                  "live updates are unavailable; polling remains available",
                )
              }
            }
          }
          Err(error) => error,
        };
        let _ = write_response(&mut stream, api_reply(Err(error)), head);
        let _ = stream.shutdown(Shutdown::Both);
        return;
      }
      dispatch(store, auth, dashboard, request)
    }
    Err(error) => api_reply(Err(error)),
  };
  let _ = write_response(&mut stream, reply, head);
  let _ = stream.shutdown(Shutdown::Both);
}

fn dispatch<S: ObjectStorage>(
  store: &Store<S>,
  auth: &Auth,
  dashboard: Option<&HostedDashboard<'_, S>>,
  request: HttpRequest<Vec<u8>>,
) -> HttpResponse<Vec<u8>> {
  if !auth.sites.is_empty() && !matches!(request.uri().path(), "/health" | "/v1/request") {
    return match auth.site(&request) {
      Ok(Some(site)) => super::browser::handle(
        dashboard.expect("configured browser dashboard"),
        &site.auth,
        &site.assets,
        site.allow_project_deletion && !site.preview,
        !site.preview,
        &request,
      ),
      Ok(None) => api_reply(Err(ApiError::new(403, "dashboard host is not allowed"))),
      Err(error) => api_reply(Err(error)),
    };
  }
  api_reply(route(store, auth, request))
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
  auth.reject_preview_api(&request)?;
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
  use crate::service::types::{
    DashboardPreviewConfig, FileTarget, Response, RunScope, STREAM_BATCH,
  };

  const MAIN_AUTHORITY: &str = "expri.example.test";
  const PREVIEW_AUTHORITY: &str = "preview.example.test";
  const DASHBOARD_PASSWORD: &[u8] = b"a-dedicated-dashboard-password";

  fn dashboard_config(previews: &[&str]) -> DashboardConfig {
    DashboardConfig {
      public_url: format!("https://{MAIN_AUTHORITY}"),
      password_env: "unused".into(),
      allow_project_deletion: false,
      previews: previews
        .iter()
        .map(|public_url| DashboardPreviewConfig {
          public_url: (*public_url).into(),
          assets_dir: "/missing/preview/assets".into(),
        })
        .collect(),
    }
  }

  fn dashboard_auth() -> Auth {
    let mut auth = auth();
    for (authority, preview) in [(MAIN_AUTHORITY, false), (PREVIEW_AUTHORITY, true)] {
      auth.sites.push(DashboardSite {
        auth: BrowserAuth::new(&format!("https://{authority}"), DASHBOARD_PASSWORD).unwrap(),
        assets: DashboardAssets::embedded(),
        preview,
        allow_project_deletion: false,
      });
    }
    auth
  }

  fn browser_request(host: Option<&str>, path: &str) -> HttpRequest<Vec<u8>> {
    let mut request = HttpRequest::builder().uri(path);
    if let Some(host) = host {
      request = request.header("host", host);
    }
    request.body(Vec::new()).unwrap()
  }

  fn auth() -> Auth {
    Auth {
      owner: b"owner-token-with-at-least-24-characters".to_vec(),
      sites: Vec::new(),
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
  fn configured_sites_reject_duplicate_authorities_invalid_origins_and_too_many_previews() {
    let main = dashboard_sites(&dashboard_config(&[]), DASHBOARD_PASSWORD).unwrap();
    assert_eq!(main.len(), 1);
    assert_eq!(main[0].auth.authority(), MAIN_AUTHORITY);
    assert!(!main[0].preview);
    for previews in [
      vec!["https://expri.example.test"],
      vec!["https://EXPRI.example.test:443/"],
      vec![
        "https://preview.example.test",
        "https://PREVIEW.example.test:443/",
      ],
    ] {
      let error = dashboard_sites(&dashboard_config(&previews), DASHBOARD_PASSWORD)
        .err()
        .unwrap();
      assert!(error.to_string().contains("authorities must be distinct"));
    }
    for invalid in [
      "http://preview.example.test",
      "https://preview.example.test/path",
      "https://user@preview.example.test",
      "https://*.expri.example.net",
    ] {
      assert!(dashboard_sites(&dashboard_config(&[invalid]), DASHBOARD_PASSWORD).is_err());
    }
    let config = dashboard_config(&["https://preview.example.test"; PREVIEW_LIMIT + 1]);
    let error = dashboard_sites(&config, DASHBOARD_PASSWORD).err().unwrap();
    assert!(error.to_string().contains("at most 8"));
    assert!(
      dashboard_sites(
        &dashboard_config(&["https://preview.example.test"]),
        DASHBOARD_PASSWORD,
      )
      .is_err()
    );
  }

  #[cfg(unix)]
  #[test]
  fn configured_preview_serves_external_assets_while_main_stays_embedded() {
    let directory = tempfile::tempdir().unwrap();
    let assets = directory.path().join("assets");
    let commit = "b".repeat(40);
    let release = assets.join("releases").join(&commit);
    std::fs::create_dir_all(&release).unwrap();
    for (filename, content) in [
      ("index.html", "<main>preview dashboard</main>"),
      (
        "login.html",
        "<main>preview login<!-- LOGIN_ERROR --></main>",
      ),
      ("app.js", "console.log('preview bundle');"),
      ("styles.css", "body { color: teal; }"),
    ] {
      std::fs::write(release.join(filename), content).unwrap();
    }
    std::fs::write(
      release.join("deployment.json"),
      serde_json::to_vec(&json!({"commit": commit, "branch": "codex/preview"})).unwrap(),
    )
    .unwrap();
    std::os::unix::fs::symlink(format!("releases/{commit}"), assets.join("current")).unwrap();
    let mut config = dashboard_config(&["https://preview.example.test"]);
    config.previews[0].assets_dir = assets;
    let mut auth = auth();
    auth.sites = dashboard_sites(&config, DASHBOARD_PASSWORD).unwrap();
    let store = Store::open(&directory.path().join("store"), MockStorage::default()).unwrap();
    let dashboard = HostedDashboard::new(&store).unwrap();
    let preview = dispatch(
      &store,
      &auth,
      Some(&dashboard),
      browser_request(Some(PREVIEW_AUTHORITY), "/login"),
    );
    assert_eq!(preview.status(), 200);
    assert_eq!(preview.headers()["x-expri-revision"], commit);
    assert!(
      String::from_utf8(preview.into_body())
        .unwrap()
        .contains("preview login")
    );
    let main = dispatch(
      &store,
      &auth,
      Some(&dashboard),
      browser_request(Some(MAIN_AUTHORITY), "/login"),
    );
    assert_eq!(main.status(), 200);
    assert!(!main.headers().contains_key("x-expri-revision"));
    assert!(
      !String::from_utf8(main.into_body())
        .unwrap()
        .contains("preview login")
    );
    let script = dispatch(
      &store,
      &auth,
      Some(&dashboard),
      browser_request(Some(PREVIEW_AUTHORITY), &format!("/assets/{commit}/app.js")),
    );
    assert_eq!(script.status(), 200);
    assert_eq!(script.body(), b"console.log('preview bundle');");
  }

  #[test]
  fn browser_dispatch_selects_exact_host_and_rejects_unknown_or_ambiguous_hosts() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let dashboard = HostedDashboard::new(&store).unwrap();
    let auth = dashboard_auth();
    for authority in [MAIN_AUTHORITY, PREVIEW_AUTHORITY] {
      let request = browser_request(Some(authority), "/login");
      assert_eq!(
        auth.site(&request).unwrap().unwrap().auth.authority(),
        authority
      );
      assert_eq!(
        dispatch(&store, &auth, Some(&dashboard), request).status(),
        200
      );
      assert_eq!(
        dispatch(
          &store,
          &auth,
          Some(&dashboard),
          browser_request(Some(authority), "/api/catalog"),
        )
        .status(),
        401
      );
    }
    for host in [
      None,
      Some("localhost:8787"),
      Some("EXPRI.example.test"),
      Some("evil.test"),
    ] {
      for path in ["/", "/login", "/app.js", "/api/catalog"] {
        assert_eq!(
          dispatch(&store, &auth, Some(&dashboard), browser_request(host, path)).status(),
          403,
          "{host:?} {path}",
        );
      }
    }
    let mut duplicate = browser_request(Some(MAIN_AUTHORITY), "/login");
    duplicate
      .headers_mut()
      .append("host", PREVIEW_AUTHORITY.parse().unwrap());
    assert_eq!(
      dispatch(&store, &auth, Some(&dashboard), duplicate).status(),
      403
    );
  }

  #[test]
  fn preview_hosts_deny_bearer_api_without_breaking_existing_service_addresses() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let auth = dashboard_auth();
    let operation = Request::ListFiles { scope: scope() };
    for host in [
      PREVIEW_AUTHORITY,
      "PREVIEW.example.test",
      "preview.example.test:443",
    ] {
      for bearer in [None, Some("owner-token-with-at-least-24-characters")] {
        let mut request = request(bearer, &operation);
        request.headers_mut().insert("host", host.parse().unwrap());
        assert_eq!(route(&store, &auth, request).unwrap_err().status, 403);
      }
    }
    for host in [
      None,
      Some(MAIN_AUTHORITY),
      Some("localhost:8787"),
      Some("service.internal"),
    ] {
      let mut request = request(Some("owner-token-with-at-least-24-characters"), &operation);
      if let Some(host) = host {
        request.headers_mut().insert("host", host.parse().unwrap());
      }
      assert!(route(&store, &auth, request).is_ok(), "{host:?}");
    }
    let health = browser_request(Some(PREVIEW_AUTHORITY), "/health");
    assert!(route(&store, &auth, health).is_ok());
  }

  #[test]
  fn dispatch_keeps_host_sessions_and_origins_isolated() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let dashboard = HostedDashboard::new(&store).unwrap();
    let auth = dashboard_auth();
    let mut cookies = Vec::new();
    for authority in [MAIN_AUTHORITY, PREVIEW_AUTHORITY] {
      let login = HttpRequest::builder()
        .method("POST")
        .uri("/login")
        .header("host", authority)
        .header("origin", format!("https://{authority}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(b"password=a-dedicated-dashboard-password".to_vec())
        .unwrap();
      let reply = dispatch(&store, &auth, Some(&dashboard), login);
      assert_eq!(reply.status(), 303);
      cookies.push(
        reply.headers()["set-cookie"]
          .to_str()
          .unwrap()
          .split(';')
          .next()
          .unwrap()
          .to_owned(),
      );
    }
    for (index, authority) in [MAIN_AUTHORITY, PREVIEW_AUTHORITY].into_iter().enumerate() {
      for (cookie_index, cookie) in cookies.iter().enumerate() {
        let mut request = browser_request(Some(authority), "/api/catalog");
        request
          .headers_mut()
          .insert("cookie", cookie.parse().unwrap());
        assert_eq!(
          dispatch(&store, &auth, Some(&dashboard), request).status(),
          if index == cookie_index { 200 } else { 401 },
        );
      }
      let mut request = browser_request(Some(authority), "/api/catalog");
      request
        .headers_mut()
        .insert("cookie", cookies[index].parse().unwrap());
      request
        .headers_mut()
        .insert("origin", "https://evil.example.test".parse().unwrap());
      assert_eq!(
        dispatch(&store, &auth, Some(&dashboard), request).status(),
        403
      );
      let hostile_login = HttpRequest::builder()
        .method("POST")
        .uri("/login")
        .header("host", authority)
        .header(
          "origin",
          format!(
            "https://{}",
            if index == 0 {
              PREVIEW_AUTHORITY
            } else {
              MAIN_AUTHORITY
            }
          ),
        )
        .header("content-type", "application/x-www-form-urlencoded")
        .body(b"password=a-dedicated-dashboard-password".to_vec())
        .unwrap();
      assert_eq!(
        dispatch(&store, &auth, Some(&dashboard), hostile_login).status(),
        403
      );
    }
  }

  #[test]
  fn only_owner_can_create_object_references() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let auth = auth();
    let operation = Request::ReferenceFile {
      source: FileTarget::Run {
        scope: scope(),
        path: "outputs/data.tar.gz".into(),
      },
      target: FileTarget::Input {
        project_id: "project".into(),
        input_id: "asset".into(),
      },
      size: 7,
      sha256: "a".repeat(64),
    };
    assert!(authorize(&store, None, &operation).is_ok());
    assert_eq!(
      authorize(&store, Some(&auth.workers[0].1), &operation)
        .unwrap_err()
        .status,
      403
    );
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
  fn tracking_writes_are_scoped_and_recovery_archives_require_owner_authority() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let auth = auth();
    let worker = Some(&auth.workers[0].1);
    let put = |scope| Request::PutDocument {
      scope,
      path: "run-state.json".into(),
      revision: 1,
      offset: 0,
      total_size: 2,
      data_base64: "e30=".into(),
    };
    assert!(authorize(&store, worker, &put(scope())).is_ok());
    assert_eq!(
      authorize(
        &store,
        worker,
        &put(RunScope {
          origin: "other".into(),
          ..scope()
        })
      )
      .unwrap_err()
      .status,
      403
    );
    let seal = |incomplete| Request::SealRun {
      scope: scope(),
      documents: Default::default(),
      streams: Default::default(),
      incomplete,
    };
    assert!(authorize(&store, worker, &seal(false)).is_ok());
    assert_eq!(
      authorize(&store, worker, &seal(true)).unwrap_err().status,
      403
    );
    assert!(authorize(&store, None, &seal(true)).is_ok());
    let archive = Request::BeginUpload {
      upload_id: "user-owned".into(),
      target: FileTarget::Run {
        scope: scope(),
        path: "result.zip".into(),
      },
      size: 1,
      sha256: "a".repeat(64),
    };
    for role in [worker, None] {
      assert_eq!(authorize(&store, role, &archive).unwrap_err().status, 403);
      assert_eq!(
        authorize(
          &store,
          role,
          &Request::CompleteUpload {
            upload_id: "result-archive-1".into()
          }
        )
        .unwrap_err()
        .status,
        403
      );
    }
  }

  #[test]
  fn dashboard_session_cannot_authorize_service_api_uploads() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let mut auth = auth();
    auth.sites.push(DashboardSite {
      auth: BrowserAuth::new(
        "https://expri.example.com",
        b"a-dedicated-dashboard-password",
      )
      .unwrap(),
      assets: DashboardAssets::embedded(),
      preview: false,
      allow_project_deletion: false,
    });
    let browser = &auth.sites[0].auth;
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
  fn hosted_management_service_requests_are_owner_only() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let auth = auth();
    let worker = &auth.workers[0].1;
    for operation in [
      Request::ProjectStorage {
        project_id: "project".into(),
      },
      Request::PreviewProjectDelete {
        project_id: "project".into(),
      },
      Request::DeleteProject {
        project_id: "project".into(),
        revision: "1".into(),
        confirmation: "project".into(),
      },
      Request::ProjectDeletion {
        project_id: "project".into(),
      },
      Request::ArchiveRun { scope: scope() },
      Request::RestoreRun { scope: scope() },
      Request::RunArchival { scope: scope() },
    ] {
      assert_eq!(
        authorize(&store, Some(worker), &operation)
          .unwrap_err()
          .status,
        403
      );
      assert!(authorize(&store, None, &operation).is_ok());
      assert_eq!(
        route(
          &store,
          &auth,
          request(Some("worker-token-with-at-least-24-characters"), &operation)
        )
        .unwrap_err()
        .status,
        403
      );
    }
  }

  #[test]
  fn project_deletion_is_opt_in_and_previews_remain_read_only() {
    let mut config = dashboard_config(&[]);
    config.allow_project_deletion = true;
    let sites = dashboard_sites(&config, DASHBOARD_PASSWORD).unwrap();
    assert!(sites[0].allow_project_deletion);
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let dashboard = HostedDashboard::new(&store).unwrap();
    let mut auth = dashboard_auth();
    // Even an incorrectly populated preview capability cannot enable writes.
    for site in &mut auth.sites {
      site.allow_project_deletion = true;
    }
    let site = &auth.sites[1];
    let login = HttpRequest::builder()
      .method("POST")
      .uri("/login")
      .header("Host", PREVIEW_AUTHORITY)
      .header("Origin", format!("https://{PREVIEW_AUTHORITY}"))
      .body(Vec::<u8>::new())
      .unwrap();
    let issued = site.auth.login(&login, DASHBOARD_PASSWORD).unwrap();
    let deletion = HttpRequest::builder().method("POST").uri("/api/projects/delete")
      .header("Host", PREVIEW_AUTHORITY).header("Origin", format!("https://{PREVIEW_AUTHORITY}"))
      .header("Cookie", issued.split(';').next().unwrap()).header("Content-Type", "application/json")
      .body(br#"{"project_id":"project","revision":"1","confirmation":"project","password":"a-dedicated-dashboard-password"}"#.to_vec()).unwrap();
    let reply = dispatch(&store, &auth, Some(&dashboard), deletion);
    assert_eq!(reply.status(), 403);
    assert!(
      std::str::from_utf8(reply.body())
        .unwrap()
        .contains("disabled")
    );
  }

  #[test]
  fn preview_sites_cannot_archive_or_restore_shared_runs() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let dashboard = HostedDashboard::new(&store).unwrap();
    let auth = dashboard_auth();
    let site = &auth.sites[1];
    let login = HttpRequest::builder()
      .method("POST")
      .uri("/login")
      .header("Host", PREVIEW_AUTHORITY)
      .header("Origin", format!("https://{PREVIEW_AUTHORITY}"))
      .body(Vec::<u8>::new())
      .unwrap();
    let issued = site.auth.login(&login, DASHBOARD_PASSWORD).unwrap();
    for path in ["/api/runs/archive", "/api/runs/restore"] {
      let write = HttpRequest::builder()
        .method("POST")
        .uri(path)
        .header("Host", PREVIEW_AUTHORITY)
        .header("Origin", format!("https://{PREVIEW_AUTHORITY}"))
        .header("Cookie", issued.split(';').next().unwrap())
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&scope()).unwrap())
        .unwrap();
      let reply = dispatch(&store, &auth, Some(&dashboard), write);
      assert_eq!(reply.status(), 403);
      assert!(
        std::str::from_utf8(reply.body())
          .unwrap()
          .contains("disabled")
      );
    }
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
