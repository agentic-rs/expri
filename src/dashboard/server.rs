use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use http::{HeaderName, HeaderValue, Method, Request, Response, Version};
use serde_json::{Value, json};

use super::Dashboard;
use crate::error::{ExpriError, Result};
use crate::metric_charts::ChartXAxis;
use crate::metrics::Reduction;

const QUERY_LIMIT: usize = 8 * 1024;
const HEADER_LIMIT: usize = 16 * 1024;
const JSON_LIMIT: usize = 512 * 1024;
const HTML_LIMIT: usize = 2 * 1024 * 1024;
const WORKERS: usize = 4;
const CONNECTION_QUEUE: usize = 16;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(3600);
const MAIN_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; frame-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";
const CHART_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'self'; base-uri 'none'; form-action 'none'";

/// Keep the dashboard on loopback, with a fixed number of request workers.
pub fn serve(dashboard: Dashboard, port: u16) -> Result<()> {
  let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
  let listener = TcpListener::bind(address)?;
  let address = listener.local_addr()?;
  let authority = format!("127.0.0.1:{}", address.port());
  println!("Dashboard: http://{authority}");
  std::io::stdout().flush()?;
  let (sender, receiver) = mpsc::sync_channel::<TcpStream>(CONNECTION_QUEUE);
  let receiver = Mutex::new(receiver);
  std::thread::scope(|scope| {
    for _ in 0..WORKERS {
      let dashboard = &dashboard;
      let authority = &authority;
      let receiver = &receiver;
      scope.spawn(move || {
        loop {
          let stream = receiver.lock().expect("connection queue lock").recv();
          match stream {
            Ok(stream) => respond(dashboard, authority, stream),
            Err(_) => break,
          }
        }
      });
    }
    let result = loop {
      match listener.accept() {
        Ok((stream, _)) => match sender.try_send(stream) {
          Ok(()) => {}
          // Keep the queue bounded without blocking the listener on slow clients.
          Err(mpsc::TrySendError::Full(stream)) => {
            let _ = stream.shutdown(Shutdown::Both);
          }
          Err(mpsc::TrySendError::Disconnected(_)) => {
            break Err(ExpriError::Message("dashboard workers stopped".to_string()));
          }
        },
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
        Err(error) => break Err(error.into()),
      }
    };
    drop(sender);
    result
  })
}

fn respond(dashboard: &Dashboard, authority: &str, mut stream: TcpStream) {
  let mut download = None;
  let (reply, head) = match read_request(&mut stream) {
    Ok(request) => {
      let head = request.method() == Method::HEAD;
      let headers: Vec<_> = request
        .headers()
        .iter()
        .map(|(name, value)| {
          (
            name.as_str(),
            value.to_str().expect("validated HTTP header"),
          )
        })
        .collect();
      let url = request.uri().to_string();
      let reply = validate_boundary(authority, request.method().as_str(), &url, &headers)
        .and_then(|()| validate_body(&headers))
        .map_or_else(
          |reply| reply,
          |()| {
            if url.split('?').next() == Some("/api/artifact") {
              return match artifact_download(dashboard, &url) {
                Ok(artifact) => {
                  download = Some(artifact);
                  Reply::bytes("application/octet-stream", Vec::new())
                }
                Err(reply) => reply,
              };
            }
            route(
              dashboard,
              authority,
              request.method().as_str(),
              &url,
              &headers,
            )
          },
        );
      (reply, head)
    }
    Err(reply) => (reply, false),
  };
  // Socket shutdown discards an unread body; it never allocates or drains it.
  let mut writer = DeadlineWriter {
    stream: &mut stream,
    deadline: Instant::now()
      + if download.is_some() && !head {
        DOWNLOAD_TIMEOUT
      } else {
        SOCKET_TIMEOUT
      },
  };
  if let Some(download) = download {
    let _ = write_download(&mut writer, download, head);
  } else {
    let _ = write_response(&mut writer, reply, head);
  }
  let _ = stream.shutdown(Shutdown::Both);
}

/// Use one deadline for the complete response, including partial socket writes.
struct DeadlineWriter<'a> {
  stream: &'a mut TcpStream,
  deadline: Instant,
}

impl Write for DeadlineWriter<'_> {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    let remaining = self.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      return Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "dashboard response write timed out",
      ));
    }
    self
      .stream
      .set_write_timeout(Some(remaining.min(SOCKET_TIMEOUT)))?;
    self.stream.write(bytes)
  }

  fn flush(&mut self) -> std::io::Result<()> {
    self.stream.flush()
  }
}

