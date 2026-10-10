use super::*;
use crate::service::client::download::mock::mock;

fn api(url: &str) -> Api {
  crate::service::storage::init_tls();
  let client = Client::builder()
    .no_proxy()
    .timeout(Duration::from_secs(3))
    .build()
    .unwrap();
  Api {
    endpoint: format!("{url}/v1/request"),
    token: "must-not-leak".into(),
    control: client.clone(),
    objects: client,
  }
}

#[test]
fn rejected_status_survives_an_unreadable_response_body() {
  for status in [401, 403, 410, 503] {
    let (url, server) = mock(1, move |_, _| {
      (
        status,
        vec![("Content-Length".into(), "100".into())],
        b"must-not-leak".to_vec(),
      )
    });
    let error = api(&url).request(&Request::Capabilities).unwrap_err();
    assert!(
      matches!(&error, ExpriError::ServiceRejected { status: returned, .. } if *returned == status)
    );
    assert!(!error.to_string().contains("must-not-leak"));
    server.join().unwrap();
  }
}

#[test]
fn rejected_status_survives_an_oversized_response_body() {
  let (url, server) = mock(1, |_, _| (401, Vec::new(), vec![b'x'; MAX_REQUEST + 1]));
  let error = api(&url).request(&Request::Capabilities).unwrap_err();
  assert!(matches!(
    error,
    ExpriError::ServiceRejected { status: 401, .. }
  ));
  server.join().unwrap();
}

#[test]
fn unreadable_successful_body_is_a_typed_transport_failure() {
  let (url, server) = mock(1, |_, _| {
    (
      200,
      vec![("Content-Length".into(), "100".into())],
      Vec::new(),
    )
  });
  let error = api(&url).request(&Request::Capabilities).unwrap_err();
  assert!(matches!(
    error,
    ExpriError::ServiceUnavailable {
      reading_response: true
    }
  ));
  server.join().unwrap();
}
