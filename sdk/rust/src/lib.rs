//! Local experiment instrumentation for Unix tasks.
//!
//! Metrics and parameters are written locally. The existing expri worker service
//! owns publication, retries, and system monitoring; dropping a [`Run`] does not
//! complete or cancel its task.
//!
//! ```no_run
//! use expri_sdk::{StepMode, TaskStatus};
//! use serde_json::json;
//!
//! let run = expri_sdk::init()?;
//! run.params(&json!({"learning_rate": 0.001}))?;
//! run.log(0, [("train/loss", 0.25)])?;
//! run.log_with_mode(1200, [("train/loss", 0.2)], StepMode::Global)?;
//! run.set_status(TaskStatus::Waiting)?;
//! run.register_artifact("checkpoint.pt", ["latest"])?;
//! # Ok::<(), expri_sdk::Error>(())
//! ```

#[cfg(not(unix))]
compile_error!("expri-sdk currently supports Unix workers only");

mod context;
mod metrics;
mod storage;

use std::error::Error as StdError;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde_json::Value;

pub use metrics::{MetricValue, StepMode};

/// SDK errors never mark the task as complete or cancel the supervisor.
#[derive(Debug)]
pub enum Error {
  Io(io::Error),
  Json(serde_json::Error),
  Invalid(String),
  Cli { status: Option<i32>, stderr: String },
}

impl fmt::Display for Error {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Io(error) => error.fmt(formatter),
      Self::Json(error) => error.fmt(formatter),
      Self::Invalid(message) => formatter.write_str(message),
      Self::Cli { status, stderr } => {
        write!(formatter, "expri CLI failed ({status:?}): {stderr}")
      }
    }
  }
}

impl StdError for Error {
  fn source(&self) -> Option<&(dyn StdError + 'static)> {
    match self {
      Self::Io(error) => Some(error),
      Self::Json(error) => Some(error),
      _ => None,
    }
  }
}

impl From<io::Error> for Error {
  fn from(error: io::Error) -> Self {
    Self::Io(error)
  }
}

impl From<serde_json::Error> for Error {
  fn from(error: serde_json::Error) -> Self {
    Self::Json(error)
  }
}

pub type Result<T> = std::result::Result<T, Error>;

fn invalid(message: impl Into<String>) -> Error {
  Error::Invalid(message.into())
}

/// The two task-owned observational states. Waiting does not suspend execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
  Running,
  Waiting,
}

impl TaskStatus {
  fn as_str(self) -> &'static str {
    match self {
      Self::Running => "running",
      Self::Waiting => "waiting",
    }
  }
}

/// Configure an attached run or an explicit standalone output directory.
#[derive(Default, Debug)]
pub struct RunBuilder {
  output_dir: Option<PathBuf>,
  run_dir: Option<PathBuf>,
  cli_binary: Option<OsString>,
}

impl RunBuilder {
  pub fn output_dir(mut self, path: impl Into<PathBuf>) -> Self {
    self.output_dir = Some(path.into());
    self
  }

  /// Bind local status and artifact commands to this specific managed run.
  pub fn run_dir(mut self, path: impl Into<PathBuf>) -> Self {
    self.run_dir = Some(path.into());
    self
  }

  /// Defaults to the captured EXPRI_BIN, then `expri` on PATH.
  pub fn cli_binary(mut self, path: impl Into<OsString>) -> Self {
    self.cli_binary = Some(path.into());
    self
  }

  pub fn init(self) -> Result<Run> {
    self.with_environment(context::Environment::capture())
  }

  fn with_environment(self, environment: context::Environment) -> Result<Run> {
    let context = context::Context::resolve(self, environment)?;
    let metrics = metrics::Metrics::open(&context)?;
    Ok(Run {
      initial_step: metrics.initial_step,
      metrics: Mutex::new(Some(metrics)),
      context,
      owner_pid: std::process::id(),
    })
  }
}

/// An attached metric logging session. A managed attempt shares one fixed base.
pub struct Run {
  initial_step: u64,
  metrics: Mutex<Option<metrics::Metrics>>,
  context: context::Context,
  owner_pid: u32,
}

/// Initialize from EXPRI_OUTPUT_DIR and the current managed run environment.
pub fn init() -> Result<Run> {
  Run::builder().init()
}

impl Run {
  pub fn builder() -> RunBuilder {
    RunBuilder::default()
  }

  /// The fixed base added to relative steps; MAX(history step) + 1, or zero.
  pub fn initial_step(&self) -> u64 {
    self.initial_step
  }

  pub fn output_dir(&self) -> &Path {
    &self.context.output_dir
  }