fn write_download(
  stream: &mut impl Write,
  download: super::artifacts::Download,
  head: bool,
) -> std::io::Result<()> {
  let super::artifacts::Download::Local {
    file,
    size,
    filename,
  } = download
  else {
    return Err(std::io::Error::other(
      "local dashboard cannot redirect cloud downloads",
    ));
  };
  write!(
    stream,
    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {size}\r\nContent-Disposition: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; sandbox\r\n\r\n",
    super::artifacts::disposition(&filename)
  )?;
  if !head {
    let copied = std::io::copy(&mut file.take(size), stream)?;
    if copied != size {
      return Err(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "artifact changed during download",
      ));
    }
  }
  stream.flush()
}

fn read_request(stream: &mut TcpStream) -> std::result::Result<Request<()>, Reply> {
  let deadline = Instant::now() + SOCKET_TIMEOUT;
  let mut bytes = Vec::with_capacity(2048);
  loop {
    if let Some(request) = parse_http_request(&bytes)? {
      return Ok(request);
    }
    if bytes.len() == HEADER_LIMIT {
      return Err(Reply::error(
        431,
        "request headers exceed the 16 KiB size limit",
      ));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      return Err(Reply::error(408, "request header read timed out"));
    }
    stream
      .set_read_timeout(Some(remaining))
      .map_err(|_| Reply::error(400, "could not read request"))?;
    let mut buffer = [0; 2048];
    let length = buffer.len().min(HEADER_LIMIT - bytes.len());
    match stream.read(&mut buffer[..length]) {
      Ok(0) => return Err(Reply::error(400, "incomplete request headers")),
      Ok(size) => bytes.extend_from_slice(&buffer[..size]),
      Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
      Err(error)
        if matches!(
          error.kind(),
          std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) =>
      {
        return Err(Reply::error(408, "request header read timed out"));
      }
      Err(_) => return Err(Reply::error(400, "could not read request")),
    }
  }
}

fn parse_http_request(bytes: &[u8]) -> std::result::Result<Option<Request<()>>, Reply> {
  let mut headers = [httparse::EMPTY_HEADER; 64];
  let mut parsed = httparse::Request::new(&mut headers);
  match parsed.parse(bytes) {
    Ok(httparse::Status::Partial) => return Ok(None),
    Ok(httparse::Status::Complete(length)) if length > HEADER_LIMIT => {
      return Err(Reply::error(
        431,
        "request headers exceed the 16 KiB size limit",
      ));
    }
    Ok(httparse::Status::Complete(_)) => {}
    Err(httparse::Error::TooManyHeaders) => {
      return Err(Reply::error(431, "request exceeds the 64-header limit"));
    }
    Err(_) => return Err(Reply::error(400, "malformed HTTP request")),
  }
  let invalid = || Reply::error(400, "malformed HTTP request");
  let mut request = Request::builder()
    .method(parsed.method.ok_or_else(invalid)?)
    .uri(parsed.path.ok_or_else(invalid)?)
    .version(if parsed.version == Some(1) {
      Version::HTTP_11
    } else {
      Version::HTTP_10
    })
    .body(())
    .map_err(|_| invalid())?;
  for header in parsed.headers {
    let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| invalid())?;
    let value = HeaderValue::from_bytes(header.value).map_err(|_| invalid())?;
    value.to_str().map_err(|_| invalid())?;
    request.headers_mut().append(name, value);
  }
  Ok(Some(request))
}

fn validate_body(headers: &[(&str, &str)]) -> std::result::Result<(), Reply> {
  let mut lengths = headers
    .iter()
    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"));
  let length = lengths.next();
  if lengths.next().is_some()
    || length.is_some_and(|(_, value)| *value != "0")
    || headers
      .iter()
      .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
  {
    return Err(Reply::error(
      400,
      "dashboard requests must not contain a body",
    ));
  }
  Ok(())
}

