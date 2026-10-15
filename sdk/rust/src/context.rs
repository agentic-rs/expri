use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::storage::Directory;
use crate::{Result, RunBuilder, invalid};

#[derive(Default)]
pub(crate) struct Environment {
  pub output_dir: Option<PathBuf>,
  pub run_dir: Option<PathBuf>,
  pub run_id: Option<OsString>,
  pub attempt: Option<OsString>,
  pub step_offset: Option<OsString>,
  pub cli_binary: Option<OsString>,
}

impl Environment {
  pub fn capture() -> Self {
    Self {
      output_dir: std::env::var_os("EXPRI_OUTPUT_DIR").map(PathBuf::from),
      run_dir: std::env::var_os("EXPRI_RUN_DIR").map(PathBuf::from),
      run_id: std::env::var_os("EXPRI_RUN_ID"),
      attempt: std::env::var_os("EXPRI_ATTEMPT"),
      step_offset: std::env::var_os("EXPRI_STEP_OFFSET"),
      cli_binary: std::env::var_os("EXPRI_BIN"),
    }
  }
}

pub(crate) struct Context {
  pub output_dir: PathBuf,
  pub run_dir: Option<PathBuf>,
  pub attempt: Option<u64>,
  pub step_offset: Option<u64>,
  pub cli_binary: OsString,
}

impl Context {
  pub fn resolve(builder: RunBuilder, environment: Environment) -> Result<Self> {
    if builder.run_dir.as_deref().is_some_and(|path| {
      path
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    }) {
      return Err(invalid(
        "run_dir must not contain parent-directory traversal",
      ));
    }
    let configured_output = environment
      .output_dir
      .as_deref()
      .map(absolute)
      .transpose()?;
    let explicit_run = builder.run_dir.as_deref().map(absolute).transpose()?;
    let output_dir = builder
      .output_dir
      .as_deref()
      .map(absolute)
      .transpose()?
      .or_else(|| explicit_run.as_ref().map(|run| run.join("outputs")))
      .or_else(|| configured_output.clone())
      .ok_or_else(|| invalid("EXPRI_OUTPUT_DIR is unset; pass output_dir explicitly"))?;
    if explicit_run
      .as_ref()
      .is_some_and(|run| output_dir != run.join("outputs"))
    {
      return Err(invalid(
        "output_dir must equal run_dir/outputs when run_dir is explicit",
      ));
    }
    let redirected = configured_output
      .as_ref()
      .is_some_and(|path| path != &output_dir);
    let inherited_run = environment.run_dir.as_deref().map(absolute).transpose()?;
    let run_dir = explicit_run
      .or_else(|| inherited_run.filter(|run| !redirected && output_dir == run.join("outputs")));
    let mut attempt = if redirected {
      None
    } else {
      environment
        .attempt
        .as_ref()
        .map(|value| decimal_u64(value.to_str().unwrap_or(""), "EXPRI_ATTEMPT", 1))
        .transpose()?
    };
    let step_offset = if redirected {
      None
    } else {
      environment
        .step_offset
        .as_ref()
        .map(|value| decimal_u64(value.to_str().unwrap_or(""), "EXPRI_STEP_OFFSET", 0))
        .transpose()?
    };
    if attempt.is_none() && step_offset.is_some() {
      return Err(invalid("EXPRI_STEP_OFFSET requires EXPRI_ATTEMPT"));
    }
    if attempt.is_some_and(|value| value > 1) && step_offset.is_none() {
      return Err(invalid("continuation attempts require EXPRI_STEP_OFFSET"));
    }
    if attempt == Some(1) && step_offset.is_some_and(|value| value != 0) {
      return Err(invalid(
        "the initial attempt must have zero EXPRI_STEP_OFFSET",
      ));
    }
    if attempt.is_none()
      && !redirected
      && configured_output.is_some()
      && let (Some(run), Some(id)) = (&run_dir, environment.run_id.as_ref())
      && run.file_name() == Some(id.as_os_str())
      && output_dir == run.join("outputs")
    {
      attempt = initial_managed_attempt(run, id.to_str().unwrap_or(""))?;
    }
    Ok(Self {
      output_dir,
      run_dir,
      attempt,
      step_offset,
      cli_binary: builder
        .cli_binary
        .or(environment.cli_binary)
        .unwrap_or_else(|| "expri".into()),
    })
  }
}

pub(crate) fn decimal_u64(value: &str, label: &str, minimum: u64) -> Result<u64> {
  if value.is_empty()
    || !value.bytes().all(|byte| byte.is_ascii_digit())
    || (value.len() > 1 && value.starts_with('0'))
  {
    return Err(invalid(format!(
      "{label} must be a canonical unsigned decimal integer"
    )));
  }
  value
    .parse::<u64>()
    .ok()
    .filter(|number| *number >= minimum)
    .ok_or_else(|| invalid(format!("{label} must fit u64")))
}

fn absolute(path: &Path) -> Result<PathBuf> {
  // Lexical normalization mirrors Python abspath, without following symlinks.
  let path = std::path::absolute(path)?;
  let mut normalized = PathBuf::new();
  for component in path.components() {
    match component {
      std::path::Component::ParentDir => {
        normalized.pop();
      }
      std::path::Component::CurDir => {}
      component => normalized.push(component.as_os_str()),
    }
  }
  Ok(normalized)
}

fn initial_managed_attempt(run_dir: &Path, run_id: &str) -> Result<Option<u64>> {
  let directory = Directory::open(run_dir, false)?;
  let raw = directory
    .read("run-state.json", 256 * 1024)?
    .ok_or_else(|| invalid("managed run-state.json is missing"))?;
  let state: Value = serde_json::from_slice(&raw)?;
  if state.get("schema_version").and_then(Value::as_u64) != Some(1)
    || state.get("run_id").and_then(Value::as_str) != Some(run_id)
  {
    return Err(invalid("managed run-state.json does not match this run"));
  }
  if let Some(history) = state.get("task_attempts").filter(|value| !value.is_null())
    && (history.get("protocol_version").and_then(Value::as_u64) != Some(1)
      || history.get("current_attempt").and_then(Value::as_u64) != Some(1))
  {
    return Err(invalid(
      "managed continuation requires EXPRI_ATTEMPT and EXPRI_STEP_OFFSET",
    ));
  }
  Ok(Some(1))
}
