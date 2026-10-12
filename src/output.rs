//! Human-readable CLI reports; serialized output is an explicit choice.

use std::collections::BTreeSet;
use std::fmt::Write;

use serde::Serialize;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::error::Result;

pub(crate) fn print_json_line(value: &impl Serialize) -> Result<()> {
  println!("{}", serde_json::to_string(value)?);
  Ok(())
}

pub(crate) fn print_report(value: &impl Serialize, json: bool) -> Result<()> {
  if json {
    println!("{}", serde_json::to_string_pretty(value)?);
  } else {
    print!("{}", render_report(&serde_json::to_value(value)?));
  }
  Ok(())
}

/// Escape terminal controls while preserving printable identifiers and filenames.
pub(crate) fn text(value: &str) -> String {
  value.chars().fold(String::new(), |mut result, character| {
    if character.is_control()
      || matches!(character, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    {
      result.extend(character.escape_default());
    } else {
      result.push(character);
    }
    result
  })
}

/// Wrap long cells rather than dropping information; widths use terminal columns.
pub(crate) fn table(headers: &[&str], rows: &[Vec<String>], limits: &[usize]) -> String {
  assert_eq!(headers.len(), limits.len());
  assert!(rows.iter().all(|row| row.len() == headers.len()));
  let headers: Vec<String> = headers.iter().map(|header| text(header)).collect();
  let escaped: Vec<Vec<String>> = rows
    .iter()
    .map(|row| row.iter().map(|cell| text(cell)).collect())
    .collect();
  let widths: Vec<usize> = headers
    .iter()
    .enumerate()
    .map(|(index, header)| {
      escaped
        .iter()
        .map(|row| row[index].width())
        .max()
        .unwrap_or(0)
        .min(limits[index])
        .max(header.width())
    })
    .collect();
  let mut result = String::new();
  line(&mut result, &headers, &widths);
  line(
    &mut result,
    &widths
      .iter()
      .map(|width| "-".repeat(*width))
      .collect::<Vec<_>>(),
    &widths,
  );
  for row in escaped {
    let cells: Vec<Vec<String>> = row
      .iter()
      .zip(&widths)
      .map(|(cell, width)| wrap(cell, *width))
      .collect();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    for index in 0..height {
      line(
        &mut result,
        &cells
          .iter()
          .map(|cell| cell.get(index).cloned().unwrap_or_default())
          .collect::<Vec<_>>(),
        &widths,
      );
    }
  }
  result
}

fn line(result: &mut String, cells: &[String], widths: &[usize]) {
  for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
    if index > 0 {
      result.push_str("  ");
    }
    result.push_str(cell);
    if index + 1 < cells.len() {
      result.extend(std::iter::repeat_n(' ', width.saturating_sub(cell.width())));
    }
  }
  result.push('\n');
}

fn wrap(value: &str, width: usize) -> Vec<String> {
  let mut lines = Vec::new();
  let mut current = String::new();
  for character in value.chars() {
    let mut next = current.clone();
    next.push(character);
    if next.width() > width && !current.is_empty() {
      lines.push(std::mem::take(&mut current));
    }
    current.push(character);
  }
  lines.push(current);
  lines
}

pub(crate) fn render_report(value: &Value) -> String {
  let mut result = String::new();
  render(value, &mut result);
  result
}

fn render(value: &Value, result: &mut String) {
  match value {
    Value::Object(_) => {
      let mut fields = Vec::new();
      let mut sections = Vec::new();
      flatten(value, "", &mut fields, &mut sections);
      if !fields.is_empty() {
        result.push_str(&table(
          &["FIELD", "VALUE"],
          &fields
            .into_iter()
            .map(|(key, value)| vec![label(&key), value])
            .collect::<Vec<_>>(),
          &[32, 80],
        ));
      }
      for (name, value) in sections {
        if !result.is_empty() {
          result.push('\n');
        }
        let _ = writeln!(result, "{}", text(&label(&name)));
        render(value, result);
      }
    }
    Value::Array(items) if items.is_empty() => result.push_str("No entries.\n"),
    Value::Array(items) if items.iter().all(Value::is_object) => {
      let mut records = Vec::new();
      let mut names = BTreeSet::new();
      let mut nested = false;
      for item in items {
        let mut fields = Vec::new();
        let mut sections = Vec::new();
        flatten(item, "", &mut fields, &mut sections);
        nested |= !sections.is_empty();
        names.extend(fields.iter().map(|(name, _)| name.clone()));
        records.push(fields);
      }
      if names.len() <= 8 && !nested && !names.is_empty() {
        let mut names: Vec<_> = names.into_iter().collect();
        const FIRST: &[&str] = &[
          "project_id",
          "origin",
          "run_id",
          "path",
          "name",
          "id",
          "status",
        ];
        names.sort_by_key(|name| {
          (
            FIRST
              .iter()
              .position(|first| name == first)
              .unwrap_or(FIRST.len()),
            name.clone(),
          )
        });
        let headers: Vec<_> = names
          .iter()
          .map(|name| label(name).to_uppercase())
          .collect();
        let rows: Vec<_> = records
          .iter()
          .map(|fields| {
            names
              .iter()
              .map(|name| {
                fields
                  .iter()
                  .find(|(key, _)| key == name)
                  .map(|(_, value)| value.clone())
                  .unwrap_or_else(|| "-".into())
              })
              .collect()
          })
          .collect();
        result.push_str(&table(
          &headers.iter().map(String::as_str).collect::<Vec<_>>(),
          &rows,
          &vec![36; headers.len()],
        ));
      } else {
        for (index, item) in items.iter().enumerate() {
          if index > 0 {
            result.push('\n');
          }
          let _ = writeln!(result, "Entry {}", index + 1);
          render(item, result);
        }
      }
    }
    Value::Array(items) => {
      let rows = items
        .iter()
        .map(|value| vec![scalar(value)])
        .collect::<Vec<_>>();
      result.push_str(&table(&["VALUE"], &rows, &[100]));
    }
    _ => result.push_str(&table(&["VALUE"], &[vec![scalar(value)]], &[100])),
  }
}

