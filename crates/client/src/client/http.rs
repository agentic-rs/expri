use std::io::Read;
use std::path::Path;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::Value;

use super::super::types::{ClientConfig, MAX_REQUEST, Request, Response, validate_client_token};
use super::fs::message;
use crate::error::{ExpriError, Result};

/// Select the same process-wide provider used by the hosted S3 implementation.
fn init_tls() {
  let _ = rustls::crypto::ring::default_provider().install_default();
}

fn resolve_token(
  config: ClientConfig,
  read_env: impl FnOnce(&str) -> std::result::Result<String, std::env::VarError>,
) -> Result<String> {
  let token = match (config.token, config.token_env) {
    (Some(token), None) => token,
    (None, Some(name)) => {
      read_env(&name).map_err(|_| message("service token environment variable is missing"))?
    }
    _ => {
      return Err(message(
        "service client configuration requires exactly one of token or token_env",
      ));
    }
  };
  validate_client_token(&token)?;
  Ok(token)
}

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
    init_tls();
    let config = crate::load_client_config(path)?;
    let url = reqwest::Url::parse(&config.url).map_err(|_| message("invalid service URL"))?;
    let token = resolve_token(config, |name| std::env::var(name))?;
    let build = |timeout| {
      Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .map_err(|_| message("cannot initialize service HTTP client"))
    };
    Ok(Self {
      endpoint: format!("{}/v1/request", url.as_str().trim_end_matches('/')),
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
      .map_err(|_| ExpriError::ServiceUnavailable {
        reading_response: false,
      })?;
    let status = response.status();
    let mut bytes = Vec::new();
    response
      .take(MAX_REQUEST as u64 + 1)
      .read_to_end(&mut bytes)
      .map_err(|_| {
        if status.is_success() {
          ExpriError::ServiceUnavailable {
            reading_response: true,
          }
        } else {
          ExpriError::ServiceRejected {
            status: status.as_u16(),
            detail: "request rejected; response could not be read".into(),
          }
        }
      })?;
    if bytes.len() > MAX_REQUEST {
      return Err(if status.is_success() {
        message("service response exceeds its size limit")
      } else {
        ExpriError::ServiceRejected {
          status: status.as_u16(),
          detail: "request rejected; response exceeds its size limit".into(),
        }
      });
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
    serde_json::from_slice(&bytes).map_err(|error| {
      message(format!(
        "invalid service response: {}",
        self.redact(&error.to_string())
      ))
    })
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

#[cfg(test)]
pub(in crate::client) mod tests;
