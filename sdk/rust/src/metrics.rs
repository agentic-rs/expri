use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Number, Value, json};

use crate::context::{Context, decimal_u64};
use crate::storage::{Directory, Lease};
use crate::{Result, invalid};

const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_PARAMS_BYTES: usize = 1024 * 1024;
const MAX_METRICS_BYTES: u64 = 128 * 1024 * 1024;

/// Whether a supplied step is relative to this session or already global.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StepMode {
  #[default]
  Relative,
  Global,
}

/// A finite metric number. Booleans are deliberately not accepted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MetricValue {
  Signed(i64),
  Unsigned(u64),
  Float(f64),
}

macro_rules! signed_values {
  ($($number:ty),+) => {$(
    impl From<$number> for MetricValue {
      fn from(value: $number) -> Self { Self::Signed(value.into()) }
    }
  )+};
}
macro_rules! unsigned_values {
  ($($number:ty),+) => {$(
    impl From<$number> for MetricValue {
      fn from(value: $number) -> Self { Self::Unsigned(value.into()) }
    }
  )+};
}
signed_values!(i8, i16, i32, i64);
unsigned_values!(u8, u16, u32, u64);

impl From<f32> for MetricValue {
  fn from(value: f32) -> Self {
    Self::Float(value.into())
  }
}

impl From<f64> for MetricValue {
  fn from(value: f64) -> Self {
    Self::Float(value)
  }
}

impl TryFrom<&Value> for MetricValue {
  type Error = crate::Error;

  fn try_from(value: &Value) -> Result<Self> {
    if let Some(value) = value.as_i64() {
      Ok(Self::Signed(value))
    } else if let Some(value) = value.as_u64() {
      Ok(Self::Unsigned(value))
    } else if let Some(value) = value.as_f64() {
      Ok(Self::Float(value))
    } else {
      Err(invalid(
        "metric values must be finite numbers, excluding booleans",
      ))
    }
  }
}

impl MetricValue {
  fn number(self) -> Result<Number> {
    match self {
      Self::Signed(value) => Ok(value.into()),
      Self::Unsigned(value) => Ok(value.into()),
      Self::Float(value) => {
        Number::from_f64(value).ok_or_else(|| invalid("metric values must be finite numbers"))
      }
    }
  }
}

pub(crate) struct Metrics {
  pub initial_step: u64,
  legacy_first_local_step: Option<u64>,
  attempt: Option<u64>,
  directory: Directory,
  attempt_directory: Option<Directory>,
  file: File,
}

impl Metrics {
  pub fn open(context: &Context) -> Result<Self> {
    let directory = Directory::open(&context.output_dir, true)?;
    let file = directory
      .file(
        "metrics.jsonl",
        libc::O_RDWR | libc::O_CREAT | libc::O_APPEND,
        false,
      )?
      .ok_or_else(|| invalid("metrics.jsonl could not be opened"))?;
    let lease = Lease::exclusive(&file)?;
    let attempt_directory = context
      .attempt
      .map(|attempt| directory.child("attempts")?.child(&attempt.to_string()))
      .transpose()?;
    let origin = attempt_directory
      .as_ref()
      .map(|directory| read_origin(directory, context))
      .transpose()?
      .flatten();
    let next_step = next_global_step(&file)?;
    let (initial_step, legacy_first_local_step) = if let Some(origin) = origin {
      origin
    } else {
      let base = next_step.ok_or_else(|| invalid("global metric steps exhausted u64"))?;
      if context.attempt.is_some_and(|attempt| attempt > 1) && context.step_offset != Some(base) {
        return Err(invalid(
          "EXPRI_STEP_OFFSET must equal the next global metric step",
        ));
      }
      if let Some(directory) = &attempt_directory {
        let raw = json_bytes(
          &json!({
            "schema_version": 2, "attempt": context.attempt.unwrap(), "step_offset": base.to_string(),
          }),
          4096,
          "attempt step origin",
        )?;
        directory.publish("step-origin.json", &raw, true)?;
      }
      (base, None)
    };
    drop(lease);
    Ok(Self {
      initial_step,
      legacy_first_local_step,
      attempt: context.attempt,
      directory,
      attempt_directory,
      file,
    })
  }

