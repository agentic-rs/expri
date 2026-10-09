//! Bounded notification connections never occupy the ingestion request pool.

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use http::Request;
use serde_json::json;

use super::browser_auth::BrowserAuth;
use super::dashboard_data::HostedDashboard;
use super::storage::ObjectStorage;
use super::store::{ApiError, ApiResult};

pub(super) const CONNECTION_LIMIT: usize = 8;

pub(super) struct Selection {
  source: String,
  run_ids: Vec<String>,
  last_revision: Option<String>,
}

pub(super) struct Permit(Arc<AtomicUsize>);

impl Drop for Permit {
  fn drop(&mut self) {
    self.0.fetch_sub(1, Ordering::AcqRel);
  }
}

// fetch_update remains available on the project's Rust 1.89 minimum version.
#[allow(deprecated)]
pub(super) fn acquire(count: &Arc<AtomicUsize>) -> ApiResult<Permit> {
  count
    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
      (value < CONNECTION_LIMIT).then_some(value + 1)
    })
    .map(|_| Permit(Arc::clone(count)))
    .map_err(|_| {
      ApiError::new(
        503,
        "live update connections are full; polling remains available",
      )
    })
}

pub(super) struct Connection {
  pub stream: TcpStream,
  pub request: Request<Vec<u8>>,
  pub selection: Selection,
  pub _permit: Permit,
}

pub(super) fn validate<S: ObjectStorage>(
  request: &Request<Vec<u8>>,
  auth: &BrowserAuth,
  dashboard: &HostedDashboard<'_, S>,
) -> ApiResult<Selection> {
  auth.authorized(request)?;
  if request.method() != "GET" || !request.body().is_empty() {
    return Err(ApiError::new(
      400,
      "live updates require an empty GET request",
    ));
  }
  let raw = request.uri().query().unwrap_or_default();
  if raw.len() > 8192 {
    return Err(ApiError::new(400, "live update selection is too large"));
  }
  let mut source = None;
  let mut run_ids = Vec::new();
  for (name, value) in form_urlencoded::parse(raw.as_bytes()) {
    match name.as_ref() {
      "source" if source.is_none() && value.len() <= 512 => source = Some(value.into_owned()),
      "run_id" if run_ids.len() < 8 => run_ids.push(value.into_owned()),
      _ => return Err(ApiError::new(400, "invalid live update selection")),
    }
  }
  let source = source.unwrap_or_default();
  crate::dashboard::updates::validate_selection(&source, &run_ids)
    .map_err(|_| ApiError::new(400, "invalid live update selection"))?;
  dashboard
    .updates(&source, &run_ids)
    .map_err(|_| ApiError::new(400, "invalid live update scope"))?;
  let mut headers = request.headers().get_all("last-event-id").iter();
  let last_revision = headers
    .next()
    .map(|value| value.to_str())
    .transpose()
    .map_err(|_| ApiError::new(400, "invalid live update cursor"))?;
  if headers.next().is_some()
    || last_revision.is_some_and(|value| {
      value.is_empty() || value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit())
    })
  {
    return Err(ApiError::new(400, "invalid live update cursor"));
  }
  Ok(Selection {
    source,
    run_ids,
    last_revision: last_revision.map(str::to_owned),
  })
}

