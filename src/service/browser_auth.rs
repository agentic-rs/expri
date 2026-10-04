use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine;
use http::Request as HttpRequest;
use reqwest::Url;
use subtle::ConstantTimeEq;

use super::store::{ApiError, ApiResult};
use crate::error::{ExpriError, Result};

const COOKIE_NAME: &str = "__Host-expri_session";
const SESSION_LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);
const SESSION_LIMIT: usize = 64;
const SESSION_ID_LENGTH: usize = 43;

struct Session {
  id: String,
  expires_at: Instant,
}

/// Browser sessions authorize read-only dashboard routes only. API bearer roles
/// stay separate so an owner or worker token never needs to reach the browser.
pub(super) struct BrowserAuth {
  origin: String,
  host: String,
  credential: Vec<u8>,
  sessions: Mutex<VecDeque<Session>>,
}

impl BrowserAuth {
  pub fn new(public_url: &str, credential: &[u8]) -> Result<Self> {
    let url = Url::parse(public_url)
      .map_err(|_| ExpriError::Message("dashboard public_url must be an HTTPS origin".into()))?;
    if url.scheme() != "https"
      || url.host_str().is_none()
      || !url.username().is_empty()
      || url.password().is_some()
      || url.path() != "/"
      || url.query().is_some()
      || url.fragment().is_some()
    {
      return Err(ExpriError::Message(
        "dashboard public_url must be an HTTPS origin without credentials, a path, query, or fragment"
          .into(),
      ));
    }
    if credential.is_empty() || credential.len() > 1024 {
      return Err(ExpriError::Message(
        "dashboard credential must contain 1 to 1024 bytes".into(),
      ));
    }
    let origin = url.origin().ascii_serialization();
    let host = origin
      .strip_prefix("https://")
      .expect("validated HTTPS origin")
      .to_owned();
    Ok(Self {
      origin,
      host,
      credential: credential.to_vec(),
      sessions: Mutex::new(VecDeque::new()),
    })
  }

  /// Use the configured public authority rather than trusting forwarded headers.
  /// The reverse proxy must preserve the browser's Host and Origin headers.
  pub fn check_boundary<T>(&self, request: &HttpRequest<T>, require_origin: bool) -> ApiResult<()> {
    self.check_host(request)?;
    match single_header(request, "origin")? {
      Some(origin) if origin != self.origin => {
        return Err(ApiError::new(403, "dashboard origin is not allowed"));
      }
      None if require_origin => {
        return Err(ApiError::new(403, "dashboard origin is required"));
      }
      _ => {}
    }
    if let Some(site) = single_header(request, "sec-fetch-site")?
      && !matches!(site, "same-origin" | "none")
    {
      return Err(ApiError::new(
        403,
        "cross-origin dashboard request is not allowed",
      ));
    }
    Ok(())
  }

  /// Public login and asset GETs can be reached through an external link. They
  /// still require the configured authority and never disclose private data.
  pub fn check_host<T>(&self, request: &HttpRequest<T>) -> ApiResult<()> {
    if single_header(request, "host")? != Some(self.host.as_str()) {
      return Err(ApiError::new(403, "dashboard host is not allowed"));
    }
    Ok(())
  }

  pub fn authorized<T>(&self, request: &HttpRequest<T>) -> ApiResult<()> {
    self.check_boundary(request, false)?;
    self.authorized_session(request)
  }

  /// HTML navigation can arrive from another site; Strict cookies prevent such
  /// navigation from sending a session. Protected HTML must also deny framing.
  pub fn authorized_page<T>(&self, request: &HttpRequest<T>) -> ApiResult<()> {
    self.check_host(request)?;
    self.authorized_session(request)
  }

  fn authorized_session<T>(&self, request: &HttpRequest<T>) -> ApiResult<()> {
    let id =
      session_cookie(request)?.ok_or_else(|| ApiError::new(401, "dashboard login required"))?;
    let mut sessions = self.sessions()?;
    prune_expired(&mut sessions, Instant::now());
    if sessions.iter().any(|session| same_session(&session.id, id)) {
      Ok(())
    } else {
      Err(ApiError::new(401, "dashboard login required"))
    }
  }

