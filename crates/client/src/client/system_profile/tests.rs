use std::collections::BTreeMap;

use super::*;
use crate::client::download::mock::mock;
use crate::client::http::tests::api;
use crate::error::ExpriError;
use expri_core::system_profile::{
  MetricAggregate, MetricKind, MetricReading, PROFILE_SCHEMA_VERSION, SystemHour, SystemSample,
};

fn capabilities() -> Vec<u8> {
  serde_json::to_vec(&Response::Capabilities {
    features: vec![SYSTEM_PROFILE_CAPABILITY.into()],
  })
  .unwrap()
}

fn batch() -> ProfileBatch {
  ProfileBatch {
    schema_version: PROFILE_SCHEMA_VERSION,
    machine_id: "machine-test".into(),
    inventories: Vec::new(),
    samples: Vec::new(),
    hours: Vec::new(),
  }
}

#[test]
fn initialization_requires_capability_and_redirects_are_not_followed() {
  let (url, server) = mock(1, |request, _| {
    assert!(matches!(
      serde_json::from_slice::<Request>(&request.body).unwrap(),
      Request::Capabilities
    ));
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::Capabilities {
        features: vec!["tracking-v1".into()],
      })
      .unwrap(),
    )
  });
  assert!(
    SystemProfileClient::from_api(api(&url))
      .err()
      .unwrap()
      .to_string()
      .contains("does not support")
  );
  server.join().unwrap();

  let (url, server) = mock(1, |_, _| {
    (
      302,
      vec![("Location".into(), "https://must-not-follow.invalid/".into())],
      capabilities(),
    )
  });
  assert!(matches!(
    SystemProfileClient::from_api(api(&url)),
    Err(ExpriError::ServiceRejected { status: 302, .. })
  ));
  server.join().unwrap();
}

#[test]
fn profile_publish_preserves_scoping_and_uses_the_existing_bearer_transport() {
  let mut count = 0;
  let (url, server) = mock(2, move |request, _| {
    assert_eq!(request.path, "/v1/request");
    assert_eq!(request.headers["authorization"], "Bearer must-not-leak");
    count += 1;
    if count == 1 {
      return (200, Vec::new(), capabilities());
    }
    let Request::PutSystemProfile { batch } = serde_json::from_slice(&request.body).unwrap() else {
      panic!("expected profile request")
    };
    assert_eq!(batch.machine_id, "machine-test");
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::SystemProfileAcknowledged {
        acknowledgement: ProfileAcknowledgement {
          machine_id: batch.machine_id,
          sample_sequence: 0,
          inventory_revision: 0,
          hour_revisions: BTreeMap::new(),
        },
      })
      .unwrap(),
    )
  });
  let client = SystemProfileClient::from_api(api(&url)).unwrap();
  assert_eq!(client.endpoint_key(), format!("{url}/v1/request"));
  assert_eq!(client.publish(&batch()).unwrap().machine_id, "machine-test");
  server.join().unwrap();
}

