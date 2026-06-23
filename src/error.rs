use std::fmt::{self, Display};
use std::io;

pub type Result<T> = std::result::Result<T, ExpriError>;

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
