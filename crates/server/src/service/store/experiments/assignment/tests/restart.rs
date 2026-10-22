use expri_core::run_control::{ControlAction, ControlResult, RunControlRequest};

use super::*;
use crate::service::store::run_control::ControlSubmission;

fn poll(scope: &RunScope, result: Option<ControlResult>) -> RunControlRequest {
  RunControlRequest::Poll {
    scope: scope.clone(),
    binding_id: "binding".into(),
    attempt: 1,
    generation: 1,
    status: "completed".into(),
    capabilities: vec![ControlAction::Restart],
    command: vec!["train".into()],
    checkpoints: vec![],
    lifecycle: None,
    requirements: None,
    resource_status: None,
    manual_paused: None,
    experiment_assignment_supported: Some(true),
    result,
  }
}

fn submit(
  store: &Store<MockStorage>,
  parent: &RunScope,
  request_id: &str,
  experiment: Option<ExperimentSelection>,
) {
  store.poll_run_control(poll(parent, None)).unwrap();
  store
    .submit_run_control(ControlSubmission {
      project_id: parent.project_id.clone(),
      origin: parent.origin.clone(),
      run_id: parent.run_id.clone(),
      request_id: request_id.into(),
      binding_id: "binding".into(),
      expected_attempt: 1,
      expected_generation: 1,
      action: ControlAction::Restart,
      arguments: None,
      checkpoint: None,
      requirements: None,
      experiment,
      reason: "Restart training".into(),
      grace_seconds: 30,
    })
    .unwrap();
}

fn deliver(
  store: &Store<MockStorage>,
  parent: &RunScope,
  request_id: &str,
  experiment: Option<ExperimentSelection>,
) {
  submit(store, parent, request_id, experiment);
  assert!(
    store
      .poll_run_control(poll(parent, None))
      .unwrap()
      .command
      .is_some()
  );
}

fn acknowledge(
  store: &Store<MockStorage>,
  parent: &RunScope,
  request_id: &str,
  child: &RunScope,
  status: &str,
) {
  store
    .poll_run_control(poll(
      parent,
      Some(ControlResult {
        request_id: request_id.into(),
        status: status.into(),
        error: (status == "failed").then(|| "Successor failed to start".into()),
        new_run_id: (status != "executing").then(|| child.run_id.clone()),
      }),
    ))
    .unwrap();
}

#[test]
fn expired_undelivered_restart_does_not_block_but_delivered_work_still_waits() {
  for status in ["requested", "delivered", "executing"] {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let parent = scope("worker", "parent");
    let child = scope("worker", "child");
    for run in [&parent, &child] {
      document(&store, run);
    }
    let group = store
      .save_experiment(selection(vec![parent.clone()], vec![]))
      .unwrap();
    let request = intent(
      "restart",
      ExperimentSelection::Existing {
        experiment_id: group.experiment_id.clone(),
      },
    );
    if status == "requested" {
      submit(
        &store,
        &parent,
        &request.request_id,
        Some(request.selection.clone()),
      );
    } else {
      deliver(
        &store,
        &parent,
        &request.request_id,
        Some(request.selection.clone()),
      );
      if status == "executing" {
        acknowledge(&store, &parent, &request.request_id, &child, "executing");
      }
    }
    // Advance the delivery deadline without sleeping or polling the source run.
    store.db().unwrap().execute(
      "UPDATE run_control_commands SET command=json_set(command,'$.expires_at_ms',0) WHERE request_id=?1",
      [&request.request_id],
    ).unwrap();
    if status == "requested" {
      store.assign_run_experiment(&child, &request).unwrap();
      let after = load(&store.db().unwrap(), "project", &group.experiment_id)
        .unwrap()
        .unwrap();
      assert!(after.lineage.is_empty());
      assert!(after.members.contains(&child));
      assert!(
        store
          .poll_run_control(poll(&parent, None))
          .unwrap()
          .command
          .is_none()
      );
    } else {
      let error = store.assign_run_experiment(&child, &request).unwrap_err();
      assert_eq!(error.status, 409, "{status}");
      assert!(error.message.contains("restart acknowledgement is pending"));
      assert!(store.dashboard_run_experiment(&child).unwrap().is_null());
      assert_eq!(
        store
          .db()
          .unwrap()
          .query_row(
            "SELECT COUNT(*) FROM experiment_assignment_receipts",
            [],
            |row| row.get::<_, usize>(0)
          )
          .unwrap(),
        0
      );
      acknowledge(&store, &parent, &request.request_id, &child, "applied");
      store.assign_run_experiment(&child, &request).unwrap();
      let after = load(&store.db().unwrap(), "project", &group.experiment_id)
        .unwrap()
        .unwrap();
      assert_eq!(after.lineage.len(), 1, "{status}");
      assert_eq!(after.lineage[0].kind, LineageKind::Restarted);
    }
  }
}

