//! A disposable, bounded cache for unchanged local scalar files.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{table::ScalarData, updates};
use crate::error::Result;

const ENTRY_LIMIT: usize = 128;
const BYTE_LIMIT: usize = 64 * 1024;
const RECOVERY: Duration = Duration::from_secs(30);

type Key = (PathBuf, bool, bool);
pub(super) type Revision = Vec<Option<String>>;

struct Entry {
  revision: Revision,
  recorded_at: Instant,
  data: ScalarData,
  warnings: Vec<Value>,
}

#[derive(Default)]
pub(super) struct Cache(Mutex<BTreeMap<Key, Entry>>);

impl Cache {
  pub fn revision(run: &Path, params: bool, metrics: bool) -> Result<Revision> {
    let mut values = Vec::new();
    if params {
      values.push(updates::fixed_revision(run, "outputs/params.json")?);
    }
    if metrics {
      values.push(updates::fixed_revision(run, "outputs/metrics.jsonl")?);
    }
    Ok(values)
  }

  pub fn get(
    &self,
    run: &Path,
    params: bool,
    metrics: bool,
    revision: &Revision,
  ) -> Option<(ScalarData, Vec<Value>)> {
    let entries = self.0.lock().expect("table cache lock");
    let entry = entries.get(&(run.into(), params, metrics))?;
    (entry.revision == *revision && entry.recorded_at.elapsed() < RECOVERY)
      .then(|| (entry.data.clone(), entry.warnings.clone()))
  }

  pub fn insert(&self, key: Key, revision: Revision, data: ScalarData, warnings: Vec<Value>) {
    if !serde_json::to_vec(&(&data, &warnings)).is_ok_and(|bytes| bytes.len() <= BYTE_LIMIT) {
      return;
    }
    let mut entries = self.0.lock().expect("table cache lock");
    if !entries.contains_key(&key) && entries.len() >= ENTRY_LIMIT {
      let oldest = entries
        .iter()
        .min_by_key(|(_, entry)| entry.recorded_at)
        .map(|(key, _)| key.clone())
        .unwrap();
      entries.remove(&oldest);
    }
    entries.insert(
      key,
      Entry {
        revision,
        recorded_at: Instant::now(),
        data,
        warnings,
      },
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cache_expires_for_recovery_and_rejects_oversize_entries() {
    let cache = Cache::default();
    let path = Path::new("run");
    let revision = vec![Some("same".into())];
    cache.insert(
      (path.into(), true, false),
      revision.clone(),
      ScalarData::default(),
      vec![],
    );
    assert!(cache.get(path, true, false, &revision).is_some());
    cache
      .0
      .lock()
      .unwrap()
      .get_mut(&(path.into(), true, false))
      .unwrap()
      .recorded_at = Instant::now() - RECOVERY;
    assert!(cache.get(path, true, false, &revision).is_none());
    cache.insert(
      (path.into(), true, false),
      revision.clone(),
      ScalarData {
        params: Some(serde_json::json!({"value":"x".repeat(BYTE_LIMIT)})),
        metrics: BTreeMap::new(),
      },
      vec![],
    );
    assert!(cache.get(path, true, false, &revision).is_none());
    for index in 0..ENTRY_LIMIT + 2 {
      cache.insert(
        (PathBuf::from(format!("run-{index}")), false, true),
        revision.clone(),
        ScalarData::default(),
        vec![],
      );
    }
    assert_eq!(cache.0.lock().unwrap().len(), ENTRY_LIMIT);
  }
}
