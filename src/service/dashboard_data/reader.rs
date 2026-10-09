use std::io::{Read, Take};

use reqwest::blocking::Response;

use super::*;
use crate::service::types::FileStorage;

impl<S: ObjectStorage> HostedDashboard<'_, S> {
  pub(super) fn metric_data(
    &self,
    scope: &RunScope,
    result: &mut RunMetrics,
    filters: &[String],
    retain_points: bool,
    deadline: Instant,
  ) -> Result<()> {
    let Some(record) = self
      .store
      .dashboard_artifact(scope, "outputs/metrics.jsonl")
      .map_err(api_error)?
    else {
      result.warnings.push(json!({"run_id": scope.run_id, "message": "outputs/metrics.jsonl is missing; no metrics have been recorded"}));
      return Ok(());
    };
    if matches!(record.storage, FileStorage::Tracking { .. }) {
      return self
        .store
        .tracking_metrics(scope, result, filters, retain_points)
        .map_err(api_error);
    }
    if record.size > METRICS_LIMIT {
      return Err(message(
        "outputs/metrics.jsonl exceeds the 16 MiB hosted read limit; download the run for complete local review",
      ));
    }
    if record.size == 0 {
      verify_bytes(&record, &[]).map_err(api_error)?;
      return Ok(());
    }
    let reader = match record.storage {
      FileStorage::Object => {
        let response = self
          .object_response(&record, None, deadline)
          .map_err(api_error)?;
        MetricReader::Object(ObjectReader {
          response: response.take(record.size),
          remaining: record.size,
          deadline,
          digest: Sha256::new(),
          expected_digest: record.sha256.clone(),
        })
      }
      FileStorage::Tracking { .. } => unreachable!("tracking metrics use their projection"),
      FileStorage::Stream => MetricReader::Stream(StreamReader {
        dashboard: self,
        record: record.clone(),
        offset: 0,
        buffer: Vec::new(),
        cursor: 0,
        deadline,
      }),
    };
    let parsed = metrics::read_event_data(
      reader,
      record.size,
      result,
      filters,
      retain_points,
      Some(2000),
    );
    if parsed.is_err() {
      result.metrics.clear();
    }
    parsed
  }

  pub(super) fn range(
    &self,
    record: &FileRecord,
    offset: u64,
    length: usize,
    deadline: Instant,
  ) -> ApiResult<Vec<u8>> {
    if offset
      .checked_add(length as u64)
      .is_none_or(|end| end > record.size)
    {
      return Err(ApiError::new(
        409,
        "dashboard artifact changed; refresh the run",
      ));
    }
    if length == 0 {
      return Ok(Vec::new());
    }
    match record.storage {
      FileStorage::Tracking { .. } => self.store.tracking_range(record, offset, length),
      FileStorage::Stream => {
        let FileTarget::Run { scope, path } = &record.target else {
          return Err(ApiError::new(500, "invalid stored stream"));
        };
        let (bytes, size) = self
          .store
          .dashboard_stream_range(scope, path, offset, length)?;
        if size < record.size || bytes.len() != length {
          return Err(ApiError::new(
            409,
            "dashboard stream changed; refresh the run",
          ));
        }
        Ok(bytes)
      }
      FileStorage::Object => {
        let mut response = self.object_response(record, Some((offset, length)), deadline)?;
        let mut bytes = Vec::with_capacity(length);
        response
          .by_ref()
          .take(length as u64 + 1)
          .read_to_end(&mut bytes)
          .map_err(|_| ApiError::new(502, "dashboard object read failed"))?;
        if bytes.len() != length {
          return Err(ApiError::new(
            502,
            "dashboard object returned an invalid byte range",
          ));
        }
        Ok(bytes)
      }
    }
  }

  fn object_response(
    &self,
    record: &FileRecord,
    range: Option<(u64, usize)>,
    deadline: Instant,
  ) -> ApiResult<Response> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      return Err(ApiError::new(
        504,
        "hosted read time budget exceeded; refresh to retry",
      ));
    }
    let url = self.store.dashboard_download_url(&record.target)?;
    let mut request = self.client.get(url).timeout(remaining.min(READ_TIMEOUT));
    if let Some((offset, length)) = range {
      request = request.header(
        reqwest::header::RANGE,
        format!("bytes={offset}-{}", offset + length as u64 - 1),
      );
    }
    let response = request
      .send()
      .map_err(|_| ApiError::new(502, "dashboard object storage unavailable"))?;
    if let Some((offset, length)) = range {
      let expected = format!(
        "bytes {offset}-{}/{}",
        offset + length as u64 - 1,
        record.size
      );
      if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        || response
          .headers()
          .get(reqwest::header::CONTENT_RANGE)
          .and_then(|value| value.to_str().ok())
          != Some(expected.as_str())
      {
        return Err(ApiError::new(
          502,
          "dashboard object returned an invalid byte range",
        ));
      }
    } else if response.status() != reqwest::StatusCode::OK {
      return Err(ApiError::new(502, "dashboard object storage unavailable"));
    }
    let expected_length = range.map_or(record.size, |(_, length)| length as u64);
    if response
      .content_length()
      .is_some_and(|size| size != expected_length)
    {
      return Err(ApiError::new(
        502,
        "dashboard object returned an invalid size",
      ));
    }
    Ok(response)
  }
}

