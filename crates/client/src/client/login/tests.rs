use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use expri_core::service_auth::CLI_TOKEN_PREFIX;

use super::*;
use crate::client::download::mock::mock;

const PASSWORD: &str = "must-not-leak password";

fn token() -> String {
  format!("{CLI_TOKEN_PREFIX}{}", "a".repeat(43))
}

fn encoded(token: &str, expires_at_ms: Option<i64>) -> Vec<u8> {
  serde_json::to_vec(&LoginResponse {
    token: token.into(),
    expires_at_ms,
  })
  .unwrap()
}

#[test]
fn login_exchanges_only_password_and_preserves_server_base_path_without_cookies() {
  let (url, server) = mock(2, |request, _| {
    assert_eq!(request.path, "/service/v1/login");
    assert_eq!(request.headers["content-type"], "application/json");
    assert!(!request.headers.contains_key("authorization"));
    assert!(!request.headers.contains_key("cookie"));
    let request: LoginRequest = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(request.password, PASSWORD);
    assert!(request.validate().is_ok());
    (
      200,
      vec![("Set-Cookie".into(), "must-not-leak=secret; Path=/".into())],
      encoded(&token(), None),
    )
  });
  for _ in 0..2 {
    let response = login(&format!("{url}/service/"), PASSWORD).unwrap();
    assert_eq!(response.token, token());
    assert!(response.expires_at_ms.is_none());
    assert!(!format!("{response:?}").contains(&token()));
    assert!(!format!("{response:?}").contains(PASSWORD));
  }
  server.join().unwrap();
}

#[test]
fn transport_policy_requires_https_except_for_loopback_and_never_echoes_url_secrets() {
  for url in [
    "https://example.net/service/",
    "https://127.0.0.1:1234",
    "http://localhost:1234",
    "http://127.9.8.7:1234",
    "http://[::1]:1234/service/",
  ] {
    validate_login_url(url).unwrap();
  }
  for url in [
    "http://example.net/must-not-leak",
    "http://192.168.1.1",
    "http://localhost.example.net",
    "https://must-not-leak@example.net",
    "https://user:must-not-leak@example.net",
    "https://example.net/?password=must-not-leak",
    "https://example.net/#must-not-leak",
    "file:///must-not-leak",
    "must-not-leak",
  ] {
    let error = validate_login_url(url).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("must-not-leak"));
  }
  assert_eq!(
    endpoint("https://example.net/base/", "logout")
      .unwrap()
      .as_str(),
    "https://example.net/base/v1/logout"
  );
}

#[test]
fn https_handshake_failure_never_sends_a_plaintext_password() {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let address = listener.local_addr().unwrap();
  let server = thread::spawn(move || {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
      match listener.accept() {
        Ok((stream, _)) => break stream,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          assert!(
            std::time::Instant::now() < deadline,
            "TLS client did not connect"
          );
          thread::sleep(Duration::from_millis(5));
        }
        Err(error) => panic!("{error}"),
      }
    };
    stream.set_nonblocking(false).unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(3)))
      .unwrap();
    let mut bytes = [0; 4096];
    let length = stream.read(&mut bytes).unwrap();
    assert!(length > 0);
    assert_eq!(bytes[0], 0x16, "client must start a TLS handshake");
    assert!(
      !bytes[..length]
        .windows(PASSWORD.len())
        .any(|part| part == PASSWORD.as_bytes())
    );
    stream
      .write_all(b"HTTP/1.1 400 Test\r\nContent-Length: 0\r\n\r\n")
      .unwrap();
  });
  let error = login(&format!("https://{address}"), PASSWORD).unwrap_err();
  assert!(matches!(
    error,
    ExpriError::ServiceUnavailable {
      reading_response: false
    }
  ));
  assert!(!error.to_string().contains(PASSWORD));
  server.join().unwrap();
}

#[test]
fn redirects_are_rejected_without_contacting_another_host_or_echoing_location() {
  for status in [301, 302, 303, 307, 308] {
    let redirected = TcpListener::bind("127.0.0.1:0").unwrap();
    redirected.set_nonblocking(true).unwrap();
    let location = format!("http://{}/must-not-leak", redirected.local_addr().unwrap());
    let (url, server) = mock(1, move |_, _| {
      (
        status,
        vec![("Location".into(), location.clone())],
        PASSWORD.as_bytes().to_vec(),
      )
    });
    let error = login(&url, PASSWORD).unwrap_err();
    assert!(
      matches!(error, ExpriError::ServiceRejected { status: received, .. } if received == status)
    );
    assert!(!error.to_string().contains("must-not-leak"));
    assert_eq!(
      redirected.accept().unwrap_err().kind(),
      std::io::ErrorKind::WouldBlock
    );
    server.join().unwrap();
  }
}

