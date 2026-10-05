use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use super::store::{ApiError, ApiResult};
use crate::error::{ExpriError, Result};

const ASSET_LIMIT: u64 = 2 * 1024 * 1024;
const MANIFEST_LIMIT: u64 = 16 * 1024;
const FILES: [&str; 4] = ["index.html", "login.html", "app.js", "styles.css"];

pub(super) enum DashboardAssets {
  Embedded,
  External { directory: PathBuf },
}

pub(super) struct Asset {
  pub body: Vec<u8>,
  pub content_type: &'static str,
  pub revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Deployment {
  commit: String,
  branch: String,
}

struct Bundle {
  directory: PathBuf,
  deployment: Deployment,
}

impl DashboardAssets {
  pub fn embedded() -> Self {
    Self::Embedded
  }

  pub fn is_external(&self) -> bool {
    matches!(self, Self::External { .. })
  }

  pub fn external(directory: PathBuf) -> Result<Self> {
    let directory = fs::canonicalize(directory)?;
    if !directory.is_dir() {
      return Err(ExpriError::Message(
        "dashboard assets_dir must be a directory".into(),
      ));
    }
    let assets = Self::External { directory };
    let validate = || -> ApiResult<()> {
      let bundle = assets.current_bundle()?;
      for filename in FILES {
        let content = read_file(&bundle.directory, filename, ASSET_LIMIT)?;
        if filename == "login.html" && !content.contains("<!-- LOGIN_ERROR -->") {
          return Err(invalid("login.html must contain the LOGIN_ERROR marker"));
        }
      }
      Ok(())
    };
    validate().map_err(|error| ExpriError::Message(error.message))?;
    Ok(assets)
  }

  pub fn current_revision(&self) -> ApiResult<Option<String>> {
    match self {
      Self::Embedded => Ok(None),
      Self::External { .. } => Ok(Some(self.current_bundle()?.deployment.commit)),
    }
  }

  pub fn page(&self, filename: &str) -> ApiResult<Asset> {
    if !matches!(filename, "index.html" | "login.html") {
      return Err(ApiError::new(404, "dashboard page not found"));
    }
    match self {
      Self::Embedded => Ok(Asset {
        body: match filename {
          "index.html" => include_str!("../../dashboard_web/index.html"),
          _ => include_str!("../../dashboard_web/login.html"),
        }
        .as_bytes()
        .to_vec(),
        content_type: "text/html; charset=utf-8",
        revision: None,
      }),
      Self::External { .. } => {
        let bundle = self.current_bundle()?;
        let mut html = read_file(&bundle.directory, filename, ASSET_LIMIT)?;
        if filename == "login.html" && !html.contains("<!-- LOGIN_ERROR -->") {
          return Err(invalid("login.html must contain the LOGIN_ERROR marker"));
        }
        let commit = &bundle.deployment.commit;
        for asset in ["app.js", "styles.css"] {
          for quote in ['"', '\''] {
            html = html.replace(
              &format!("{quote}/{asset}{quote}"),
              &format!("{quote}/assets/{commit}/{asset}{quote}"),
            );
          }
        }
        html = html.replace(
          "<span class=\"read-only\">Read only</span>",
          &format!(
            "<span class=\"read-only\">Read only · AB · {}</span>",
            &commit[..8]
          ),
        );
        let branch = escape(&bundle.deployment.branch);
        let short_branch = escape(
          &bundle
            .deployment
            .branch
            .chars()
            .take(36)
            .collect::<String>(),
        );
        let label = format!(
          "<span class=\"deployment-revision\">AB · {} · built from <code title=\"Built from {branch}\">{short_branch}</code></span>",
          &commit[..8]
        );
        let closing = if html.contains("</footer>") {
          "</footer>"
        } else {
          "</main>"
        };
        html = html.replacen(closing, &format!("{label}{closing}"), 1);
        if html.len() as u64 > ASSET_LIMIT {
          return Err(invalid("rewritten dashboard page exceeds the 2 MiB limit"));
        }
        Ok(Asset {
          body: html.into_bytes(),
          content_type: "text/html; charset=utf-8",
          revision: Some(commit.clone()),
        })
      }
    }
  }

