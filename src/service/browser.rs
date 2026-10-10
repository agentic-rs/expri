use http::{Request as HttpRequest, Response as HttpResponse};
use serde_json::json;

mod project_management;

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
  allow_project_deletion: bool,
  request: &HttpRequest<Vec<u8>>,
) -> HttpResponse<Vec<u8>> {
  match route(dashboard, auth, assets, allow_project_deletion, request) {
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

fn redirect(location: &str, cookie: Option<String>) -> HttpResponse<Vec<u8>> {
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

#[cfg(test)]
fn login_page(assets: &DashboardAssets, incorrect: bool) -> ApiResult<HttpResponse<Vec<u8>>> {
  login_page_for_run(assets, incorrect, "")
}

fn login_page_for_run(
  assets: &DashboardAssets,
  incorrect: bool,
  query: &str,
) -> ApiResult<HttpResponse<Vec<u8>>> {
  let message = if incorrect {
    "<p class=\"login-error\" role=\"alert\">The dashboard password is incorrect.</p>"
  } else {
    ""
  };
  let mut page = assets.page("login.html")?;
  let html = String::from_utf8(page.body)
    .expect("validated UTF-8 login page")
    .replace("<!-- LOGIN_ERROR -->", message)
    .replace(
      "action=\"/login\"",
      &format!("action=\"/login{}\"", query.replace('&', "&amp;")),
    );
  page.body = html.into_bytes();
  Ok(asset_response(if incorrect { 401 } else { 200 }, page))
}

/// Preserve only a bounded run identity through sign-in, never a redirect URL.
fn run_query(request: &HttpRequest<Vec<u8>>) -> ApiResult<String> {
  let Some(raw) = request.uri().query() else {
    return Ok(String::new());
  };
  if raw.len() > 512 {
    return Err(ApiError::new(400, "invalid dashboard run link"));
  }
  let mut fields = std::collections::BTreeMap::new();
  for (name, value) in form_urlencoded::parse(raw.as_bytes()) {
    if !matches!(name.as_ref(), "project_id" | "origin" | "run_id")
      || super::types::validate_component(&value).is_err()
      || fields
        .insert(name.into_owned(), value.into_owned())
        .is_some()
    {
      return Err(ApiError::new(400, "invalid dashboard run link"));
    }
  }
  if fields.len() != 3 {
    return Err(ApiError::new(
      400,
      "dashboard run link requires project_id, origin, and run_id",
    ));
  }
  let query = form_urlencoded::Serializer::new(String::new())
    .extend_pairs(
      ["project_id", "origin", "run_id"]
        .into_iter()
        .map(|name| (name, fields[name].as_str())),
    )
    .finish();
  Ok(format!("?{query}"))
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
  allow_project_deletion: bool,
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
      return login_page_for_run(assets, false, &run_query(request)?);
    }
    ("POST", "/login") => {
      let query = run_query(request)?;
      auth.check_boundary(request, true)?;
      return match auth.login(request, &password(request)?) {
        Ok(cookie) => Ok(redirect(&format!("/{query}"), Some(cookie))),
        Err(error) if error.status == 401 => login_page_for_run(assets, true, &query),
        Err(error) => Err(error),
      };
    }
    ("POST", "/logout") => {
      if request.uri().query().is_some() || !request.body().is_empty() {
        return Err(ApiError::new(400, "logout does not accept fields"));
      }
      return Ok(redirect("/login", Some(auth.logout(request)?)));
    }
    ("POST", "/api/projects/delete") => {
      return project_management::delete(dashboard, auth, allow_project_deletion, request);
    }
    _ if !matches!(method, "GET" | "HEAD") => {
      return Err(ApiError::new(405, "dashboard routes require GET or HEAD"));
    }
    _ => {}
  }
  if matches!(path, "/" | "/index.html") {
    let query = run_query(request)?;
    match auth.authorized_page(request) {
      Ok(()) => {}
      Err(error) if error.status == 401 => return Ok(redirect(&format!("/login{query}"), None)),
      Err(error) => return Err(error),
    }
  } else if !matches!(path, "/styles.css" | "/app.js")
    && !(assets.is_external() && path.starts_with("/assets/"))
  {
    auth.authorized(request)?;
  }
  if matches!(path, "/" | "/index.html" | "/styles.css" | "/app.js") || path.starts_with("/assets/")
  {
    if request.uri().query().is_some() && !matches!(path, "/" | "/index.html") {
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
  if let Some(reply) = project_management::read(dashboard, allow_project_deletion, request)? {
    return Ok(reply);
  }
  if path == "/api/artifact" {
    return match crate::dashboard::server::artifact_download(dashboard, uri) {
      Ok(download) => download_response(method, download),
      Err(reply) => Ok(response(reply.status, reply.content_type, reply.body)),
    };
  }
  if path == "/api/input" {
    let (project_id, input_id) = input_selection(request)?;
    return download_response(method, dashboard.input_download(&project_id, &input_id)?);
  }
  if path == "/api/result-zip" {
    let (source, run_id) = result_zip_selection(request)?;
    let download = dashboard
      .result_zip_download(&source, &run_id)
      .map_err(|_| ApiError::new(404, "result ZIP is unavailable"))?;
    return download_response(method, download);
  }
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

fn result_zip_selection(request: &HttpRequest<Vec<u8>>) -> ApiResult<(String, String)> {
  let query = request.uri().query().unwrap_or_default();
  if query.len() > 8192 || !request.body().is_empty() {
    return Err(ApiError::new(400, "invalid result ZIP selection"));
  }
  let mut fields = std::collections::BTreeMap::new();
  for (name, value) in form_urlencoded::parse(query.as_bytes()) {
    if !matches!(name.as_ref(), "source" | "run_id")
      || value.is_empty()
      || fields
        .insert(name.into_owned(), value.into_owned())
        .is_some()
    {
      return Err(ApiError::new(400, "invalid result ZIP selection"));
    }
  }
  let source = fields
    .remove("source")
    .ok_or_else(|| ApiError::new(400, "missing result ZIP source"))?;
  let run_id = fields
    .remove("run_id")
    .ok_or_else(|| ApiError::new(400, "missing result ZIP run"))?;
  crate::dashboard::updates::validate_selection(&source, std::slice::from_ref(&run_id))
    .map_err(|_| ApiError::new(400, "invalid result ZIP selection"))?;
  Ok((source, run_id))
}

fn input_selection(request: &HttpRequest<Vec<u8>>) -> ApiResult<(String, String)> {
  let query = request.uri().query().unwrap_or_default();
  if query.len() > 512 || !request.body().is_empty() {
    return Err(ApiError::new(400, "invalid input selection"));
  }
  let mut fields = std::collections::BTreeMap::new();
  for (name, value) in form_urlencoded::parse(query.as_bytes()) {
    if !matches!(name.as_ref(), "project_id" | "input_id")
      || super::types::validate_component(&value).is_err()
      || fields
        .insert(name.into_owned(), value.into_owned())
        .is_some()
    {
      return Err(ApiError::new(400, "invalid input selection"));
    }
  }
  if fields.len() != 2 {
    return Err(ApiError::new(
      400,
      "input selection requires project_id and input_id",
    ));
  }
  Ok((
    fields.remove("project_id").unwrap(),
    fields.remove("input_id").unwrap(),
  ))
}

fn download_response(
  method: &str,
  download: crate::dashboard::artifacts::Download,
) -> ApiResult<HttpResponse<Vec<u8>>> {
  let crate::dashboard::artifacts::Download::Cloud {
    url,
    size,
    filename,
  } = download
  else {
    return Err(ApiError::new(500, "invalid hosted artifact download"));
  };
  let mut reply = response(
    if method == "HEAD" { 200 } else { 303 },
    "application/octet-stream",
    Vec::new(),
  );
  reply
    .headers_mut()
    .insert("Referrer-Policy", "no-referrer".parse().unwrap());
  reply.headers_mut().insert(
    "Content-Security-Policy",
    "default-src 'none'; sandbox".parse().unwrap(),
  );
  if method == "HEAD" {
    // Signed S3 GET links cannot be followed with HEAD; describe the attachment.
    reply.headers_mut().insert("Content-Length", size.into());
    reply.headers_mut().insert(
      "Content-Disposition",
      crate::dashboard::artifacts::disposition(&filename)
        .parse()
        .expect("safe attachment header"),
    );
  } else {
    reply.headers_mut().insert(
      "Location",
      url
        .parse()
        .map_err(|_| ApiError::new(502, "invalid object storage download URL"))?,
    );
  }
  Ok(reply)
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
      false,
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
  fn artifact_download_requires_session_and_scoped_output_before_signed_redirect() {
    use crate::service::types::{CompletedPart, FileTarget, Request, RunScope};
    let temporary = tempfile::tempdir().unwrap();
    let storage = MockStorage::default();
    let store = Store::open(temporary.path(), storage.clone()).unwrap();
    let scope = RunScope {
      project_id: "project".into(),
      origin: "worker".into(),
      run_id: "run-1".into(),
    };
    store
      .execute(Request::BeginUpload {
        upload_id: "download-checkpoint".into(),
        target: FileTarget::Run {
          scope,
          path: "outputs/model.pt".into(),
        },
        size: 8,
        sha256: "a".repeat(64),
      })
      .unwrap();
    storage.stage("download-checkpoint", 8);
    store
      .execute(Request::RecordPart {
        upload_id: "download-checkpoint".into(),
        part: CompletedPart {
          part_number: 1,
          etag: "part".into(),
        },
      })
      .unwrap();
    store
      .execute(Request::CompleteUpload {
        upload_id: "download-checkpoint".into(),
      })
      .unwrap();
    let auth = BrowserAuth::new("https://expri.example.com", b"dashboard-password").unwrap();
    let path =
      "/api/artifact?source=hosted%3Aproject%3Aworker&run_id=run-1&path=outputs%2Fmodel.pt";
    let anonymous = handle(&store, &auth, &request("GET", path, b""));
    assert_eq!(anonymous.status(), 401);
    assert!(!anonymous.headers().contains_key("Location"));
    let anonymous_list = handle(
      &store,
      &auth,
      &request(
        "GET",
        "/api/artifacts?source=hosted%3Aproject%3Aworker&run_id=run-1",
        b"",
      ),
    );
    assert_eq!(anonymous_list.status(), 401);
    let mut login = request("POST", "/login", b"password=dashboard-password");
    login
      .headers_mut()
      .insert("Origin", "https://expri.example.com".parse().unwrap());
    login.headers_mut().insert(
      "Content-Type",
      "application/x-www-form-urlencoded".parse().unwrap(),
    );
    let logged_in = handle(&store, &auth, &login);
    let cookie = logged_in.headers()["Set-Cookie"]
      .to_str()
      .unwrap()
      .split(';')
      .next()
      .unwrap();
    let mut download = request("GET", path, b"");
    download
      .headers_mut()
      .insert("Cookie", cookie.parse().unwrap());
    let reply = handle(&store, &auth, &download);
    assert_eq!(reply.status(), 303);
    assert_eq!(reply.headers()["Referrer-Policy"], "no-referrer");
    assert!(
      reply.headers()["Location"]
        .to_str()
        .unwrap()
        .starts_with("https://storage.invalid/projects/project/runs/worker/run-1/")
    );
    assert!(reply.body().is_empty());
    *download.method_mut() = http::Method::HEAD;
    let head = handle(&store, &auth, &download);
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["Content-Length"], "8");
    assert!(!head.headers().contains_key("Location"));
    assert!(head.body().is_empty());
    *download.method_mut() = http::Method::GET;
    download
      .headers_mut()
      .insert("Origin", "https://attacker.invalid".parse().unwrap());
    let forbidden = handle(&store, &auth, &download);
    assert_eq!(forbidden.status(), 403);
    assert!(!forbidden.headers().contains_key("Location"));
    download.headers_mut().remove("Origin");
    *download.uri_mut() =
      "/api/artifact?source=hosted%3Aother%3Aworker&run_id=run-1&path=outputs%2Fmodel.pt"
        .parse()
        .unwrap();
    assert_eq!(handle(&store, &auth, &download).status(), 404);
    *download.uri_mut() =
      "/api/artifact?source=hosted%3Aproject%3Aworker&run_id=run-1&path=inputs%2Fprivate"
        .parse()
        .unwrap();
    assert_eq!(handle(&store, &auth, &download).status(), 400);
  }

  #[test]
  fn private_input_listing_and_download_require_a_session_and_exact_project_scope() {
    use crate::service::types::{CompletedPart, FileTarget, Request};
    let temporary = tempfile::tempdir().unwrap();
    let storage = MockStorage::default();
    let store = Store::open(temporary.path(), storage.clone()).unwrap();
    store
      .execute(Request::BeginUpload {
        upload_id: "private-input-upload".into(),
        target: FileTarget::Input {
          project_id: "project".into(),
          input_id: "dataset-v1".into(),
        },
        size: 8,
        sha256: "a".repeat(64),
      })
      .unwrap();
    storage.stage("private-input-upload", 8);
    store
      .execute(Request::RecordPart {
        upload_id: "private-input-upload".into(),
        part: CompletedPart {
          part_number: 1,
          etag: "part".into(),
        },
      })
      .unwrap();
    store
      .execute(Request::CompleteUpload {
        upload_id: "private-input-upload".into(),
      })
      .unwrap();
    let auth = BrowserAuth::new("https://expri.example.com", b"dashboard-password").unwrap();
    let path = "/api/input?project_id=project&input_id=dataset-v1";
    assert_eq!(
      handle(&store, &auth, &request("GET", path, b"")).status(),
      401
    );
    assert_eq!(
      handle(
        &store,
        &auth,
        &request("GET", "/api/storage?project_id=project&kind=input", b"")
      )
      .status(),
      401
    );
    let mut login = request("POST", "/login", b"");
    login
      .headers_mut()
      .insert("Origin", "https://expri.example.com".parse().unwrap());
    let cookie = auth.login(&login, b"dashboard-password").unwrap();
    let mut input = request("GET", path, b"");
    input
      .headers_mut()
      .insert("Cookie", cookie.split(';').next().unwrap().parse().unwrap());
    let listing = handle(
      &store,
      &auth,
      &request_with_cookie("/api/storage?project_id=project&kind=input", &cookie),
    );
    assert_eq!(listing.status(), 200);
    let body = std::str::from_utf8(listing.body()).unwrap();
    assert!(body.contains(path));
    assert!(!body.contains("storage.invalid") && !body.contains("projects/project/inputs/"));
    let reply = handle(&store, &auth, &input);
    assert_eq!(reply.status(), 303);
    assert_eq!(reply.headers()["Referrer-Policy"], "no-referrer");
    assert!(
      reply.headers()["Location"]
        .to_str()
        .unwrap()
        .contains("projects/project/inputs/dataset-v1/")
    );
    assert!(reply.body().is_empty());
    *input.method_mut() = http::Method::HEAD;
    let head = handle(&store, &auth, &input);
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["Content-Length"], "8");
    assert!(
      head.headers()["Content-Disposition"]
        .to_str()
        .unwrap()
        .contains("dataset-v1")
    );
    assert!(!head.headers().contains_key("Location"));
    *input.method_mut() = http::Method::GET;
    for invalid in [
      "/api/input?project_id=other&input_id=dataset-v1",
      "/api/input?project_id=project&input_id=missing",
    ] {
      *input.uri_mut() = invalid.parse().unwrap();
      assert_eq!(handle(&store, &auth, &input).status(), 404);
    }
    for invalid in [
      "/api/input?project_id=project&input_id=dataset-v1&input_id=dataset-v1",
      "/api/input?project_id=project&input_id=dataset-v1&token=secret",
      "/api/input?project_id=project&input_id=..",
      "/api/artifact?source=hosted-project%3Aproject&run_id=worker%3Arun&path=inputs%2Fdataset-v1",
    ] {
      *input.uri_mut() = invalid.parse().unwrap();
      assert_eq!(handle(&store, &auth, &input).status(), 400);
    }
    *input.uri_mut() = path.parse().unwrap();
    input
      .headers_mut()
      .insert("Origin", "https://outside.invalid".parse().unwrap());
    assert_eq!(handle(&store, &auth, &input).status(), 403);
  }

  fn request_with_cookie(path: &str, cookie: &str) -> HttpRequest<Vec<u8>> {
    let mut request = request("GET", path, b"");
    request
      .headers_mut()
      .insert("Cookie", cookie.split(';').next().unwrap().parse().unwrap());
    request
  }

  #[test]
  fn result_zip_route_requires_session_and_valid_selection() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let auth = BrowserAuth::new("https://expri.example.com", b"dashboard-password").unwrap();
    let mut login = request("POST", "/login", b"");
    login
      .headers_mut()
      .insert("Origin", "https://expri.example.com".parse().unwrap());
    let cookie = auth.login(&login, b"dashboard-password").unwrap();
    let endpoint = "/api/result-zip";
    let valid = format!("{endpoint}?source=hosted:project:worker&run_id=run-1");
    let mut download = request("GET", &valid, b"");
    assert_eq!(handle(&store, &auth, &download).status(), 401);
    download
      .headers_mut()
      .insert("Cookie", cookie.split(';').next().unwrap().parse().unwrap());
    assert_eq!(handle(&store, &auth, &download).status(), 404);
    for path in [
      endpoint.to_owned(),
      format!("{valid}&run_id=run-2"),
      format!("{valid}&path=inputs/private"),
      format!(
        "{endpoint}?source=hosted-project:project&run_id={}",
        "r".repeat(257)
      ),
    ] {
      *download.uri_mut() = path.parse().unwrap();
      let reply = handle(&store, &auth, &download);
      assert_eq!(reply.status(), 400);
      assert!(!reply.headers().contains_key("Location"));
    }
    *download.uri_mut() = valid.parse().unwrap();
    download
      .headers_mut()
      .insert("Origin", "https://attacker.invalid".parse().unwrap());
    assert_eq!(handle(&store, &auth, &download).status(), 403);
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
  fn run_links_survive_authentication_without_accepting_redirect_urls() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path(), MockStorage::default()).unwrap();
    let auth = BrowserAuth::new(
      "https://expri.example.com",
      b"a-dedicated-dashboard-password",
    )
    .unwrap();
    let query = "?project_id=vision&origin=gpu-1&run_id=run-123";
    let reply = handle(&store, &auth, &request("GET", &format!("/{query}"), b""));
    assert_eq!(reply.status(), 303);
    assert_eq!(reply.headers()["Location"], format!("/login{query}"));
    let page = handle(
      &store,
      &auth,
      &request("GET", &format!("/login{query}"), b""),
    );
    assert_eq!(page.status(), 200);
    assert!(
      String::from_utf8_lossy(page.body())
        .contains("action=\"/login?project_id=vision&amp;origin=gpu-1&amp;run_id=run-123\"")
    );
    let mut login = request(
      "POST",
      &format!("/login{query}"),
      b"password=a-dedicated-dashboard-password",
    );
    login.headers_mut().insert(
      "Content-Type",
      "application/x-www-form-urlencoded".parse().unwrap(),
    );
    login
      .headers_mut()
      .insert("Origin", "https://expri.example.com".parse().unwrap());
    let reply = handle(&store, &auth, &login);
    assert_eq!(reply.status(), 303);
    assert_eq!(reply.headers()["Location"], format!("/{query}"));
    let cookie = reply.headers()["Set-Cookie"]
      .to_str()
      .unwrap()
      .split(';')
      .next()
      .unwrap();
    let mut dashboard = request("GET", &format!("/{query}"), b"");
    dashboard
      .headers_mut()
      .insert("Cookie", cookie.parse().unwrap());
    assert_eq!(handle(&store, &auth, &dashboard).status(), 200);
    for invalid in [
      "?return_to=https://evil.example",
      "?project_id=vision&origin=gpu-1",
      "?project_id=vision&origin=gpu-1&run_id=run-123&run_id=run-456",
      "?project_id=vision&origin=gpu-1&run_id=%22%3E%3Cscript%3E",
    ] {
      for path in ["/", "/login"] {
        assert_eq!(
          handle(
            &store,
            &auth,
            &request("GET", &format!("{path}{invalid}"), b"")
          )
          .status(),
          400
        );
      }
    }
    assert_eq!(
      handle(
        &store,
        &auth,
        &request("GET", &format!("/app.js{query}"), b"")
      )
      .status(),
      400
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
      "/api/projects",
      "/api/updates?source=hosted:project:worker&run_id=run-1",
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
  fn update_probe_requires_dashboard_session_and_keeps_responses_private() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Store::open(temporary.path(), MockStorage::default()).unwrap();
    let auth = BrowserAuth::new("https://expri.example.com", b"a-dashboard-password").unwrap();
    assert_eq!(
      handle(&store, &auth, &request("GET", "/api/updates", b"")).status(),
      401
    );
    let mut login = request("POST", "/login", b"");
    login
      .headers_mut()
      .insert("Origin", "https://expri.example.com".parse().unwrap());
    let issued = auth.login(&login, b"a-dashboard-password").unwrap();
    let cookie = issued.split(';').next().unwrap();
    let mut probe = request(
      "GET",
      "/api/updates?source=hosted:project:worker&run_id=missing",
      b"",
    );
    probe
      .headers_mut()
      .insert("Cookie", cookie.parse().unwrap());
    let reply = handle(&store, &auth, &probe);
    assert_eq!(reply.status(), 200);
    assert_eq!(reply.headers()["Cache-Control"], "no-store");
    let value: serde_json::Value = serde_json::from_slice(reply.body()).unwrap();
    assert_eq!(value["runs"][0]["missing"], true);
    let mut invalid = request(
      "GET",
      "/api/updates?source=hosted:project:worker&run_id=..%2Foutside",
      b"",
    );
    invalid
      .headers_mut()
      .insert("Cookie", cookie.parse().unwrap());
    assert_eq!(handle(&store, &auth, &invalid).status(), 400);
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
  fn authenticated_dashboard_rejects_general_data_mutations() {
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
        "/api/updates",
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
      |request: &HttpRequest<Vec<u8>>| super::handle(&dashboard, &auth, &assets, false, request);
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
