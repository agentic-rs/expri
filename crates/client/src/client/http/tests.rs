use super::*;
use crate::client::download::mock::mock;

pub(in crate::client) fn api(url: &str) -> Api {
  init_tls();
  let client = Client::builder()
    .no_proxy()
    .redirect(reqwest::redirect::Policy::none())
    .timeout(Duration::from_secs(3))
    .build()
    .unwrap();
  Api {
    publication_epoch: 0,
    endpoint: format!("{url}/v1/request"),
    token: "must-not-leak".into(),
    control: client.clone(),
    objects: client,
    async_objects: None,
    stop: None,
    stream_budget: None,
    stream_backlog: Arc::new(AtomicBool::new(false)),
  }
}

#[test]
fn explicit_owner_epoch_capture_is_scope_bound_and_does_not_refresh_an_old_api_clone() {
  let scope = RunScope {
    project_id: "selected-project".into(),
    origin: "selected-worker".into(),
    run_id: "selected-run".into(),
  };
  let selected = scope.clone();
  let (url, server) = mock(4, move |request, _| {
    let operation: Request = serde_json::from_slice(&request.body).unwrap();
    let response = match operation {
      Request::Capabilities => Response::Capabilities {
        features: vec!["run-task-continuation-v1".into()],
      },
      Request::RunPublication { scope } => {
        assert_eq!(scope, selected);
        Response::RunAttempt { epoch: 3 }
      }
      Request::AtRunEpoch { epoch, request } => {
        assert_eq!(epoch, 3);
        let Request::AppendTracking { scope, .. } = *request else {
          panic!("wrapped run mutation");
        };
        assert_eq!(scope, selected);
        Response::Acknowledged { offset: 1 }
      }
      Request::AppendTracking { .. } => {
        return (
          409,
          Vec::new(),
          br#"{"error":"stale publisher refused"}"#.to_vec(),
        );
      }
      other => panic!("unexpected epoch capture request {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let mut explicit = api(&url);
  let old = explicit.clone();
  explicit.capture_run_epoch(&scope).unwrap();
  assert_eq!(explicit.publication_epoch, 3);
  assert_eq!(old.publication_epoch, 0);
  let append = Request::AppendTracking {
    scope,
    path: "outputs/metrics.jsonl".into(),
    offset: 0,
    data_base64: "Cg==".into(),
  };
  explicit.request(&append).unwrap();
  assert!(matches!(
    old.request(&append).unwrap_err(),
    ExpriError::ServiceRejected { status: 409, .. }
  ));
  server.join().unwrap();
}

#[test]
fn concurrent_jobs_with_the_same_environment_name_keep_distinct_bound_tokens() {
  let (url, server) = mock(2, |request, _| {
    let token = request.headers["authorization"].as_str();
    assert!(matches!(token, "Bearer worker-one" | "Bearer worker-two"));
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::Capabilities {
        features: vec![token.into()],
      })
      .unwrap(),
    )
  });
  let bindings = ["worker-one", "worker-two"].map(|token| {
    let config = ClientConfig {
      url: url.clone(),
      project_id: None,
      token: None,
      token_env: Some("SHARED_NAME".into()),
    };
    ResolvedClient::from_config_with_env(config, |name| {
      assert_eq!(name, "SHARED_NAME");
      Ok(token.into())
    })
    .unwrap()
  });
  let tasks = bindings.map(|binding| {
    std::thread::spawn(move || {
      let expected = format!("Bearer {}", binding.token);
      let bound = Api::new_with_resolved(&binding, Some(Arc::new(AtomicBool::new(false)))).unwrap();
      let cloned = bound.clone();
      let Response::Capabilities { features } = cloned.request(&Request::Capabilities).unwrap()
      else {
        panic!("capabilities expected")
      };
      assert_eq!(features, vec![expected]);
    })
  });
  for task in tasks {
    task.join().unwrap();
  }
  server.join().unwrap();
}

#[test]
fn malformed_client_configuration_diagnostics_do_not_echo_embedded_credentials() {
  let config = tempfile::NamedTempFile::new().unwrap();
  let path = config.path().canonicalize().unwrap();
  std::fs::write(&path, "url = \"https://must-not-leak@example.invalid/\n").unwrap();
  let detail = Api::new(&path).err().unwrap().to_string();
  assert_eq!(detail, "service client configuration is invalid");
}

#[test]
fn shared_object_upload_cancels_while_waiting_for_a_response_after_body_delivery() {
  use std::io::{BufRead, BufReader, Read};
  use std::sync::mpsc;
  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  let origin = format!("http://{}", listener.local_addr().unwrap());
  let resolved = ResolvedClient {
    endpoint: format!("{origin}/v1/request"),
    token: "bound-token".into(),
  };
  let stop = Arc::new(AtomicBool::new(false));
  let api = Api::new_with_resolved(&resolved, Some(Arc::clone(&stop))).unwrap();
  let (delivered, body_received) = mpsc::channel();
  let (release, released) = mpsc::channel();
  let server = std::thread::spawn(move || {
    let (stream, _) = listener.accept().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut length = 0;
    loop {
      line.clear();
      reader.read_line(&mut line).unwrap();
      assert!(!line.to_ascii_lowercase().starts_with("authorization:"));
      if let Some((key, value)) = line.split_once(':')
        && key.eq_ignore_ascii_case("content-length")
      {
        length = value.trim().parse::<usize>().unwrap();
      }
      if line == "\r\n" {
        break;
      }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    assert_eq!(body, b"checkpoint bytes");
    delivered.send(()).unwrap();
    released.recv().unwrap();
    // Deliberately never send response headers. Dropping the client's future
    // must cancel this wait rather than waiting for the normal900s timeout.
  });
  let file = tempfile::NamedTempFile::new().unwrap();
  std::fs::write(file.path(), b"checkpoint bytes").unwrap();
  let source = std::fs::File::open(file.path()).unwrap();
  let (finished, result) = mpsc::channel();
  let task = std::thread::spawn(move || {
    finished
      .send(api.put_object(&format!("{origin}/part"), source, 16))
      .unwrap();
  });
  body_received.recv_timeout(Duration::from_secs(3)).unwrap();
  stop.store(true, Ordering::Release);
  let failure = result
    .recv_timeout(Duration::from_secs(1))
    .expect("shared object cancellation was not prompt")
    .unwrap_err();
  assert!(matches!(failure, ExpriError::DownloadCancelled));
  task.join().unwrap();
  release.send(()).unwrap();
  server.join().unwrap();
}

#[test]
fn stored_tokens_in_flat_and_global_configs_use_bearer_auth_and_redact_rejections() {
  for global in [false, true] {
    let (url, server) = mock(1, |request, _| {
      assert_eq!(request.path, "/service/v1/request");
      assert_eq!(request.headers["authorization"], "Bearer must-not-leak");
      (
        401,
        Vec::new(),
        br#"{"error":"must-not-leak rejected"}"#.to_vec(),
      )
    });
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path().canonicalize().unwrap();
    let table = if global { "[server]\n" } else { "" };
    std::fs::write(
      &path,
      format!("{table}url='{url}/service/'\ntoken='must-not-leak'\n"),
    )
    .unwrap();
    let api = Api::new(&path).unwrap();
    let error = api.request(&Request::Capabilities).unwrap_err();
    assert!(matches!(
      error,
      ExpriError::ServiceRejected { status: 401, .. }
    ));
    assert!(!error.to_string().contains("must-not-leak"));
    assert!(error.to_string().contains("[redacted]"));
    server.join().unwrap();
  }
}

#[test]
fn valid_configuration_does_not_contact_the_endpoint_until_request() {
  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let address = listener.local_addr().unwrap();
  let file = tempfile::NamedTempFile::new().unwrap();
  std::fs::write(
    file.path(),
    format!("[server]\nurl='http://{address}'\ntoken='stored-secret'\n"),
  )
  .unwrap();
  let api = Api::new(file.path()).unwrap();
  assert_eq!(api.endpoint, format!("http://{address}/v1/request"));
  assert_eq!(
    listener.accept().unwrap_err().kind(),
    std::io::ErrorKind::WouldBlock
  );
}

#[test]
fn resolved_environment_tokens_use_the_stored_token_character_policy_without_leaking() {
  let config: ClientConfig =
    toml::from_str("url='https://example.net'\ntoken_env='MUST_NOT_LEAK_ENV'\n").unwrap();
  for token in [
    "",
    "must-not-leak space",
    "must-not-leak\n",
    "must-not-leak-🔒",
    "must-not-leak\u{007f}",
  ] {
    let error = resolve_token(config.clone(), |name| {
      assert_eq!(name, "MUST_NOT_LEAK_ENV");
      Ok(token.into())
    })
    .unwrap_err();
    assert_eq!(
      error.to_string(),
      "service token must be nonempty ASCII graphic characters"
    );
    assert!(!format!("{error:?} {error}").contains("must-not-leak"));
    assert!(!error.to_string().contains("MUST_NOT_LEAK_ENV"));
  }
  assert_eq!(
    resolve_token(config.clone(), |_| Ok("x".into())).unwrap(),
    "x"
  );
  let error = resolve_token(config, |_| Err(std::env::VarError::NotPresent)).unwrap_err();
  assert_eq!(
    error.to_string(),
    "service token environment variable is missing"
  );
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

#[test]
fn malformed_successful_json_never_echoes_the_service_bearer_token() {
  let (url, server) = mock(1, |_, _| {
    (200, Vec::new(), br#"{"kind":"must-not-leak"}"#.to_vec())
  });
  let error = api(&url).request(&Request::Capabilities).unwrap_err();
  let detail = error.to_string();
  assert!(detail.contains("invalid service response"));
  assert!(detail.contains("[redacted]"));
  assert!(!detail.contains("must-not-leak"));
  assert!(detail.len() <= 512 + "invalid service response: ".len());
  server.join().unwrap();
}