  /// Unversioned names remain compatible; generated HTML always pins a release.
  pub fn asset(&self, path: &str) -> ApiResult<Option<Asset>> {
    let (bundle, filename) = if let Some(versioned) = path.strip_prefix("/assets/") {
      let (commit, filename) = versioned
        .split_once('/')
        .ok_or_else(|| ApiError::new(404, "dashboard asset not found"))?;
      if !valid_commit(commit) || !matches!(filename, "app.js" | "styles.css") {
        return Err(ApiError::new(404, "dashboard asset not found"));
      }
      match self {
        Self::Embedded => return Err(ApiError::new(404, "dashboard asset not found")),
        Self::External { directory } => {
          if let Err(error) = fs::symlink_metadata(directory.join("releases").join(commit)) {
            return Err(if error.kind() == std::io::ErrorKind::NotFound {
              ApiError::new(404, "dashboard asset release not found")
            } else {
              io_error(error)
            });
          }
          (Some(release(directory, commit)?), filename)
        }
      }
    } else {
      let filename = match path {
        "/app.js" => "app.js",
        "/styles.css" => "styles.css",
        _ => return Ok(None),
      };
      (
        match self {
          Self::Embedded => None,
          Self::External { .. } => Some(self.current_bundle()?),
        },
        filename,
      )
    };
    let content_type = if filename == "app.js" {
      "text/javascript; charset=utf-8"
    } else {
      "text/css; charset=utf-8"
    };
    if let Some(bundle) = bundle {
      Ok(Some(Asset {
        body: read_file(&bundle.directory, filename, ASSET_LIMIT)?.into_bytes(),
        content_type,
        revision: Some(bundle.deployment.commit),
      }))
    } else {
      let content = if filename == "app.js" {
        include_str!("../../dashboard_web/app.js")
      } else {
        include_str!("../../dashboard_web/styles.css")
      };
      Ok(Some(Asset {
        body: content.as_bytes().to_vec(),
        content_type,
        revision: None,
      }))
    }
  }