  pub fn login<T>(&self, request: &HttpRequest<T>, supplied: &[u8]) -> ApiResult<String> {
    self.check_boundary(request, true)?;
    let previous = session_cookie(request)?;
    if !bool::from(supplied.ct_eq(self.credential.as_slice())) {
      return Err(ApiError::new(401, "dashboard credential is incorrect"));
    }
    let id = new_session_id()?;
    let now = Instant::now();
    let mut sessions = self.sessions()?;
    prune_expired(&mut sessions, now);
    if let Some(previous) = previous {
      sessions.retain(|session| !same_session(&session.id, previous));
    }
    while sessions.len() >= SESSION_LIMIT {
      sessions.pop_front();
    }
    sessions.push_back(Session {
      id: id.clone(),
      expires_at: now + SESSION_LIFETIME,
    });
    Ok(format!(
      "{COOKIE_NAME}={id}; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
      SESSION_LIFETIME.as_secs()
    ))
  }

  pub fn logout<T>(&self, request: &HttpRequest<T>) -> ApiResult<String> {
    self.check_boundary(request, true)?;
    let id = session_cookie(request)?;
    let mut sessions = self.sessions()?;
    prune_expired(&mut sessions, Instant::now());
    if let Some(id) = id {
      sessions.retain(|session| !same_session(&session.id, id));
    }
    Ok(format!(
      "{COOKIE_NAME}=; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
    ))
  }

  fn sessions(&self) -> ApiResult<MutexGuard<'_, VecDeque<Session>>> {
    self
      .sessions
      .lock()
      .map_err(|_| ApiError::new(500, "dashboard session state is unavailable"))
  }
}

fn single_header<'a, T>(request: &'a HttpRequest<T>, name: &str) -> ApiResult<Option<&'a str>> {
  let mut values = request.headers().get_all(name).iter();
  let value = values
    .next()
    .map(|value| value.to_str())
    .transpose()
    .map_err(|_| ApiError::new(403, "dashboard request header is invalid"))?;
  if values.next().is_some() {
    return Err(ApiError::new(403, "dashboard request header is ambiguous"));
  }
  Ok(value)
}

fn session_cookie<T>(request: &HttpRequest<T>) -> ApiResult<Option<&str>> {
  let mut session = None;
  for header in request.headers().get_all("cookie") {
    let header = header
      .to_str()
      .map_err(|_| ApiError::new(401, "dashboard session cookie is invalid"))?;
    for pair in header.split(';') {
      let Some((name, value)) = pair.trim().split_once('=') else {
        return Err(ApiError::new(401, "dashboard session cookie is invalid"));
      };
      let name = name.trim();
      let value = value.trim();
      if name != COOKIE_NAME {
        continue;
      }
      if session.is_some()
        || value.len() != SESSION_ID_LENGTH
        || !value
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
      {
        return Err(ApiError::new(401, "dashboard session cookie is invalid"));
      }
      session = Some(value);
    }
  }
  Ok(session)
}

fn new_session_id() -> ApiResult<String> {
  let mut random = [0_u8; 32];
  rustls::crypto::ring::default_provider()
    .secure_random
    .fill(&mut random)
    .map_err(|_| ApiError::new(500, "dashboard session could not be created"))?;
  Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
}

