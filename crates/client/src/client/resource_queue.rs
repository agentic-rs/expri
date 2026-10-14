//! Resource scheduling over the existing bounded, authenticated service API.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use expri_core::resource_queue::*;
use sha2::{Digest, Sha256};

use super::{Api, ResolvedClient, fs};
use crate::error::Result;
use crate::types::{Request, Response};

#[derive(Clone)]
pub struct ResourceQueueClient {
  api: Api,
}

impl ResourceQueueClient {
  pub fn new(config: &Path) -> Result<Self> {
    Self::from_api(Api::new(config)?)
  }

  /// Bind queue execution and its heartbeat lane to the same credential and
  /// cooperative stop flag. Control requests retain their bounded 30s timeout.
  pub fn with_stop(config: &Path, stop: Arc<AtomicBool>) -> Result<Self> {
    Self::from_api(Api::new_with_resolved(
      &ResolvedClient::resolve(config)?,
      Some(stop),
    )?)
  }

  fn from_api(api: Api) -> Result<Self> {
    let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
      return Err(fs::message("service did not return its capabilities"));
    };
    if !features
      .iter()
      .any(|feature| feature == RESOURCE_QUEUE_CAPABILITY)
    {
      return Err(fs::message(
        "service does not support resource-queue-v2 task start confirmation; upgrade the server",
      ));
    }
    Ok(Self { api })
  }

  pub fn request(&self, request: &QueueRequest) -> Result<QueueResponse> {
    request.validate()?;
    let Response::Queue { response } = self.api.request(&Request::Queue {
      request: request.clone(),
    })?
    else {
      return Err(fs::message("service returned an unexpected queue response"));
    };
    response.validate_for(request)?;
    Ok(response)
  }

  /// Keep global transfer receipts separate when the configured server changes.
  /// The namespace contains no credential material.
  pub fn server_namespace(&self) -> String {
    Sha256::digest(self.api.endpoint.as_bytes())
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect()
  }

  /// Verify a pinned source into a private file before the worker extracts it.
  /// Errors deliberately omit signed URLs and provider response bodies.
  pub fn download_source(
    &self,
    attempt: &AttemptIdentity,
    expected: &SourceRef,
    destination: &Path,
  ) -> Result<()> {
    expected.validate()?;
    let QueueResponse::SourceUrl { url, size, sha256 } =
      self.request(&QueueRequest::SourceUrl {
        attempt: attempt.clone(),
      })?
    else {
      unreachable!()
    };
    if size != expected.size || sha256 != expected.sha256 {
      return Err(fs::message(
        "queue source does not match the submitted snapshot",
      ));
    }
    let parent = destination
      .parent()
      .ok_or_else(|| fs::message("queue source requires a parent directory"))?;
    fs::directory(parent)?;
    fs::optional_regular(destination)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut digest = Sha256::new();
    let received = self.receive_source(&url, size, &mut temporary, &mut digest)?;
    let actual: String = digest
      .finalize()
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect();
    if received != size || actual != sha256 {
      return Err(fs::message("queue source failed integrity verification"));
    }
    temporary.as_file().sync_all()?;
    self.api.check_cancelled()?;
    temporary
      .persist(destination)
      .map_err(|error| error.error)?;
    fs::sync_directory(parent)
  }

  fn receive_source(
    &self,
    url: &str,
    size: u64,
    temporary: &mut tempfile::NamedTempFile,
    digest: &mut Sha256,
  ) -> Result<u64> {
    self.api.check_cancelled()?;
    let mut received = 0_u64;
    let mut save = |bytes: &[u8]| -> Result<()> {
      self.api.check_cancelled()?;
      received = received
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| fs::message("queue source exceeds its size"))?;
      if received > size {
        return Err(fs::message("queue source exceeds its submitted size"));
      }
      digest.update(bytes);
      temporary.write_all(bytes)?;
      Ok(())
    };
    if let Some(client) = &self.api.async_objects {
      let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| fs::message("cannot initialize cancellable queue source download"))?;
      runtime.block_on(async {
        let mut request = Box::pin(client.get(url).send());
        let mut response = loop {
          self.api.check_cancelled()?;
          match tokio::time::timeout(Duration::from_millis(50), &mut request).await {
            Ok(response) => {
              break response
                .and_then(reqwest::Response::error_for_status)
                .map_err(|_| fs::message("queue source download failed"))?;
            }
            Err(_) => continue,
          }
        };
        check_length(response.content_length(), size)?;
        loop {
          let mut chunk = Box::pin(response.chunk());
          let bytes = loop {
            self.api.check_cancelled()?;
            match tokio::time::timeout(Duration::from_millis(50), &mut chunk).await {
              Ok(chunk) => {
                break chunk.map_err(|_| fs::message("cannot read queue source"))?;
              }
              Err(_) => continue,
            }
          };
          let Some(bytes) = bytes else { break };
          save(&bytes)?;
        }
        Ok::<_, crate::error::ExpriError>(())
      })?;
    } else {
      let mut response = self
        .api
        .objects
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|_| fs::message("queue source download failed"))?;
      check_length(response.content_length(), size)?;
      let mut buffer = [0_u8; 64 * 1024];
      loop {
        let count = response
          .read(&mut buffer)
          .map_err(|_| fs::message("cannot read queue source"))?;
        if count == 0 {
          break;
        }
        save(&buffer[..count])?;
      }
    }
    self.api.check_cancelled()?;
    Ok(received)
  }
}

fn check_length(length: Option<u64>, size: u64) -> Result<()> {
  if length.is_some_and(|length| length != size) {
    return Err(fs::message("queue source has an unexpected size"));
  }
  Ok(())
}

#[cfg(test)]
mod tests;
