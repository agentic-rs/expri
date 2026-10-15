use serde_json::{Value, json};

/// Keep the metadata shape identical for filesystem and hosted dashboards.
pub fn run_metadata(record: &Value, truncated: &mut bool) -> Value {
  let mut state = projection(
    &record["state"],
    &[
      "schema_version",
      "run_id",
      "task",
      "command",
      "status",
      "started_at",
      "finished_at",
      "exit_code",
      "task_exit_code",
      "detached",
      "logging_error",
      "machine_id",
      "system_capture_id",
      "system_capture_status",
      "system_capture_error",
    ],
    truncated,
  );
  if state.is_object()
    && let Some(supervisor) = record["state"].get("supervisor")
    && let Some(supervisor) = supervisor_metadata(supervisor, truncated)
  {
    state["supervisor"] = supervisor;
  }
  if state.is_object()
    && let Some(assets) = record["state"]["assets"].as_array()
  {
    state["asset_count"] = json!(assets.len());
    if assets.len() > 16 {
      *truncated = true;
    }
    let assets: Vec<_> = assets
      .iter()
      .take(16)
      .map(|asset| {
        let mut output = projection(asset, &["path", "size", "sha256"], truncated);
        if output.is_object() {
          output["source"] = projection(
            &asset["source"],
            &[
              "kind",
              "input_id",
              "url",
              "repo_id",
              "repo_type",
              "filename",
              "revision",
            ],
            truncated,
          );
          output["availability"] = projection(&asset["availability"], &["input_id"], truncated);
          output["producer"] =
            projection(&asset["producer"], &["origin", "run_id", "path"], truncated);
        }
        output
      })
      .collect();
    state["assets"] = preview(&json!(assets), truncated);
  }
  let mut snapshot = projection(
    &record["snapshot"],
    &["schema_version", "run_id", "created_at"],
    truncated,
  );
  if snapshot.is_object() {
    snapshot["source"] = projection(
      &record["snapshot"]["source"],
      &[
        "kind",
        "git_head",
        "checkout_manifest_sha256",
        "sync_state_sha256",
      ],
      truncated,
    );
    snapshot["file_count"] = json!(record["snapshot"]["files"].as_array().map(Vec::len));
  }
  let mut environment = projection(
    &record["environment"],
    &[
      "schema_version",
      "base_python",
      "python",
      "environment_path",
      "reuse_packages",
      "reuse_extras",
      "fingerprint",
      "lock_sha256",
      "pyproject_sha256",
      "install_project",
      "cache",
    ],
    truncated,
  );
  if environment.is_object() {
    for field in ["base_manifest", "combined_manifest"] {
      environment[field] = projection(
        &record["environment"][field],
        &[
          "python",
          "python_prefix",
          "base_prefix",
          "marker_env",
          "torch",
          "gpu_driver",
        ],
        truncated,
      );
      if environment[field].is_object() {
        environment[field]["package_count"] = json!(
          record["environment"][field]["packages"]
            .as_object()
            .map(serde_json::Map::len)
        );
      }
    }
  }
  json!({"state": state, "snapshot": snapshot, "environment": environment})
}

/// Supervisor state also contains private handoff details. Project its public history
/// explicitly, independently of the generic preview's object and depth limits.
fn supervisor_metadata(value: &Value, truncated: &mut bool) -> Option<Value> {
  const EVENT_LIMIT: usize = 16;
  const BYTE_LIMIT: usize = 16 * 1024;
  let Some(generation) = value["generation"].as_u64().filter(|value| *value > 0) else {
    *truncated = true;
    return None;
  };
  let Some(events) = value["events"]
    .as_array()
    .filter(|_| value["protocol_version"] == 1 && value["mode"].as_str() == Some("live"))
  else {
    *truncated = true;
    return None;
  };
  let mut omitted = events.len() > EVENT_LIMIT;
  // Reserve ample space for the fixed envelope, including total_count.
  let mut remaining = BYTE_LIMIT - 512;
  let mut output = Vec::new();
  for event in events.iter().rev().take(EVENT_LIMIT) {
    let Some(event) = supervisor_event(event, &mut omitted) else {
      omitted = true;
      continue;
    };
    let size = serde_json::to_vec(&event).unwrap().len() + 1;
    if size > remaining {
      omitted = true;
      break;
    }
    remaining -= size;
    output.push(event);
  }
  output.reverse();
  *truncated |= omitted;
  Some(json!({
    "protocol_version": 1,
    "generation": generation,
    "mode": "live",
    "events": output,
    "total_count": events.len(),
    "truncated": omitted,
  }))
}

