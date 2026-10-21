//! Run-owned system observations, retaining original values and measurement intervals.
//! These records never carry training steps.

use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};
use crate::identifiers::validate_component;
use crate::system_profile::{InventoryRevision, SystemSample, validate_inventory, validate_sample};

pub const RUN_SYSTEM_CAPABILITY: &str = "run-system-metrics-v1";
pub const RUN_SYSTEM_PATH: &str = "outputs/system-profile.jsonl";
pub const RUN_SYSTEM_SCHEMA_VERSION: u32 = 1;
/// Keep the first actual observation in each wall-clock minute of each capture.
pub const RUN_SYSTEM_SAMPLE_INTERVAL_MS: i64 = 60_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunSystemRecord {
  Capture {
    schema_version: u32,
    capture_id: String,
    machine_id: String,
    start_ms: i64,
  },
  Inventory {
    inventory: InventoryRevision,
  },
  Sample {
    sample: SystemSample,
  },
  End {
    end_ms: i64,
    complete: bool,
    warnings: Vec<String>,
  },
}

impl RunSystemRecord {
  pub fn validate(&self) -> Result<()> {
    match self {
      Self::Capture {
        schema_version,
        capture_id,
        machine_id,
        start_ms,
      } => {
        if *schema_version != RUN_SYSTEM_SCHEMA_VERSION || *start_ms < 0 {
          return Err(ExpriError::Message(
            "invalid run system capture header".into(),
          ));
        }
        validate_component(capture_id)?;
        validate_component(machine_id)?;
      }
      Self::Inventory { inventory } => {
        if inventory.revision == 0 || inventory.revision > i64::MAX as u64 {
          return Err(ExpriError::Message(
            "invalid run system inventory revision".into(),
          ));
        }
        validate_inventory(&inventory.inventory)?;
      }
      Self::Sample { sample } => validate_sample(sample)?,
      Self::End {
        end_ms, warnings, ..
      } => {
        if *end_ms < 0
          || warnings.len() > 32
          || warnings
            .iter()
            .any(|warning| warning.len() > 1024 || warning.chars().any(char::is_control))
        {
          return Err(ExpriError::Message(
            "invalid run system capture completion".into(),
          ));
        }
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn exact_counter_strings_round_trip_without_training_steps() {
    let value = json!({
      "kind": "sample",
      "sample": {
        "sequence": 7, "session_id": "session-1", "observed_at_ms": 10000,
        "interval_ms": 1000, "collection_duration_ms": 12, "inventory_revision": 1,
        "metrics": {"network/eth0/received_bytes": {"kind": "counter_delta", "value": 10.0}},
        "counters": {"network/eth0/received_bytes": "18446744073709551615"}, "warnings": []
      }
    });
    let record: RunSystemRecord = serde_json::from_value(value.clone()).unwrap();
    record.validate().unwrap();
    assert_eq!(serde_json::to_value(record).unwrap(), value);
  }

  #[test]
  fn capture_identity_and_completion_are_bounded() {
    let capture = RunSystemRecord::Capture {
      schema_version: RUN_SYSTEM_SCHEMA_VERSION,
      capture_id: "capture-1".into(),
      machine_id: "machine-1".into(),
      start_ms: 0,
    };
    capture.validate().unwrap();
    for (capture_id, start_ms, schema_version) in [
      ("../capture", 0, 1),
      ("capture-1", -1, 1),
      ("capture-1", 0, 2),
    ] {
      assert!(
        RunSystemRecord::Capture {
          schema_version,
          capture_id: capture_id.into(),
          machine_id: "machine-1".into(),
          start_ms,
        }
        .validate()
        .is_err()
      );
    }
    assert!(
      RunSystemRecord::End {
        end_ms: 10,
        complete: false,
        warnings: vec!["a".repeat(1025)]
      }
      .validate()
      .is_err()
    );
    assert!(
      serde_json::from_value::<RunSystemRecord>(json!({
        "kind": "capture", "schema_version": 1, "capture_id": "capture-1",
        "machine_id": "machine-1", "start_ms": 0, "step": 42
      }))
      .is_err()
    );
  }
}