fn flatten<'a>(
  value: &'a Value,
  prefix: &str,
  fields: &mut Vec<(String, String)>,
  sections: &mut Vec<(String, &'a Value)>,
) {
  if let Value::Object(object) = value {
    for (name, value) in object {
      let key = if prefix.is_empty() {
        name.clone()
      } else {
        format!("{prefix}.{name}")
      };
      match value {
        Value::Object(_) => flatten(value, &key, fields, sections),
        Value::Array(items) if items.iter().any(|item| item.is_object() || item.is_array()) => {
          sections.push((key, value))
        }
        Value::Array(items) if !items.is_empty() => {
          fields.push((key, items.iter().map(scalar).collect::<Vec<_>>().join(", ")))
        }
        Value::Array(_) if name == "warnings" => {}
        Value::Array(_) => sections.push((key, value)),
        _ => fields.push((key, scalar(value))),
      }
    }
  }
}

fn scalar(value: &Value) -> String {
  match value {
    Value::Null => "-".into(),
    Value::Bool(value) => if *value { "yes" } else { "no" }.into(),
    Value::String(value) => value.clone(),
    _ => value.to_string(),
  }
}

fn label(value: &str) -> String {
  let mut label = value.replace('_', " ").replace('.', " / ");
  if let Some(first) = label.chars().next() {
    label.replace_range(..first.len_utf8(), &first.to_uppercase().to_string());
  }
  label
}

pub(crate) fn bytes(value: u64) -> String {
  if value < 1024 {
    return format!("{value} B");
  }
  let mut size = value as f64;
  let mut unit = "B";
  for next in ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"] {
    size /= 1024.0;
    unit = next;
    if size < 1024.0 {
      break;
    }
  }
  format!("{size:.1} {unit}")
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn tables_align_unicode_columns_escape_controls_and_retain_long_values() {
    let output = table(
      &["NAME", "STATE"],
      &[
        vec!["训练".into(), "ready".into()],
        vec!["x".into(), "ready".into()],
      ],
      &[8, 8],
    );
    let starts: Vec<_> = output
      .lines()
      .skip(2)
      .map(|line| line.split("ready").next().unwrap().width())
      .collect();
    assert_eq!(starts, [6, 6]);
    let output = table(&["PATH"], &[vec!["very-long-路径-name".into()]], &[6]);
    assert_eq!(
      output.lines().skip(2).collect::<String>(),
      "very-long-路径-name"
    );
    assert_eq!(
      text("x\n\t\u{1b}[31m\u{202e}y"),
      "x\\n\\t\\u{1b}[31m\\u{202e}y"
    );
    let output = table(&["X\u{1b}[31m\u{202e}"], &[vec!["safe".into()]], &[20]);
    assert!(!output.contains('\u{1b}'));
    assert!(!output.contains('\u{202e}'));
    assert!(output.contains("\\u{1b}"));
  }

  #[test]
  fn reports_present_nested_records_without_losing_fields_or_nulls() {
    let value = json!({"server":{"url":"https://example.net","token_configured":false},"items":[{"path":"x","size":4},{"path":"y","status":"pending"}],"warnings":[],"files":[]});
    let output = render_report(&value);
    for text in [
      "FIELD",
      "Server / url",
      "https://example.net",
      "no",
      "PATH",
      "SIZE",
      "STATUS",
      "pending",
      "No entries.",
    ] {
      assert!(output.contains(text), "missing {text}: {output}");
    }
    assert!(!output.starts_with('{'));
  }

  #[test]
  fn byte_sizes_cover_zero_and_large_values() {
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(1536), "1.5 KiB");
    assert_eq!(bytes(u64::MAX), "16.0 EiB");
  }
}
