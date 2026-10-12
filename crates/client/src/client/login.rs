//! Password exchange and credential revocation, separate from authenticated requests.

use std::io::Read;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use expri_core::service::validate_client_token;
use expri_core::service_auth::{
  LoginRequest, LoginResponse, MAX_AUTH_REQUEST_BYTES, MAX_AUTH_RESPONSE_BYTES,
};
use reqwest::blocking::Client;

use super::fs::message;
use super::http::resolve_token;
use crate::error::{ExpriError, Result};

/// Exchange a server password for an opaque, individually revocable CLI token.
/// This function never saves credentials, follows redirects, or sends a bearer token.
pub fn login(url: &str, password: &str) -> Result<LoginResponse> {
  let request = LoginRequest {
    password: password.into(),
  };
  request.validate()?;
  let endpoint = endpoint(url, "login")?;
  let body =
    serde_json::to_vec(&request).map_err(|_| message("cannot encode CLI login request"))?;
  if body.len() > MAX_AUTH_REQUEST_BYTES {
    return Err(message("CLI login request exceeds its size limit"));
  }
  let response = client(&endpoint)?
    .post(endpoint)
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .body(body)
    .send()
    .map_err(|_| ExpriError::ServiceUnavailable {
      reading_response: false,
    })?;
  if response.status() != reqwest::StatusCode::OK {
    return Err(rejection(response.status().as_u16(), true));
  }
  let mut bytes = Vec::new();
  response
    .take(MAX_AUTH_RESPONSE_BYTES as u64 + 1)
    .read_to_end(&mut bytes)
    .map_err(|_| ExpriError::ServiceUnavailable {
      reading_response: true,
    })?;
  if bytes.len() > MAX_AUTH_RESPONSE_BYTES {
    return Err(message("CLI login response exceeds its size limit"));
  }
  let response: LoginResponse = serde_json::from_slice(&bytes)
    .map_err(|_| message("server returned an invalid CLI login response"))?;
  if response.token == password {
    return Err(message("server returned an invalid CLI login response"));
  }
  response.validate(now_ms()?)?;
  Ok(response)
}

/// Validate transport policy before collecting a password, without network access.
pub fn validate_login_url(url: &str) -> Result<()> {
  endpoint(url, "login").map(|_| ())
}

/// Revoke the provided issued token on the same configured server.
/// Static credentials may authenticate this endpoint but are left unchanged.
pub fn revoke_login(url: &str, token: &str) -> Result<()> {
  validate_client_token(token)?;
  let endpoint = endpoint(url, "logout")?;
  let response = client(&endpoint)?
    .post(endpoint)
    .bearer_auth(token)
    .body(Vec::new())
    .send()
    .map_err(|_| ExpriError::ServiceUnavailable {
      reading_response: false,
    })?;
  if response.status() != reqwest::StatusCode::NO_CONTENT {
    return Err(rejection(response.status().as_u16(), false));
  }
  Ok(())
}

/// Resolve the selected client's credentials and request server-side revocation.
/// Local configuration is never edited by the transport layer.
pub fn logout(config: &Path) -> Result<()> {
  let config = crate::load_client_config(config)?;
  let url = config.url.clone();
  let token = resolve_token(config, |name| std::env::var(name))?;
  revoke_login(&url, &token)
}

fn endpoint(base: &str, operation: &str) -> Result<reqwest::Url> {
  let url = reqwest::Url::parse(base).map_err(|_| message("invalid login server URL"))?;
  if !matches!(url.scheme(), "http" | "https")
    || url.host_str().is_none()
    || !url.username().is_empty()
    || url.password().is_some()
    || url.query().is_some()
    || url.fragment().is_some()
  {
    return Err(message(
      "login server URL must be HTTP(S) without embedded credentials, query or fragment",
    ));
  }
  if url.scheme() == "http" && !loopback(&url) {
    return Err(message(
      "CLI login and logout require HTTPS except for a loopback server",
    ));
  }
  reqwest::Url::parse(&format!(
    "{}/v1/{operation}",
    url.as_str().trim_end_matches('/')
  ))
  .map_err(|_| message("invalid login server URL"))
}

fn loopback(url: &reqwest::Url) -> bool {
  url.host_str().is_some_and(|host| {
    host == "localhost"
      || host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
  })
}

fn client(endpoint: &reqwest::Url) -> Result<Client> {
  let _ = rustls::crypto::ring::default_provider().install_default();
  let builder = Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    .connect_timeout(Duration::from_secs(10))
    .timeout(Duration::from_secs(30));
  // Local credentials must never reach an implicitly configured proxy.
  let builder = if loopback(endpoint) {
    builder.no_proxy()
  } else {
    builder
  };
  builder
    .build()
    .map_err(|_| message("cannot initialize CLI authentication client"))
}

fn rejection(status: u16, login: bool) -> ExpriError {
  let detail = match (status, login) {
    (401, true) => "login password was rejected",
    (401, false) => "CLI token was rejected or already revoked",
    (404 | 405, true) => {
      "server does not support CLI login; update the server or configure a client token"
    }
    (404 | 405, false) => "server does not support CLI logout; update the server",
    (301 | 302 | 303 | 307 | 308, _) => {
      "CLI authentication redirect was rejected; use the server's direct HTTPS URL"
    }
    (429, _) => "too many CLI authentication requests; retry later",
    (_, true) => "CLI login request was rejected",
    (_, false) => "CLI logout request was rejected",
  };
  ExpriError::ServiceRejected {
    status,
    detail: detail.into(),
  }
}

fn now_ms() -> Result<i64> {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .ok()
    .and_then(|time| i64::try_from(time.as_millis()).ok())
    .ok_or_else(|| message("system clock is unavailable for CLI token validation"))
}

#[cfg(test)]
mod tests;
