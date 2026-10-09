//! Incremental JSONL projection with ordered metric identities and bounded chart reads.
use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
  ApiError, ApiResult, FileTarget, ObjectStorage, RunScope, Store, bad, database, target_json,
};
use crate::metrics::{MetricPoint, MetricSeries, MetricSummary, RunMetrics};

pub(super) const POINT_LIMIT: usize = 2000;

#[derive(Default, Serialize, Deserialize)]
struct MetricState {
  line_start: u64,
  line_number: u64,
  oversized: bool,
  previous_step: Option<u64>,
  warned_steps: bool,
  first_timestamp: Option<String>,
  warnings: Vec<String>,
}

fn warning(state: &mut MetricState, text: String) {
  if state.warnings.len() < 100 {
    state.warnings.push(text.chars().take(1024).collect());
  }
}

pub(super) fn index_metrics(
  tx: &Transaction<'_>,
  key: &str,
  offset: u64,
  bytes: &[u8],
) -> ApiResult<()> {
  let saved: Option<(String, Vec<u8>)> = tx
    .query_row(
      "SELECT record,pending FROM tracking_metric_state WHERE target=?1",
      [key],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)?;
  let (mut state, mut pending) = if let Some((state, pending)) = saved {
    (
      serde_json::from_str::<MetricState>(&state)
        .map_err(|_| ApiError::new(500, "invalid metric projection state"))?,
      pending,
    )
  } else {
    (MetricState::default(), Vec::new())
  };
  for (index, byte) in bytes.iter().enumerate() {
    if !state.oversized {
      if pending.len() < crate::metrics::LINE_LIMIT {
        pending.push(*byte);
      } else {
        pending.clear();
        state.oversized = true;
      }
    }
    if *byte != b'\n' {
      continue;
    }
    state.line_number += 1;
    index_line(tx, key, &mut state, &pending, false)?;
    pending.clear();
    state.oversized = false;
    state.line_start = offset + index as u64 + 1;
  }
  tx.execute("INSERT INTO tracking_metric_state(target,record,pending) VALUES(?1,?2,?3) ON CONFLICT(target) DO UPDATE SET record=excluded.record,pending=excluded.pending",
    params![key,serde_json::to_string(&state).map_err(|_|ApiError::new(500,"cannot encode metric projection"))?,pending]).map_err(database)?;
  Ok(())
}

fn index_line(
  tx: &Transaction<'_>,
  key: &str,
  state: &mut MetricState,
  pending: &[u8],
  final_line: bool,
) -> ApiResult<()> {
  let line = state.line_number;
  if state.oversized {
    warning(
      state,
      format!("metrics.jsonl line {line} exceeds the 1 MiB limit; skipped"),
    );
  } else if !pending.iter().all(u8::is_ascii_whitespace) {
    let parsed = serde_json::from_slice::<Value>(pending)
      .map_err(|error| {
        if final_line && error.is_eof() {
          "incomplete final row".to_string()
        } else {
          "invalid JSON".to_string()
        }
      })
      .and_then(crate::metrics::event);
    match parsed {
      Ok((step, timestamp, values)) => {
        if state.first_timestamp.is_none() {
          state.first_timestamp = timestamp.clone();
        }
        if state.previous_step.is_some_and(|previous| step < previous) && !state.warned_steps {
          warning(
            state,
            format!("metrics.jsonl line {line}: steps decreased; last values use recorded order"),
          );
          state.warned_steps = true;
        }
        state.previous_step = Some(step);
        for (name, value) in values {
          index_point(
            tx,
            key,
            state.line_start,
            name,
            MetricPoint {
              step,
              value,
              timestamp: timestamp.clone(),
            },
          )?;
        }
      }
      Err(error) => warning(
        state,
        format!("metrics.jsonl line {line}: {error}; skipped"),
      ),
    }
  }
  Ok(())
}