  fn current_bundle(&self) -> ApiResult<Bundle> {
    let Self::External { directory } = self else {
      return Err(invalid("embedded assets have no release bundle"));
    };
    let current = directory.join("current");
    let metadata = fs::symlink_metadata(&current).map_err(io_error)?;
    if !metadata.file_type().is_symlink() {
      return Err(invalid("current must be a release symlink"));
    }
    let target = fs::read_link(current).map_err(io_error)?;
    let components: Vec<_> = target.components().collect();
    let commit = match components.as_slice() {
      [Component::Normal(prefix), Component::Normal(commit)] if *prefix == "releases" => {
        commit.to_str().filter(|commit| valid_commit(commit))
      }
      _ => None,
    }
    .ok_or_else(|| invalid("current must point to releases/<40-character git commit>"))?;
    release(directory, commit)
  }
}

fn release(directory: &Path, commit: &str) -> ApiResult<Bundle> {
  real_directory(directory)?;
  let releases = directory.join("releases");
  real_directory(&releases)?;
  let bundle = releases.join(commit);
  real_directory(&bundle)?;
  let canonical_releases = fs::canonicalize(&releases).map_err(io_error)?;
  let canonical_bundle = fs::canonicalize(&bundle).map_err(io_error)?;
  if canonical_bundle.parent() != Some(canonical_releases.as_path()) {
    return Err(invalid("release bundle must remain inside releases"));
  }
  let deployment: Deployment =
    serde_json::from_str(&read_file(&bundle, "deployment.json", MANIFEST_LIMIT)?)
      .map_err(|_| invalid("deployment.json must contain commit and branch"))?;
  if deployment.commit != commit || !valid_commit(&deployment.commit) {
    return Err(invalid(
      "deployment commit must match the release directory",
    ));
  }
  if deployment.branch.is_empty()
    || deployment.branch.len() > 256
    || deployment.branch.chars().any(char::is_control)
  {
    return Err(invalid(
      "deployment branch must contain 1 to 256 bytes without control characters",
    ));
  }
  Ok(Bundle {
    directory: bundle,
    deployment,
  })
}

fn valid_commit(commit: &str) -> bool {
  commit.len() == 40
    && commit
      .bytes()
      .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn real_directory(path: &Path) -> ApiResult<fs::Metadata> {
  let metadata = fs::symlink_metadata(path).map_err(io_error)?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(invalid(
      "dashboard bundle directories must be real directories",
    ));
  }
  Ok(metadata)
}

fn read_file(directory: &Path, filename: &str, limit: u64) -> ApiResult<String> {
  let directory_metadata = real_directory(directory)?;
  let path = directory.join(filename);
  let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
  if !metadata.is_file() || metadata.file_type().is_symlink() {
    return Err(invalid(
      "dashboard assets must be regular files without symlinks",
    ));
  }
  if metadata.len() > limit {
    return Err(invalid(format!("{filename} exceeds its size limit")));
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file: File = options.open(&path).map_err(io_error)?;
  let opened = file.metadata().map_err(io_error)?;
  let after = real_directory(directory)?;
  if !opened.is_file() || !same_file(&metadata, &opened) || !same_file(&directory_metadata, &after)
  {
    return Err(invalid("dashboard asset changed while opening"));
  }
  let canonical = fs::canonicalize(&path).map_err(io_error)?;
  if canonical.parent() != Some(directory) {
    return Err(invalid(
      "dashboard asset must remain inside its release bundle",
    ));
  }
  let mut bytes = Vec::new();
  file
    .take(limit + 1)
    .read_to_end(&mut bytes)
    .map_err(io_error)?;
  if bytes.len() as u64 > limit {
    return Err(invalid(format!("{filename} exceeds its size limit")));
  }
  String::from_utf8(bytes).map_err(|_| invalid(format!("{filename} must be UTF-8")))
}

fn same_file(first: &fs::Metadata, second: &fs::Metadata) -> bool {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    first.dev() == second.dev() && first.ino() == second.ino()
  }
  #[cfg(not(unix))]
  {
    first.is_file() == second.is_file() && first.len() == second.len()
  }
}

fn escape(text: &str) -> String {
  text
    .replace('&', "&amp;")
    .replace('<', "&lt;")
    .replace('>', "&gt;")
    .replace('"', "&quot;")
    .replace('\'', "&#39;")
}
fn invalid(message: impl Into<String>) -> ApiError {
  ApiError::new(500, format!("dashboard asset bundle: {}", message.into()))
}
fn io_error(error: std::io::Error) -> ApiError {
  invalid(error.to_string())
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  use std::os::unix::fs::symlink;

  const FIRST: &str = "1111111111111111111111111111111111111111";
  const SECOND: &str = "2222222222222222222222222222222222222222";

  pub(super) fn bundle(root: &Path, commit: &str, branch: &str) -> PathBuf {
    let directory = root.join("releases").join(commit);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("index.html"), "<main><span class=\"read-only\">Read only</span><script src=\"/app.js\"></script><link href=\"/styles.css\"><footer></footer></main>").unwrap();
    fs::write(
      directory.join("login.html"),
      "<main><link href='/styles.css'><!-- LOGIN_ERROR --></main>",
    )
    .unwrap();
    fs::write(
      directory.join("app.js"),
      format!("const revision = '{commit}';"),
    )
    .unwrap();
    fs::write(directory.join("styles.css"), format!("/* {commit} */")).unwrap();
    fs::write(
      directory.join("deployment.json"),
      serde_json::to_vec(&serde_json::json!({"commit": commit, "branch": branch})).unwrap(),
    )
    .unwrap();
    directory
  }

  fn activate(root: &Path, commit: &str) {
    let _ = fs::remove_file(root.join("current"));
    symlink(format!("releases/{commit}"), root.join("current")).unwrap();
  }

