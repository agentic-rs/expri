use http::{Request as HttpRequest, Response as HttpResponse};
use serde_json::json;

use super::browser_assets::{Asset, DashboardAssets};
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
    // Native login/logout form POSTs must retain their same-origin Origin.
    .header("Referrer-Policy", "same-origin")
    .header("Content-Security-Policy", MAIN_CSP)
    .body(body)
    .expect("static response headers")
}

pub(super) fn handle<S: ObjectStorage>(
  dashboard: &HostedDashboard<'_, S>,
  auth: &BrowserAuth,
  assets: &DashboardAssets,
  request: &HttpRequest<Vec<u8>>,
) -> HttpResponse<Vec<u8>> {
  match route(dashboard, auth, assets, request) {
    Ok(reply) => reply,
    Err(error) => response(
      error.status,
      "application/json",
      serde_json::to_vec(&json!({"error": error.message})).unwrap_or_default(),
    ),
  }
}

fn asset_response(status: u16, asset: Asset) -> HttpResponse<Vec<u8>> {
  let mut reply = response(status, asset.content_type, asset.body);
  if let Some(revision) = asset.revision {
    reply.headers_mut().insert(
      "X-Expri-Revision",
      revision.parse().expect("validated git commit"),
    );
  }
  reply
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

fn login_page(assets: &DashboardAssets, incorrect: bool) -> ApiResult<HttpResponse<Vec<u8>>> {
  let message = if incorrect {
    "<p class=\"login-error\" role=\"alert\">The dashboard password is incorrect.</p>"
  } else {
    ""
  };
  let mut page = assets.page("login.html")?;
  let html = String::from_utf8(page.body)
    .expect("validated UTF-8 login page")
    .replace("<!-- LOGIN_ERROR -->", message);
  page.body = html.into_bytes();
  Ok(asset_response(if incorrect { 401 } else { 200 }, page))
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
  assets: &DashboardAssets,
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
      return login_page(assets, false);
    }
    ("POST", "/login") => {
      if request.uri().query().is_some() {
        return Err(ApiError::new(400, "login does not accept query parameters"));
      }
      auth.check_boundary(request, true)?;
      return match auth.login(request, &password(request)?) {
        Ok(cookie) => Ok(redirect("/", Some(cookie))),
        Err(error) if error.status == 401 => login_page(assets, true),
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
  } else if !matches!(path, "/styles.css" | "/app.js")
    && !(assets.is_external() && path.starts_with("/assets/"))
  {
    auth.authorized(request)?;
  }
  if matches!(path, "/" | "/index.html" | "/styles.css" | "/app.js") || path.starts_with("/assets/")
  {
    if request.uri().query().is_some() {
      return Err(ApiError::new(
        400,
        "dashboard pages and assets do not accept query parameters",
      ));
    }
    if matches!(path, "/" | "/index.html") {
      return Ok(asset_response(200, assets.page("index.html")?));
    }
    if let Some(asset) = assets.asset(path)? {
      return Ok(asset_response(200, asset));
    }
  }
  let uri = request
    .uri()
    .path_and_query()
    .map_or("/", |uri| uri.as_str());
  let reply = crate::dashboard::server::route_content(dashboard, uri);
  let mut result = response(reply.status, reply.content_type, reply.body);
  if let Some(revision) = assets.current_revision()? {
    result.headers_mut().insert(
      "X-Expri-Revision",
      revision.parse().expect("validated git commit"),
    );
  }
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
    super::handle(
      &HostedDashboard::new(store).unwrap(),
      auth,
      &DashboardAssets::embedded(),
      request,
    )
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
      handle(&store, &auth, &request("GET", "/login", b"")).headers()["Referrer-Policy"],
      "same-origin"
    );
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
    let mut page = request("GET", "/", b"");
    page.headers_mut().insert("Cookie", cookie.parse().unwrap());
    assert_eq!(
      handle(&store, &auth, &page).headers()["Referrer-Policy"],
      "same-origin"
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
    let page = login_page(&DashboardAssets::embedded(), true).unwrap();
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
        ("Origin", "null"),
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

#[cfg(all(test, unix))]
mod external_tests {
  use super::*;
  use crate::service::store::Store;
  use crate::service::store::tests::MockStorage;
  use std::fs;
  use std::os::unix::fs::symlink;
  use std::path::Path;

  const FIRST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
  const SECOND: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

  fn bundle(root: &Path, commit: &str) {
    let directory = root.join("releases").join(commit);
    fs::create_dir_all(&directory).unwrap();
    for (name, content) in [
      (
        "index.html",
        "<main><script src=\"/app.js\"></script><link href=\"/styles.css\"><footer></footer></main>",
      ),
      (
        "login.html",
        "<main><link href=\"/styles.css\"><!-- LOGIN_ERROR --></main>",
      ),
      ("app.js", commit),
      ("styles.css", commit),
    ] {
      fs::write(directory.join(name), content).unwrap();
    }
    fs::write(
      directory.join("deployment.json"),
      serde_json::to_vec(&json!({"commit": commit, "branch": "codex/preview"})).unwrap(),
    )
    .unwrap();
  }

  fn request(method: &str, path: &str, cookie: Option<&str>) -> HttpRequest<Vec<u8>> {
    let mut builder = HttpRequest::builder()
      .method(method)
      .uri(path)
      .header("Host", "ab.expri.example.com");
    if let Some(cookie) = cookie {
      builder = builder.header("Cookie", cookie);
    }
    builder.body(Vec::new()).unwrap()
  }

  #[test]
  fn external_index_login_and_versioned_assets_preserve_auth_and_revision() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("assets");
    bundle(&root, FIRST);
    bundle(&root, SECOND);
    symlink(format!("releases/{FIRST}"), root.join("current")).unwrap();
    let assets = DashboardAssets::external(root.clone()).unwrap();
    let store = Store::open(&temporary.path().join("store"), MockStorage::default()).unwrap();
    let dashboard = HostedDashboard::new(&store).unwrap();
    let auth = BrowserAuth::new(
      "https://ab.expri.example.com",
      b"a-preview-dashboard-password",
    )
    .unwrap();
    let handle =
      |request: &HttpRequest<Vec<u8>>| super::handle(&dashboard, &auth, &assets, request);
    assert_eq!(handle(&request("GET", "/", None)).status(), 303);
    assert_eq!(handle(&request("GET", "/api/catalog", None)).status(), 401);
    let login = handle(&request("GET", "/login", None));
    assert_eq!(login.status(), 200);
    assert_eq!(login.headers()["X-Expri-Revision"], FIRST);
    assert!(
      String::from_utf8(login.body().clone())
        .unwrap()
        .contains(&format!("/assets/{FIRST}/styles.css"))
    );
    assert!(
      login.headers()["Content-Security-Policy"]
        .to_str()
        .unwrap()
        .contains("form-action 'self'")
    );
    let mut incorrect = request("POST", "/login", None);
    incorrect
      .headers_mut()
      .insert("Origin", "https://ab.expri.example.com".parse().unwrap());
    incorrect.headers_mut().insert(
      "Content-Type",
      "application/x-www-form-urlencoded".parse().unwrap(),
    );
    *incorrect.body_mut() = b"password=incorrect".to_vec();
    let reply = handle(&incorrect);
    assert_eq!(reply.status(), 401);
    assert_eq!(reply.headers()["X-Expri-Revision"], FIRST);
    assert!(
      String::from_utf8(reply.body().clone())
        .unwrap()
        .contains("password is incorrect")
    );
    let cookie = auth
      .login(&incorrect, b"a-preview-dashboard-password")
      .unwrap();
    let cookie = cookie.split(';').next().unwrap();
    let index = handle(&request("GET", "/index.html", Some(cookie)));
    assert_eq!(index.status(), 200);
    assert_eq!(index.headers()["X-Expri-Revision"], FIRST);
    assert!(
      String::from_utf8(index.body().clone())
        .unwrap()
        .contains(&format!("/assets/{FIRST}/app.js"))
    );
    assert_eq!(
      handle(&request("GET", "/api/catalog", Some(cookie))).headers()["X-Expri-Revision"],
      FIRST
    );
    fs::remove_file(root.join("current")).unwrap();
    symlink(format!("releases/{SECOND}"), root.join("current")).unwrap();
    assert_eq!(
      handle(&request("GET", "/login", None)).headers()["X-Expri-Revision"],
      SECOND
    );
    let retained = handle(&request("GET", &format!("/assets/{FIRST}/app.js"), None));
    assert_eq!(retained.status(), 200);
    assert_eq!(retained.headers()["X-Expri-Revision"], FIRST);
    assert_eq!(retained.body(), FIRST.as_bytes());
    for path in [
      "/assets/../app.js",
      "/assets/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/deployment.json",
      "/assets/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/app.js?unexpected=1",
    ] {
      assert!(
        handle(&request("GET", path, None))
          .status()
          .is_client_error(),
        "{path}"
      );
    }
    let mut hostile = request("GET", &format!("/assets/{FIRST}/app.js"), None);
    hostile
      .headers_mut()
      .insert("Host", "hostile.example.com".parse().unwrap());
    assert_eq!(handle(&hostile).status(), 403);
    assert_eq!(
      handle(&request("POST", "/app.js", Some(cookie))).status(),
      405
    );
  }
}
