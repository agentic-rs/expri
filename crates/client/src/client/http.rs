use std::io::Read;
use std::path::Path;
use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::types::{ClientConfig, MAX_REQUEST, Request, Response, validate_client_token};
use super::fs::message;
use crate::error::{ExpriError, Result};

/// Select the same process-wide provider used by the hosted S3 implementation.
fn init_tls() {
  let _ = rustls::crypto::ring::default_provider().install_default();
}

pub(super) fn resolve_token(
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

/// An immutable credential binding. Serialize only into the worker's private
/// job registry; this type deliberately does not implement Debug.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedClient {
  endpoint: String,
  token: String,
}

impl ResolvedClient {
  pub fn resolve(path: &Path) -> Result<Self> {
    Self::from_config(crate::load_client_config(path)?)
  }

  pub fn from_config(config: ClientConfig) -> Result<Self> {
    Self::from_config_with_env(config, |name| std::env::var(name))
  }

  fn from_config_with_env(
    config: ClientConfig,
    read_env: impl FnOnce(&str) -> std::result::Result<String, std::env::VarError>,
  ) -> Result<Self> {
    let url = reqwest::Url::parse(&config.url).map_err(|_| message("invalid service URL"))?;
    let token = resolve_token(config, read_env)?;
    let resolved = Self {
      endpoint: format!("{}/v1/request", url.as_str().trim_end_matches('/')),
      token,
    };
    resolved.validate()?;
    Ok(resolved)
  }

  pub fn validate(&self) -> Result<()> {
    let url = reqwest::Url::parse(&self.endpoint)
      .map_err(|_| message("invalid resolved service endpoint"))?;
    if !matches!(url.scheme(), "http" | "https")
      || !url.username().is_empty()
      || url.password().is_some()
      || url.query().is_some()
      || url.fragment().is_some()
      || !url.path().ends_with("/v1/request")
    {
      return Err(message("invalid resolved service endpoint"));
    }
    validate_client_token(&self.token)
  }

  pub fn endpoint(&self) -> &str {
    &self.endpoint
  }

  pub fn redact(&self, detail: &str) -> String {
    detail
      .replace(&self.token, "[redacted]")
      .chars()
      .filter(|character| !character.is_control())
      .take(512)
      .collect()
  }
}

#[derive(Clone)]
pub(crate) struct Api {
  pub(super) endpoint: String,
  token: String,
  control: Client,
  pub(super) objects: Client,
  async_objects: Option<reqwest::Client>,
  stop: Option<Arc<AtomicBool>>,
  pub(super) stream_budget: Option<u64>,
  stream_backlog: Arc<AtomicBool>,
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
    Self::new_with_resolved(&ResolvedClient::resolve(path)?, None)
  }

  pub(crate) fn new_with_resolved(
    resolved: &ResolvedClient,
    stop: Option<Arc<AtomicBool>>,
  ) -> Result<Self> {
    init_tls();
    resolved.validate()?;
    let build = |timeout| {
      Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .map_err(|_| message("cannot initialize service HTTP client"))
    };
    Ok(Self {
      endpoint: resolved.endpoint.clone(),
      token: resolved.token.clone(),
      control: build(Duration::from_secs(30))?,
      objects: build(Duration::from_secs(900))?,
      async_objects: if stop.is_some() {
        Some(
          reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(900))
            .build()
            .map_err(|_| message("cannot initialize object HTTP client"))?,
        )
      } else {
        None
      },
      stream_budget: stop.as_ref().map(|_| 256 * 1024),
      stream_backlog: Arc::new(AtomicBool::new(false)),
      stop,
    })
  }

  pub(super) fn cancelled(&self) -> bool {
    self
      .stop
      .as_ref()
      .is_some_and(|stop| stop.load(Ordering::Acquire))
  }

  pub(super) fn cooperative(&self) -> bool {
    self.stop.is_some()
  }

  pub(super) fn reset_stream_backlog(&self) {
    self.stream_backlog.store(false, Ordering::Release);
  }

  pub(super) fn note_stream_backlog(&self, progressed: bool) {
    if self.cooperative() && progressed {
      self.stream_backlog.store(true, Ordering::Release);
    }
  }

  pub(super) fn backlog_remaining(&self) -> bool {
    self.cooperative() && self.stream_backlog.load(Ordering::Acquire)
  }

  pub(super) fn binding_id(&self) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(self.endpoint.as_bytes());
    hash.update([0]);
    hash.update(self.token.as_bytes());
    hash
      .finalize()
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect()
  }

  pub(super) fn cancel(&self) {
    if let Some(stop) = &self.stop {
      stop.store(true, Ordering::Release);
    }
  }

  pub(super) fn belongs_to(&self, stop: &Arc<AtomicBool>) -> bool {
    self
      .stop
      .as_ref()
      .is_some_and(|bound| Arc::ptr_eq(bound, stop))
  }

  pub(super) fn put_object(&self, url: &str, file: std::fs::File, length: u64) -> Result<String> {
    self.check_cancelled()?;
    let (status, headers) = if let Some(client) = &self.async_objects {
      use tokio::io::AsyncReadExt;
      let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| message("cannot initialize cancellable object upload"))?;
      runtime.block_on(async {
        let stream =
          tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file).take(length));
        let mut request = Box::pin(
          client
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, length)
            .body(reqwest::Body::wrap_stream(stream))
            .send(),
        );
        loop {
          self.check_cancelled()?;
          match tokio::time::timeout(Duration::from_millis(50), &mut request).await {
            Ok(response) => {
              let response = response.map_err(|_| {
                message("object part upload failed; acknowledged parts are saved for retry")
              })?;
              break Ok::<_, ExpriError>((response.status(), response.headers().clone()));
            }
            Err(_) => continue,
          }
        }
      })?
    } else {
      let response = self
        .objects
        .put(url)
        .body(reqwest::blocking::Body::sized(file.take(length), length))
        .send()
        .map_err(|_| {
          message("object part upload failed; acknowledged parts are saved for retry")
        })?;
      (response.status(), response.headers().clone())
    };
    if !status.is_success() {
      return Err(message(format!("object part upload returned {status}")));
    }
    headers
      .get(reqwest::header::ETAG)
      .and_then(|value| value.to_str().ok())
      .filter(|value| !value.is_empty() && value.len() <= 256)
      .map(str::to_owned)
      .ok_or_else(|| message("object part upload returned no valid ETag"))
  }

  pub(super) fn check_cancelled(&self) -> Result<()> {
    if self.cancelled() {
      Err(ExpriError::DownloadCancelled)
    } else {
      Ok(())
    }
  }

  pub(super) fn request(&self, request: &Request) -> Result<Response> {
    self.check_cancelled()?;
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