fn write_response(stream: &mut impl Write, reply: Reply, head: bool) -> std::io::Result<()> {
  let mut response = Response::builder()
    .status(reply.status)
    .header("Content-Type", reply.content_type)
    .header("Content-Length", reply.body.len())
    .header("Connection", "close")
    .header("Cache-Control", "no-store")
    .header("X-Content-Type-Options", "nosniff")
    .header(
      "Content-Security-Policy",
      if reply.chart { CHART_CSP } else { MAIN_CSP },
    );
  if reply.status == 405 {
    response = response.header("Allow", "GET, HEAD");
  }
  let response = response
    .body(reply.body)
    .expect("validated HTTP response headers");
  let status = response.status();
  write!(
    stream,
    "HTTP/1.1 {} {}\r\n",
    status.as_u16(),
    status.canonical_reason().unwrap_or("Unknown")
  )?;
  for (name, value) in response.headers() {
    stream.write_all(name.as_str().as_bytes())?;
    stream.write_all(b": ")?;
    stream.write_all(value.as_bytes())?;
    stream.write_all(b"\r\n")?;
  }
  stream.write_all(b"\r\n")?;
  if !head {
    stream.write_all(response.body())?;
  }
  stream.flush()
}

pub(crate) struct Reply {
  pub status: u16,
  pub content_type: &'static str,
  pub body: Vec<u8>,
  pub chart: bool,
}

impl Reply {
  fn bytes(content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
    let body = body.into();
    let limit = if content_type.starts_with("text/html") {
      HTML_LIMIT
    } else {
      JSON_LIMIT
    };
    if body.len() > limit {
      return Self::error(
        413,
        format!(
          "dashboard response exceeds the {} KiB size limit; reduce the row/log limits or select fewer runs and metrics",
          limit / 1024
        ),
      );
    }
    Self {
      status: 200,
      content_type,
      body,
      chart: false,
    }
  }

  fn json(value: Value) -> Self {
    let mut writer = LimitedWriter(Vec::new());
    if serde_json::to_writer(&mut writer, &value).is_err() {
      return Self::error(
        413,
        "dashboard JSON response exceeds the 512 KiB size limit; reduce the row/log limits or select fewer runs and metrics",
      );
    }
    Self::bytes("application/json; charset=utf-8", writer.0)
  }

  fn error(status: u16, message: impl Into<String>) -> Self {
    let mut reply = Self::json(json!({"error": message.into()}));
    reply.status = status;
    reply
  }
}

struct LimitedWriter(Vec<u8>);

impl Write for LimitedWriter {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    if self.0.len().saturating_add(bytes.len()) > JSON_LIMIT {
      return Err(std::io::Error::other("JSON response limit exceeded"));
    }
    self.0.extend_from_slice(bytes);
    Ok(bytes.len())
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

#[derive(Debug, Default)]
struct Query(BTreeMap<String, Vec<String>>);

impl Query {
  fn parse(raw: &str) -> std::result::Result<Self, Reply> {
    if raw.len() > QUERY_LIMIT {
      return Err(Reply::error(400, "query exceeds the 8 KiB size limit"));
    }
    let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in form_urlencoded::parse(raw.as_bytes()) {
      let values = fields.entry(name.into_owned()).or_default();
      values.push(value.into_owned());
    }
    for (name, values) in &fields {
      if values.len() > 1 && !matches!(name.as_str(), "run_id" | "metric" | "param") {
        return Err(Reply::error(400, format!("duplicate query field: {name}")));
      }
    }
    Ok(Self(fields))
  }

  fn allow(&self, fields: &[&str]) -> std::result::Result<(), Reply> {
    if let Some(name) = self.0.keys().find(|name| !fields.contains(&name.as_str())) {
      return Err(Reply::error(400, format!("unknown query field: {name}")));
    }
    Ok(())
  }

  fn optional(&self, name: &str) -> Option<&str> {
    self
      .0
      .get(name)
      .and_then(|values| values.first())
      .map(String::as_str)
  }

  fn many(&self, name: &str) -> Vec<String> {
    self.0.get(name).cloned().unwrap_or_default()
  }

  fn required(&self, name: &str) -> std::result::Result<&str, Reply> {
    if self.0.get(name).is_some_and(|values| values.len() > 1) {
      return Err(Reply::error(400, format!("duplicate query field: {name}")));
    }
    self
      .optional(name)
      .filter(|value| !value.is_empty())
      .ok_or_else(|| Reply::error(400, format!("missing query field: {name}")))
  }

  fn number(
    &self,
    name: &str,
    default: usize,
    min: usize,
    max: usize,
  ) -> std::result::Result<usize, Reply> {
    let Some(value) = self.optional(name) else {
      return Ok(default);
    };
    value
      .parse::<usize>()
      .ok()
      .filter(|value| (min..=max).contains(value))
      .ok_or_else(|| {
        Reply::error(
          400,
          format!("{name} must be an integer from {min} to {max}"),
        )
      })
  }

