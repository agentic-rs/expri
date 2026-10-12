use std::thread;

use expri_core::service_catalog::{
  MAX_CATALOG_PAGE_BYTES, ReceivedRunFile, ReportedRunFile, RunArchiveFilter, RunFileAvailability,
  RunFileEntry, RunListEntry, encode_run_cursor,
};

use super::*;
use crate::client::download::mock::mock;
use crate::client::http::tests::api;
use crate::error::ExpriError;
use crate::types::{FileStorage, RunScope};

fn encoded(response: &Response) -> Vec<u8> {
  serde_json::to_vec(response).unwrap()
}

fn capabilities() -> Vec<u8> {
  encoded(&Response::Capabilities {
    features: vec![SERVICE_CATALOG_CAPABILITY.into()],
  })
}

fn client_with_page(response: Response) -> (ServiceCatalogClient, thread::JoinHandle<()>) {
  let mut count = 0;
  let (url, server) = mock(2, move |_, _| {
    count += 1;
    let body = if count == 1 {
      capabilities()
    } else {
      encoded(&response)
    };
    (200, Vec::new(), body)
  });
  (ServiceCatalogClient::from_api(api(&url)).unwrap(), server)
}

fn scope(id: &str) -> RunScope {
  RunScope {
    project_id: "vision".into(),
    origin: "gpu-1".into(),
    run_id: id.into(),
  }
}

fn run_query() -> RunListQuery {
  RunListQuery {
    project_id: Some("vision".into()),
    origin: Some("gpu-1".into()),
    archival: RunArchiveFilter::Active,
    limit: 20,
    cursor: None,
  }
}

fn run(id: &str) -> RunListEntry {
  RunListEntry {
    scope: scope(id),
    task: Some("train".into()),
    status: "completed".into(),
    started_at: Some("2026-10-01T10:00:00Z".into()),
    finished_at: Some("2026-10-01T11:00:00Z".into()),
    exit_code: Some(0),
    archived: false,
    warnings: Vec::new(),
  }
}

fn run_page() -> RunListPage {
  RunListPage {
    runs: vec![run("run-002"), run("run-001")],
    next_cursor: None,
  }
}

fn files_query() -> RunFilesQuery {
  RunFilesQuery {
    scope: scope("run-002"),
    prefix: None,
    limit: 20,
    cursor: None,
  }
}

fn received(digest: Option<&str>) -> ReceivedRunFile {
  ReceivedRunFile {
    size: 4,
    sha256: digest.map(str::to_owned),
    storage: FileStorage::Object,
  }
}

fn reported(digest: Option<&str>, label: &str) -> ReportedRunFile {
  ReportedRunFile {
    size: 4,
    sha256: digest.map(str::to_owned),
    labels: vec![label.into()],
    sync_status: Some("registered".into()),
    sync_error: None,
  }
}

fn received_file(path: &str) -> RunFileEntry {
  RunFileEntry {
    path: path.into(),
    availability: RunFileAvailability::Received,
    received: Some(received(Some(&"a".repeat(64)))),
    reported: None,
    labels_pending: false,
  }
}

fn files_page() -> RunFilesPage {
  RunFilesPage {
    scope: scope("run-002"),
    files: vec![
      received_file("outputs/one.pt"),
      received_file("outputs/two.pt"),
    ],
    next_cursor: None,
    inventory_recorded_at: Some("2026-10-01T11:00:00Z".into()),
    reported_inventory_truncated: false,
    warnings: Vec::new(),
  }
}

#[test]
fn initialization_requires_catalog_capability_and_does_not_follow_redirects() {
  let (url, server) = mock(1, |request, _| {
    assert!(matches!(
      serde_json::from_slice::<Request>(&request.body).unwrap(),
      Request::Capabilities
    ));
    (
      200,
      Vec::new(),
      encoded(&Response::Capabilities {
        features: vec!["tracking-v1".into()],
      }),
    )
  });
  let error = ServiceCatalogClient::from_api(api(&url)).err().unwrap();
  assert!(error.to_string().contains("service-catalog-v1"));
  server.join().unwrap();

  let (url, server) = mock(1, |_, _| {
    (
      302,
      vec![("Location".into(), "https://must-not-follow.invalid/".into())],
      capabilities(),
    )
  });
  assert!(matches!(
    ServiceCatalogClient::from_api(api(&url)),
    Err(ExpriError::ServiceRejected { status: 302, .. })
  ));
  server.join().unwrap();
}

