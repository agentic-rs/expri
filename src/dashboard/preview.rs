use serde_json::{Value, json};

/// Keep the metadata shape identical for filesystem and hosted dashboards.
pub(crate) fn run_metadata(record: &Value, truncated: &mut bool) -> Value {
  let state = projection(
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
    ],
    truncated,
  );
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

pub(crate) fn projection(value: &Value, fields: &[&str], truncated: &mut bool) -> Value {
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

pub(crate) fn bounded_warnings(warnings: &[Value]) -> Vec<Value> {
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

/// Small previews keep browser responses bounded even for unusually large params.
pub(crate) fn preview(value: &Value, truncated: &mut bool) -> Value {
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
