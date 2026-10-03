use serde_json::{Value, json};

pub(super) fn projection(value: &Value, fields: &[&str], truncated: &mut bool) -> Value {
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

pub(super) fn bounded_warnings(warnings: &[Value]) -> Vec<Value> {
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
pub(super) fn preview(value: &Value, truncated: &mut bool) -> Value {
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
