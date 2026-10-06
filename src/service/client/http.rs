use std::io::Read;
use std::path::Path;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::Value;

use super::super::types::{ClientConfig, MAX_REQUEST, Request, Response};
use super::fs::{self, message};
use crate::error::{ExpriError, Result};

pub(super) struct Api {
  pub(super) endpoint: String,
  token: String,
  control: Client,
  pub(super) objects: Client,
}

impl Api {
  pub(super) fn redact(&self, detail: &str) -> String {
    detail
      .replace(&self.token, "[redacted]")
      .chars()
      .filter(|character| !character.is_control())
      .take(512)
      .collect()
  }

  pub(super) fn new(path: &Path) -> Result<Self> {
    super::super::storage::init_tls();
    let config: ClientConfig = toml::from_str(
      &String::from_utf8(fs::read_bounded(path, 32 * 1024)?)
        .map_err(|_| message("service client config must be UTF-8"))?,
    )?;
    let url = reqwest::Url::parse(&config.url).map_err(|_| message("invalid service URL"))?;
    if !matches!(url.scheme(), "http" | "https")
      || !url.username().is_empty()
      || url.password().is_some()
      || url.query().is_some()
      || url.fragment().is_some()
    {
      return Err(message(
        "service URL must be HTTP(S) without embedded credentials, query or fragment",
      ));
    }
    let token = std::env::var(&config.token_env)
      .map_err(|_| message("service token environment variable is missing"))?;
    if token.is_empty() || token.chars().any(char::is_control) {
      return Err(message("service token environment variable is invalid"));
    }
    let build = |timeout| {
      Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .map_err(|_| message("cannot initialize service HTTP client"))
    };
    Ok(Self {
      endpoint: format!("{}/v1/request", config.url.trim_end_matches('/')),
      token,
      control: build(Duration::from_secs(30))?,
      objects: build(Duration::from_secs(900))?,
    })
  }

  pub(super) fn request(&self, request: &Request) -> Result<Response> {
    let body = serde_json::to_vec(request)?;
    if body.len() > MAX_REQUEST {
      return Err(message("service request exceeds its size limit"));
    }
    let response = self
      .control
      .post(&self.endpoint)
      .bearer_auth(&self.token)
      .header(reqwest::header::CONTENT_TYPE, "application/json")
      .body(body)
      .send()
      .map_err(|_| message("service request failed; saved work can be retried"))?;
    let status = response.status();
    let mut bytes = Vec::new();
    response
      .take(MAX_REQUEST as u64 + 1)
      .read_to_end(&mut bytes)
      .map_err(|_| message("service response could not be read; saved work can be retried"))?;
    if bytes.len() > MAX_REQUEST {
      return Err(message("service response exceeds its size limit"));
    }
    if !status.is_success() {
      let detail = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| {
          value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
        })
        .unwrap_or_else(|| "request rejected".into());
      return Err(ExpriError::ServiceRejected {
        status: status.as_u16(),
        detail: self.redact(&detail),
      });
    }
    serde_json::from_slice(&bytes).map_err(Into::into)
  }

  pub(super) fn signed_url(&self, request: Request) -> Result<String> {
    let Response::Url { url } = self.request(&request)? else {
      return Err(message("service did not return an object URL"));
    };
    let parsed = reqwest::Url::parse(&url).map_err(|_| message("invalid object URL"))?;
    if !matches!(parsed.scheme(), "http" | "https")
      || !parsed.username().is_empty()
      || parsed.password().is_some()
      || parsed.fragment().is_some()
    {
      return Err(message("invalid object URL"));
    }
    Ok(url)
  }
}
