//! Bounded password exchange and opaque, individually revocable CLI credentials.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};

pub const CLI_TOKEN_PREFIX: &str = "expri_cli_";
pub const MAX_AUTH_REQUEST_BYTES: usize = 4096;
pub const MAX_AUTH_RESPONSE_BYTES: usize = 4096;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
  pub password: String,
}

impl fmt::Debug for LoginRequest {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("LoginRequest")
      .field("password", &"[redacted]")
      .finish()
  }
}

impl LoginRequest {
  pub fn validate(&self) -> Result<()> {
    if !(16..=256).contains(&self.password.len())
      || !self
        .password
        .bytes()
        .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
      return Err(invalid(
        "login password must contain 16 to 256 printable ASCII characters",
      ));
    }
    Ok(())
  }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginResponse {
  pub token: String,
  #[serde(default)]
  pub expires_at_ms: Option<i64>,
}

impl fmt::Debug for LoginResponse {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("LoginResponse")
      .field("token", &"[redacted]")
      .field("expires_at_ms", &self.expires_at_ms)
      .finish()
  }
}

impl LoginResponse {
  pub fn validate(&self, now_ms: i64) -> Result<()> {
    validate_cli_token(&self.token)?;
    if self
      .expires_at_ms
      .is_some_and(|expiry| expiry <= 0 || expiry <= now_ms)
    {
      return Err(invalid("server returned an invalid CLI token expiration"));
    }
    Ok(())
  }
}

pub fn validate_cli_token(token: &str) -> Result<()> {
  let suffix = token
    .strip_prefix(CLI_TOKEN_PREFIX)
    .ok_or_else(|| invalid("server returned an invalid issued CLI token"))?;
  if suffix.len() != 43
    || !suffix
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
  {
    return Err(invalid("server returned an invalid issued CLI token"));
  }
  Ok(())
}

fn invalid(message: &str) -> ExpriError {
  ExpriError::Message(message.into())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn password_policy_is_bounded_and_allows_spaces_without_echoing_values() {
    for password in [
      "printable password 123".into(),
      "x".repeat(16),
      "x".repeat(256),
    ] {
      LoginRequest { password }.validate().unwrap();
    }
    for password in [
      "short-secret".into(),
      "x".repeat(257),
      "must-not-leak\nsecret".into(),
      "must-not-leak-🔒secret".into(),
    ] {
      let request = LoginRequest { password };
      let error = request.validate().unwrap_err();
      assert!(!format!("{request:?} {error:?} {error}").contains("must-not-leak"));
      assert!(!format!("{request:?}").contains("short-secret"));
    }
  }

  #[test]
  fn issued_tokens_require_exact_prefix_length_and_base64url_alphabet() {
    for suffix in ["a".repeat(43), format!("{}_-A0", "b".repeat(39))] {
      validate_cli_token(&format!("{CLI_TOKEN_PREFIX}{suffix}")).unwrap();
    }
    for token in [
      "must-not-leak-static-token".into(),
      format!("{CLI_TOKEN_PREFIX}{}", "a".repeat(42)),
      format!("{CLI_TOKEN_PREFIX}{}", "a".repeat(44)),
      format!("{CLI_TOKEN_PREFIX}{}=", "a".repeat(42)),
      format!("{CLI_TOKEN_PREFIX}{}/", "a".repeat(42)),
      format!("{CLI_TOKEN_PREFIX}{}+", "a".repeat(42)),
      format!("{CLI_TOKEN_PREFIX}{}\n", "a".repeat(42)),
      format!("{CLI_TOKEN_PREFIX}{}é", "a".repeat(41)),
    ] {
      let error = validate_cli_token(&token).unwrap_err();
      assert!(!error.to_string().contains(&token));
    }
  }

  #[test]
  fn response_allows_until_revoke_and_only_future_positive_optional_expiration() {
    let token = format!("{CLI_TOKEN_PREFIX}{}", "a".repeat(43));
    for expires_at_ms in [None, Some(1001)] {
      LoginResponse {
        token: token.clone(),
        expires_at_ms,
      }
      .validate(1000)
      .unwrap();
    }
    for expires_at_ms in [Some(-1), Some(0), Some(999), Some(1000)] {
      assert!(
        LoginResponse {
          token: token.clone(),
          expires_at_ms
        }
        .validate(1000)
        .is_err()
      );
    }
    let response = LoginResponse {
      token,
      expires_at_ms: None,
    };
    assert!(!format!("{response:?}").contains(CLI_TOKEN_PREFIX));
    let encoded = serde_json::to_string(&response).unwrap();
    let restored: LoginResponse = serde_json::from_str(&encoded).unwrap();
    restored.validate(1000).unwrap();
  }

  #[test]
  fn authentication_wire_types_reject_unknown_fields() {
    assert!(
      serde_json::from_str::<LoginRequest>(
        r#"{"password":"printable password 123","token":"must-not-leak"}"#
      )
      .is_err()
    );
    let token = format!("{CLI_TOKEN_PREFIX}{}", "a".repeat(43));
    assert!(
      serde_json::from_value::<LoginResponse>(
        serde_json::json!({"token":token,"expires_at_ms":null,"password":"must-not-leak"})
      )
      .is_err()
    );
  }
}
