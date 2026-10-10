//! Validation for project, origin, run, and private input identifiers.

use crate::error::{ExpriError, Result};

pub fn validate_component(value: &str) -> Result<()> {
  if value.is_empty()
    || value.len() > 96
    || value == "."
    || value == ".."
    || !value
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
  {
    return Err(ExpriError::Message("invalid service identifier".into()));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn identifiers_allow_ascii_components_up_to_the_length_limit() {
    for value in ["vision", "gpu-1", "run_2026.10"] {
      validate_component(value).unwrap();
    }
    validate_component(&"x".repeat(96)).unwrap();
    assert!(validate_component(&"x".repeat(97)).is_err());
  }

  #[test]
  fn identifiers_reject_paths_and_non_ascii_characters() {
    for value in [
      "",
      ".",
      "..",
      "project/run",
      "project\\run",
      "run one",
      "é",
      "run\0",
    ] {
      let error = validate_component(value).unwrap_err();
      assert_eq!(error.to_string(), "invalid service identifier", "{value:?}");
    }
  }
}
