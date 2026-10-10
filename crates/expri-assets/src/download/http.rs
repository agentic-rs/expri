use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::{Method, StatusCode, Url, header};
use serde::{Deserialize, Serialize};

use super::{
  Descriptor, Result, Source, cache_file, check_cancelled, digest, directories, inspect, lease,
  message, open, optional_regular, publish, sync_directory, transfer_directory, verified,
};

const CHECKPOINT_BYTES: u64 = 1024 * 1024;
const MAX_REDIRECTS: usize = 8;
const STATE_LIMIT: u64 = 8192;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Progress {
  version: u32,
  offset: u64,
  total: Option<u64>,
  etag: Option<String>,
  complete: bool,
}

impl Progress {
  fn fresh() -> Self {
    Self {
      version: 1,
      ..Self::default()
    }
  }
}

fn client() -> Result<Client> {
  let _ = rustls::crypto::ring::default_provider().install_default();
  Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    .connect_timeout(Duration::from_secs(10))
    .timeout(Duration::from_secs(900))
    .user_agent(concat!("expri/", env!("CARGO_PKG_VERSION")))
    .build()
    .map_err(|_| message("cannot initialize asset HTTP client"))
}

pub(super) fn resolve(
  repo: &Path,
  source: &Source,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<Descriptor> {
  let client = client()?;
  let pinned = match source {
    Source::HuggingFace { .. } => {
      resolve_hugging_face(&client, "https://huggingface.co", source, cancelled)?
    }
    Source::Url { .. } => source.clone(),
    Source::Expri { .. } => return Err(message("private asset needs the service downloader")),
  };
  let url = source_url(&pinned)?;
  download(repo, &pinned, &url, None, &client, cancelled)
}

fn resolve_hugging_face(
  client: &Client,
  endpoint: &str,
  source: &Source,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<Source> {
  let Source::HuggingFace {
    repo_id,
    repo_type,
    filename,
    revision,
    requested_revision,
  } = source
  else {
    return Err(message("asset source is not Hugging Face"));
  };
  let requested = requested_revision.as_deref().unwrap_or(revision);
  let mut lookup = source.clone();
  if let Source::HuggingFace { revision, .. } = &mut lookup {
    *revision = requested.to_string();
  }
  let url = hugging_face_url(endpoint, &lookup)?;
  let metadata = request(client, Method::HEAD, &url, None, None, cancelled)?;
  if !metadata.response.status().is_success() {
    return Err(message(format!(
      "Hugging Face file lookup returned HTTP {}",
      metadata.response.status().as_u16()
    )));
  }
  let commit = metadata
    .commit
    .ok_or_else(|| message("Hugging Face did not identify the file's resolved commit"))?;
  Ok(Source::HuggingFace {
    repo_id: repo_id.clone(),
    repo_type: repo_type.clone(),
    filename: filename.clone(),
    revision: commit,
    requested_revision: Some(requested.to_string()),
  })
}

pub(super) fn ensure(
  repo: &Path,
  descriptor: &Descriptor,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  let client = client()?;
  let url = source_url(&descriptor.source)?;
  download(
    repo,
    &descriptor.source,
    &url,
    Some(descriptor),
    &client,
    cancelled,
  )?;
  Ok(())
}

fn source_url(source: &Source) -> Result<Url> {
  match source {
    Source::Url { url } => Url::parse(url).map_err(|_| message("invalid asset URL")),
    Source::HuggingFace { .. } => hugging_face_url("https://huggingface.co", source),
    Source::Expri { .. } => Err(message("private asset needs the service downloader")),
  }
}

fn hugging_face_url(endpoint: &str, source: &Source) -> Result<Url> {
  let Source::HuggingFace {
    repo_id,
    repo_type,
    filename,
    revision,
    ..
  } = source
  else {
    return Err(message("asset source is not Hugging Face"));
  };
  let mut url = Url::parse(endpoint).map_err(|_| message("invalid Hugging Face endpoint"))?;
  {
    let mut segments = url
      .path_segments_mut()
      .map_err(|_| message("invalid Hugging Face endpoint"))?;
    segments.clear();
    if repo_type == "dataset" {
      segments.push("datasets");
    }
    for part in repo_id.split('/') {
      segments.push(part);
    }
    // A branch containing a slash is one encoded path segment, not another directory.
    segments.push("resolve").push(revision);
    for part in filename.split('/') {
      segments.push(part);
    }
  }
  Ok(url)
}

struct Received {
  response: Response,
  commit: Option<String>,
}

fn request(
  client: &Client,
  method: Method,
  initial: &Url,
  offset: Option<u64>,
  etag: Option<&str>,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<Received> {
  let mut url = initial.clone();
  let mut commit = None;
  for hop in 0..=MAX_REDIRECTS {
    check_cancelled(cancelled)?;
    validate_url(&url)?;
    let mut request = client
      .request(method.clone(), url.clone())
      .header(header::ACCEPT_ENCODING, "identity");
    if let Some(offset) = offset {
      request = request.header(header::RANGE, format!("bytes={offset}-"));
      if let Some(etag) = etag {
        request = request.header(header::IF_RANGE, etag);
      }
    }
    let response = request
      .send()
      .map_err(|_| message("asset request failed; retry to resume saved progress"))?;
    check_cancelled(cancelled)?;
    // A CDN cannot supply or replace repository identity on the Hub's behalf.
    if url.origin() == initial.origin()
      && let Some(value) = response.headers().get("x-repo-commit")
    {
      let value = value
        .to_str()
        .ok()
        .filter(|value| valid_commit(value))
        .ok_or_else(|| message("Hugging Face returned an invalid repository commit"))?;
      if commit.as_deref().is_some_and(|saved| saved != value) {
        return Err(message(
          "Hugging Face repository changed during its redirect",
        ));
      }
      commit = Some(value.to_string());
    }
    if matches!(
      response.status(),
      StatusCode::MOVED_PERMANENTLY
        | StatusCode::FOUND
        | StatusCode::SEE_OTHER
        | StatusCode::TEMPORARY_REDIRECT
        | StatusCode::PERMANENT_REDIRECT
    ) {
      if hop == MAX_REDIRECTS {
        return Err(message("asset redirect limit exceeded"));
      }
      let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 16 * 1024)
        .ok_or_else(|| message("asset redirect has no valid location"))?;
      let next = url
        .join(location)
        .map_err(|_| message("invalid asset redirect"))?;
      validate_url(&next)?;
      if url.scheme() == "https" && next.scheme() != "https" {
        return Err(message("asset redirect would downgrade HTTPS"));
      }
      url = next;
      continue;
    }
    return Ok(Received { response, commit });
  }
  Err(message("asset redirect limit exceeded"))
}

fn validate_url(url: &Url) -> Result<()> {
  if !matches!(url.scheme(), "http" | "https")
    || url.host_str().is_none()
    || !url.username().is_empty()
    || url.password().is_some()
    || url.fragment().is_some()
  {
    return Err(message(
      "asset URL must use HTTP(S) without credentials or a fragment",
    ));
  }
  Ok(())
}

fn valid_commit(value: &str) -> bool {
  value.len() == 40
    && value
      .bytes()
      .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn strong_etag(response: &Response) -> Option<String> {
  response
    .headers()
    .get(header::ETAG)?
    .to_str()
    .ok()
    .filter(|value| {
      value.len() <= 256
        && value.starts_with('"')
        && value.ends_with('"')
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    })
    .map(str::to_string)
}

fn load(directory: &Path) -> Result<Progress> {
  let path = directory.join("progress.json");
  optional_regular(&path)?;
  if inspect(&path)?.is_none() {
    return Ok(Progress::fresh());
  }
  let mut bytes = Vec::new();
  open(&path)?.take(STATE_LIMIT + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > STATE_LIMIT {
    return Err(message("asset progress exceeds its size limit"));
  }
  let progress: Progress =
    serde_json::from_slice(&bytes).map_err(|_| message("asset progress is invalid"))?;
  if progress.version != 1
    || progress.total.is_some_and(|total| progress.offset > total)
    || (progress.complete && progress.total != Some(progress.offset))
    || progress.etag.as_ref().is_some_and(|value| {
      value.len() > 256
        || !value.starts_with('"')
        || !value.ends_with('"')
        || value.bytes().any(|byte| !(0x20..=0x7e).contains(&byte))
    })
  {
    return Err(message("asset progress is inconsistent"));
  }
  Ok(progress)
}

fn save(directory: &Path, progress: &Progress, file: &File) -> Result<()> {
  file.sync_all()?;
  let path = directory.join("progress.json");
  optional_regular(&path)?;
  let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
  serde_json::to_writer(&mut temporary, progress)?;
  temporary.write_all(b"\n")?;
  temporary.as_file().sync_all()?;
  optional_regular(&path)?;
  temporary.persist(&path).map_err(|error| error.error)?;
  sync_directory(directory)
}

fn part_file(path: &Path) -> Result<File> {
  optional_regular(path)?;
  let mut options = OpenOptions::new();
  options.read(true).write(true).create(true).truncate(false);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options
      .mode(0o600)
      .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options.open(path)?;
  if !file.metadata()?.is_file() {
    return Err(message("asset transfer must be a regular file"));
  }
  optional_regular(path)?;
  Ok(file)
}

fn range(response: &Response) -> Result<(u64, u64, u64)> {
  let value = response
    .headers()
    .get(header::CONTENT_RANGE)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.strip_prefix("bytes "))
    .ok_or_else(|| message("asset response has no valid byte range"))?;
  let (interval, total) = value
    .split_once('/')
    .ok_or_else(|| message("asset response has no valid byte range"))?;
  let (start, end) = interval
    .split_once('-')
    .ok_or_else(|| message("asset response has no valid byte range"))?;
  let parse = |value: &str| -> Result<u64> {
    if value.is_empty() || value.bytes().any(|byte| !byte.is_ascii_digit()) {
      return Err(message("asset response has no valid byte range"));
    }
    value
      .parse()
      .map_err(|_| message("asset byte range is too large"))
  };
  let (start, end, total) = (parse(start)?, parse(end)?, parse(total)?);
  if start > end || end >= total {
    return Err(message("asset response has an inconsistent byte range"));
  }
  Ok((start, end, total))
}

fn reset(directory: &Path, progress: &mut Progress, file: &mut File) -> Result<()> {
  *progress = Progress::fresh();
  file.set_len(0)?;
  file.seek(SeekFrom::Start(0))?;
  save(directory, progress, file)
}

fn download(
  repo: &Path,
  source: &Source,
  url: &Url,
  expected: Option<&Descriptor>,
  client: &Client,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<Descriptor> {
  let directory = transfer_directory(repo, source, expected)?;
  directories(&directory)?;
  let _lease = lease(&directory.join(".asset.lock"), cancelled)?;
  if let Some(expected) = expected
    && verified(
      &cache_file(repo, &expected.sha256),
      expected.size,
      &expected.sha256,
      cancelled,
    )?
  {
    return Ok(expected.clone());
  }
  let path = directory.join("part");
  let mut progress = load(&directory)?;
  let mut file = part_file(&path)?;
  if file.metadata()?.len() < progress.offset {
    reset(&directory, &mut progress, &mut file)?;
  }
  file.set_len(progress.offset)?;
  let pinned_commit = match source {
    Source::HuggingFace { revision, .. } => Some(revision.as_str()),
    _ => None,
  };
  if !progress.complete {
    if progress.offset > 0 && progress.etag.is_none() && pinned_commit.is_none() {
      // Without a strong validator a partial public file cannot be joined safely.
      reset(&directory, &mut progress, &mut file)?;
    }
    receive(
      client,
      url,
      pinned_commit,
      &directory,
      &mut progress,
      &mut file,
      expected,
      cancelled,
    )?;
  } else if expected.is_none() {
    // Import/update of a URL deliberately checks the current source again.
    reset(&directory, &mut progress, &mut file)?;
    receive(
      client,
      url,
      pinned_commit,
      &directory,
      &mut progress,
      &mut file,
      expected,
      cancelled,
    )?;
  }
  let (size, sha256) = digest(&path, cancelled)?;
  let descriptor = Descriptor {
    version: 1,
    source: source.clone(),
    size,
    sha256,
  };
  descriptor.validate()?;
  if let Some(expected) = expected
    && (size != expected.size || descriptor.sha256 != expected.sha256)
  {
    reset(&directory, &mut progress, &mut file)?;
    return Err(message(
      "asset source no longer matches its descriptor; use assets update to select new bytes",
    ));
  }
  publish(repo, &path, &descriptor, cancelled)?;
  // Staging never shares an inode with the immutable cache, so crashes/retries
  // cannot truncate files already bound to a run.
  std::fs::remove_file(path)?;
  std::fs::remove_file(directory.join("progress.json"))?;
  sync_directory(&directory)?;
  Ok(descriptor)
}

#[allow(clippy::too_many_arguments)]
fn receive(
  client: &Client,
  url: &Url,
  pinned_commit: Option<&str>,
  directory: &Path,
  progress: &mut Progress,
  file: &mut File,
  expected: Option<&Descriptor>,
  cancelled: &mut dyn FnMut() -> Result<bool>,
) -> Result<()> {
  let offset = (progress.offset > 0).then_some(progress.offset);
  let received = request(
    client,
    Method::GET,
    url,
    offset,
    progress.etag.as_deref(),
    cancelled,
  )?;
  if let (Some(expected), Some(actual)) = (pinned_commit, received.commit.as_deref())
    && expected != actual
  {
    reset(directory, progress, file)?;
    return Err(message(
      "Hugging Face returned a different repository commit",
    ));
  }
  let mut response = received.response;
  if response
    .headers()
    .get(header::CONTENT_ENCODING)
    .is_some_and(|value| value != "identity")
  {
    return Err(message("asset response unexpectedly encoded its bytes"));
  }
  let etag = strong_etag(&response);
  let total = match response.status() {
    StatusCode::OK => {
      // A server may ignore Range, or If-Range may identify a newer version.
      reset(directory, progress, file)?;
      response.content_length()
    }
    StatusCode::PARTIAL_CONTENT if offset.is_some() => {
      let (start, end, total) = range(&response)?;
      if start != progress.offset
        || end != total - 1
        || progress.total.is_some_and(|previous| previous != total)
        || response
          .content_length()
          .is_some_and(|length| length != total - start)
        || (progress.etag.is_some() && progress.etag != etag)
      {
        reset(directory, progress, file)?;
        return Err(message(
          "asset changed or returned an inconsistent resumed byte range",
        ));
      }
      Some(total)
    }
    StatusCode::RANGE_NOT_SATISFIABLE if offset.is_some() => {
      // A lost terminating frame can leave every byte saved but not yet marked
      // complete. Validate that the next request proves the same EOF/version.
      let total = response
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes */"))
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok());
      if total != Some(progress.offset)
        || progress.total.is_some_and(|saved| Some(saved) != total)
        || (pinned_commit.is_none() && (progress.etag.is_none() || progress.etag != etag))
      {
        reset(directory, progress, file)?;
        return Err(message(
          "asset response did not confirm its saved end or version",
        ));
      }
      progress.total = total;
      progress.complete = true;
      return save(directory, progress, file);
    }
    status => {
      return Err(message(format!(
        "asset download returned HTTP {}",
        status.as_u16()
      )));
    }
  };
  if let Some(expected) = expected
    && total.is_some_and(|total| total != expected.size)
  {
    reset(directory, progress, file)?;
    return Err(message(
      "asset source size no longer matches its descriptor",
    ));
  }
  progress.etag = etag;
  progress.total = total;
  progress.complete = false;
  file.seek(SeekFrom::Start(progress.offset))?;
  save(directory, progress, file)?;
  let mut saved = progress.offset;
  let mut buffer = [0u8; 64 * 1024];
  loop {
    // Progress is checkpointed before cancellation or network errors are returned.
    if let Err(error) = check_cancelled(cancelled) {
      save(directory, progress, file)?;
      return Err(error);
    }
    let count = match response.read(&mut buffer) {
      Ok(count) => count,
      Err(_) => {
        save(directory, progress, file)?;
        return Err(message(
          "asset download interrupted; retry to resume saved progress",
        ));
      }
    };
    if count == 0 {
      break;
    }
    let next = progress
      .offset
      .checked_add(count as u64)
      .ok_or_else(|| message("asset download exceeds its supported size"))?;
    if total.is_some_and(|total| next > total)
      || expected.is_some_and(|expected| next > expected.size)
    {
      reset(directory, progress, file)?;
      return Err(message("asset download exceeded its recorded size"));
    }
    file.write_all(&buffer[..count])?;
    progress.offset = next;
    if progress.offset - saved >= CHECKPOINT_BYTES {
      save(directory, progress, file)?;
      saved = progress.offset;
    }
  }
  if total.is_some_and(|total| total != progress.offset)
    || expected.is_some_and(|expected| expected.size != progress.offset)
  {
    save(directory, progress, file)?;
    return Err(message(
      "asset download ended early; retry to resume saved progress",
    ));
  }
  progress.total = Some(progress.offset);
  progress.complete = true;
  save(directory, progress, file)
}

#[cfg(test)]
mod tests {
  use super::super::tests::{Mock, response};
  use super::*;

  fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let repo = temporary.path().canonicalize().unwrap();
    (temporary, repo)
  }

  fn hf_source(revision: &str, requested: Option<&str>) -> Source {
    Source::HuggingFace {
      repo_id: "owner/model".into(),
      repo_type: "model".into(),
      filename: "weights/model.bin".into(),
      revision: revision.into(),
      requested_revision: requested.map(str::to_string),
    }
  }

  #[test]
  fn hugging_face_updates_resolve_requested_reference_then_download_resolved_commit() {
    let (_temporary, repo) = fixture();
    let commit = "a".repeat(40);
    let commit_head = commit.clone();
    let commit_get = commit.clone();
    let mut mock = Mock::start(vec![
      Box::new(move |request| {
        assert!(
          request.starts_with("HEAD /owner/model/resolve/main/weights/model.bin HTTP/1.1"),
          "{request}"
        );
        response(
          "302 Found",
          &format!(
            "Content-Length: 0\r\nX-Repo-Commit: {commit_head}\r\nLocation: /metadata?signature=temporary-secret\r\n"
          ),
          b"",
        )
      }),
      Box::new(|request| {
        assert!(request.starts_with("HEAD /metadata?signature=temporary-secret HTTP/1.1"));
        response("200 OK", "Content-Length: 10\r\n", b"")
      }),
      Box::new(move |request| {
        assert!(
          request.starts_with(&format!(
            "GET /owner/model/resolve/{commit_get}/weights/model.bin HTTP/1.1"
          )),
          "{request}"
        );
        assert!(!request.contains("authorization:"));
        response(
          "200 OK",
          "Content-Length: 10\r\nETag: \"bytes\"\r\n",
          b"model data",
        )
      }),
    ]);
    let client = client().unwrap();
    let source = hf_source(&"b".repeat(40), Some("main"));
    let pinned = resolve_hugging_face(&client, &mock.url, &source, &mut || Ok(false)).unwrap();
    assert_eq!(pinned, hf_source(&commit, Some("main")));
    let url = hugging_face_url(&mock.url, &pinned).unwrap();
    let descriptor = download(&repo, &pinned, &url, None, &client, &mut || Ok(false)).unwrap();
    descriptor.validate().unwrap();
    assert_eq!(descriptor.source, pinned);
    assert!(
      !toml::to_string(&descriptor)
        .unwrap()
        .contains("temporary-secret")
    );
    mock.finish();
  }

  #[test]
  fn hugging_face_named_file_url_encodes_revision_and_filename_segments() {
    let source = Source::HuggingFace {
      repo_id: "owner/data".into(),
      repo_type: "dataset".into(),
      filename: "folder/my data.parquet".into(),
      revision: "refs/pr/12".into(),
      requested_revision: None,
    };
    assert_eq!(
      hugging_face_url("https://huggingface.co", &source)
        .unwrap()
        .as_str(),
      "https://huggingface.co/datasets/owner/data/resolve/refs%2Fpr%2F12/folder/my%20data.parquet"
    );
  }

  #[test]
  fn missing_or_invalid_hugging_face_commit_is_rejected() {
    for headers in [
      "Content-Length: 0\r\n",
      "Content-Length: 0\r\nX-Repo-Commit: main\r\n",
    ] {
      let mut mock = Mock::start(vec![Box::new(move |_| response("200 OK", headers, b""))]);
      let error = resolve_hugging_face(
        &client().unwrap(),
        &mock.url,
        &hf_source("main", None),
        &mut || Ok(false),
      )
      .unwrap_err()
      .to_string();
      assert!(error.contains("commit"));
      mock.finish();
    }
  }

  #[test]
  fn pinned_hugging_face_download_resumes_without_etag_and_rejects_a_different_commit() {
    let (_temporary, repo) = fixture();
    let source = hf_source(&"a".repeat(40), Some("main"));
    let mut mock = Mock::start(vec![
      Box::new(|_| response("200 OK", "Content-Length: 10\r\n", b"0123")),
      Box::new(|request| {
        assert!(request.contains("range: bytes=4-\r\n"));
        assert!(!request.contains("if-range:"));
        response(
          "206 Partial Content",
          "Content-Length: 6\r\nContent-Range: bytes 4-9/10\r\n",
          b"456789",
        )
      }),
    ]);
    let url = hugging_face_url(&mock.url, &source).unwrap();
    let client = client().unwrap();
    assert!(download(&repo, &source, &url, None, &client, &mut || Ok(false)).is_err());
    let descriptor = download(&repo, &source, &url, None, &client, &mut || Ok(false)).unwrap();
    assert_eq!(descriptor.size, 10);
    mock.finish();

    let (_temporary, repo) = fixture();
    let mut mock = Mock::start(vec![Box::new(|_| {
      response(
        "200 OK",
        &format!(
          "Content-Length: 10\r\nX-Repo-Commit: {}\r\n",
          "b".repeat(40)
        ),
        b"different!",
      )
    })]);
    let url = hugging_face_url(&mock.url, &source).unwrap();
    assert!(
      download(&repo, &source, &url, None, &client, &mut || Ok(false))
        .unwrap_err()
        .to_string()
        .contains("different repository commit")
    );
    mock.finish();
  }

  #[test]
  fn range_416_can_confirm_complete_saved_bytes_after_a_lost_terminating_frame() {
    let (_temporary, repo) = fixture();
    let mut mock = Mock::start(vec![Box::new(|request| {
      assert!(request.contains("range: bytes=10-\r\n"));
      assert!(request.contains("if-range: \"v1\"\r\n"));
      response(
        "416 Range Not Satisfiable",
        "Content-Length: 0\r\nContent-Range: bytes */10\r\nETag: \"v1\"\r\n",
        b"",
      )
    })]);
    let source = Source::Url {
      url: format!("{}/file", mock.url),
    };
    let directory = transfer_directory(&repo, &source, None).unwrap();
    directories(&directory).unwrap();
    let mut file = part_file(&directory.join("part")).unwrap();
    file.write_all(b"0123456789").unwrap();
    save(
      &directory,
      &Progress {
        version: 1,
        offset: 10,
        total: Some(10),
        etag: Some("\"v1\"".into()),
        complete: false,
      },
      &file,
    )
    .unwrap();
    let descriptor = download(
      &repo,
      &source,
      &source_url(&source).unwrap(),
      None,
      &client().unwrap(),
      &mut || Ok(false),
    )
    .unwrap();
    assert_eq!(descriptor.size, 10);
    mock.finish();
  }
}
