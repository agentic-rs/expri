//! Compact scalar columns and a shared order for filesystem and hosted tables.
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{ExpriError, Result};
use crate::metrics::{MetricSummary, Reduction};

pub const COLUMN_LIMIT: usize = 8;
const KEY_LIMIT: usize = 256;
const DISCOVERY_LIMIT: usize = 100;
// Count more candidates than the response can display so a common key first
// seen in a later run can displace rare early keys. Keep an explicit memory
// bound by retaining the first 4,096 candidates per group, then returning the
// most common 100 within that pool. After the budget is exhausted, retained
// counts remain exact but new keys are omitted and discovery is truncated.
const DISCOVERY_CANDIDATE_LIMIT: usize = 4096;
const STRING_LIMIT: usize = 256;

pub struct ListQuery<'a> {
  pub archived: bool,
  pub origin: Option<&'a str>,
  /// Hosted experiment ID, or "ungrouped" for runs without membership.
  pub experiment_id: Option<&'a str>,
  /// Hosted experiment summaries, paginated before member history.
  pub group_experiments: bool,
  pub search: Option<&'a str>,
  pub task: Option<&'a str>,
  pub status: Option<&'a str>,
  pub limit: usize,
  pub offset: usize,
  pub table: Option<&'a TableOptions>,
}

#[derive(Clone, Debug)]
pub struct TableOptions {
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
pub struct ScalarData {
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

pub struct TableRow {
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
pub struct Columns {
  params: BTreeMap<String, DiscoveredColumn>,
  metrics: BTreeMap<String, DiscoveredColumn>,
  sampled_run_count: usize,
  pub truncated: bool,
}

struct DiscoveredColumn {
  label: String,
  run_count: usize,
}

impl Columns {
  pub fn add(&mut self, data: &ScalarData) {
    self.sampled_run_count += 1;
    if let Some(params) = &data.params {
      self.parameters(params, "", &[], 0);
    }
    for name in data.metrics.keys() {
      if !bounded_key(name) || name.trim().is_empty() || name.chars().any(char::is_control) {
        self.truncated = true;
        continue;
      }
      Self::record_column(&mut self.metrics, name, name, &mut self.truncated);
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
        Self::record_column(
          &mut self.params,
          &key,
          &path.join(" / "),
          &mut self.truncated,
        );
      }
    }
  }

  fn record_column(
    columns: &mut BTreeMap<String, DiscoveredColumn>,
    key: &str,
    label: &str,
    truncated: &mut bool,
  ) {
    if let Some(column) = columns.get_mut(key) {
      column.run_count += 1;
    } else if columns.len() == DISCOVERY_CANDIDATE_LIMIT {
      *truncated = true;
    } else {
      columns.insert(
        key.into(),
        DiscoveredColumn {
          label: label.into(),
          run_count: 1,
        },
      );
      *truncated |= columns.len() > DISCOVERY_LIMIT;
    }
  }

  fn ranked_columns(columns: &BTreeMap<String, DiscoveredColumn>) -> Vec<Value> {
    let mut columns: Vec<_> = columns.iter().collect();
    columns.sort_unstable_by(|(left_key, left), (right_key, right)| {
      right
        .run_count
        .cmp(&left.run_count)
        .then_with(|| left.label.cmp(&right.label))
        .then_with(|| left_key.cmp(right_key))
    });
    columns
      .into_iter()
      .take(DISCOVERY_LIMIT)
      .map(|(key, column)| json!({"key":key,"label":column.label,"run_count":column.run_count}))
      .collect()
  }

  pub fn response(&self) -> Value {
    json!({"params":Self::ranked_columns(&self.params),
      "metrics":Self::ranked_columns(&self.metrics),"sampled_run_count":self.sampled_run_count,"truncated":self.truncated})
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
    .or_else(|| {
      (run["queue_only"] == true)
        .then(|| run["submitted_at"].as_str())
        .flatten()
    })
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
