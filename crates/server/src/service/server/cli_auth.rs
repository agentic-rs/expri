//! Dedicated CLI authentication never creates or accepts browser sessions.

use std::collections::VecDeque;

use base64::Engine;
use expri_core::service_auth::{
  CLI_TOKEN_PREFIX, LoginRequest, LoginResponse, MAX_AUTH_REQUEST_BYTES,
};
use sha2::{Digest, Sha256};

use super::*;

const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_ATTEMPTS: usize = 10;

#[derive(Default)]
pub(super) struct LoginLimiter {
  attempts: Mutex<VecDeque<Instant>>,
}

impl LoginLimiter {
  fn check(&self, now: Instant) -> ApiResult<()> {
    let mut attempts = self
      .attempts
      .lock()
      .map_err(|_| ApiError::new(503, "CLI login is temporarily unavailable"))?;
    while attempts
      .front()
      .is_some_and(|first| now.saturating_duration_since(*first) >= LOGIN_WINDOW)
    {
      attempts.pop_front();
    }
    if attempts.len() >= LOGIN_ATTEMPTS {
      return Err(ApiError::new(
        429,
        "CLI login attempt limit reached; retry later",
      ));
    }
    attempts.push_back(now);
    Ok(())
  }
}

pub(super) fn now_ms() -> i64 {
  chrono::Utc::now().timestamp_millis()
}

pub(super) fn token_digest(token: &str) -> [u8; 32] {
  Sha256::digest(token.as_bytes()).into()
}

pub(super) fn primary_site<'a, T>(
  auth: &'a Auth,
  request: &HttpRequest<T>,
) -> ApiResult<&'a DashboardSite> {
  let site = auth
    .sites
    .iter()
    .find(|site| !site.preview)
    .ok_or_else(|| ApiError::new(404, "CLI login is unavailable on this server"))?;
  if request_host(request)? != Some(site.auth.authority()) {
    return Err(ApiError::new(
      403,
      "CLI authentication is not allowed for this request",
    ));
  }
  Ok(site)
}

fn reject_browser_headers(request: &HttpRequest<Vec<u8>>, login: bool) -> ApiResult<()> {
  if request.headers().keys().any(|name| {
    matches!(name.as_str(), "origin" | "cookie" | "referer")
      || name.as_str().starts_with("sec-fetch-")
      || (login && name.as_str() == "authorization")
  }) {
    return Err(ApiError::new(
      403,
      "CLI authentication is not allowed for this request",
    ));
  }
  Ok(())
}

fn json_content_type(request: &HttpRequest<Vec<u8>>) -> ApiResult<()> {
  let mut values = request.headers().get_all(http::header::CONTENT_TYPE).iter();
  if !values
    .next()
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| {
      value
        .split(';')
        .next()
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
    })
    || values.next().is_some()
  {
    return Err(ApiError::new(415, "CLI login requires application/json"));
  }
  Ok(())
}

fn generate_token() -> ApiResult<String> {
  let mut bytes = [0_u8; 32];
  rustls::crypto::ring::default_provider()
    .secure_random
    .fill(&mut bytes)
    .map_err(|_| ApiError::new(503, "CLI login is temporarily unavailable"))?;
  Ok(format!(
    "{CLI_TOKEN_PREFIX}{}",
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
  ))
}

pub(super) fn handle<S: ObjectStorage>(
  store: &Store<S>,
  auth: &Auth,
  request: &HttpRequest<Vec<u8>>,
) -> HttpResponse<Vec<u8>> {
  let result = exchange(store, auth, request);
  let mut response = match result {
    Ok(response) => response,
    Err(error) => api_reply(Err(error)),
  };
  if response.status() == http::StatusCode::TOO_MANY_REQUESTS {
    response
      .headers_mut()
      .insert(http::header::RETRY_AFTER, "60".parse().unwrap());
  }
  response
}

fn exchange<S: ObjectStorage>(
  store: &Store<S>,
  auth: &Auth,
  request: &HttpRequest<Vec<u8>>,
) -> ApiResult<HttpResponse<Vec<u8>>> {
  if request.uri().query().is_some() {
    return Err(ApiError::new(
      400,
      "CLI authentication does not accept query parameters",
    ));
  }
  if request.method() != http::Method::POST {
    return Err(ApiError::new(405, "CLI authentication requires POST"));
  }
  let site = primary_site(auth, request)?;
  let login = request.uri().path() == "/v1/login";
  reject_browser_headers(request, login)?;
  if request.body().len() > MAX_AUTH_REQUEST_BYTES {
    return Err(ApiError::new(
      413,
      "CLI authentication request exceeds its size limit",
    ));
  }
  if login {
    json_content_type(request)?;
    auth.login_limiter.check(Instant::now())?;
    let request: LoginRequest = serde_json::from_slice(request.body())
      .map_err(|_| ApiError::new(400, "invalid CLI login request"))?;
    request
      .validate()
      .map_err(|_| ApiError::new(400, "invalid CLI login request"))?;
    if !site.auth.verifies_credential(request.password.as_bytes()) {
      return Err(ApiError::new(401, "CLI login was rejected"));
    }
    let token = generate_token()?;
    let epoch = site.auth.cli_credential_epoch(&auth.owner);
    store.issue_cli_credential(&token_digest(&token), &epoch, now_ms())?;
    let reply = LoginResponse {
      token,
      expires_at_ms: None,
    };
    let bytes = serde_json::to_vec(&reply)
      .map_err(|_| ApiError::new(500, "cannot encode CLI login response"))?;
    Ok(super::super::browser::response(
      200,
      "application/json",
      bytes,
    ))
  } else {
    if !request.body().is_empty() {
      return Err(ApiError::new(400, "CLI logout requires an empty body"));
    }
    match auth.role(store, request)? {
      Role::IssuedOwner { digest } => {
        let epoch = site.auth.cli_credential_epoch(&auth.owner);
        store.revoke_cli_credential(&digest, &epoch, now_ms())?;
      }
      _ => {
        return Err(ApiError::new(
          409,
          "static credentials cannot be revoked through CLI logout",
        ));
      }
    }
    Ok(super::super::browser::response(
      204,
      "application/json",
      Vec::new(),
    ))
  }
}

#[cfg(test)]
mod tests;