pub(super) fn seal_metric_tail(
  tx: &Transaction<'_>,
  target: &FileTarget,
  size: u64,
) -> ApiResult<()> {
  let key = target_json(target)?;
  let saved: Option<(String, Vec<u8>)> = tx
    .query_row(
      "SELECT record,pending FROM tracking_metric_state WHERE target=?1",
      [&key],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(database)?;
  if let Some((raw, pending)) = saved {
    let mut state: MetricState =
      serde_json::from_str(&raw).map_err(|_| ApiError::new(500, "invalid metric projection"))?;
    if !pending.is_empty() || state.oversized {
      state.line_number += 1;
      index_line(tx, &key, &mut state, &pending, true)?;
      state.line_start = size;
      state.oversized = false;
      tx.execute(
        "UPDATE tracking_metric_state SET record=?2,pending=x'' WHERE target=?1",
        params![
          key,
          serde_json::to_string(&state)
            .map_err(|_| ApiError::new(500, "cannot encode metric projection"))?
        ],
      )
      .map_err(database)?;
    }
  }
  Ok(())
}

fn index_point(
  tx: &Transaction<'_>,
  key: &str,
  offset: u64,
  name: String,
  point: MetricPoint,
) -> ApiResult<()> {
  let saved:Option<(String,Option<u64>,Option<u64>)>=tx.query_row("SELECT record,first_timestamp_ordinal,last_timestamp_ordinal FROM tracking_metric_summaries WHERE target=?1 AND name=?2",params![key,name],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(database)?;
  let (summary, mut first_timestamp, mut last_timestamp) = if let Some((raw, first, last)) = saved {
    let mut summary: MetricSummary =
      serde_json::from_str(&raw).map_err(|_| ApiError::new(500, "invalid metric summary"))?;
    summary.count += 1;
    summary.missing_timestamp_count += usize::from(point.timestamp.is_none());
    summary.last = point.clone();
    if point.value < summary.min.value {
      summary.min = point.clone();
    }
    if point.value > summary.max.value {
      summary.max = point.clone();
    }
    (summary, first, last)
  } else {
    (
      MetricSummary {
        count: 1,
        missing_timestamp_count: usize::from(point.timestamp.is_none()),
        last: point.clone(),
        min: point.clone(),
        max: point.clone(),
      },
      None,
      None,
    )
  };
  let ordinal = summary.count as u64;
  if point.timestamp.is_some() {
    first_timestamp.get_or_insert(ordinal);
    last_timestamp = Some(ordinal);
  }
  tx.execute("INSERT INTO tracking_metric_rows(target,name,ordinal,raw_offset,step,value,timestamp) VALUES(?1,?2,?3,?4,?5,?6,?7)",
    params![key,name,ordinal,offset,point.step.to_string(),point.value,point.timestamp]).map_err(database)?;
  tx.execute("INSERT INTO tracking_metric_summaries(target,name,record,first_timestamp_ordinal,last_timestamp_ordinal) VALUES(?1,?2,?3,?4,?5)
    ON CONFLICT(target,name) DO UPDATE SET record=excluded.record,first_timestamp_ordinal=excluded.first_timestamp_ordinal,last_timestamp_ordinal=excluded.last_timestamp_ordinal",
    params![key,name,serde_json::to_string(&summary).map_err(|_|ApiError::new(500,"cannot encode metric summary"))?,first_timestamp,last_timestamp]).map_err(database)?;
  Ok(())
}

impl<S: ObjectStorage> Store<S> {
  pub(in crate::service) fn tracking_metrics(
    &self,
    scope: &RunScope,
    result: &mut RunMetrics,
    filters: &[String],
    retain_points: bool,
  ) -> ApiResult<()> {
    crate::metrics::validate_filters(filters).map_err(bad)?;
    let key = target_json(&FileTarget::Run {
      scope: scope.clone(),
      path: "outputs/metrics.jsonl".into(),
    })?;
    let db = self.db()?;
    let saved: Option<(String, Vec<u8>)> = db
      .query_row(
        "SELECT record,pending FROM tracking_metric_state WHERE target=?1",
        [&key],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .optional()
      .map_err(database)?;
    if let Some((raw, pending)) = saved {
      let state: MetricState =
        serde_json::from_str(&raw).map_err(|_| ApiError::new(500, "invalid metric projection"))?;
      result.first_metric_timestamp = state.first_timestamp;
      result.warnings.extend(
        state
          .warnings
          .into_iter()
          .map(|message| json!({"run_id":scope.run_id,"message":message})),
      );
      if !pending.is_empty() || state.oversized {
        result.warnings.push(json!({"run_id":scope.run_id,"message":"metrics.jsonl has an incomplete final row; not indexed yet"}));
      }
    }
    let mut statement=db.prepare("SELECT name,record,first_timestamp_ordinal,last_timestamp_ordinal FROM tracking_metric_summaries WHERE target=?1 ORDER BY name LIMIT 2001").map_err(database)?;
    let records = statement
      .query_map([&key], |row| {
        Ok((
          row.get::<_, String>(0)?,
          row.get::<_, String>(1)?,
          row.get::<_, Option<u64>>(2)?,
          row.get::<_, Option<u64>>(3)?,
        ))
      })
      .map_err(database)?
      .collect::<std::result::Result<Vec<_>, _>>()
      .map_err(database)?;
    if records.len() > 2000 {
      return Err(ApiError::new(
        413,
        "hosted metric preview exceeds the 2000-series limit",
      ));
    }
    let mut sampled = false;
    for (name, raw, first_timestamp, last_timestamp) in records {
      if !filters.is_empty() && !filters.contains(&name) {
        continue;
      }
      let summary: MetricSummary =
        serde_json::from_str(&raw).map_err(|_| ApiError::new(500, "invalid metric summary"))?;
      let mut points = Vec::new();
      if retain_points {
        let count = summary.count as u64;
        let mut ordinals = BTreeSet::from([1, count]);
        ordinals.extend(first_timestamp);
        ordinals.extend(last_timestamp);
        let budget = count.min((POINT_LIMIT - 4) as u64);
        for index in 0..budget {
          ordinals.insert(
            1 + (u128::from(index) * u128::from(count - 1)
              / u128::from(budget.saturating_sub(1).max(1))) as u64,
          );
        }
        let slots = std::iter::repeat_n("?", ordinals.len())
          .collect::<Vec<_>>()
          .join(",");
        let mut params = vec![
          rusqlite::types::Value::Text(key.clone()),
          rusqlite::types::Value::Text(name.clone()),
        ];
        params.extend(
          ordinals
            .into_iter()
            .map(|ordinal| rusqlite::types::Value::Integer(ordinal as i64)),
        );
        let mut query=db.prepare(&format!("SELECT step,value,timestamp FROM tracking_metric_rows WHERE target=? AND name=? AND ordinal IN ({slots}) ORDER BY ordinal")).map_err(database)?;
        points = query
          .query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
              row.get::<_, String>(0)?,
              row.get::<_, f64>(1)?,
              row.get::<_, Option<String>>(2)?,
            ))
          })
          .map_err(database)?
          .map(|row| {
            row.map_err(database).and_then(|(step, value, timestamp)| {
              Ok(MetricPoint {
                step: step
                  .parse()
                  .map_err(|_| ApiError::new(500, "invalid metric step"))?,
                value,
                timestamp,
              })
            })
          })
          .collect::<ApiResult<Vec<_>>>()?;
        sampled |= summary.count > points.len();
      }
      result
        .metrics
        .insert(name, MetricSeries { summary, points });
    }
    if sampled {
      result.warnings.push(json!({"run_id":scope.run_id,"message":"Hosted chart curves are sampled to bound memory; last/min/max summaries use every recorded point."}));
    }
    Ok(())
  }
}