  pub fn log<I, K, V>(&mut self, step: u64, metrics: I, mode: StepMode) -> Result<()>
  where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: Into<MetricValue>,
  {
    let global_step = match mode {
      StepMode::Relative => {
        if self.legacy_first_local_step.is_some_and(|step| step != 0) {
          return Err(invalid(
            "legacy attempt step origin uses a first-local-step anchor; use global mode or start a new attempt",
          ));
        }
        self
          .initial_step
          .checked_add(step)
          .ok_or_else(|| invalid("mapped global step overflows u64"))?
      }
      StepMode::Global => step,
    };
    let mut values = Map::new();
    for (name, value) in metrics {
      let name = name.as_ref();
      validate_name(name)?;
      if values
        .insert(name.to_owned(), Value::Number(value.into().number()?))
        .is_some()
      {
        return Err(invalid("metric names must be unique within an event"));
      }
    }
    validate_metrics(&Value::Object(values.clone()))?;
    let mut event = json!({
      "schema_version": 1,
      "step": global_step,
      "step_mode": match mode { StepMode::Relative => "relative", StepMode::Global => "global" },
      "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
      "metrics": values,
    });
    if mode == StepMode::Relative {
      event["local_step"] = step.into();
    }
    if let Some(attempt) = self.attempt {
      event["attempt"] = attempt.into();
    }
    let raw = json_bytes(&event, MAX_EVENT_BYTES, "metrics event")?;
    let _lease = Lease::exclusive(&self.file)?;
    let size = self.file.metadata()?.len();
    if size != 0 {
      let mut last = [0];
      if self.file.read_at(&mut last, size - 1)? != 1 || last != *b"\n" {
        return Err(invalid(
          "metrics.jsonl ends without a newline; repair the incomplete row before appending",
        ));
      }
    }
    // O_APPEND plus the interoperable flock protects complete cooperative rows.
    (&self.file).write_all(&raw)?;
    Ok(())
  }

  pub fn params(&mut self, params: &Value) -> Result<()> {
    if !params.is_object() {
      return Err(invalid("params must be a JSON object with string keys"));
    }
    validate_params_depth(params, 0)?;
    let envelope = json!({"schema_version": 1, "params": params});
    let raw = json_bytes(&envelope, MAX_PARAMS_BYTES, "params")?;
    let directory = if self.attempt.is_some_and(|attempt| attempt > 1) {
      self.attempt_directory.as_ref().unwrap()
    } else {
      &self.directory
    };
    if let Some(existing) = directory.read("params.json", MAX_PARAMS_BYTES)? {
      return same_params(&existing, &envelope);
    }
    if !directory.publish("params.json", &raw, false)? {
      let existing = directory
        .read("params.json", MAX_PARAMS_BYTES)?
        .ok_or_else(|| invalid("params.json disappeared during publication"))?;
      same_params(&existing, &envelope)?;
    }
    Ok(())
  }

  pub fn flush(&self) -> Result<()> {
    self.file.sync_data()?;
    Ok(())
  }
}

fn same_params(raw: &[u8], expected: &Value) -> Result<()> {
  let existing: Value = serde_json::from_slice(raw)?;
  if &existing != expected {
    return Err(invalid(
      "params.json already contains different effective parameters",
    ));
  }
  Ok(())
}

fn read_origin(directory: &Directory, context: &Context) -> Result<Option<(u64, Option<u64>)>> {
  let Some(raw) = directory.read("step-origin.json", 4096)? else {
    return Ok(None);
  };
  let origin: Value = serde_json::from_slice(&raw)?;
  let version = origin.get("schema_version").and_then(Value::as_u64);
  let fields = if version == Some(1) { 4 } else { 3 };
  if !matches!(version, Some(1 | 2))
    || origin
      .as_object()
      .is_none_or(|object| object.len() != fields)
    || origin.get("attempt").and_then(Value::as_u64) != context.attempt
  {
    return Err(invalid("attempt step origin does not match this attempt"));
  }
  let base = decimal_u64(
    origin
      .get("step_offset")
      .and_then(Value::as_str)
      .unwrap_or(""),
    "attempt step offset",
    0,
  )?;
  let first = if version == Some(1) {
    Some(decimal_u64(
      origin
        .get("first_local_step")
        .and_then(Value::as_str)
        .unwrap_or(""),
      "attempt first local step",
      0,
    )?)
  } else {
    None
  };
  if context.attempt.is_some_and(|attempt| attempt > 1) && context.step_offset != Some(base) {
    return Err(invalid(
      "EXPRI_STEP_OFFSET conflicts with the durable attempt step origin",
    ));
  }
  Ok(Some((base, first)))
}

