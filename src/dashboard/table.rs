//! Compact scalar columns and a shared order for filesystem and hosted tables.
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{ExpriError, Result};
use crate::metrics::{MetricSummary, Reduction};

pub(crate) const COLUMN_LIMIT: usize = 8;
const KEY_LIMIT: usize = 256;
const DISCOVERY_LIMIT: usize = 100;
const STRING_LIMIT: usize = 256;

pub(crate) struct ListQuery<'a> {
  pub origin: Option<&'a str>,
  pub search: Option<&'a str>,
  pub task: Option<&'a str>,
  pub status: Option<&'a str>,
  pub limit: usize,
  pub offset: usize,
  pub table: Option<&'a TableOptions>,
}

#[derive(Clone, Debug)]
pub(crate) struct TableOptions {
  pub params: Vec<String>,
  pub metrics: Vec<String>,
  pub reduction: Reduction,
  pub sort: String,
  pub descending: bool,
}

impl TableOptions {
  pub fn parse(
    params: Vec<String>,
    metrics: Vec<String>,
    reduction: &str,
    sort: Option<&str>,
    direction: Option<&str>,
  ) -> Result<Self> {
    if params.len() + metrics.len() > COLUMN_LIMIT
      || params.iter().collect::<BTreeSet<_>>().len() != params.len()
      || metrics.iter().collect::<BTreeSet<_>>().len() != metrics.len()
    {
      return Err(message("select at most eight distinct table columns"));
    }
    for key in &params {
      validate_pointer(key)?;
    }
    crate::metrics::validate_filters(&metrics)?;
    if metrics.iter().any(|key| !bounded_key(key)) {
      return Err(message(
        "metric table columns exceed the 256-byte JSON key budget",
      ));
    }
    let reduction = match reduction {
      "last" => Reduction::Last,
      "min" => Reduction::Min,
      "max" => Reduction::Max,
      _ => return Err(message("reduction must be last, min, or max")),
    };
    let sort = sort.unwrap_or("started_at");
    if !matches!(sort, "started_at" | "run_id" | "status" | "origin")
      && !sort
        .strip_prefix("param:")
        .is_some_and(|key| params.iter().any(|selected| selected == key))
      && !sort
        .strip_prefix("metric:")
        .is_some_and(|key| metrics.iter().any(|selected| selected == key))
    {
      return Err(message(
        "sort must be started_at, run_id, status, origin, or a selected table column",
      ));
    }
    let descending = match direction.unwrap_or("desc") {
      "asc" => false,
      "desc" => true,
      _ => return Err(message("direction must be asc or desc")),
    };
    Ok(Self {
      params,
      metrics,
      reduction,
      sort: sort.into(),
      descending,
    })
  }

  pub fn needs_params(&self) -> bool {
    !self.params.is_empty()
  }

  pub fn needs_metrics(&self) -> bool {
    !self.metrics.is_empty()
  }

  pub fn sort_rows(&self, rows: &mut [TableRow]) {
    rows.sort_by(|left, right| {
      let order = if self.sort == "started_at" {
        compare_optional(
          timestamp(&left.run),
          timestamp(&right.run),
          self.descending,
          Ord::cmp,
        )
      } else if let Some(key) = self.sort.strip_prefix("metric:") {
        compare_optional(
          left
            .metrics
            .get(key)
            .map(|values| values[self.reduction_index()]),
          right
            .metrics
            .get(key)
            .map(|values| values[self.reduction_index()]),
          self.descending,
          f64::total_cmp,
        )
      } else {
        compare_optional(
          self.sort_value(left),
          self.sort_value(right),
          self.descending,
          |left, right| compare_scalar(left, right),
        )
      };
      order
        .then_with(|| {
          left.run["run_id"]
            .as_str()
            .cmp(&right.run["run_id"].as_str())
        })
        .then_with(|| {
          left.run["origin"]
            .as_str()
            .cmp(&right.run["origin"].as_str())
        })
    });
  }

