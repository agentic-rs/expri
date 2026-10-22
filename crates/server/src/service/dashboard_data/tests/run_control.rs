use super::*;
use crate::service::store::tests::MockStorage;
use expri_core::run_control::{ControlAction, ControlLifecycle, RunControlRequest};

const SOURCE: &str = "hosted:project:worker";
const START: &str = "2026-10-19T00:00:00Z";
const FINISH: &str = "2026-10-19T02:00:00Z";

fn scope() -> RunScope {
  RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run-control".into(),
  }
}

fn publish(store: &Store<MockStorage>, status: &str, generation: u64) {
  let bytes = serde_json::to_vec(&json!({"schema_version":1,"run_id":"run-control","task":"train","status":status,
    "started_at":START,"finished_at":(status == "completed").then_some(FINISH),"exit_code":(status == "completed").then_some(0),
    "supervisor":{"generation":generation}})).unwrap();
  store
    .execute(Request::PutDocument {
      scope: scope(),
      path: "run-state.json".into(),
      revision: 1,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

fn observe(
  store: &Store<MockStorage>,
  status: &str,
  attempt: u64,
  generation: u64,
  finished: bool,
) {
  store
    .execute(Request::RunControl {
      request: RunControlRequest::Poll {
        scope: scope(),
        binding_id: "control-binding".into(),
        attempt,
        generation,
        status: status.into(),
        capabilities: vec![
          ControlAction::Pause,
          ControlAction::Resume,
          ControlAction::Stop,
        ],
        command: vec!["python".into(), "train.py".into()],
        checkpoints: vec![],
        lifecycle: Some(ControlLifecycle {
          started_at: START.into(),
          finished_at: finished.then(|| FINISH.into()),
          exit_code: finished.then_some(130),
        }),
        requirements: None,
        resource_status: None,
        manual_paused: None,
        experiment_assignment_supported: None,
        result: None,
      },
    })
    .unwrap();
}

#[test]
fn live_lifecycle_overlay_updates_detail_filters_and_probes_without_mutating_published_state_or_cache()
 {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  publish(&store, "running", 1);
  let dashboard = HostedDashboard::new(&store).unwrap();
  let selected = [scope().run_id];
  let original = dashboard.updates(SOURCE, &selected).unwrap();
  assert_eq!(
    dashboard
      .list(SOURCE, None, None, Some("running"), 100, 0)
      .unwrap()["total_count"],
    1
  );
  observe(&store, "paused", 1, 1, false);
  let paused_probe = dashboard.updates(SOURCE, &selected).unwrap();
  assert_ne!(paused_probe["source_revision"], original["source_revision"]);
  assert_ne!(
    paused_probe["runs"][0]["metadata_revision"],
    original["runs"][0]["metadata_revision"]
  );
  let paused = dashboard.detail(SOURCE, "run-control").unwrap();
  assert_eq!(paused["run"]["status"], "paused");
  assert_eq!(paused["run"]["recorded_status"], "running");
  assert_eq!(paused["state"]["status"], "running");
  assert_eq!(
    dashboard
      .list(SOURCE, None, None, Some("paused"), 100, 0)
      .unwrap()["total_count"],
    1
  );
  assert_eq!(
    dashboard
      .list(SOURCE, None, None, Some("running"), 100, 0)
      .unwrap()["total_count"],
    0
  );
  assert_eq!(
    store
      .dashboard_cached_overview(&scope(), "tracking:1")
      .unwrap()
      .unwrap()["run"]["status"],
    "running"
  );
  observe(&store, "paused", 1, 1, false);
  assert_eq!(dashboard.updates(SOURCE, &selected).unwrap(), paused_probe);
  observe(&store, "waiting", 1, 1, false);
  assert_eq!(
    dashboard.detail(SOURCE, "run-control").unwrap()["run"]["status"],
    "waiting"
  );
  observe(&store, "cancelled", 1, 1, true);
  let stopped = dashboard.detail(SOURCE, "run-control").unwrap();
  assert_eq!(stopped["run"]["status"], "cancelled");
  assert_eq!(stopped["run"]["finished_at"], FINISH);
  assert_eq!(stopped["run"]["exit_code"], 130);
}

#[test]
fn continuation_overlay_clears_old_completion_and_preserves_the_original_start() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  publish(&store, "completed", 1);
  let dashboard = HostedDashboard::new(&store).unwrap();
  observe(&store, "running", 2, 2, false);
  let detail = dashboard.detail(SOURCE, "run-control").unwrap();
  assert_eq!(detail["run"]["status"], "running");
  assert_eq!(detail["run"]["started_at"], START);
  assert!(detail["run"]["finished_at"].is_null());
  assert!(detail["run"]["exit_code"].is_null());
  assert_eq!(detail["run"]["control"]["attempt"], 2);
  assert_eq!(detail["state"]["status"], "completed");
}

#[test]
fn newer_published_generations_and_terminal_same_attempt_records_fence_stale_observations() {
  for (published_status, published_generation, worker_status, worker_generation) in
    [("running", 2, "paused", 1), ("completed", 1, "running", 1)]
  {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    publish(&store, published_status, published_generation);
    observe(&store, worker_status, 1, worker_generation, false);
    assert_eq!(
      HostedDashboard::new(&store)
        .unwrap()
        .detail(SOURCE, "run-control")
        .unwrap()["run"]["status"],
      published_status
    );
  }
}

#[test]
fn pending_resource_reason_reaches_rows_and_detail_without_truncating_the_policy() {
  use expri_core::resource_requirements::{
    DiskObservation, DiskRequirement, ResourcePhase, ResourceRequirements, ResourceStatus,
  };
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  publish(&store, "running", 1);
  let path = format!("/mnt/{}", "p".repeat(600));
  let requirements = ResourceRequirements {
    disk: vec![DiskRequirement {
      path: path.clone(),
      min_free_bytes: 100,
      resume_free_bytes: 120,
    }],
    recovery_seconds: 30,
  };
  store
    .execute(Request::RunControl {
      request: RunControlRequest::Poll {
        scope: scope(),
        binding_id: "control-binding".into(),
        attempt: 1,
        generation: 1,
        status: "pending".into(),
        capabilities: vec![ControlAction::Stop, ControlAction::SetRequirements],
        command: vec!["train".into()],
        checkpoints: vec![],
        lifecycle: Some(ControlLifecycle {
          started_at: START.into(),
          finished_at: None,
          exit_code: None,
        }),
        requirements: Some(requirements.clone()),
        resource_status: Some(Box::new(ResourceStatus {
          phase: ResourcePhase::Blocked,
          blocked: true,
          checked_at: START.into(),
          recovery_started_at: None,
          disk: vec![DiskObservation {
            path: path.clone(),
            available_bytes: Some(90),
            min_free_bytes: 100,
            resume_free_bytes: 120,
            phase: ResourcePhase::Blocked,
            error: None,
          }],
        })),
        manual_paused: Some(false),
        experiment_assignment_supported: None,
        result: None,
      },
    })
    .unwrap();
  let dashboard = HostedDashboard::new(&store).unwrap();
  let detail = dashboard.detail(SOURCE, "run-control").unwrap();
  assert_eq!(detail["run"]["status"], "pending");
  assert_eq!(detail["run"]["requirements"], json!(requirements));
  assert_eq!(detail["run"]["resource_status"]["disk"][0]["path"], path);
  assert_eq!(detail["run"]["manual_paused"], false);
  assert!(detail["state"].get("resource_status").is_none());
  let rows = dashboard
    .list(SOURCE, None, None, Some("pending"), 100, 0)
    .unwrap();
  assert_eq!(rows["total_count"], 1);
  assert_eq!(rows["runs"][0]["resource_status"]["disk"][0]["path"], path);
}
