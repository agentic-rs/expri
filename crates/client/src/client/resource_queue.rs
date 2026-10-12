//! Resource scheduling over the existing bounded, authenticated service API.

use std::io::{Read, Write};
use std::path::Path;

use expri_core::resource_queue::*;
use sha2::{Digest, Sha256};

use super::{Api, fs};
use crate::error::Result;
use crate::types::{Request, Response};

pub struct ResourceQueueClient {
  api: Api,
}

impl ResourceQueueClient {
  pub fn new(config: &Path) -> Result<Self> {
    let api = Api::new(config)?;
    let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
      return Err(fs::message("service did not return its capabilities"));
    };
    if !features
      .iter()
      .any(|feature| feature == RESOURCE_QUEUE_CAPABILITY)
    {
      return Err(fs::message(
        "service does not support resource-queue-v1; upgrade the server",
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
    let mut response = self
      .api
      .objects
      .get(url)
      .send()
      .and_then(reqwest::blocking::Response::error_for_status)
      .map_err(|_| fs::message("queue source download failed"))?;
    if response
      .content_length()
      .is_some_and(|length| length != size)
    {
      return Err(fs::message("queue source has an unexpected size"));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut digest = Sha256::new();
    let mut received = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
      let count = response
        .read(&mut buffer)
        .map_err(|_| fs::message("cannot read queue source"))?;
      if count == 0 {
        break;
      }
      received = received
        .checked_add(count as u64)
        .ok_or_else(|| fs::message("queue source exceeds its size"))?;
      if received > size {
        return Err(fs::message("queue source exceeds its submitted size"));
      }
      digest.update(&buffer[..count]);
      temporary.write_all(&buffer[..count])?;
    }
    let actual: String = digest
      .finalize()
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect();
    if received != size || actual != sha256 {
      return Err(fs::message("queue source failed integrity verification"));
    }
    temporary.as_file().sync_all()?;
    temporary
      .persist(destination)
      .map_err(|error| error.error)?;
    fs::sync_directory(parent)
  }
}