pub(super) fn serve<S: ObjectStorage>(
  mut connection: Connection,
  auth: &BrowserAuth,
  dashboard: &HostedDashboard<'_, S>,
  stop: &AtomicBool,
) -> std::io::Result<()> {
  if auth.authorized(&connection.request).is_err() {
    return Ok(());
  }
  connection
    .stream
    .set_write_timeout(Some(Duration::from_secs(2)))?;
  connection.stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\nX-Accel-Buffering: no\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: same-origin\r\nConnection: close\r\n\r\n: connected\n\n")?;
  connection.stream.flush()?;
  let mut heartbeat = Instant::now();
  while !stop.load(Ordering::Acquire) && auth.authorized(&connection.request).is_ok() {
    if let Ok(update) =
      dashboard.updates(&connection.selection.source, &connection.selection.run_ids)
      && let Some(revision) = update["catalog_revision"].as_str()
      && connection.selection.last_revision.as_deref() != Some(revision)
    {
      let data = json!({"catalog_revision": revision});
      write!(
        connection.stream,
        "event: updates\nid: {revision}\ndata: {data}\n\n"
      )?;
      connection.stream.flush()?;
      connection.selection.last_revision = Some(revision.to_owned());
      heartbeat = Instant::now();
    }
    if heartbeat.elapsed() >= Duration::from_secs(15) {
      connection.stream.write_all(b": keepalive\n\n")?;
      connection.stream.flush()?;
      heartbeat = Instant::now();
    }
    std::thread::sleep(Duration::from_millis(500));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn live_connections_are_bounded_and_release_capacity_on_disconnect() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut permits = (0..CONNECTION_LIMIT)
      .map(|_| acquire(&count).unwrap())
      .collect::<Vec<_>>();
    assert_eq!(acquire(&count).err().unwrap().status, 503);
    permits.pop();
    assert!(acquire(&count).is_ok());
    drop(permits);
    assert_eq!(count.load(Ordering::Acquire), 0);
  }
  fn session(auth: &BrowserAuth) -> Request<Vec<u8>> {
    let mut request = Request::builder()
      .method("GET")
      .uri("/api/events")
      .header("Host", "expri.example.com")
      .header("Origin", "https://expri.example.com")
      .body(Vec::new())
      .unwrap();
    let cookie = auth.login(&request, b"dashboard-password").unwrap();
    request
      .headers_mut()
      .insert("Cookie", cookie.split(';').next().unwrap().parse().unwrap());
    request
  }

  #[test]
  fn notifications_require_a_session_and_valid_bounded_selection() {
    let directory = tempfile::tempdir().unwrap();
    let store = super::super::store::Store::open(
      directory.path(),
      super::super::store::tests::MockStorage::default(),
    )
    .unwrap();
    let dashboard = HostedDashboard::new(&store).unwrap();
    let auth = BrowserAuth::new("https://expri.example.com", b"dashboard-password").unwrap();
    let mut request = session(&auth);
    assert!(validate(&request, &auth, &dashboard).is_ok());
    request
      .headers_mut()
      .insert("Origin", "https://attacker.invalid".parse().unwrap());
    assert_eq!(
      validate(&request, &auth, &dashboard).err().unwrap().status,
      403
    );
    request.headers_mut().remove("Origin");
    for path in [
      "/api/events?run_id=run",
      "/api/events?source=a&source=b",
      "/api/events?token=secret",
      "/api/events?source=hosted:project:worker&run_id=run&run_id=run",
    ] {
      *request.uri_mut() = path.parse().unwrap();
      assert_eq!(
        validate(&request, &auth, &dashboard).err().unwrap().status,
        400
      );
    }
    *request.uri_mut() = "/api/events".parse().unwrap();
    request
      .headers_mut()
      .insert("Last-Event-ID", "invalid".parse().unwrap());
    assert_eq!(
      validate(&request, &auth, &dashboard).err().unwrap().status,
      400
    );
    request.headers_mut().remove("Last-Event-ID");
    request.headers_mut().remove("Cookie");
    assert_eq!(
      validate(&request, &auth, &dashboard).err().unwrap().status,
      401
    );
  }

  #[test]
  fn committed_revisions_notify_without_holding_ingestion_and_logout_closes_stream() {
    use base64::Engine;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    let directory = tempfile::tempdir().unwrap();
    let store = super::super::store::Store::open(
      directory.path(),
      super::super::store::tests::MockStorage::default(),
    )
    .unwrap();
    let dashboard = HostedDashboard::new(&store).unwrap();
    let auth = BrowserAuth::new("https://expri.example.com", b"dashboard-password").unwrap();
    let request = session(&auth);
    let selection = validate(&request, &auth, &dashboard).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    client
      .set_read_timeout(Some(Duration::from_secs(3)))
      .unwrap();
    let (stream, _) = listener.accept().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let connection = Connection {
      stream,
      request: request.clone(),
      selection,
      _permit: acquire(&count).unwrap(),
    };
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
      let server = scope.spawn(|| serve(connection, &auth, &dashboard, &stop));
      let mut reader = BufReader::new(client);
      let mut line = String::new();
      loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
          break;
        }
      }
      let mut first = String::new();
      while !first.contains("event: updates\nid: 0\ndata: {\"catalog_revision\":\"0\"}\n\n") {
        line.clear();
        reader.read_line(&mut line).unwrap();
        first.push_str(&line);
      }
      store
        .execute(super::super::types::Request::AppendTracking {
          scope: super::super::store::tests::scope(),
          path: "logs/stdout.log".into(),
          offset: 0,
          data_base64: base64::engine::general_purpose::STANDARD.encode(b"private log bytes"),
        })
        .unwrap();
      let mut changed = String::new();
      while !changed.contains("data:") || !changed.ends_with("\n\n") {
        line.clear();
        reader.read_line(&mut line).unwrap();
        changed.push_str(&line);
      }
      assert!(changed.contains("id: 1\n"));
      assert!(!changed.contains("private log bytes"));
      auth.logout(&request).unwrap();
      let mut tail = String::new();
      reader.read_to_string(&mut tail).unwrap();
      server.join().unwrap().unwrap();
    });
    assert_eq!(count.load(Ordering::Acquire), 0);
  }
}
