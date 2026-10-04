use http::{Request as HttpRequest, Response as HttpResponse};
use serde_json::json;

use super::browser_auth::BrowserAuth;
use super::dashboard_data::HostedDashboard;
use super::storage::ObjectStorage;
use super::store::{ApiError, ApiResult};

const MAIN_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; frame-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
const CHART_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'self'; base-uri 'none'; form-action 'none'";

pub(super) fn response(status: u16, content_type: &str, body: Vec<u8>) -> HttpResponse<Vec<u8>> {
  HttpResponse::builder()
    .status(status)
    .header("Content-Type", content_type)
    .header("Content-Length", body.len())
    .header("Connection", "close")
    .header("Cache-Control", "no-store")
    .header("X-Content-Type-Options", "nosniff")
    .header("Referrer-Policy", "no-referrer")
    .header("Content-Security-Policy", MAIN_CSP)
    .body(body)
    .expect("static response headers")
}

pub(super) fn handle<S: ObjectStorage>(
  dashboard: &HostedDashboard<'_, S>,
  auth: &BrowserAuth,
  request: &HttpRequest<Vec<u8>>,
) -> HttpResponse<Vec<u8>> {
  match route(dashboard, auth, request) {
    Ok(reply) => reply,
    Err(error) => response(
      error.status,
      "application/json",
      serde_json::to_vec(&json!({"error": error.message})).unwrap_or_default(),
    ),
  }
}

fn redirect(location: &'static str, cookie: Option<String>) -> HttpResponse<Vec<u8>> {
  let mut reply = response(303, "text/plain; charset=utf-8", Vec::new());
  reply
    .headers_mut()
    .insert("Location", location.parse().unwrap());
  if let Some(cookie) = cookie {
    reply.headers_mut().insert(
      "Set-Cookie",
      cookie.parse().expect("generated session cookie"),
    );
  }
  reply
}

fn login_page(incorrect: bool) -> HttpResponse<Vec<u8>> {
  let message = if incorrect {
    "<p class=\"login-error\" role=\"alert\">The dashboard password is incorrect.</p>"
  } else {
    ""
  };
  let html =
    include_str!("../../dashboard_web/login.html").replace("<!-- LOGIN_ERROR -->", message);
  response(
    if incorrect { 401 } else { 200 },
    "text/html; charset=utf-8",
    html.into_bytes(),
  )
}

fn password(request: &HttpRequest<Vec<u8>>) -> ApiResult<Vec<u8>> {
  let mut content_types = request.headers().get_all("content-type").iter();
  if !content_types
    .next()
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| {
      value.split(';').next().is_some_and(|mime| {
        mime
          .trim()
          .eq_ignore_ascii_case("application/x-www-form-urlencoded")
      })
    })
    || content_types.next().is_some()
  {
    return Err(ApiError::new(415, "login requires an HTML form"));
  }
  if request.body().len() > 4096 {
    return Err(ApiError::new(413, "login form exceeds 4 KiB"));
  }
  let fields: Vec<_> = form_urlencoded::parse(request.body()).collect();
  if fields.len() != 1 || fields[0].0 != "password" || fields[0].1.len() > 256 {
    return Err(ApiError::new(400, "login requires one password field"));
  }
  Ok(fields[0].1.as_bytes().to_vec())
}