#[test]
fn retransmitted_float_samples_and_summaries_keep_their_exact_wire_values() {
  let value = 4.264386931886279e-8;
  let mut batch = batch();
  batch.samples.push(SystemSample {
    sequence: 1,
    session_id: "session".into(),
    observed_at_ms: 10_000,
    interval_ms: 10_000,
    collection_duration_ms: 1,
    inventory_revision: 1,
    metrics: BTreeMap::from([(
      "cpu/usage_percent".into(),
      MetricReading {
        kind: MetricKind::Gauge,
        value,
      },
    )]),
    counters: BTreeMap::new(),
    warnings: Vec::new(),
  });
  let sum = value * 10_000.0;
  batch.hours.push(SystemHour {
    hour_start_ms: 0,
    revision: 1,
    sample_count: 1,
    first_sample_at_ms: 10_000,
    last_sample_at_ms: 10_000,
    inventory_revisions: vec![1],
    warnings: Vec::new(),
    metrics: BTreeMap::from([(
      "cpu/usage_percent".into(),
      MetricAggregate {
        kind: MetricKind::Gauge,
        sample_count: 1,
        coverage_ms: 10_000,
        sum,
        min: value,
        max: value,
        last: value,
        last_at_ms: 10_000,
      },
    )]),
  });
  let mut count = 0;
  let mut previous = None;
  let (url, server) = mock(3, move |request, _| {
    count += 1;
    if count == 1 {
      return (200, Vec::new(), capabilities());
    }
    let Request::PutSystemProfile { batch } = serde_json::from_slice(&request.body).unwrap() else {
      panic!("expected system profile publication")
    };
    assert_eq!(
      batch.samples[0].metrics["cpu/usage_percent"]
        .value
        .to_bits(),
      value.to_bits()
    );
    assert_eq!(
      batch.hours[0].metrics["cpu/usage_percent"].sum.to_bits(),
      sum.to_bits()
    );
    if let Some(previous) = &previous {
      assert_eq!(&batch, previous);
    }
    previous = Some(batch.clone());
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::SystemProfileAcknowledged {
        acknowledgement: ProfileAcknowledgement {
          machine_id: batch.machine_id,
          sample_sequence: 1,
          inventory_revision: 0,
          hour_revisions: BTreeMap::from([(0, 1)]),
        },
      })
      .unwrap(),
    )
  });
  let client = SystemProfileClient::from_api(api(&url)).unwrap();
  let first = client.publish(&batch).unwrap();
  assert_eq!(client.publish(&batch).unwrap(), first);
  server.join().unwrap();
}

#[test]
fn acknowledgements_for_another_batch_are_rejected() {
  for wrong_machine in [true, false] {
    let mut count = 0;
    let (url, server) = mock(2, move |_, _| {
      count += 1;
      if count == 1 {
        return (200, Vec::new(), capabilities());
      }
      (
        200,
        Vec::new(),
        serde_json::to_vec(&Response::SystemProfileAcknowledged {
          acknowledgement: ProfileAcknowledgement {
            machine_id: if wrong_machine {
              "another-machine"
            } else {
              "machine-test"
            }
            .into(),
            sample_sequence: if wrong_machine { 0 } else { 42 },
            inventory_revision: 0,
            hour_revisions: BTreeMap::new(),
          },
        })
        .unwrap(),
      )
    });
    let client = SystemProfileClient::from_api(api(&url)).unwrap();
    assert!(
      client
        .publish(&batch())
        .unwrap_err()
        .to_string()
        .contains("inconsistent")
    );
    server.join().unwrap();
  }
}

#[test]
fn auth_errors_keep_their_status_and_redact_secrets() {
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
  let client = SystemProfileClient::from_api(api(&url)).unwrap();
  let error = client.machine("machine-test", None).unwrap_err();
  assert!(matches!(
    error,
    ExpriError::ServiceRejected { status: 403, .. }
  ));
  assert!(!error.to_string().contains("must-not-leak"));
  server.join().unwrap();
}

#[test]
fn invalid_bounds_and_inconsistent_history_cursors_are_rejected() {
  let mut count = 0;
  let (url, server) = mock(2, move |_, _| {
    count += 1;
    if count == 1 {
      return (200, Vec::new(), capabilities());
    }
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::SystemHistory {
        history: ProfileHistory {
          machine_id: "machine-test".into(),
          resolution: ProfileResolution::Detail,
          samples: vec![SystemSample {
            sequence: 1,
            session_id: "session".into(),
            observed_at_ms: 10_000,
            interval_ms: 0,
            collection_duration_ms: 1,
            inventory_revision: 1,
            metrics: BTreeMap::new(),
            counters: BTreeMap::new(),
            warnings: Vec::new(),
          }],
          hours: Vec::new(),
          next_cursor: Some(42),
        },
      })
      .unwrap(),
    )
  });
  let client = SystemProfileClient::from_api(api(&url)).unwrap();
  assert!(client.machines(0, None).is_err());
  assert!(client.machines(1, Some("../escape")).is_err());
  assert!(client.machine("machine-test", Some(0)).is_err());
  let query = ProfileHistoryQuery {
    machine_id: "machine-test".into(),
    start_ms: 0,
    end_ms: 20_000,
    resolution: ProfileResolution::Detail,
    cursor: None,
    limit: 1,
  };
  assert!(
    client
      .history(&query)
      .unwrap_err()
      .to_string()
      .contains("cursor")
  );
  server.join().unwrap();
}
