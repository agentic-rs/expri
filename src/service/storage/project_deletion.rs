use std::collections::HashMap;

use quick_xml::{Reader, events::Event};
use s3::error::S3Error;
use serde::{Deserialize, de::DeserializeOwned};

use super::{Result, S3Storage, backend_error, check_status, invalid};

const VERSION_BATCH: usize = 16;
const VERSION_XML_LIMIT: usize = 128 * 1024;

#[derive(Deserialize)]
struct Versioning {
  #[serde(rename = "Status")]
  status: Option<String>,
}

#[derive(Deserialize)]
struct Version {
  #[serde(rename = "Key")]
  key: String,
  #[serde(rename = "VersionId")]
  version_id: String,
}

#[derive(Deserialize)]
struct VersionPage {
  #[serde(rename = "Prefix")]
  prefix: String,
  #[serde(rename = "IsTruncated")]
  truncated: bool,
  #[serde(rename = "NextKeyMarker")]
  next_key: Option<String>,
  #[serde(rename = "NextVersionIdMarker")]
  next_version: Option<String>,
  #[serde(rename = "Version", default)]
  versions: Vec<Version>,
  #[serde(rename = "DeleteMarker", default)]
  markers: Vec<Version>,
}

impl S3Storage {
  /// Purge versions of exactly one generated key. A bounded batch is retried
  /// through the durable cleanup queue; already deleted versions are harmless.
  pub(super) fn purge_object(&self, key: &str) -> Result<()> {
    let key = self.object_key(key)?;
    let bucket = self
      .bucket
      .with_extra_query(HashMap::from([("versioning".into(), "".into())]))
      .map_err(|error| backend_error("get bucket versioning", error))?;
    let response = bucket
      .get_object("")
      .map_err(|error| backend_error("get bucket versioning", error))?;
    check_status("get bucket versioning", response.status_code())?;
    let versioning: Versioning = parse_xml(response.as_slice(), b"VersioningConfiguration")?;
    match versioning.status.as_deref() {
      None => return self.delete_unversioned(&key),
      Some("Enabled" | "Suspended") => {}
      _ => return Err(invalid("S3 bucket versioning status is unknown")),
    }
    let bucket = self
      .bucket
      .with_extra_query(HashMap::from([
        ("versions".into(), "".into()),
        ("prefix".into(), key.clone()),
        ("max-keys".into(), VERSION_BATCH.to_string()),
      ]))
      .map_err(|error| backend_error("list object versions", error))?;
    let response = bucket
      .get_object("")
      .map_err(|error| backend_error("list object versions", error))?;
    check_status("list object versions", response.status_code())?;
    let page: VersionPage = parse_xml(response.as_slice(), b"ListVersionsResult")?;
    if page.prefix != key || page.versions.len() + page.markers.len() > VERSION_BATCH {
      return Err(invalid("S3 object version listing is invalid"));
    }
    // Prefix listing may include similarly named keys. Never delete them.
    for version in page.versions.iter().chain(&page.markers) {
      if !version.key.starts_with(&key) || !valid_version(&version.version_id) {
        return Err(invalid("S3 object version listing is invalid"));
      }
      if version.key != key {
        continue;
      }
      let bucket = self
        .bucket
        .with_extra_query(HashMap::from([(
          "versionId".into(),
          version.version_id.clone(),
        )]))
        .map_err(|error| backend_error("delete object version", error))?;
      match bucket.delete_object(&key) {
        Ok(response) if response.status_code() == 404 => {}
        Ok(response) => check_status("delete object version", response.status_code())?,
        Err(S3Error::HttpFailWithBody(404, _)) => {}
        Err(error) => return Err(backend_error("delete object version", error)),
      }
    }
    if page.truncated {
      let Some(next_key) = page.next_key else {
        return Err(invalid("S3 object version pagination is invalid"));
      };
      if !next_key.starts_with(&key) || next_key < key {
        return Err(invalid("S3 object version pagination is invalid"));
      }
      if next_key == key {
        if page
          .next_version
          .as_deref()
          .is_none_or(|value| !valid_version(value))
        {
          return Err(invalid("S3 object version pagination is invalid"));
        }
        // S3 orders keys lexically. Versions of the exact key are before all
        // longer prefix matches; the next cycle lists the remaining first page.
        return Err(invalid("S3 object versions remain; cleanup will retry"));
      }
    }
    Ok(())
  }

  fn delete_unversioned(&self, key: &str) -> Result<()> {
    match self.bucket.delete_object(key) {
      Ok(response) if response.status_code() == 404 => Ok(()),
      Ok(response) => check_status("delete object", response.status_code()),
      Err(S3Error::HttpFailWithBody(404, _)) => Ok(()),
      Err(error) => Err(backend_error("delete object", error)),
    }
  }
}

fn valid_version(value: &str) -> bool {
  !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

fn parse_xml<T: DeserializeOwned>(bytes: &[u8], root: &[u8]) -> Result<T> {
  let failure = || invalid("S3 cleanup returned an invalid XML response");
  if bytes.len() > VERSION_XML_LIMIT {
    return Err(failure());
  }
  let mut reader = Reader::from_reader(bytes);
  reader.config_mut().trim_text(true);
  let mut depth = 0usize;
  let mut root_seen = false;
  loop {
    match reader.read_event().map_err(|_| failure())? {
      Event::Start(element) => {
        if depth == 0 {
          if root_seen || element.local_name().as_ref() != root {
            return Err(failure());
          }
          root_seen = true;
        }
        depth += 1;
        if depth > 16 {
          return Err(failure());
        }
      }
      Event::Empty(element) if depth == 0 => {
        if root_seen || element.local_name().as_ref() != root {
          return Err(failure());
        }
        root_seen = true;
      }
      Event::End(_) => depth = depth.checked_sub(1).ok_or_else(failure)?,
      Event::DocType(_) => return Err(failure()),
      Event::Eof => break,
      Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if depth == 0 => {
        return Err(failure());
      }
      _ => {}
    }
  }
  if !root_seen || depth != 0 {
    return Err(failure());
  }
  quick_xml::de::from_reader(bytes).map_err(|_| failure())
}