  pub fn run_dir(&self) -> Option<&Path> {
    self.context.run_dir.as_deref()
  }

  /// Append a relative step. The resulting step is `initial_step() + step`.
  pub fn log<I, K, V>(&self, step: u64, metrics: I) -> Result<()>
  where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: Into<MetricValue>,
  {
    self.log_with_mode(step, metrics, StepMode::Relative)
  }

  /// Append a metric event using relative or explicit global step coordinates.
  pub fn log_with_mode<I, K, V>(&self, step: u64, metrics: I, mode: StepMode) -> Result<()>
  where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: Into<MetricValue>,
  {
    self.with_metrics(|writer| writer.log(step, metrics, mode))
  }

  /// Write immutable effective parameters. The root must be a JSON object.
  /// Repeating identical parameters is accepted, including across SDK languages.
  pub fn params(&self, params: &Value) -> Result<()> {
    self.with_metrics(|writer| writer.params(params))
  }

  /// Sync local metric bytes to storage. Does not wait for publication.
  pub fn flush(&self) -> Result<()> {
    self.with_metrics(|writer| writer.flush())
  }

  /// Request a live task status through the captured local expri binary.
  pub fn set_status(&self, status: TaskStatus) -> Result<Value> {
    self.check_open()?;
    let mut command = self.cli_command()?;
    command
      .args(["runs", "set-status", status.as_str(), "--json", "--run-dir"])
      .arg(self.context.run_dir.as_ref().unwrap());
    cli_report(&mut command)
  }

  /// Hand a finalized local file to the existing publisher. Labels are `best`
  /// and `latest`. Relative paths start at this session's output directory.
  pub fn register_artifact<I, S>(&self, path: impl AsRef<Path>, labels: I) -> Result<Value>
  where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
  {
    self.check_open()?;
    let mut command = self.cli_command()?;
    let path = path.as_ref();
    let path = if path.is_absolute() {
      path.to_path_buf()
    } else {
      self.context.output_dir.join(path)
    };
    command
      .args(["artifact", "--json", "register"])
      .arg(path)
      .arg("--run-dir")
      .arg(self.context.run_dir.as_ref().unwrap());
    let mut seen = std::collections::BTreeSet::new();
    for label in labels {
      let label = label.as_ref();
      if !matches!(label, "best" | "latest") {
        return Err(invalid("artifact labels must be best or latest"));
      }
      if !seen.insert(label.to_owned()) {
        return Err(invalid("artifact labels must be unique"));
      }
      command.arg("--label").arg(label);
    }
    cli_report(&mut command)
  }

  /// Release local SDK descriptors. Does not finish or cancel the task.
  pub fn close(&self) -> Result<()> {
    self.check_owner()?;
    self
      .metrics
      .lock()
      .map_err(|_| invalid("SDK mutex poisoned"))?
      .take();
    Ok(())
  }

  pub fn is_closed(&self) -> bool {
    if self.owner_pid != std::process::id() {
      return true;
    }
    self
      .metrics
      .lock()
      .map_or(true, |metrics| metrics.is_none())
  }

  fn with_metrics<T>(
    &self,
    operation: impl FnOnce(&mut metrics::Metrics) -> Result<T>,
  ) -> Result<T> {
    self.check_owner()?;
    let mut guard = self
      .metrics
      .lock()
      .map_err(|_| invalid("SDK mutex poisoned"))?;
    let metrics = guard.as_mut().ok_or_else(|| invalid("run SDK is closed"))?;
    operation(metrics)
  }

  fn check_open(&self) -> Result<()> {
    self.with_metrics(|_| Ok(()))
  }

  fn check_owner(&self) -> Result<()> {
    if self.owner_pid != std::process::id() {
      return Err(invalid(
        "initialize an SDK Run in each process; do not reuse one inherited through fork",
      ));
    }
    Ok(())
  }

  fn cli_command(&self) -> Result<Command> {
    if self.context.run_dir.is_none() {
      return Err(invalid(
        "status and artifact operations require a managed run_dir",
      ));
    }
    Ok(Command::new(&self.context.cli_binary))
  }
}

fn cli_report(command: &mut Command) -> Result<Value> {
  let output = command.output()?;
  if !output.status.success() {
    return Err(Error::Cli {
      status: output.status.code(),
      stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    });
  }
  let report: Value = serde_json::from_slice(&output.stdout)?;
  if !report.is_object() {
    return Err(invalid("expri CLI must return a JSON object report"));
  }
  Ok(report)
}

#[cfg(test)]
mod tests;