#[test]
fn run_pages_use_bearer_transport_and_preserve_requested_scope_and_cursor() {
  let query = RunListQuery {
    limit: 2,
    cursor: Some(encode_run_cursor(&scope("run-003")).unwrap()),
    ..run_query()
  };
  let expected = query.clone();
  let mut count = 0;
  let (url, server) = mock(2, move |request, _| {
    assert_eq!(request.path, "/v1/request");
    assert_eq!(request.headers["authorization"], "Bearer must-not-leak");
    count += 1;
    if count == 1 {
      return (200, Vec::new(), capabilities());
    }
    let Request::BrowseRuns { query } = serde_json::from_slice(&request.body).unwrap() else {
      panic!("expected run browsing request");
    };
    assert_eq!(query.project_id, expected.project_id);
    assert_eq!(query.origin, expected.origin);
    assert_eq!(query.archival, expected.archival);
    assert_eq!(query.limit, expected.limit);
    assert_eq!(query.cursor, expected.cursor);
    let mut page = run_page();
    page.next_cursor = Some(encode_run_cursor(&page.runs.last().unwrap().scope).unwrap());
    (200, Vec::new(), encoded(&Response::RunPage { page }))
  });
  let client = ServiceCatalogClient::from_api(api(&url)).unwrap();
  let page = client.runs(&query).unwrap();
  assert_eq!(page.runs.len(), 2);
  assert_eq!(
    page.next_cursor,
    Some(encode_run_cursor(&scope("run-001")).unwrap())
  );
  server.join().unwrap();
}

#[test]
fn absent_filters_allow_a_server_restricted_worker_page_and_duplicate_ids_across_origins() {
  let query = RunListQuery {
    project_id: None,
    origin: None,
    ..run_query()
  };
  let mut page = run_page();
  page.runs[1].scope.run_id = "run-002".into();
  page.runs[1].scope.origin = "gpu-2".into();
  let (client, server) = client_with_page(Response::RunPage { page });
  let page = client.runs(&query).unwrap();
  assert_eq!(page.runs[0].scope.run_id, page.runs[1].scope.run_id);
  assert_ne!(page.runs[0].scope.origin, page.runs[1].scope.origin);
  server.join().unwrap();
}

#[test]
fn inconsistent_run_scope_archival_order_and_continuation_are_rejected() {
  let mut cases = Vec::new();
  let mut page = run_page();
  page.runs[0].scope.project_id = "other-project".into();
  cases.push((run_query(), page));
  let mut page = run_page();
  page.runs[0].scope.origin = "other-worker".into();
  cases.push((run_query(), page));
  let mut page = run_page();
  page.runs[0].archived = true;
  cases.push((run_query(), page));
  let mut page = run_page();
  page.runs.reverse();
  cases.push((run_query(), page));
  let mut page = run_page();
  page.runs[1] = page.runs[0].clone();
  cases.push((run_query(), page));
  let mut page = run_page();
  page.next_cursor = Some(encode_run_cursor(&scope("run-000")).unwrap());
  cases.push((run_query(), page));
  let query = RunListQuery {
    cursor: Some(encode_run_cursor(&scope("run-002")).unwrap()),
    ..run_query()
  };
  cases.push((query, run_page()));
  cases.push((
    RunListQuery {
      limit: 1,
      ..run_query()
    },
    run_page(),
  ));
  let mut page = run_page();
  page.runs[0].status = "must-not-leak".into();
  cases.push((run_query(), page));
  let mut page = run_page();
  page.runs[1].scope.run_id = page.runs[0].scope.run_id.clone();
  page.runs[0].scope.project_id = "z".into();
  page.runs[1].scope.project_id = "a".into();
  cases.push((
    RunListQuery {
      project_id: None,
      origin: None,
      ..run_query()
    },
    page,
  ));
  for (query, page) in cases {
    let (client, server) = client_with_page(Response::RunPage { page });
    let detail = client.runs(&query).unwrap_err().to_string();
    assert!(detail.contains("inconsistent run catalog"));
    assert!(!detail.contains("must-not-leak"));
    server.join().unwrap();
  }
}