  fn sort_value<'a>(&self, row: &'a TableRow) -> Option<&'a Value> {
    if let Some(key) = self.sort.strip_prefix("param:") {
      return row
        .params
        .as_ref()
        .and_then(|params| params.pointer(key))
        .filter(|value| scalar(value));
    }
    row.run.get(&self.sort).filter(|value| scalar(value))
  }

  fn reduction_index(&self) -> usize {
    match self.reduction {
      Reduction::Last => 0,
      Reduction::Min => 1,
      Reduction::Max => 2,
    }
  }

  pub fn project(&self, mut row: TableRow) -> Value {
    let mut truncated = false;
    let params: BTreeMap<_, _> = self
      .params
      .iter()
      .map(|key| {
        let value = row
          .params
          .as_ref()
          .and_then(|params| params.pointer(key))
          .filter(|value| scalar(value));
        (
          key.clone(),
          value
            .map(|value| preview_scalar(value, &mut truncated))
            .unwrap_or(Value::Null),
        )
      })
      .collect();
    let metrics: BTreeMap<_, _> = self
      .metrics
      .iter()
      .map(|key| {
        (
          key.clone(),
          row
            .metrics
            .get(key)
            .map(|values| values[self.reduction_index()]),
        )
      })
      .collect();
    row.run["table_values"] = json!({"params":params,"metrics":metrics});
    row.run["table_values_truncated"] = json!(truncated);
    row.run
  }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct ScalarData {
  pub params: Option<Value>,
  pub metrics: BTreeMap<String, [f64; 3]>,
}

impl ScalarData {
  pub fn add_metric(&mut self, name: String, summary: MetricSummary) {
    self.metrics.insert(
      name,
      [summary.last.value, summary.min.value, summary.max.value],
    );
  }
}

pub(crate) struct TableRow {
  pub run: Value,
  pub params: Option<Value>,
  pub metrics: BTreeMap<String, [f64; 3]>,
}

impl TableRow {
  pub fn new(run: Value, data: ScalarData) -> Self {
    Self {
      run,
      params: data.params,
      metrics: data.metrics,
    }
  }
}

#[derive(Default)]
pub(crate) struct Columns {
  params: BTreeMap<String, String>,
  metrics: BTreeSet<String>,
  pub truncated: bool,
}

impl Columns {
  pub fn add(&mut self, data: &ScalarData) {
    if let Some(params) = &data.params {
      self.parameters(params, "", &[], 0);
    }
    for name in data.metrics.keys() {
      if !bounded_key(name) || name.trim().is_empty() || name.chars().any(char::is_control) {
        self.truncated = true;
        continue;
      }
      if self.metrics.contains(name) {
        continue;
      }
      if self.metrics.len() == DISCOVERY_LIMIT {
        self.truncated = true;
      } else {
        self.metrics.insert(name.clone());
      }
    }
  }

  fn parameters(&mut self, value: &Value, pointer: &str, path: &[String], depth: usize) {
    let Some(fields) = value.as_object() else {
      return;
    };
    if depth > 16 {
      self.truncated = true;
      return;
    }
    for (name, value) in fields {
      let key = format!("{pointer}/{}", name.replace('~', "~0").replace('/', "~1"));
      let mut path = path.to_vec();
      path.push(name.clone());
      if validate_pointer(&key).is_err() {
        self.truncated = true;
      } else if value.is_object() {
        self.parameters(value, &key, &path, depth + 1);
      } else if scalar(value) || value.is_null() {
        if self.params.contains_key(&key) {
          continue;
        }
        if self.params.len() == DISCOVERY_LIMIT {
          self.truncated = true;
        } else {
          self.params.insert(key, path.join(" / "));
        }
      }
    }
  }

  pub fn response(&self) -> Value {
    json!({"params":self.params.iter().map(|(key,label)|json!({"key":key,"label":label})).collect::<Vec<_>>(),
      "metrics":self.metrics.iter().map(|key|json!({"key":key,"label":key})).collect::<Vec<_>>(),"truncated":self.truncated})
  }
}

