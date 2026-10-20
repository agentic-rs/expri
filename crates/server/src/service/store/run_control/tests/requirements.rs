use super::*;
use expri_core::resource_requirements::{
  DiskObservation, DiskRequirement, ResourcePhase, ResourceStatus,
};

fn policy() -> ResourceRequirements {
  ResourceRequirements {
    disk: vec![DiskRequirement {
      path: "outputs".into(),
      min_free_bytes: 100,
      resume_free_bytes: 120,
    }],
    recovery_seconds: 30,
  }
}

fn observation() -> ResourceStatus {
  ResourceStatus {
    phase: ResourcePhase::Blocked,
    blocked: true,
    checked_at: "2026-10-20T00:00:01Z".into(),
    recovery_started_at: None,
    disk: vec![DiskObservation {
      path: "outputs".into(),
      available_bytes: Some(90),
      min_free_bytes: 100,
      resume_free_bytes: 120,
      phase: ResourcePhase::Blocked,
      error: None,
    }],
  }
}

fn pending() -> RunControlRequest {
  let mut request = poll(None);
  let RunControlRequest::Poll {
    status,
    capabilities,
    requirements,
    resource_status,
    manual_paused,
    ..
  } = &mut request;
  *status = "pending".into();
  *capabilities = vec![ControlAction::Stop, ControlAction::SetRequirements];
  *requirements = Some(policy());
  *resource_status = Some(Box::new(observation()));
  *manual_paused = Some(false);
  request
}

#[test]
fn pending_policy_edits_are_fenced_idempotent_and_worker_acknowledged() {
  let (_directory, store) = fixture();
  store.poll_run_control_at(pending(), AT + 1).unwrap();
  let initial = view(&store, AT + 1);
  assert_eq!(initial["status"], "pending");
  assert_eq!(initial["requirements"], json!(policy()));
  assert_eq!(initial["resource_status"]["disk"][0]["available_bytes"], 90);
  assert_eq!(initial["manual_paused"], false);
  let mut edit = selection(ControlAction::SetRequirements);
  edit.requirements = Some(ResourceRequirements::default());
  store.submit_run_control_at(edit.clone(), AT + 2).unwrap();
  let command = store
    .poll_run_control_at(pending(), AT + 3)
    .unwrap()
    .command
    .unwrap();
  assert_eq!(command.action, ControlAction::SetRequirements);
  assert!(command.requirements.unwrap().is_empty());
  assert!(store.submit_run_control_at(edit.clone(), AT + 4).is_ok());
  edit.requirements = Some(policy());
  assert_eq!(
    store
      .submit_run_control_at(edit, AT + 4)
      .unwrap_err()
      .status,
    409
  );
  let mut acknowledged = pending();
  let RunControlRequest::Poll {
    requirements,
    resource_status,
    result: ack,
    ..
  } = &mut acknowledged;
  *requirements = Some(ResourceRequirements::default());
  *resource_status = None;
  *ack = Some(result("applied"));
  store.poll_run_control_at(acknowledged, AT + 5).unwrap();
  assert_eq!(view(&store, AT + 5)["request"]["status"], "applied");
  assert_eq!(view(&store, AT + 5)["requirements"]["disk"], json!([]));
}

#[test]
fn requirements_payload_is_validated_and_cannot_ride_another_action() {
  for action in [ControlAction::Pause, ControlAction::SetRequirements] {
    let (_directory, store) = fixture();
    let mut edit = selection(action);
    edit.requirements = (action == ControlAction::Pause).then(policy);
    assert_eq!(
      store
        .submit_run_control_at(edit, AT + 1)
        .unwrap_err()
        .status,
      400
    );
  }
  let (_directory, store) = fixture();
  store.poll_run_control_at(pending(), AT + 1).unwrap();
  let mut edit = selection(ControlAction::SetRequirements);
  let mut invalid = policy();
  invalid.disk[0].resume_free_bytes = invalid.disk[0].min_free_bytes;
  edit.requirements = Some(invalid);
  assert_eq!(
    store
      .submit_run_control_at(edit, AT + 2)
      .unwrap_err()
      .status,
    400
  );
  let mut stale = selection(ControlAction::SetRequirements);
  stale.requirements = Some(policy());
  stale.expected_generation += 1;
  assert_eq!(
    store
      .submit_run_control_at(stale, AT + 2)
      .unwrap_err()
      .status,
    409
  );
}

#[test]
fn invalid_observations_never_replace_the_last_valid_worker_facts() {
  let (_directory, store) = fixture();
  store.poll_run_control_at(pending(), AT + 1).unwrap();
  let before = view(&store, AT + 1);
  for variant in 0..3 {
    let mut request = pending();
    let RunControlRequest::Poll {
      requirements,
      resource_status,
      ..
    } = &mut request;
    match variant {
      0 => *requirements = None,
      1 => resource_status.as_mut().unwrap().disk[0].min_free_bytes = 101,
      _ => resource_status.as_mut().unwrap().checked_at = "not a timestamp".into(),
    }
    assert_eq!(
      store
        .poll_run_control_at(request, AT + 2)
        .unwrap_err()
        .status,
      400
    );
    assert_eq!(view(&store, AT + 2), before);
  }
}

#[test]
fn old_workers_cannot_receive_requirement_edits() {
  let (_directory, store) = fixture();
  let legacy = view(&store, AT);
  assert!(legacy.get("manual_paused").is_none());
  assert!(legacy.get("requirements").is_none());
  let mut edit = selection(ControlAction::SetRequirements);
  edit.requirements = Some(policy());
  assert_eq!(
    store
      .submit_run_control_at(edit, AT + 1)
      .unwrap_err()
      .status,
    409
  );
}