#[test]
fn a_structurally_valid_page_must_also_fit_the_catalog_byte_budget() {
  let mut page = RunListPage {
    runs: Vec::new(),
    next_cursor: None,
  };
  for index in 0..100 {
    let mut entry = run(&format!("run-{:03}", 999 - index));
    entry.task = Some("t".repeat(4096));
    entry.warnings = vec!["w".repeat(512); 24];
    page.runs.push(entry);
    if serde_json::to_vec(&page).unwrap().len() > MAX_CATALOG_PAGE_BYTES {
      break;
    }
  }
  assert!(serde_json::to_vec(&page).unwrap().len() > MAX_CATALOG_PAGE_BYTES);
  let response = Response::RunPage { page };
  assert!(encoded(&response).len() < crate::types::MAX_REQUEST);
  let (client, server) = client_with_page(response);
  let error = client
    .runs(&RunListQuery {
      limit: 100,
      ..run_query()
    })
    .unwrap_err();
  assert!(error.to_string().contains("inconsistent run catalog"));
  server.join().unwrap();
}

#[test]
fn file_pages_preserve_pending_confirmed_unconfirmed_and_different_version_readings() {
  let a = "a".repeat(64);
  let b = "b".repeat(64);
  let page = RunFilesPage {
    files: vec![
      RunFileEntry {
        path: "outputs/a-pending.pt".into(),
        availability: RunFileAvailability::ReportedOnly,
        received: None,
        reported: Some(reported(Some(&a), "best")),
        labels_pending: true,
      },
      RunFileEntry {
        path: "outputs/b-confirmed.pt".into(),
        availability: RunFileAvailability::Received,
        received: Some(received(Some(&a))),
        reported: Some(reported(Some(&a), "best")),
        labels_pending: false,
      },
      RunFileEntry {
        path: "outputs/c-unconfirmed.pt".into(),
        availability: RunFileAvailability::Received,
        received: Some(received(Some(&a))),
        reported: Some(reported(None, "latest")),
        labels_pending: true,
      },
      RunFileEntry {
        path: "outputs/d-stale.pt".into(),
        availability: RunFileAvailability::ReceivedDifferentVersion,
        received: Some(received(Some(&b))),
        reported: Some(reported(Some(&a), "latest")),
        labels_pending: true,
      },
      RunFileEntry {
        path: "run-state.json".into(),
        availability: RunFileAvailability::Received,
        received: Some(ReceivedRunFile {
          size: 1024,
          sha256: None,
          storage: FileStorage::Tracking {
            revision: 2,
            sealed: false,
          },
        }),
        reported: None,
        labels_pending: false,
      },
    ],
    next_cursor: Some("run-state.json".into()),
    ..files_page()
  };
  let mut count = 0;
  let (url, server) = mock(2, move |request, _| {
    assert_eq!(request.headers["authorization"], "Bearer must-not-leak");
    count += 1;
    if count == 1 {
      return (200, Vec::new(), capabilities());
    }
    let Request::BrowseRunFiles { query } = serde_json::from_slice(&request.body).unwrap() else {
      panic!("expected file browsing request");
    };
    assert_eq!(query.scope, page.scope);
    assert!(query.prefix.is_none());
    (
      200,
      Vec::new(),
      encoded(&Response::RunFilesPage { page: page.clone() }),
    )
  });
  let client = ServiceCatalogClient::from_api(api(&url)).unwrap();
  let page = client.files(&files_query()).unwrap();
  assert_eq!(
    page.files[0].availability,
    RunFileAvailability::ReportedOnly
  );
  assert!(page.files[0].labels_pending);
  assert_eq!(page.files[1].availability, RunFileAvailability::Received);
  assert!(!page.files[1].labels_pending);
  assert_eq!(page.files[2].availability, RunFileAvailability::Received);
  assert!(page.files[2].labels_pending);
  assert_eq!(
    page.files[3].availability,
    RunFileAvailability::ReceivedDifferentVersion
  );
  assert_eq!(
    page.files[3].received.as_ref().unwrap().sha256.as_deref(),
    Some(b.as_str())
  );
  assert_eq!(
    page.files[3].reported.as_ref().unwrap().sha256.as_deref(),
    Some(a.as_str())
  );
  assert_eq!(page.next_cursor.as_deref(), Some("run-state.json"));
  server.join().unwrap();
}

#[test]
fn prefixes_with_sql_wildcard_characters_remain_literal() {
  let query = RunFilesQuery {
    prefix: Some("outputs/a_%".into()),
    ..files_query()
  };
  let page = RunFilesPage {
    files: vec![received_file("outputs/a_%literal.pt")],
    ..files_page()
  };
  let (client, server) = client_with_page(Response::RunFilesPage { page });
  assert_eq!(
    client.files(&query).unwrap().files[0].path,
    "outputs/a_%literal.pt"
  );
  server.join().unwrap();
}

