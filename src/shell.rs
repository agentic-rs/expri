pub fn quote(value: impl AsRef<str>) -> String {
  let value = value.as_ref();
  if value.is_empty() {
    return "''".to_string();
  }
  if value
    .bytes()
    .all(|byte| byte.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&byte))
  {
    return value.to_string();
  }
  format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn join(parts: &[String]) -> String {
  parts.iter().map(quote).collect::<Vec<_>>().join(" ")
}