#[test]
fn assignment_before_restart_acknowledgement_retries_atomically_after_reopen() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  let parent = scope("worker", "parent");
  let child = scope("worker", "child");
  for run in [&parent, &child] {
    document(&store, run);
  }
  let request = intent(
    "restart",
    ExperimentSelection::JoinRun {
      run_id: parent.run_id.clone(),
      origin: Some(parent.origin.clone()),
    },
  );
  deliver(
    &store,
    &parent,
    &request.request_id,
    Some(request.selection.clone()),
  );
  let error = store.assign_run_experiment(&child, &request).unwrap_err();
  assert_eq!(error.status, 409);
  assert!(error.message.contains("restart acknowledgement is pending"));
  assert!(
    store.experiments("project").unwrap()["experiments"]
      .as_array()
      .unwrap()
      .is_empty()
  );
  for table in [
    "experiment_assignment_intents",
    "experiment_assignment_receipts",
  ] {
    assert_eq!(
      store
        .db()
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
          .get::<_, usize>(0))
        .unwrap(),
      0
    );
  }

  // A service restart or an offline worker does not lose the eventual linkage.
  drop(store);
  let store = Store::open(directory.path(), storage).unwrap();
  acknowledge(&store, &parent, &request.request_id, &child, "applied");
  let receipt = store.assign_run_experiment(&child, &request).unwrap();
  let group = load(&store.db().unwrap(), "project", &receipt.experiment_id)
    .unwrap()
    .unwrap();
  assert_eq!(group.members, vec![parent.clone(), child.clone()]);
  assert_eq!(
    group.lineage,
    vec![Lineage {
      parent,
      child: child.clone(),
      kind: LineageKind::Restarted,
      checkpoint: None,
      step: None
    }]
  );
  assert_eq!(
    store.assign_run_experiment(&child, &request).unwrap(),
    receipt
  );
  assert_eq!(
    load(&store.db().unwrap(), "project", &receipt.experiment_id)
      .unwrap()
      .unwrap()
      .revision,
    group.revision
  );

  let mut edited = edit(&group);
  edited.lineage.clear();
  edited.members.retain(|member| member != &child);
  let edited = store.save_experiment(edited).unwrap();
  assert_eq!(
    store.assign_run_experiment(&child, &request).unwrap(),
    receipt
  );
  assert_eq!(
    load(&store.db().unwrap(), "project", &receipt.experiment_id)
      .unwrap()
      .unwrap(),
    edited
  );
}

#[test]
fn confirmed_restart_adds_lineage_to_an_existing_experiment() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let parent = scope("worker", "parent");
  let child = scope("worker", "child");
  for run in [&parent, &child] {
    document(&store, run);
  }
  let group = store
    .save_experiment(selection(vec![parent.clone()], vec![]))
    .unwrap();
  let request = intent(
    "restart",
    ExperimentSelection::Existing {
      experiment_id: group.experiment_id.clone(),
    },
  );
  deliver(
    &store,
    &parent,
    &request.request_id,
    Some(request.selection.clone()),
  );
  acknowledge(&store, &parent, &request.request_id, &child, "applied");
  let receipt = store.assign_run_experiment(&child, &request).unwrap();
  assert_eq!(receipt.experiment_id, group.experiment_id);
  let after = load(&store.db().unwrap(), "project", &receipt.experiment_id)
    .unwrap()
    .unwrap();
  assert_eq!(after.name, group.name);
  assert_eq!(after.description, group.description);
  assert_eq!(after.lineage[0].parent, parent);
  assert_eq!(after.lineage[0].child, child);
  assert_eq!(after.lineage[0].kind, LineageKind::Restarted);
}

#[test]
fn unrelated_or_unconfirmed_restart_ids_do_not_invent_lineage() {
  for case in [
    "no-command",
    "other-request",
    "opt-out",
    "other-selection",
    "other-origin",
    "other-project",
    "other-successor",
    "failed",
  ] {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let parent = scope("worker", "parent");
    let child = scope("worker", "child");
    for run in [&parent, &child] {
      document(&store, run);
    }
    let group = store
      .save_experiment(selection(vec![parent.clone()], vec![]))
      .unwrap();
    let request = intent(
      "assignment",
      if case == "other-project" {
        ExperimentSelection::New {
          name: "Same selection, separate project".into(),
        }
      } else {
        ExperimentSelection::Existing {
          experiment_id: group.experiment_id.clone(),
        }
      },
    );
    if case != "no-command" {
      let mut source = parent.clone();
      if case == "other-origin" {
        source.origin = "another-worker".into();
      } else if case == "other-project" {
        source.project_id = "another-project".into();
      }
      document(&store, &source);
      let command_id = if case == "other-request" {
        "different-request"
      } else {
        &request.request_id
      };
      let experiment = match case {
        "opt-out" => None,
        "other-selection" => Some(ExperimentSelection::New {
          name: "Different experiment".into(),
        }),
        _ => Some(request.selection.clone()),
      };
      deliver(&store, &source, command_id, experiment);
      if matches!(case, "other-successor" | "failed") {
        let successor = if case == "other-successor" {
          scope("worker", "other-child")
        } else {
          child.clone()
        };
        acknowledge(
          &store,
          &source,
          command_id,
          &successor,
          if case == "failed" {
            "failed"
          } else {
            "applied"
          },
        );
      }
    }
    assert!(
      run_control::experiment_restart_parent(&store.db().unwrap(), &child, &request)
        .unwrap()
        .is_none(),
      "{case}"
    );
    let receipt = store.assign_run_experiment(&child, &request).unwrap();
    let after = load(&store.db().unwrap(), "project", &receipt.experiment_id)
      .unwrap()
      .unwrap();
    assert!(after.lineage.is_empty(), "{case}");
    assert!(after.members.contains(&child), "{case}");
  }
}