#[test]
fn inconsistent_file_scope_prefix_order_cursor_and_availability_are_rejected() {
  let mut cases = Vec::new();
  let mut page = files_page();
  page.scope.run_id = "another-run".into();
  cases.push((files_query(), page));
  let mut page = files_page();
  page.files.reverse();
  cases.push((files_query(), page));
  let mut page = files_page();
  page.files[1] = page.files[0].clone();
  cases.push((files_query(), page));
  let mut page = files_page();
  page.next_cursor = Some("outputs/wrong-cursor.pt".into());
  cases.push((files_query(), page));
  cases.push((
    RunFilesQuery {
      cursor: Some("outputs/one.pt".into()),
      ..files_query()
    },
    files_page(),
  ));
  cases.push((
    RunFilesQuery {
      prefix: Some("outputs/a_%".into()),
      ..files_query()
    },
    files_page(),
  ));
  cases.push((
    RunFilesQuery {
      limit: 1,
      ..files_query()
    },
    files_page(),
  ));
  let mut page = files_page();
  page.files[0].availability = RunFileAvailability::ReportedOnly;
  cases.push((files_query(), page));
  let mut page = files_page();
  page.files[0].reported = Some(reported(Some(&"a".repeat(64)), "best"));
  page.files[0].availability = RunFileAvailability::ReceivedDifferentVersion;
  cases.push((files_query(), page));
  let mut page = files_page();
  page.files[0].reported = Some(reported(None, "latest"));
  page.files[0].availability = RunFileAvailability::ReceivedDifferentVersion;
  page.files[0].labels_pending = true;
  cases.push((files_query(), page));
  let mut page = files_page();
  page.files[0].labels_pending = true;
  cases.push((files_query(), page));
  for (query, page) in cases {
    let (client, server) = client_with_page(Response::RunFilesPage { page });
    let detail = client.files(&query).unwrap_err().to_string();
    assert!(detail.contains("inconsistent run file catalog"));
    assert!(!detail.contains("must-not-leak"));
    server.join().unwrap();
  }
}

#[test]
fn invalid_queries_do_not_contact_the_service_and_rejections_redact_credentials() {
  let (url, server) = mock(1, |_, _| (200, Vec::new(), capabilities()));
  let client = ServiceCatalogClient::from_api(api(&url)).unwrap();
  server.join().unwrap();
  let errors = [
    client
      .runs(&RunListQuery {
        limit: 0,
        ..run_query()
      })
      .unwrap_err(),
    client
      .runs(&RunListQuery {
        project_id: Some("../escape".into()),
        ..run_query()
      })
      .unwrap_err(),
    client
      .files(&RunFilesQuery {
        prefix: Some("outputs/../secret".into()),
        ..files_query()
      })
      .unwrap_err(),
  ];
  // With the listener closed, a request would fail as ServiceUnavailable.
  assert!(
    errors
      .into_iter()
      .all(|error| matches!(error, ExpriError::Message(_)))
  );

  let mut count = 0;
  let (url, server) = mock(2, move |_, _| {
    count += 1;
    if count == 1 {
      return (200, Vec::new(), capabilities());
    }
    (
      403,
      Vec::new(),
      br#"{"error":"denied must-not-leak"}"#.to_vec(),
    )
  });
  let client = ServiceCatalogClient::from_api(api(&url)).unwrap();
  let error = client.runs(&run_query()).unwrap_err();
  assert!(matches!(
    error,
    ExpriError::ServiceRejected { status: 403, .. }
  ));
  assert!(!error.to_string().contains("must-not-leak"));
  server.join().unwrap();
}

#[test]
fn file_catalogs_reject_unexpected_download_urls_without_echoing_them() {
  let mut count = 0;
  let (url, server) = mock(2, move |_, _| {
    count += 1;
    if count == 1 {
      return (200, Vec::new(), capabilities());
    }
    let mut response = serde_json::to_value(Response::RunFilesPage { page: files_page() }).unwrap();
    response["page"]["files"][0]["download_url"] =
      serde_json::json!("https://must-not-leak@example.invalid/object?signature=secret");
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let client = ServiceCatalogClient::from_api(api(&url)).unwrap();
  let detail = client.files(&files_query()).unwrap_err().to_string();
  assert!(detail.contains("invalid service response"));
  assert!(!detail.contains("must-not-leak"));
  assert!(!detail.contains("signature=secret"));
  server.join().unwrap();
}