fn supervisor_event(value: &Value, truncated: &mut bool) -> Option<Value> {
  let generation = value["generation"].as_u64().filter(|value| *value > 0)?;
  let status = value["status"].as_str()?;
  if value["mode"].as_str() != Some("live")
    || !["restarting", "completed", "failed"].contains(&status)
  {
    return None;
  }
  let timestamp = |field: &str| -> Option<Value> {
    match value.get(field) {
      Some(Value::String(text))
        if text.len() <= 64 && chrono::DateTime::parse_from_rfc3339(text).is_ok() =>
      {
        Some(json!(text))
      }
      None | Some(Value::Null) if field != "requested_at" => Some(Value::Null),
      _ => None,
    }
  };
  let request_id = supervisor_text(value.get("request_id")?, 96, truncated)?;
  let reason = supervisor_text(value.get("reason")?, 512, truncated)?;
  if request_id.as_str()?.is_empty() || reason.as_str()?.is_empty() {
    return None;
  }
  let error = match value.get("error") {
    None | Some(Value::Null) => Value::Null,
    Some(error) => supervisor_text(error, 512, truncated)?,
  };
  Some(json!({
    "generation": generation,
    "request_id": request_id,
    "reason": reason,
    "mode": "live",
    "status": status,
    "requested_at": timestamp("requested_at")?,
    "completed_at": timestamp("completed_at")?,
    "failed_at": timestamp("failed_at")?,
    "worker_restarted": value["worker_restarted"].as_bool(),
    "error": error,
  }))
}

fn supervisor_text(value: &Value, limit: usize, truncated: &mut bool) -> Option<Value> {
  let text = value.as_str()?;
  let filtered: String = text
    .chars()
    .filter(|value| !value.is_control())
    .take(limit)
    .collect();
  if filtered != text {
    *truncated = true;
  }
  Some(json!(filtered))
}

pub fn projection(value: &Value, fields: &[&str], truncated: &mut bool) -> Value {
  if !value.is_object() {
    return Value::Null;
  }
  let fields: serde_json::Map<_, _> = fields
    .iter()
    .filter_map(|field| {
      value
        .get(*field)
        .map(|value| ((*field).to_string(), value.clone()))
    })
    .collect();
  preview(&Value::Object(fields), truncated)
}

pub fn bounded_warnings(warnings: &[Value]) -> Vec<Value> {
  let mut output: Vec<_> = warnings
    .iter()
    .take(100)
    .map(|warning| preview(warning, &mut false))
    .collect();
  if warnings.len() > 100 {
    output.push(
      json!({"message": "Additional warnings omitted; inspect records with expri runs list/show."}),
    );
  }
  output
}

/// Retain diagnostics for one visible run independently of the list-wide limit.
pub fn run_warnings(warnings: &[Value], run_id: &str) -> Vec<Value> {
  let mut unique = Vec::new();
  for warning in warnings {
    if warning["run_id"].as_str() == Some(run_id) && !unique.contains(warning) {
      unique.push(warning.clone());
    }
  }
  let mut output = bounded_warnings(&unique);
  // The truncation notice belongs to this run too.
  for warning in &mut output {
    warning["run_id"] = json!(run_id);
  }
  output
}

/// Small previews keep browser responses bounded even for unusually large params.
pub fn preview(value: &Value, truncated: &mut bool) -> Value {
  preview_inner(value, 0, &mut (16 * 1024), truncated).unwrap_or(Value::Null)
}

