use super::*;
use crate::client::download::mock::mock;
use crate::client::http::tests::api;
use crate::types::RunScope;
use expri_core::experiment_catalog::{ExperimentLimits, Lineage, LineageKind};

fn catalog() -> ExperimentCatalog {
  let scope = |origin: &str| RunScope {
    project_id: "project".into(),
    origin: origin.into(),
    run_id: "run-one".into(),
  };
  ExperimentCatalog {
    project_id: "project".into(),
    revision: "1".into(),
    limits: ExperimentLimits::default(),
    experiments: vec![Experiment {
      experiment_id: "exp-one".into(),
      name: "Shared goal".into(),
      description: "Two workers".into(),
      revision: "1".into(),
      created_at: "2026-10-20T00:00:00Z".into(),
      updated_at: "2026-10-20T00:00:00Z".into(),
      members: vec![scope("worker-a"), scope("worker-b")],
      membership_count: 2,
      unavailable_runs: vec![scope("worker-a")],
      lineage: vec![Lineage {
        parent: scope("worker-a"),
        child: scope("worker-b"),
        kind: LineageKind::Restarted,
        checkpoint: None,
        step: Some(u64::MAX.to_string()),
      }],
    }],
  }
}

fn client(response: Response) -> (ExperimentCatalogClient, std::thread::JoinHandle<()>) {
  let mut count = 0;
  let (url, server) = mock(2, move |request, _| {
    assert_eq!(request.headers["authorization"], "Bearer must-not-leak");
    count += 1;
    let reply = if count == 1 {
      assert!(matches!(
        serde_json::from_slice::<Request>(&request.body).unwrap(),
        Request::Capabilities
      ));
      Response::Capabilities {
        features: vec![EXPERIMENT_CATALOG_CAPABILITY.into()],
      }
    } else {
      assert!(
        matches!(serde_json::from_slice::<Request>(&request.body).unwrap(), Request::ListExperiments { project_id } if project_id == "project")
      );
      response.clone()
    };
    (200, Vec::new(), serde_json::to_vec(&reply).unwrap())
  });
  (
    ExperimentCatalogClient::from_api(api(&url)).unwrap(),
    server,
  )
}

#[test]
fn discovery_preserves_qualified_members_unavailability_and_lineage_steps() {
  let (client, server) = client(Response::Experiments { catalog: catalog() });
  let experiment = client.show("project", "exp-one").unwrap();
  assert_eq!(experiment.members[0].run_id, experiment.members[1].run_id);
  assert_ne!(experiment.members[0].origin, experiment.members[1].origin);
  assert_eq!(experiment.unavailable_runs.len(), 1);
  assert_eq!(
    experiment.lineage[0].step.as_deref(),
    Some("18446744073709551615")
  );
  server.join().unwrap();
}

#[test]
fn show_requires_exact_id_and_never_matches_a_name() {
  let (client, server) = client(Response::Experiments { catalog: catalog() });
  assert!(
    client
      .show("project", "Shared-goal")
      .unwrap_err()
      .to_string()
      .contains("ID was not found")
  );
  server.join().unwrap();
}

#[test]
fn catalog_rejects_mismatched_projects_members_edges_and_counts() {
  let mut cases = Vec::new();
  let mut value = catalog();
  value.project_id = "other".into();
  cases.push(value);
  let mut value = catalog();
  value.experiments[0].members[0].project_id = "other".into();
  cases.push(value);
  let mut value = catalog();
  value.experiments[0].lineage[0].child.origin = "missing".into();
  cases.push(value);
  let mut value = catalog();
  value.experiments[0].lineage[0].parent.project_id = "other".into();
  cases.push(value);
  let mut value = catalog();
  value.experiments[0].membership_count = 1;
  cases.push(value);
  let mut value = catalog();
  value.experiments[0].unavailable_runs[0].origin = "missing".into();
  cases.push(value);
  let mut value = catalog();
  value.experiments[0].lineage[0].step = Some("-1".into());
  cases.push(value);
  let mut value = catalog();
  let mut edge = value.experiments[0].lineage[0].clone();
  std::mem::swap(&mut edge.parent, &mut edge.child);
  value.experiments[0].lineage.push(edge);
  cases.push(value);
  let mut value = catalog();
  let mut duplicate = value.experiments[0].clone();
  duplicate.experiment_id = "exp-two".into();
  value.experiments.push(duplicate);
  cases.push(value);
  for catalog in cases {
    let (client, server) = client(Response::Experiments { catalog });
    assert!(
      client
        .list("project")
        .unwrap_err()
        .to_string()
        .contains("inconsistent experiment catalog")
    );
    server.join().unwrap();
  }
  let (client, server) = client(Response::ExperimentSelectionValidated);
  assert!(
    client
      .list("project")
      .unwrap_err()
      .to_string()
      .contains("did not return an experiment catalog")
  );
  server.join().unwrap();
}

#[test]
fn older_servers_and_invalid_selectors_fail_without_catalog_requests() {
  let (url, server) = mock(1, |_, _| {
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::Capabilities {
        features: Vec::new(),
      })
      .unwrap(),
    )
  });
  let error = ExperimentCatalogClient::from_api(api(&url))
    .err()
    .unwrap()
    .to_string();
  assert!(error.contains("upgrade the server") && error.contains("owner credentials"));
  server.join().unwrap();
  let (url, server) = mock(1, |_, _| {
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::Capabilities {
        features: vec![EXPERIMENT_CATALOG_CAPABILITY.into()],
      })
      .unwrap(),
    )
  });
  let client = ExperimentCatalogClient::from_api(api(&url)).unwrap();
  server.join().unwrap();
  assert!(matches!(
    client.list("../bad"),
    Err(crate::error::ExpriError::Message(_))
  ));
  assert!(matches!(
    client.show("project", "../bad"),
    Err(crate::error::ExpriError::Message(_))
  ));
}