  #[test]
  fn current_pages_pin_assets_and_retained_releases_survive_activation() {
    let temporary = tempfile::tempdir().unwrap();
    bundle(temporary.path(), FIRST, "codex/first<&\"");
    bundle(temporary.path(), SECOND, "codex/second");
    activate(temporary.path(), FIRST);
    let assets = DashboardAssets::external(temporary.path().into()).unwrap();
    let first = assets.page("index.html").unwrap();
    assert_eq!(first.revision.as_deref(), Some(FIRST));
    let html = String::from_utf8(first.body).unwrap();
    assert!(html.contains(&format!("/assets/{FIRST}/app.js")));
    assert!(html.contains(&format!("/assets/{FIRST}/styles.css")));
    assert!(html.contains("codex/first&lt;&amp;&quot;"));
    assert!(html.contains("Read only · AB · 11111111"));
    assert!(html.contains("built from"));
    assert!(!html.contains("first<&\""));
    activate(temporary.path(), SECOND);
    assert_eq!(assets.current_revision().unwrap().as_deref(), Some(SECOND));
    assert_eq!(
      assets.page("login.html").unwrap().revision.as_deref(),
      Some(SECOND)
    );
    assert_eq!(
      assets
        .asset("/app.js")
        .unwrap()
        .unwrap()
        .revision
        .as_deref(),
      Some(SECOND)
    );
    let retained = assets
      .asset(&format!("/assets/{FIRST}/app.js"))
      .unwrap()
      .unwrap();
    assert_eq!(retained.revision.as_deref(), Some(FIRST));
    assert!(String::from_utf8(retained.body).unwrap().contains(FIRST));
    fs::remove_file(temporary.path().join("current")).unwrap();
    assert!(assets.page("index.html").is_err());
    assert!(
      assets
        .asset(&format!("/assets/{FIRST}/styles.css"))
        .unwrap()
        .is_some()
    );
  }

  #[test]
  fn malformed_manifest_unsafe_paths_and_missing_assets_never_fall_back() {
    let temporary = tempfile::tempdir().unwrap();
    let release = bundle(temporary.path(), FIRST, "codex/test");
    activate(temporary.path(), FIRST);
    let assets = DashboardAssets::external(temporary.path().into()).unwrap();
    for path in [
      "/assets/../app.js",
      "/assets/%2e%2e/app.js",
      "/assets/111/app.js",
      "/assets/1111111111111111111111111111111111111111/../app.js",
      "/assets/1111111111111111111111111111111111111111/deployment.json",
    ] {
      assert!(assets.asset(path).is_err(), "{path}");
    }
    fs::write(
      release.join("deployment.json"),
      "{\"commit\":\"wrong\",\"branch\":\"main\"}",
    )
    .unwrap();
    assert!(assets.page("index.html").is_err());
    assert!(DashboardAssets::external(temporary.path().into()).is_err());
    bundle(temporary.path(), FIRST, "codex/test");
    fs::remove_file(release.join("app.js")).unwrap();
    assert!(assets.asset("/app.js").is_err());
    assert!(DashboardAssets::external(temporary.path().into()).is_err());
    fs::remove_file(temporary.path().join("current")).unwrap();
    symlink("releases/../outside", temporary.path().join("current")).unwrap();
    assert!(assets.page("index.html").is_err());
  }

  #[test]
  fn bundles_reject_symlinks_non_utf8_oversized_files_and_incomplete_login() {
    let temporary = tempfile::tempdir().unwrap();
    let release = bundle(temporary.path(), FIRST, "codex/test");
    activate(temporary.path(), FIRST);
    let assets = DashboardAssets::external(temporary.path().into()).unwrap();
    let outside = temporary.path().join("private.txt");
    fs::write(&outside, "private").unwrap();
    fs::remove_file(release.join("styles.css")).unwrap();
    symlink(outside, release.join("styles.css")).unwrap();
    assert!(assets.asset("/styles.css").is_err());
    fs::remove_file(release.join("styles.css")).unwrap();
    fs::write(release.join("styles.css"), [0xff]).unwrap();
    assert!(assets.asset("/styles.css").is_err());
    fs::write(
      release.join("styles.css"),
      vec![b'x'; ASSET_LIMIT as usize + 1],
    )
    .unwrap();
    assert!(assets.asset("/styles.css").is_err());
    fs::write(release.join("login.html"), "<main>Login</main>").unwrap();
    assert!(assets.page("login.html").is_err());
    fs::remove_dir_all(&release).unwrap();
    symlink(temporary.path(), &release).unwrap();
    assert!(assets.page("index.html").is_err());
  }
}
