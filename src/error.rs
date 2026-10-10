use std::fmt::{self, Display};
use std::io;
use std::process::ExitStatus;

pub type Result<T> = std::result::Result<T, ExpriError>;

/// Preserve shell-style signal exit codes alongside ordinary process exits.
pub fn command_exit_code(status: &ExitStatus) -> Option<i32> {
  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    status
      .code()
      .or_else(|| status.signal().map(|signal| 128 + signal))
  }
  #[cfg(not(unix))]
  {
    status.code()
  }
}

#[derive(Debug)]
pub enum ExpriError {
  Io(io::Error),
  IoContext {
    action: &'static str,
    path: String,
    source: io::Error,
  },
  Toml(toml::de::Error),
  Json(serde_json::Error),
  Glob(globset::Error),
  Zip(zip::result::ZipError),
  CommandFailed {
    program: String,
    code: Option<i32>,
  },
  ServiceRejected {
    status: u16,
    detail: String,
  },
  ServiceUnavailable {
    reading_response: bool,
  },
  DownloadBusy {
    initializing: bool,
  },
  DownloadChanged,
  DownloadCancelled,
  Message(String),
}

impl ExpriError {
  pub fn exit_code(&self) -> i32 {
    match self {
      Self::CommandFailed {
        code: Some(code), ..
      } => *code,
      _ => 1,
    }
  }
}

impl Display for ExpriError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Io(error) => write!(formatter, "{error}"),
      Self::IoContext {
        action,
        path,
        source,
      } => {
        write!(formatter, "failed to {action} {path}: {source}")
      }
      Self::Toml(error) => write!(formatter, "{error}"),
      Self::Json(error) => write!(formatter, "{error}"),
      Self::Glob(error) => write!(formatter, "{error}"),
      Self::Zip(error) => write!(formatter, "{error}"),
      Self::CommandFailed { program, code } => match code {
        Some(code) => write!(formatter, "{program} exited with status {code}"),
        None => write!(formatter, "{program} terminated by signal"),
      },
      Self::ServiceRejected { status, detail } => {
        let status = reqwest::StatusCode::from_u16(*status)
          .map(|value| value.to_string())
          .unwrap_or_else(|_| status.to_string());
        write!(formatter, "service returned {status}: {detail}")
      }
      Self::ServiceUnavailable { reading_response } => write!(
        formatter,
        "service {}; saved work can be retried",
        if *reading_response {
          "response could not be read"
        } else {
          "request failed"
        }
      ),
      Self::DownloadBusy { initializing } => write!(
        formatter,
        "another fetch is {} this run; retry after it finishes",
        if *initializing {
          "initializing"
        } else {
          "downloading"
        }
      ),
      Self::DownloadChanged => write!(
        formatter,
        "selected checkpoint changed; waiting for its registered file record"
      ),
      Self::DownloadCancelled => write!(formatter, "input or asset preparation was cancelled"),
      Self::Message(message) => write!(formatter, "{message}"),
    }
  }
}

impl std::error::Error for ExpriError {}

impl From<io::Error> for ExpriError {
  fn from(error: io::Error) -> Self {
    Self::Io(error)
  }
}

impl From<toml::de::Error> for ExpriError {
  fn from(error: toml::de::Error) -> Self {
    Self::Toml(error)
  }
}

impl From<serde_json::Error> for ExpriError {
  fn from(error: serde_json::Error) -> Self {
    Self::Json(error)
  }
}

impl From<globset::Error> for ExpriError {
  fn from(error: globset::Error) -> Self {
    Self::Glob(error)
  }
}

impl From<zip::result::ZipError> for ExpriError {
  fn from(error: zip::result::ZipError) -> Self {
    Self::Zip(error)
  }
}