fn same_session(left: &str, right: &str) -> bool {
  bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

fn prune_expired(sessions: &mut VecDeque<Session>, now: Instant) {
  sessions.retain(|session| session.expires_at > now);
}

#[cfg(test)]
mod tests {
  use super::*;

  const PUBLIC_URL: &str = "https://expri.example.test";
  const CREDENTIAL: &[u8] = b"a separate dashboard credential";

  fn auth() -> BrowserAuth {
    BrowserAuth::new(PUBLIC_URL, CREDENTIAL).unwrap()
  }

  fn request(origin: Option<&str>, cookie: Option<&str>) -> HttpRequest<()> {
    let mut builder = HttpRequest::builder().header("host", "expri.example.test");
    if let Some(origin) = origin {
      builder = builder.header("origin", origin);
    }
    if let Some(cookie) = cookie {
      builder = builder.header("cookie", cookie);
    }
    builder.body(()).unwrap()
  }

  fn cookie(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
  }

  #[test]
  fn configured_public_url_is_only_an_https_origin() {
    for invalid in [
      "http://expri.example.test",
      "https://user@expri.example.test",
      "https://expri.example.test/dashboard",
      "https://expri.example.test/?query=yes",
      "https://expri.example.test/#fragment",
      "not a URL",
    ] {
      assert!(BrowserAuth::new(invalid, CREDENTIAL).is_err());
    }
    assert!(BrowserAuth::new(PUBLIC_URL, b"").is_err());
    let auth = BrowserAuth::new("https://expri.example.test:8443/", CREDENTIAL).unwrap();
    let request = HttpRequest::builder()
      .header("host", "expri.example.test:8443")
      .header("origin", "https://expri.example.test:8443")
      .body(())
      .unwrap();
    assert!(auth.check_boundary(&request, true).is_ok());
  }

  #[test]
  fn login_requires_the_exact_host_and_origin() {
    let auth = auth();
    for origin in [None, Some("null"), Some("https://evil.example.test")] {
      assert_eq!(
        auth
          .login(&request(origin, None), CREDENTIAL)
          .unwrap_err()
          .status,
        403
      );
    }
    let wrong_host = HttpRequest::builder()
      .header("host", "evil.example.test")
      .header("origin", PUBLIC_URL)
      .body(())
      .unwrap();
    assert_eq!(auth.login(&wrong_host, CREDENTIAL).unwrap_err().status, 403);
    let mut duplicate = request(Some(PUBLIC_URL), None);
    duplicate
      .headers_mut()
      .append("origin", PUBLIC_URL.parse().unwrap());
    assert_eq!(auth.login(&duplicate, CREDENTIAL).unwrap_err().status, 403);
    let mut duplicate = request(Some(PUBLIC_URL), None);
    duplicate
      .headers_mut()
      .append("host", "expri.example.test".parse().unwrap());
    assert_eq!(auth.login(&duplicate, CREDENTIAL).unwrap_err().status, 403);
    assert_eq!(
      auth
        .login(&request(Some(PUBLIC_URL), None), b"incorrect")
        .unwrap_err()
        .status,
      401
    );
  }

  #[test]
  fn sessions_are_secure_expire_and_are_revoked() {
    let auth = auth();
    let issued = auth
      .login(&request(Some(PUBLIC_URL), None), CREDENTIAL)
      .unwrap();
    for attribute in [
      "Secure",
      "HttpOnly",
      "SameSite=Strict",
      "Path=/",
      "Max-Age=28800",
    ] {
      assert!(issued.contains(attribute));
    }
    let browser = request(None, Some(cookie(&issued)));
    assert!(auth.authorized(&browser).is_ok());
    assert_eq!(auth.logout(&browser).unwrap_err().status, 403);
    assert!(auth.authorized(&browser).is_ok());
    let logout = auth
      .logout(&request(Some(PUBLIC_URL), Some(cookie(&issued))))
      .unwrap();
    assert!(logout.contains("Max-Age=0"));
    assert_eq!(auth.authorized(&browser).unwrap_err().status, 401);

    let issued = auth
      .login(&request(Some(PUBLIC_URL), None), CREDENTIAL)
      .unwrap();
    auth.sessions().unwrap()[0].expires_at = Instant::now() - Duration::from_secs(1);
    assert_eq!(
      auth
        .authorized(&request(None, Some(cookie(&issued))))
        .unwrap_err()
        .status,
      401
    );
    assert!(auth.sessions().unwrap().is_empty());
  }

  #[test]
  fn reauthentication_rotates_session_and_sessions_are_bounded() {
    let auth = auth();
    let first = auth
      .login(&request(Some(PUBLIC_URL), None), CREDENTIAL)
      .unwrap();
    let rotated = auth
      .login(&request(Some(PUBLIC_URL), Some(cookie(&first))), CREDENTIAL)
      .unwrap();
    assert_ne!(cookie(&first), cookie(&rotated));
    assert_eq!(
      auth
        .authorized(&request(None, Some(cookie(&first))))
        .unwrap_err()
        .status,
      401
    );
    for _ in 0..SESSION_LIMIT {
      auth
        .login(&request(Some(PUBLIC_URL), None), CREDENTIAL)
        .unwrap();
    }
    assert_eq!(auth.sessions().unwrap().len(), SESSION_LIMIT);
    assert_eq!(
      auth
        .authorized(&request(None, Some(cookie(&rotated))))
        .unwrap_err()
        .status,
      401
    );
  }

  #[test]
  fn hostile_origins_and_fetch_metadata_cannot_read_a_session() {
    let auth = auth();
    let issued = auth
      .login(&request(Some(PUBLIC_URL), None), CREDENTIAL)
      .unwrap();
    let hostile = request(Some("https://evil.example.test"), Some(cookie(&issued)));
    assert_eq!(auth.authorized(&hostile).unwrap_err().status, 403);
    for site in ["cross-site", "same-site", "unexpected"] {
      let mut browser = request(None, Some(cookie(&issued)));
      browser
        .headers_mut()
        .insert("sec-fetch-site", site.parse().unwrap());
      assert_eq!(auth.authorized(&browser).unwrap_err().status, 403);
    }
    for site in ["same-origin", "none"] {
      let mut browser = request(None, Some(cookie(&issued)));
      browser
        .headers_mut()
        .insert("sec-fetch-site", site.parse().unwrap());
      assert!(auth.authorized(&browser).is_ok());
    }
  }

  #[test]
  fn public_pages_allow_external_navigation_without_weakening_api_boundary() {
    let auth = auth();
    let mut navigation = request(Some("https://external.example.test"), None);
    navigation
      .headers_mut()
      .insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert!(auth.check_host(&navigation).is_ok());
    assert_eq!(auth.authorized_page(&navigation).unwrap_err().status, 401);
    assert_eq!(auth.authorized(&navigation).unwrap_err().status, 403);
    assert_eq!(auth.login(&navigation, CREDENTIAL).unwrap_err().status, 403);
    assert_eq!(auth.logout(&navigation).unwrap_err().status, 403);
    let issued = auth
      .login(&request(Some(PUBLIC_URL), None), CREDENTIAL)
      .unwrap();
    assert!(
      auth
        .authorized_page(&request(None, Some(cookie(&issued))))
        .is_ok()
    );
    navigation
      .headers_mut()
      .insert("host", "external.example.test".parse().unwrap());
    assert_eq!(auth.check_host(&navigation).unwrap_err().status, 403);
  }

  #[test]
  fn duplicate_and_malformed_session_cookies_are_rejected() {
    let auth = auth();
    let issued = auth
      .login(&request(Some(PUBLIC_URL), None), CREDENTIAL)
      .unwrap();
    let valid = cookie(&issued);
    let duplicate = format!("{valid}; {valid}");
    assert_eq!(
      auth
        .authorized(&request(None, Some(&duplicate)))
        .unwrap_err()
        .status,
      401
    );
    let spaced_duplicate = format!("{valid}; {COOKIE_NAME} = other");
    assert_eq!(
      auth
        .authorized(&request(None, Some(&spaced_duplicate)))
        .unwrap_err()
        .status,
      401
    );
    let mut browser = request(None, Some(valid));
    browser
      .headers_mut()
      .append("cookie", valid.parse().unwrap());
    assert_eq!(auth.authorized(&browser).unwrap_err().status, 401);
    for invalid in [
      "__Host-expri_session=",
      "__Host-expri_session=short",
      "broken",
    ] {
      assert_eq!(
        auth
          .authorized(&request(None, Some(invalid)))
          .unwrap_err()
          .status,
        401
      );
    }
    assert!(
      auth
        .authorized(&request(None, Some(&format!("other=value; {valid}"))))
        .is_ok()
    );
  }

  #[test]
  fn bearer_credentials_do_not_authorize_browser_access() {
    let auth = auth();
    let mut browser = request(None, None);
    browser.headers_mut().insert(
      "authorization",
      "Bearer a separate dashboard credential".parse().unwrap(),
    );
    assert_eq!(auth.authorized(&browser).unwrap_err().status, 401);
  }
}
