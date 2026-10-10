use std::path::Path;

use serde_json::{Value, json};

use super::fs;
use crate::error::Result;
use crate::run_artifacts::{INVENTORY_LIMIT, INVENTORY_PATH, scan};

/// Publish names and sizes, while checkpoints still require explicit selection.
pub(super) fn record(run_dir: &Path) -> Result<()> {
  if crate::service::registrations::is_closed(run_dir)? {
    let path = run_dir.join(INVENTORY_PATH);
    fs::optional_regular(&path)?;
    if fs::inspect(&path)?.is_none() {
      return Err(fs::message("sealed run has no artifact inventory"));
    }
    return Ok(());
  }
  let inventory = scan(run_dir)?;
  let mut registrations = crate::service::registrations::records(run_dir)?;
  registrations.sort_by(|first, second| {
    second
      .labels
      .len()
      .cmp(&first.labels.len())
      .then_with(|| first.path.cmp(&second.path))
  });
  let mut entries = std::collections::BTreeMap::new();
  let mut truncated = inventory.truncated;
  let mut encoded_bytes = 0;
  let mut candidates = Vec::new();
  for registration in registrations {
    let mut entry = json!({
      "path":registration.path,"size":registration.size,
      "sync_status":registration.sync_status,
    });
    if let Some(digest) = registration.sha256 {
      entry["sha256"] = json!(digest);
    }
    if !registration.labels.is_empty() {
      entry["labels"] = json!(registration.labels);
    }
    if let Some(error) = registration.sync_error {
      entry["sync_error"] = json!(error);
    }
    candidates.push(entry);
  }
  candidates.extend(
    inventory
      .files
      .iter()
      .map(serde_json::to_value)
      .collect::<std::result::Result<Vec<_>, _>>()?,
  );
  for entry in candidates {
    let path = entry["path"].as_str().expect("artifact path").to_string();
    if entries.contains_key(&path) {
      continue;
    }
    let length = serde_json::to_vec_pretty(&entry)?.len() + 24;
    if entries.len() == crate::run_artifacts::FILE_LIMIT
      || encoded_bytes + length > INVENTORY_LIMIT - 1024
    {
      truncated = true;
      continue;
    }
    encoded_bytes += length;
    entries.insert(path, entry);
  }
  let files = entries.into_values().collect::<Vec<_>>();
  fs::directories(&run_dir.join("outputs"))?;
  let path = run_dir.join(INVENTORY_PATH);
  if fs::inspect(&path)?.is_some() {
    let previous = fs::read_bounded(&path, INVENTORY_LIMIT as u64)
      .ok()
      .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    if let Some(previous) = previous
      && previous["schema_version"] == 1
      && previous["files"] == serde_json::to_value(&files)?
      && previous["truncated"] == truncated
    {
      return Ok(());
    }
  }
  fs::atomic_json(
    &path,
    &json!({
      "schema_version": 1,
      "recorded_at": chrono::Utc::now().to_rfc3339(),
      "files": files,
      "truncated": truncated,
    }),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn inventory_changes_only_when_reported_outputs_change() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("outputs")).unwrap();
    record(&std::fs::canonicalize(root.path()).unwrap()).unwrap();
    let first = std::fs::read(root.path().join(INVENTORY_PATH)).unwrap();
    record(&std::fs::canonicalize(root.path()).unwrap()).unwrap();
    assert_eq!(
      std::fs::read(root.path().join(INVENTORY_PATH)).unwrap(),
      first
    );
    std::fs::write(root.path().join("outputs/checkpoint.pt"), "checkpoint").unwrap();
    record(&std::fs::canonicalize(root.path()).unwrap()).unwrap();
    let changed: Value =
      serde_json::from_slice(&std::fs::read(root.path().join(INVENTORY_PATH)).unwrap()).unwrap();
    assert_eq!(changed["files"][0]["path"], "outputs/checkpoint.pt");
    assert_eq!(changed["files"][0]["size"], 10);
    assert!(
      std::fs::read(root.path().join(INVENTORY_PATH))
        .unwrap()
        .len()
        < INVENTORY_LIMIT
    );
  }

  #[test]
  fn escaped_names_stay_bounded_and_corrupt_inventory_is_repaired() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("outputs")).unwrap();
    for index in 0..210 {
      std::fs::write(
        root
          .path()
          .join("outputs")
          .join(format!("{}-{index}.pt", "q\"".repeat(90))),
        [],
      )
      .unwrap();
    }
    std::fs::write(root.path().join(INVENTORY_PATH), "interrupted record").unwrap();
    record(&std::fs::canonicalize(root.path()).unwrap()).unwrap();
    let bytes = std::fs::read(root.path().join(INVENTORY_PATH)).unwrap();
    assert!(bytes.len() <= INVENTORY_LIMIT);
    let manifest: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(manifest["truncated"], true);
    record(&std::fs::canonicalize(root.path()).unwrap()).unwrap();
    assert_eq!(
      std::fs::read(root.path().join(INVENTORY_PATH)).unwrap(),
      bytes
    );
  }
}