fn validate_pointer(key: &str) -> Result<()> {
  if !key.starts_with('/') || !bounded_key(key) || key.chars().any(char::is_control) {
    return Err(message(
      "parameter columns must use JSON pointers of at most 256 UTF-8 bytes without control characters",
    ));
  }
  let mut bytes = key.bytes();
  while let Some(byte) = bytes.next() {
    if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
      return Err(message(
        "parameter columns contain an invalid JSON pointer escape",
      ));
    }
  }
  Ok(())
}

fn bounded_key(key: &str) -> bool {
  key.len() <= KEY_LIMIT && serde_json::to_vec(key).is_ok_and(|bytes| bytes.len() <= KEY_LIMIT + 2)
}

fn scalar(value: &Value) -> bool {
  value.is_boolean() || value.is_number() || value.is_string()
}

fn timestamp(run: &Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
  run["started_at"]
    .as_str()
    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
}

fn compare_optional<T>(
  left: Option<T>,
  right: Option<T>,
  descending: bool,
  compare: impl FnOnce(&T, &T) -> Ordering,
) -> Ordering {
  match (left, right) {
    (Some(left), Some(right)) => {
      let order = compare(&left, &right);
      if descending { order.reverse() } else { order }
    }
    (Some(_), None) => Ordering::Less,
    (None, Some(_)) => Ordering::Greater,
    (None, None) => Ordering::Equal,
  }
}

fn compare_scalar(left: &Value, right: &Value) -> Ordering {
  match (left, right) {
    (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
    (Value::Number(left), Value::Number(right)) => compare_number(left, right),
    (Value::String(left), Value::String(right)) => left.cmp(right),
    _ => scalar_rank(left).cmp(&scalar_rank(right)),
  }
}

fn scalar_rank(value: &Value) -> u8 {
  match value {
    Value::Bool(_) => 0,
    Value::Number(_) => 1,
    _ => 2,
  }
}

/// Compare decimal representations without rounding large integer parameters to f64.
fn compare_number(left: &serde_json::Number, right: &serde_json::Number) -> Ordering {
  fn decimal(number: &serde_json::Number) -> (bool, i32, String) {
    let raw = number.to_string();
    let negative = raw.starts_with('-');
    let raw = raw.trim_start_matches('-');
    let (mantissa, exponent) = raw
      .split_once(['e', 'E'])
      .map_or((raw, 0), |(mantissa, exponent)| {
        (mantissa, exponent.parse::<i32>().unwrap_or(0))
      });
    let decimals = mantissa
      .split_once('.')
      .map_or(0, |(_, decimals)| decimals.len() as i32);
    let digits = mantissa
      .replace('.', "")
      .trim_start_matches('0')
      .to_string();
    if digits.is_empty() {
      return (false, i32::MIN, String::new());
    }
    (negative, exponent - decimals + digits.len() as i32, digits)
  }
  let (left_negative, left_magnitude, left_digits) = decimal(left);
  let (right_negative, right_magnitude, right_digits) = decimal(right);
  if left_negative != right_negative {
    return right_negative.cmp(&left_negative);
  }
  let magnitude = left_magnitude.cmp(&right_magnitude);
  let order = magnitude.then_with(|| {
    let count = left_digits.len().max(right_digits.len());
    left_digits
      .bytes()
      .chain(std::iter::repeat(b'0'))
      .take(count)
      .cmp(
        right_digits
          .bytes()
          .chain(std::iter::repeat(b'0'))
          .take(count),
      )
  });
  if left_negative {
    order.reverse()
  } else {
    order
  }
}

fn preview_scalar(value: &Value, truncated: &mut bool) -> Value {
  let Some(value) = value.as_str() else {
    return value.clone();
  };
  if serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= STRING_LIMIT) {
    return json!(value);
  }
  *truncated = true;
  let mut output = String::new();
  let mut budget = STRING_LIMIT - 5; // Quotes plus a UTF-8 ellipsis.
  for character in value.chars() {
    let cost = match character {
      '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
      character if character.is_control() => 6,
      _ => character.len_utf8(),
    };
    if cost > budget {
      break;
    }
    budget -= cost;
    output.push(character);
  }
  output.push('…');
  json!(output)
}

fn message(value: &str) -> ExpriError {
  ExpriError::Message(value.into())
}

#[cfg(test)]
mod tests;