#[test]
fn explicit_new_or_different_experiment_preserves_the_source_group() {
  for new_group in [false, true] {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let parent = scope("worker", "parent");
    let child = scope("worker", "child");
    for run in [&parent, &child] {
      document(&store, run);
    }
    let original = store
      .save_experiment(selection(vec![parent.clone()], vec![]))
      .unwrap();
    let target = if new_group {
      ExperimentSelection::New {
        name: "New direction".into(),
      }
    } else {
      let other = store.save_experiment(selection(vec![], vec![])).unwrap();
      ExperimentSelection::Existing {
        experiment_id: other.experiment_id,
      }
    };
    let request = intent("restart", target);
    deliver(
      &store,
      &parent,
      &request.request_id,
      Some(request.selection.clone()),
    );
    acknowledge(&store, &parent, &request.request_id, &child, "applied");
    let receipt = store.assign_run_experiment(&child, &request).unwrap();
    let after = load(&store.db().unwrap(), "project", &receipt.experiment_id)
      .unwrap()
      .unwrap();
    assert_eq!(after.members, vec![child]);
    assert!(after.lineage.is_empty());
    assert_eq!(
      load(&store.db().unwrap(), "project", &original.experiment_id)
        .unwrap()
        .unwrap(),
      original
    );
  }
}

#[test]
fn manual_source_removal_before_assignment_does_not_restore_its_membership() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let parent = scope("worker", "parent");
  let child = scope("worker", "child");
  for run in [&parent, &child] {
    document(&store, run);
  }
  let original = store
    .save_experiment(selection(vec![parent.clone()], vec![]))
    .unwrap();
  let request = intent(
    "restart",
    ExperimentSelection::Existing {
      experiment_id: original.experiment_id.clone(),
    },
  );
  deliver(
    &store,
    &parent,
    &request.request_id,
    Some(request.selection.clone()),
  );
  let mut changed = edit(&original);
  changed.members.clear();
  store.save_experiment(changed).unwrap();
  acknowledge(&store, &parent, &request.request_id, &child, "applied");
  let receipt = store.assign_run_experiment(&child, &request).unwrap();
  let after = load(&store.db().unwrap(), "project", &receipt.experiment_id)
    .unwrap()
    .unwrap();
  assert_eq!(after.members, vec![child]);
  assert!(after.lineage.is_empty());
  assert!(store.dashboard_run_experiment(&parent).unwrap().is_null());
}

#[test]
fn automatic_restart_keeps_manual_relationships_and_rejects_cycles_atomically() {
  for reverse in [false, true] {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let parent = scope("worker", "parent");
    let child = scope("worker", "child");
    for run in [&parent, &child] {
      document(&store, run);
    }
    let manual = if reverse {
      edge(&child, &parent)
    } else {
      edge(&parent, &child)
    };
    let original = store
      .save_experiment(selection(vec![parent.clone(), child.clone()], vec![manual]))
      .unwrap();
    let request = intent(
      "restart",
      ExperimentSelection::Existing {
        experiment_id: original.experiment_id.clone(),
      },
    );
    deliver(
      &store,
      &parent,
      &request.request_id,
      Some(request.selection.clone()),
    );
    acknowledge(&store, &parent, &request.request_id, &child, "applied");
    let assigned = store.assign_run_experiment(&child, &request);
    if reverse {
      assert_eq!(assigned.unwrap_err().status, 400);
      assert_eq!(
        store
          .db()
          .unwrap()
          .query_row(
            "SELECT COUNT(*) FROM experiment_assignment_receipts",
            [],
            |row| row.get::<_, usize>(0)
          )
          .unwrap(),
        0
      );
    } else {
      assigned.unwrap();
    }
    let after = load(&store.db().unwrap(), "project", &original.experiment_id)
      .unwrap()
      .unwrap();
    assert_eq!(after.lineage, original.lineage);
    assert_eq!(after.members, original.members);
  }
}