enum MetricReader<'a, S> {
  Object(ObjectReader),
  Stream(StreamReader<'a, S>),
}

struct ObjectReader {
  response: Take<Response>,
  remaining: u64,
  deadline: Instant,
  digest: Sha256,
  expected_digest: Option<String>,
}

impl Read for ObjectReader {
  fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
    if buffer.is_empty() || self.remaining == 0 {
      return Ok(0);
    }
    if Instant::now() >= self.deadline {
      return Err(std::io::Error::other("hosted read time budget exceeded"));
    }
    let count = self
      .response
      .read(buffer)
      .map_err(|_| std::io::Error::other("dashboard metric object read failed"))?;
    if count == 0 {
      return Err(std::io::Error::other(
        "dashboard metric object ended before its recorded size",
      ));
    }
    self.remaining -= count as u64;
    self.digest.update(&buffer[..count]);
    if self.remaining == 0
      && let Some(expected) = &self.expected_digest
      && hex_digest(&self.digest.clone().finalize()) != *expected
    {
      return Err(std::io::Error::other(
        "dashboard metric object failed integrity verification",
      ));
    }
    Ok(count)
  }
}

impl<S: ObjectStorage> Read for MetricReader<'_, S> {
  fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
    match self {
      Self::Object(response) => response.read(buffer),
      Self::Stream(reader) => reader.read(buffer),
    }
  }
}

struct StreamReader<'a, S> {
  dashboard: &'a HostedDashboard<'a, S>,
  record: FileRecord,
  offset: u64,
  buffer: Vec<u8>,
  cursor: usize,
  deadline: Instant,
}

impl<S: ObjectStorage> Read for StreamReader<'_, S> {
  fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
    if output.is_empty() || self.offset == self.record.size && self.cursor == self.buffer.len() {
      return Ok(0);
    }
    if Instant::now() >= self.deadline {
      return Err(std::io::Error::other("hosted read time budget exceeded"));
    }
    if self.cursor == self.buffer.len() {
      let length = (self.record.size - self.offset).min(STREAM_BATCH as u64) as usize;
      self.buffer = self
        .dashboard
        .range(&self.record, self.offset, length, self.deadline)
        .map_err(|error| std::io::Error::other(error.message))?;
      self.offset += self.buffer.len() as u64;
      self.cursor = 0;
    }
    let count = output.len().min(self.buffer.len() - self.cursor);
    output[..count].copy_from_slice(&self.buffer[self.cursor..self.cursor + count]);
    self.cursor += count;
    Ok(count)
  }
}