fn route<S: ObjectStorage>(
  dashboard: &HostedDashboard<'_, S>,
  auth: &BrowserAuth,
  request: &HttpRequest<Vec<u8>>,
) -> ApiResult<HttpResponse<Vec<u8>>> {
  auth.check_host(request)?;
  let path = request.uri().path();
  let method = request.method().as_str();
  if matches!(method, "GET" | "HEAD") && !request.body().is_empty() {
    return Err(ApiError::new(
      400,
      "dashboard read requests must not contain a body",
    ));
  }
  match (method, path) {
    ("GET" | "HEAD", "/login") => {
      if request.uri().query().is_some() {
        return Err(ApiError::new(400, "login does not accept query parameters"));
      }
      return Ok(login_page(false));
    }
    ("POST", "/login") => {
      if request.uri().query().is_some() {
        return Err(ApiError::new(400, "login does not accept query parameters"));
      }
      auth.check_boundary(request, true)?;
      return match auth.login(request, &password(request)?) {
        Ok(cookie) => Ok(redirect("/", Some(cookie))),
        Err(error) if error.status == 401 => Ok(login_page(true)),
        Err(error) => Err(error),
      };
    }
    ("POST", "/logout") => {
      if request.uri().query().is_some() || !request.body().is_empty() {
        return Err(ApiError::new(400, "logout does not accept fields"));
      }
      return Ok(redirect("/login", Some(auth.logout(request)?)));
    }
    _ if !matches!(method, "GET" | "HEAD") => {
      return Err(ApiError::new(405, "dashboard routes require GET or HEAD"));
    }
    _ => {}
  }
  if matches!(path, "/" | "/index.html") {
    match auth.authorized_page(request) {
      Ok(()) => {}
      Err(error) if error.status == 401 => return Ok(redirect("/login", None)),
      Err(error) => return Err(error),
    }
  } else if !matches!(path, "/styles.css" | "/app.js") {
    auth.authorized(request)?;
  }
  let uri = request
    .uri()
    .path_and_query()
    .map_or("/", |uri| uri.as_str());
  let reply = crate::dashboard::server::route_content(dashboard, uri);
  let mut result = response(reply.status, reply.content_type, reply.body);
  if reply.chart {
    result
      .headers_mut()
      .insert("Content-Security-Policy", CHART_CSP.parse().unwrap());
  }
  Ok(result)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::service::store::Store;
  use crate::service::store::tests::MockStorage;

  fn handle<S: ObjectStorage>(
    store: &Store<S>,
    auth: &BrowserAuth,
    request: &HttpRequest<Vec<u8>>,
  ) -> HttpResponse<Vec<u8>> {
    super::handle(&HostedDashboard::new(store).unwrap(), auth, request)
  }

  fn request(method: &str, path: &str, body: &[u8]) -> HttpRequest<Vec<u8>> {
    HttpRequest::builder()
      .method(method)
      .uri(path)
      .header("Host", "expri.example.com")
      .body(body.to_vec())
      .unwrap()
  }

  #[test]
  fn login_guards_and_session_routes_are_separate_from_bearer_api() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path(), MockStorage::default()).unwrap();
    let auth = BrowserAuth::new(
      "https://expri.example.com",
      b"a-dedicated-dashboard-password",
    )
    .unwrap();
    assert_eq!(
      handle(&store, &auth, &request("GET", "/", b"")).status(),
      303
    );
    let mut api = request("GET", "/api/catalog", b"");
    api.headers_mut().insert(
      "Authorization",
      "Bearer a-dedicated-dashboard-password".parse().unwrap(),
    );
    assert_eq!(handle(&store, &auth, &api).status(), 401);
    let mut login = request("POST", "/login", b"password=a-dedicated-dashboard-password");
    login.headers_mut().insert(
      "Content-Type",
      "application/x-www-form-urlencoded".parse().unwrap(),
    );
    assert_eq!(handle(&store, &auth, &login).status(), 403);
    login
      .headers_mut()
      .insert("Origin", "https://expri.example.com".parse().unwrap());
    let reply = handle(&store, &auth, &login);
    assert_eq!(reply.status(), 303);
    let cookie = reply.headers()["Set-Cookie"]
      .to_str()
      .unwrap()
      .split(';')
      .next()
      .unwrap();
    let mut catalog = request("GET", "/api/catalog", b"");
    catalog
      .headers_mut()
      .insert("Cookie", cookie.parse().unwrap());
    let reply = handle(&store, &auth, &catalog);
    assert_eq!(reply.status(), 200);
    assert_eq!(
      serde_json::from_slice::<serde_json::Value>(reply.body()).unwrap()["access_mode"],
      "hosted"
    );
    let mut logout = request("POST", "/logout", b"");
    logout
      .headers_mut()
      .insert("Cookie", cookie.parse().unwrap());
    logout
      .headers_mut()
      .insert("Origin", "https://expri.example.com".parse().unwrap());
    assert_eq!(handle(&store, &auth, &logout).status(), 303);
    assert_eq!(handle(&store, &auth, &catalog).status(), 401);
  }

  #[test]
  fn login_never_reflects_supplied_credentials_or_accepts_duplicate_fields() {
    let mut req = request("POST", "/login", b"password=secret&password=other");
    req.headers_mut().insert(
      "Content-Type",
      "application/x-www-form-urlencoded".parse().unwrap(),
    );
    assert_eq!(password(&req).unwrap_err().status, 400);
    let page = login_page(true);
    assert!(
      !String::from_utf8(page.body().clone())
        .unwrap()
        .contains("<!-- LOGIN_ERROR -->")
    );
    assert!(
      page.headers()["Content-Security-Policy"]
        .to_str()
        .unwrap()
        .contains("form-action 'self'")
    );
  }

  #[test]
  fn authenticated_private_routes_reject_hostile_origins_and_fetch_metadata() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path(), MockStorage::default()).unwrap();
    let auth = BrowserAuth::new(
      "https://expri.example.com",
      b"a-dedicated-dashboard-password",
    )
    .unwrap();
    let mut login = request("POST", "/login", b"");
    login
      .headers_mut()
      .insert("Origin", "https://expri.example.com".parse().unwrap());
    let issued = auth
      .login(&login, b"a-dedicated-dashboard-password")
      .unwrap();
    let cookie = issued.split(';').next().unwrap();
    for path in [
      "/api/catalog",
      "/api/runs",
      "/api/run?run_id=run-1",
      "/api/log?run_id=run-1",
      "/api/compare?run_id=run-1&run_id=run-2",
      "/api/chart?run_id=run-1",
    ] {
      for (header, value) in [
        ("Origin", "https://hostile.example.net"),
        ("Sec-Fetch-Site", "cross-site"),
        ("Sec-Fetch-Site", "same-site"),
      ] {
        let mut private = request("GET", path, b"");
        private
          .headers_mut()
          .insert("Cookie", cookie.parse().unwrap());
        private.headers_mut().insert(header, value.parse().unwrap());
        assert_eq!(
          handle(&store, &auth, &private).status(),
          403,
          "{path}: {header}"
        );
      }
    }
    let mut private = request("GET", "/api/catalog", b"");
    private
      .headers_mut()
      .insert("Cookie", cookie.parse().unwrap());
    private
      .headers_mut()
      .insert("Sec-Fetch-Site", "same-origin".parse().unwrap());
    assert_eq!(handle(&store, &auth, &private).status(), 200);
  }

  #[test]
  fn public_login_and_assets_allow_external_navigation_without_private_reads() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path(), MockStorage::default()).unwrap();
    let auth = BrowserAuth::new(
      "https://expri.example.com",
      b"a-dedicated-dashboard-password",
    )
    .unwrap();
    for (path, status) in [
      ("/login", 200),
      ("/styles.css", 200),
      ("/app.js", 200),
      ("/", 303),
    ] {
      let mut navigation = request("GET", path, b"");
      navigation
        .headers_mut()
        .insert("Sec-Fetch-Site", "cross-site".parse().unwrap());
      navigation
        .headers_mut()
        .insert("Origin", "https://external.example.net".parse().unwrap());
      let reply = handle(&store, &auth, &navigation);
      assert_eq!(reply.status(), status, "{path}");
      if path == "/" {
        assert_eq!(reply.headers()["Location"], "/login");
      }
      navigation
        .headers_mut()
        .insert("Host", "external.example.net".parse().unwrap());
      assert_eq!(handle(&store, &auth, &navigation).status(), 403, "{path}");
    }
  }

  #[test]
  fn authenticated_dashboard_has_no_data_mutation_routes() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path(), MockStorage::default()).unwrap();
    let auth = BrowserAuth::new(
      "https://expri.example.com",
      b"a-dedicated-dashboard-password",
    )
    .unwrap();
    let mut login = request("POST", "/login", b"");
    login
      .headers_mut()
      .insert("Origin", "https://expri.example.com".parse().unwrap());
    let issued = auth
      .login(&login, b"a-dedicated-dashboard-password")
      .unwrap();
    let cookie = issued.split(';').next().unwrap();
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
      for path in [
        "/",
        "/api/catalog",
        "/api/runs",
        "/api/chart",
        "/unexpected",
      ] {
        let mut mutation = request(method, path, br#"{"operation":"write"}"#);
        mutation
          .headers_mut()
          .insert("Cookie", cookie.parse().unwrap());
        mutation
          .headers_mut()
          .insert("Origin", "https://expri.example.com".parse().unwrap());
        assert_eq!(
          handle(&store, &auth, &mutation).status(),
          405,
          "{method} {path}"
        );
      }
    }
  }
}