#[test]
fn wrong_password_and_unsupported_server_errors_preserve_status_without_echoing_bodies() {
  for (status, expected) in [
    (401, "login password was rejected"),
    (
      404,
      "server does not support CLI login; update the server or configure a client token",
    ),
    (
      405,
      "server does not support CLI login; update the server or configure a client token",
    ),
    (429, "too many CLI authentication requests; retry later"),
    (503, "CLI login request was rejected"),
  ] {
    let (url, server) = mock(1, move |_, _| {
      (
        status,
        Vec::new(),
        format!("{{\"error\":\"{PASSWORD} {}\"}}", token()).into_bytes(),
      )
    });
    let error = login(&url, PASSWORD).unwrap_err();
    let ExpriError::ServiceRejected {
      status: received,
      detail,
    } = error
    else {
      panic!("expected typed rejection")
    };
    assert_eq!(received, status);
    assert_eq!(detail, expected);
    assert!(!detail.contains(PASSWORD));
    assert!(!detail.contains(CLI_TOKEN_PREFIX));
    server.join().unwrap();
  }
}

#[test]
fn malformed_shared_or_expired_credentials_are_rejected_without_echoing_secrets() {
  for body in [
    format!("{{\"token\":\"{PASSWORD}\"}}must-not-leak").into_bytes(),
    encoded("must-not-leak-static-token", None),
    encoded(&format!("{CLI_TOKEN_PREFIX}{}", "a".repeat(42)), None),
    encoded(&token(), Some(0)),
    encoded(&token(), Some(1)),
    serde_json::to_vec(
      &serde_json::json!({"token":token(),"expires_at_ms":null,"password":PASSWORD}),
    )
    .unwrap(),
  ] {
    let (url, server) = mock(1, move |_, _| (200, Vec::new(), body.clone()));
    let error = login(&url, PASSWORD).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("must-not-leak"));
    assert!(!error.to_string().contains(&token()));
    server.join().unwrap();
  }
  let password = token();
  let (url, server) = mock(1, |_, _| (200, Vec::new(), encoded(&token(), None)));
  let error = login(&url, &password).unwrap_err();
  assert_eq!(
    error.to_string(),
    "server returned an invalid CLI login response"
  );
  server.join().unwrap();
}

#[test]
fn response_limit_accepts_four_kib_and_rejects_one_extra_byte() {
  for length in [MAX_AUTH_RESPONSE_BYTES, MAX_AUTH_RESPONSE_BYTES + 1] {
    let mut body = encoded(&token(), None);
    body.resize(length, b' ');
    let (url, server) = mock(1, move |_, _| (200, Vec::new(), body.clone()));
    let response = login(&url, PASSWORD);
    if length == MAX_AUTH_RESPONSE_BYTES {
      assert_eq!(response.unwrap().token, token());
    } else {
      assert_eq!(
        response.unwrap_err().to_string(),
        "CLI login response exceeds its size limit"
      );
    }
    server.join().unwrap();
  }
}

#[test]
fn unreadable_successful_response_is_a_typed_transport_error() {
  let (url, server) = mock(1, |_, _| {
    (
      200,
      vec![("Content-Length".into(), "100".into())],
      b"must-not-leak".to_vec(),
    )
  });
  let error = login(&url, PASSWORD).unwrap_err();
  assert!(matches!(
    error,
    ExpriError::ServiceUnavailable {
      reading_response: true
    }
  ));
  assert!(!error.to_string().contains("must-not-leak"));
  server.join().unwrap();
}

#[test]
fn invalid_passwords_fail_before_contacting_the_endpoint() {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  for password in [
    "short".into(),
    "x".repeat(257),
    "must-not-leak\npassword".into(),
  ] {
    let error = login(&url, &password).unwrap_err();
    assert!(!error.to_string().contains("must-not-leak"));
  }
  assert_eq!(
    listener.accept().unwrap_err().kind(),
    std::io::ErrorKind::WouldBlock
  );
}

#[test]
fn revocation_sends_only_bearer_credentials_and_requires_no_content() {
  let expected = token();
  let (url, server) = mock(1, move |request, _| {
    assert_eq!(request.path, "/service/v1/logout");
    assert!(request.body.is_empty());
    assert_eq!(
      request.headers["authorization"],
      format!("Bearer {expected}")
    );
    assert!(!request.headers.contains_key("cookie"));
    (204, Vec::new(), Vec::new())
  });
  revoke_login(&format!("{url}/service/"), &token()).unwrap();
  server.join().unwrap();
  for status in [200, 302, 401, 404, 503] {
    let (url, server) = mock(1, move |_, _| (status, Vec::new(), encoded(&token(), None)));
    let error = revoke_login(&url, &token()).unwrap_err();
    assert!(
      matches!(error, ExpriError::ServiceRejected { status: received, .. } if received == status)
    );
    assert!(!error.to_string().contains(&token()));
    server.join().unwrap();
  }
}

#[test]
fn configured_logout_preserves_local_configuration_and_resolves_stored_credentials() {
  let expected = token();
  let (url, server) = mock(1, move |request, _| {
    assert_eq!(
      request.headers["authorization"],
      format!("Bearer {expected}")
    );
    (204, Vec::new(), Vec::new())
  });
  let directory = tempfile::tempdir().unwrap();
  let config = directory.path().join("config.toml");
  let contents = format!(
    "[server]\nurl='{url}'\ntoken='{}'\n[system_profile]\ninterval_seconds=20\n",
    token()
  );
  std::fs::write(&config, &contents).unwrap();
  logout(&config).unwrap();
  assert_eq!(std::fs::read_to_string(&config).unwrap(), contents);
  assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
  server.join().unwrap();
}