  fn runs(&self, minimum: usize) -> std::result::Result<Vec<String>, Reply> {
    let ids = self.many("run_id");
    if !(minimum..=8).contains(&ids.len()) {
      return Err(Reply::error(400, format!("select {minimum} to 8 run IDs")));
    }
    for (index, id) in ids.iter().enumerate() {
      if id.is_empty() || ids[..index].contains(id) {
        return Err(Reply::error(400, "run IDs must be nonempty and distinct"));
      }
    }
    Ok(ids)
  }
}

fn validate_boundary(
  authority: &str,
  method: &str,
  url: &str,
  headers: &[(&str, &str)],
) -> std::result::Result<(), Reply> {
  let values = |name: &str| {
    headers
      .iter()
      .filter(|(field, _)| field.eq_ignore_ascii_case(name))
      .map(|(_, value)| *value)
      .collect::<Vec<_>>()
  };
  if values("host") != [authority] {
    return Err(Reply::error(403, "Host must match the dashboard address"));
  }
  let origin = format!("http://{authority}");
  let origins = values("origin");
  if !origins.is_empty() && origins != [origin.as_str()] {
    return Err(Reply::error(403, "Origin must match the dashboard address"));
  }
  let sites = values("sec-fetch-site");
  if sites.len() > 1
    || sites
      .iter()
      .any(|site| site.eq_ignore_ascii_case("cross-site"))
  {
    return Err(Reply::error(
      403,
      "cross-site dashboard requests are forbidden",
    ));
  }
  if !matches!(method, "GET" | "HEAD") {
    return Err(Reply::error(405, "only GET and HEAD are supported"));
  }
  if !url.starts_with('/') || url.starts_with("//") || url.contains('#') {
    return Err(Reply::error(400, "invalid request path"));
  }
  Ok(())
}

fn route(
  dashboard: &Dashboard,
  authority: &str,
  method: &str,
  url: &str,
  headers: &[(&str, &str)],
) -> Reply {
  match route_checked(dashboard, authority, method, url, headers) {
    Ok(reply) | Err(reply) => reply,
  }
}

fn route_checked(
  dashboard: &impl super::DashboardView,
  authority: &str,
  method: &str,
  url: &str,
  headers: &[(&str, &str)],
) -> std::result::Result<Reply, Reply> {
  validate_boundary(authority, method, url, headers)?;
  route_content_checked(dashboard, url)
}

/// The caller supplies its own authentication and request boundary.
pub(crate) fn route_content(dashboard: &impl super::DashboardView, url: &str) -> Reply {
  match route_content_checked(dashboard, url) {
    Ok(reply) | Err(reply) => reply,
  }
}

/// The caller must enforce its authentication and HTTP request boundary before
/// resolving a file or minting a signed object-storage URL.
pub(crate) fn artifact_download(
  dashboard: &impl super::DashboardView,
  url: &str,
) -> std::result::Result<super::artifacts::Download, Reply> {
  let (path, raw_query) = url.split_once('?').unwrap_or((url, ""));
  if path != "/api/artifact" {
    return Err(Reply::error(404, "dashboard route not found"));
  }
  let query = Query::parse(raw_query)?;
  query.allow(&["source", "run_id", "path"])?;
  dashboard
    .artifact_download(
      query.required("source")?,
      query.required("run_id")?,
      query.required("path")?,
    )
    .map_err(service_error)
}

fn route_content_checked(
  dashboard: &impl super::DashboardView,
  url: &str,
) -> std::result::Result<Reply, Reply> {
  let (path, raw_query) = url.split_once('?').unwrap_or((url, ""));
  let query = Query::parse(raw_query)?;
  let source = query.optional("source").unwrap_or("local");
  match path {
    "/" | "/index.html" => {
      query.allow(&[])?;
      Ok(Reply::bytes(
        "text/html; charset=utf-8",
        include_bytes!("../../dashboard_web/index.html").as_slice(),
      ))
    }
    "/styles.css" => {
      query.allow(&[])?;
      Ok(Reply::bytes(
        "text/css; charset=utf-8",
        include_bytes!("../../dashboard_web/styles.css").as_slice(),
      ))
    }
    "/app.js" => {
      query.allow(&[])?;
      Ok(Reply::bytes(
        "text/javascript; charset=utf-8",
        include_bytes!("../../dashboard_web/app.js").as_slice(),
      ))
    }
    "/api/catalog" => {
      query.allow(&[])?;
      Ok(Reply::json(dashboard.catalog().map_err(service_error)?))
    }
    "/api/projects" => {
      query.allow(&[])?;
      let projects = dashboard
        .projects()
        .map_err(service_error)?
        .ok_or_else(|| Reply::error(404, "project browsing is unavailable"))?;
      Ok(Reply::json(projects))
    }
    "/api/updates" => {
      query.allow(&["source", "run_id"])?;
      let ids = query.runs(0)?;
      Ok(Reply::json(
        dashboard
          .updates(query.optional("source").unwrap_or(""), &ids)
          .map_err(service_error)?,
      ))
    }
    "/api/runs" => {
      query.allow(&[
        "source",
        "search",
        "task",
        "status",
        "limit",
        "offset",
        "param",
        "metric",
        "reduction",
        "sort",
        "direction",
        "origin",
      ])?;
      let limit = query.number("limit", 100, 1, 1000)?;
      let offset = query.number("offset", 0, 0, usize::MAX)?;
      let table = ["param", "metric", "reduction", "sort", "direction"]
        .iter()
        .any(|field| query.optional(field).is_some())
        .then(|| {
          super::table::TableOptions::parse(
            query.many("param"),
            query.many("metric"),
            query.optional("reduction").unwrap_or("last"),
            query.optional("sort"),
            query.optional("direction"),
          )
        })
        .transpose()
        .map_err(service_error)?;
      Ok(Reply::json(
        dashboard
          .list_table(
            source,
            &super::table::ListQuery {
              origin: query.optional("origin"),
              search: query.optional("search"),
              task: query.optional("task"),
              status: query.optional("status"),
              limit,
              offset,
              table: table.as_ref(),
            },
          )
          .map_err(service_error)?,
      ))
    }
    "/api/run-columns" => {
      query.allow(&["source"])?;
      Ok(Reply::json(
        dashboard.columns(source).map_err(service_error)?,
      ))
    }
    "/api/run" => {
      query.allow(&["source", "run_id"])?;
      Ok(Reply::json(
        dashboard
          .detail(source, query.required("run_id")?)
          .map_err(service_error)?,
      ))
    }
    "/api/artifacts" => {
      query.allow(&["source", "run_id"])?;
      Ok(Reply::json(
        dashboard
          .artifacts(source, query.required("run_id")?)
          .map_err(service_error)?,
      ))
    }
    "/api/log" => {
      query.allow(&["source", "run_id", "stream", "tail"])?;
      let stream = query.optional("stream").unwrap_or("stdout");
      if !matches!(stream, "stdout" | "stderr") {
        return Err(Reply::error(400, "stream must be stdout or stderr"));
      }
      let tail = query.number("tail", 100, 0, 1000)?;
      Ok(Reply::json(
        dashboard
          .log(source, query.required("run_id")?, stream, tail)
          .map_err(service_error)?,
      ))
    }
    "/api/compare" => {
      query.allow(&["source", "run_id", "metric", "reduction"])?;
      let ids = query.runs(2)?;
      let reduction = match query.optional("reduction").unwrap_or("last") {
        "last" => Reduction::Last,
        "min" => Reduction::Min,
        "max" => Reduction::Max,
        _ => return Err(Reply::error(400, "reduction must be last, min, or max")),
      };
      Ok(Reply::json(
        dashboard
          .compare(source, &ids, &query.many("metric"), reduction)
          .map_err(service_error)?,
      ))
    }
    "/api/chart" => {
      query.allow(&["source", "run_id", "metric", "x_axis"])?;
      let ids = query.runs(1)?;
      let x_axis =
        ChartXAxis::parse(query.optional("x_axis").unwrap_or("step")).map_err(service_error)?;
      let html = dashboard
        .chart(source, &ids, &query.many("metric"), x_axis)
        .map_err(service_error)?;
      let mut reply = Reply::bytes("text/html; charset=utf-8", html);
      reply.chart = true;
      Ok(reply)
    }
    _ => Err(Reply::error(404, "dashboard route not found")),
  }
}

fn service_error(error: ExpriError) -> Reply {
  let status = match &error {
    ExpriError::Message(message) if message == "object storage unavailable" => 502,
    ExpriError::Message(message)
      if message.starts_with("run is missing: ")
        || message.starts_with("unknown source: ")
        || message.starts_with("artifact is missing: ") =>
    {
      404
    }
    ExpriError::Message(_) | ExpriError::Json(_) | ExpriError::Toml(_) => 400,
    _ => 500,
  };
  Reply::error(status, error.to_string())
}

#[cfg(test)]
mod tests;