fn next_global_step(file: &File) -> Result<Option<u64>> {
  let size = file.metadata()?.len();
  if size > MAX_METRICS_BYTES {
    return Err(invalid(
      "metrics.jsonl exceeds the 128 MiB continuation limit",
    ));
  }
  let mut maximum = None::<u64>;
  let mut offset = 0;
  let mut pending = Vec::new();
  let mut buffer = [0; 65536];
  while offset < size {
    let limit = buffer.len().min((size - offset) as usize);
    let count = file.read_at(&mut buffer[..limit], offset)?;
    if count == 0 {
      return Err(invalid(
        "metrics.jsonl changed while mapping continuation steps",
      ));
    }
    offset += count as u64;
    pending.extend_from_slice(&buffer[..count]);
    let mut start = 0;
    for end in pending
      .iter()
      .enumerate()
      .filter_map(|(index, byte)| (*byte == b'\n').then_some(index))
    {
      let row = &pending[start..end];
      if row.len() + 1 > MAX_EVENT_BYTES {
        return Err(invalid("metrics.jsonl contains an oversized event"));
      }
      if !row.iter().all(u8::is_ascii_whitespace) {
        let step = history_step(row)?;
        maximum = Some(maximum.map_or(step, |current| current.max(step)));
      }
      start = end + 1;
    }
    pending.drain(..start);
    if pending.len() > MAX_EVENT_BYTES {
      return Err(invalid("metrics.jsonl contains an oversized event"));
    }
  }
  if !pending.is_empty() {
    return Err(invalid(
      "metrics.jsonl ends without a newline; repair the incomplete row before appending",
    ));
  }
  Ok(match maximum {
    Some(maximum) => maximum.checked_add(1),
    None => Some(0),
  })
}

fn history_step(raw: &[u8]) -> Result<u64> {
  let event: Value = serde_json::from_slice(raw)?;
  let step = event
    .get("step")
    .and_then(Value::as_u64)
    .ok_or_else(|| invalid("metrics.jsonl contains an invalid global step"))?;
  if event
    .get("schema_version")
    .is_some_and(|version| version.as_u64() != Some(1))
  {
    return Err(invalid(
      "metrics.jsonl contains an unsupported schema version",
    ));
  }
  if let Some(timestamp) = event.get("timestamp") {
    validate_timestamp(timestamp)?;
  }
  validate_metrics(event.get("metrics").unwrap_or(&Value::Null))?;
  Ok(step)
}

fn validate_timestamp(timestamp: &Value) -> Result<()> {
  let timestamp = timestamp
    .as_str()
    .ok_or_else(|| invalid("metrics.jsonl timestamp must be RFC3339"))?;
  let bytes = timestamp.as_bytes();
  if bytes.len() < 20
    || bytes[4] != b'-'
    || bytes[7] != b'-'
    || !matches!(bytes[10], b'T' | b't')
    || bytes[13] != b':'
    || bytes[16] != b':'
    || DateTime::parse_from_rfc3339(&timestamp.to_ascii_uppercase()).is_err()
  {
    return Err(invalid("metrics.jsonl timestamp must be RFC3339"));
  }
  Ok(())
}

fn validate_metrics(metrics: &Value) -> Result<()> {
  let values = metrics
    .as_object()
    .filter(|values| !values.is_empty())
    .ok_or_else(|| invalid("metrics must be a nonempty mapping"))?;
  for (name, value) in values {
    validate_name(name)?;
    if !value.is_number() || value.as_f64().is_none_or(|value| !value.is_finite()) {
      return Err(invalid(
        "metric values must be finite numbers, excluding booleans",
      ));
    }
  }
  Ok(())
}

fn validate_name(name: &str) -> Result<()> {
  if name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
    return Err(invalid(
      "metric names must contain 1..256 UTF-8 bytes without control characters",
    ));
  }
  Ok(())
}

fn validate_params_depth(value: &Value, depth: usize) -> Result<()> {
  match value {
    Value::Array(children) => {
      if depth >= 64 {
        return Err(invalid(
          "params must not exceed 64 object or array levels, including the parameter root",
        ));
      }
      for child in children {
        validate_params_depth(child, depth + 1)?;
      }
    }
    Value::Object(children) => {
      if depth >= 64 {
        return Err(invalid(
          "params must not exceed 64 object or array levels, including the parameter root",
        ));
      }
      for child in children.values() {
        validate_params_depth(child, depth + 1)?;
      }
    }
    _ => {}
  }
  Ok(())
}

fn json_bytes(value: &Value, limit: usize, label: &str) -> Result<Vec<u8>> {
  let mut raw = serde_json::to_vec(value)?;
  raw.push(b'\n');
  if raw.len() > limit {
    return Err(invalid(format!("{label} exceeds its metadata limit")));
  }
  Ok(raw)
}