fn preview_inner(
  value: &Value,
  depth: usize,
  budget: &mut usize,
  truncated: &mut bool,
) -> Option<Value> {
  if *budget < 8 || depth > 5 {
    *truncated = true;
    return None;
  }
  match value {
    Value::Object(values) => {
      *budget -= 2; // JSON braces; key charges below include separators.
      let mut output = serde_json::Map::new();
      if values.len() > 24 {
        *truncated = true;
      }
      for (name, value) in values.iter().take(24) {
        let key_size = serde_json::to_string(name).unwrap().len() + 3;
        if key_size > *budget {
          *truncated = true;
          break;
        }
        *budget -= key_size;
        if let Some(value) = preview_inner(value, depth + 1, budget, truncated) {
          output.insert(name.clone(), value);
        }
      }
      Some(Value::Object(output))
    }
    Value::Array(values) => {
      *budget -= 2;
      if values.len() > 16 {
        *truncated = true;
      }
      let mut output = Vec::new();
      for value in values.iter().take(16) {
        if *budget < 8 {
          *truncated = true;
          break;
        }
        *budget -= 1; // Account for a comma before each retained element.
        if let Some(value) = preview_inner(value, depth + 1, budget, truncated) {
          output.push(value);
        }
      }
      Some(Value::Array(output))
    }
    _ => {
      let value = if let Value::String(text) = value {
        let mut characters = text.chars();
        let mut text: String = characters.by_ref().take(512).collect();
        if characters.next().is_some() {
          *truncated = true;
          text.push('…');
        }
        Value::String(text)
      } else {
        value.clone()
      };
      let size = serde_json::to_vec(&value).unwrap().len() + 1;
      if size > *budget {
        *truncated = true;
        return None;
      }
      *budget -= size;
      Some(value)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn supervisor_event_fixture(generation: u64) -> Value {
    json!({
      "generation": generation,
      "request_id": format!("request-{generation}"),
      "reason": "Upgrade supervisor",
      "mode": "live",
      "status": "completed",
      "requested_at": "2026-10-15T01:00:00Z",
      "completed_at": "2026-10-15T01:00:01Z",
      "failed_at": null,
      "worker_restarted": false,
      "error": null,
    })
  }

  #[test]
  fn supervisor_preview_projects_only_public_history_and_preserves_raw_records() {
    let mut event = supervisor_event_fixture(2);
    event["target_digest"] = json!("private-target");
    event["handoff_fd"] = json!(12);
    let record = json!({"state": {"supervisor": {
      "protocol_version": 1, "generation": 2, "mode": "live", "events": [event],
      "control_socket": "/private/control.sock", "task_pid": 123,
      "target_path": "/private/expri", "process_identity": {"secret": "private"},
    }}});
    let original = record.clone();
    let mut truncated = false;
    let preview = run_metadata(&record, &mut truncated);
    assert_eq!(
      preview["state"]["supervisor"],
      json!({
        "protocol_version": 1, "generation": 2, "mode": "live",
        "events": [supervisor_event_fixture(2)], "total_count": 1, "truncated": false,
      })
    );
    assert!(!preview.to_string().contains("private"));
    assert!(!preview.to_string().contains("handoff_fd"));
    assert!(!preview.to_string().contains("task_pid"));
    assert_eq!(record, original);
    assert!(!truncated);
  }

  #[test]
  fn supervisor_preview_keeps_latest_events_without_generic_depth_truncation() {
    let events: Vec<_> = (2..=21).map(supervisor_event_fixture).collect();
    let record = json!({"state": {"supervisor": {
      "protocol_version": 1, "generation": 21, "mode": "live", "events": events,
    }}});
    let mut truncated = false;
    let preview = run_metadata(&record, &mut truncated);
    let supervisor = &preview["state"]["supervisor"];
    assert_eq!(supervisor["total_count"], 20);
    assert_eq!(supervisor["events"].as_array().unwrap().len(), 16);
    assert_eq!(supervisor["events"][0]["generation"], 6);
    assert_eq!(supervisor["events"][15]["generation"], 21);
    assert_eq!(supervisor["events"][15]["status"], "completed");
    assert_eq!(supervisor["truncated"], true);
    assert!(truncated);
  }

  #[test]
  fn supervisor_preview_bounds_text_bytes_and_does_not_invent_worker_restart_evidence() {
    let mut event = supervisor_event_fixture(2);
    event["reason"] = json!(format!("{}\n\0", "😀".repeat(600)));
    event["error"] = json!("😀".repeat(600));
    event["worker_restarted"] = json!({"private": true});
    let record = json!({"state": {"supervisor": {
      "protocol_version": 1, "generation": 2, "mode": "live", "events": vec![event; 16],
    }}});
    let mut truncated = false;
    let preview = run_metadata(&record, &mut truncated);
    let supervisor = &preview["state"]["supervisor"];
    assert!(serde_json::to_vec(supervisor).unwrap().len() <= 16 * 1024);
    let events = supervisor["events"].as_array().unwrap();
    assert!(!events.is_empty());
    assert!(events.len() < 16);
    assert_eq!(events[0]["reason"].as_str().unwrap().chars().count(), 512);
    assert!(
      !events[0]["reason"]
        .as_str()
        .unwrap()
        .chars()
        .any(char::is_control)
    );
    assert!(events[0]["worker_restarted"].is_null());
    assert_eq!(supervisor["total_count"], 16);
    assert_eq!(supervisor["truncated"], true);
    assert!(truncated);
  }

  #[test]
  fn supervisor_preview_rejects_invalid_protocols_and_events_without_affecting_legacy_records() {
    let mut truncated = false;
    let preview = run_metadata(&json!({"state": {"status": "running"}}), &mut truncated);
    assert!(preview["state"].get("supervisor").is_none());
    assert!(!truncated);
    for invalid in [
      Value::Null,
      json!([]),
      json!({}),
      json!({
        "protocol_version": 2, "generation": 1, "mode": "live", "events": [],
      }),
      json!({"protocol_version": 1, "generation": -1, "mode": "live", "events": []}),
    ] {
      let mut truncated = false;
      let preview = run_metadata(&json!({"state": {"supervisor": invalid}}), &mut truncated);
      assert!(preview["state"].get("supervisor").is_none());
      assert!(truncated);
    }
    let mut event = supervisor_event_fixture(2);
    event["requested_at"] = json!("invalid");
    let mut restarted = supervisor_event_fixture(3);
    restarted["worker_restarted"] = json!(true);
    let record = json!({"state": {"supervisor": {
      "protocol_version": 1, "generation": 3, "mode": "live", "events": [event, restarted],
    }}});
    let mut truncated = false;
    let preview = run_metadata(&record, &mut truncated);
    assert_eq!(
      preview["state"]["supervisor"]["events"]
        .as_array()
        .unwrap()
        .len(),
      1
    );
    assert_eq!(
      preview["state"]["supervisor"]["events"][0]["worker_restarted"],
      true
    );
    assert_eq!(preview["state"]["supervisor"]["total_count"], 2);
    assert!(truncated);
  }

  #[test]
  fn asset_preview_preserves_short_lineage_and_omits_private_connection_settings() {
    let record = json!({"state": {"assets": [{
      "path": "models/base.pt", "size": 10, "sha256": "a".repeat(64),
      "source": {"kind": "expri", "input_id": "base-v1", "endpoint": "https://service.example.net", "project_id": "example-project", "token": "secret"},
      "availability": {"input_id": "base-v1", "token": "secret"},
      "producer": {"origin": "worker-1", "run_id": "run-1", "path": "outputs/best.pt", "project_id": "example-project"},
      "extra": "omitted"
    }]}});
    let mut truncated = false;
    let preview = run_metadata(&record, &mut truncated);
    assert_eq!(preview["state"]["asset_count"], 1);
    assert_eq!(
      preview["state"]["assets"][0]["source"],
      json!({"kind": "expri", "input_id": "base-v1"})
    );
    assert_eq!(
      preview["state"]["assets"][0]["producer"],
      json!({"origin": "worker-1", "run_id": "run-1", "path": "outputs/best.pt"})
    );
    assert_eq!(
      preview["state"]["assets"][0]["availability"],
      json!({"input_id": "base-v1"})
    );
    assert!(!preview.to_string().contains("secret"));
    assert!(!truncated);
  }

  #[test]
  fn asset_preview_is_bounded_even_when_run_metadata_has_many_assets() {
    let asset = json!({"path": "data/audio.tar.zst", "size": 10, "source": {"kind": "local"}});
    let record = json!({"state": {"assets": vec![asset; 100]}});
    let mut truncated = false;
    let preview = run_metadata(&record, &mut truncated);
    assert_eq!(preview["state"]["asset_count"], 100);
    assert_eq!(preview["state"]["assets"].as_array().unwrap().len(), 16);
    assert!(truncated);
  }
}
