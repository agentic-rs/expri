use super::super::tests::{document, edge, edit, scope, selection};
use super::*;
use crate::service::store::tests::MockStorage;

fn intent(id: &str, selection: ExperimentSelection) -> RunExperimentIntent {
  RunExperimentIntent {
    request_id: id.into(),
    selection,
  }
}

fn new(id: &str) -> RunExperimentIntent {
  intent(
    id,
    ExperimentSelection::New {
      name: "  Training v2  ".into(),
    },
  )
}

#[test]
fn new_intent_is_stable_across_children_and_replays_do_not_undo_manual_edits() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let a = scope("first", "a");
  let b = scope("second", "b");
  let c = scope("third", "c");
  for run in [&a, &b, &c] {
    document(&store, run);
  }
  let request = new("launch");
  let first = store.assign_run_experiment(&a, &request).unwrap();
  let second = store.assign_run_experiment(&b, &request).unwrap();
  assert_eq!(first.experiment_id, second.experiment_id);
  assert_eq!(first.name, "Training v2");
  let group = load(&store.db().unwrap(), "project", &first.experiment_id)
    .unwrap()
    .unwrap();
  assert_eq!(group.members.len(), 2);
  let mut changed = edit(&group);
  changed.members.retain(|member| member != &a);
  changed.name = "Renamed in the website".into();
  let group = store.save_experiment(changed).unwrap();
  assert_eq!(store.assign_run_experiment(&a, &request).unwrap(), first);
  assert!(store.dashboard_run_experiment(&a).unwrap().is_null());
  store
    .delete_experiment("project", &group.experiment_id, &group.revision)
    .unwrap();
  assert_eq!(store.assign_run_experiment(&b, &request).unwrap(), second);
  assert_eq!(
    store
      .assign_run_experiment(&c, &request)
      .unwrap_err()
      .status,
    410
  );
  assert_eq!(
    store
      .assign_run_experiment(
        &a,
        &intent(
          "launch",
          ExperimentSelection::New {
            name: "Changed payload".into()
          }
        )
      )
      .unwrap_err()
      .status,
    409
  );
  assert!(
    store.experiments("project").unwrap()["experiments"]
      .as_array()
      .unwrap()
      .is_empty()
  );
}

#[test]
fn joining_an_existing_group_preserves_names_description_and_lineage() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let a = scope("first", "a");
  let b = scope("second", "b");
  let child = scope("third", "child");
  for run in [&a, &b, &child] {
    document(&store, run);
  }
  let before = store
    .save_experiment(selection(vec![a.clone(), b.clone()], vec![edge(&a, &b)]))
    .unwrap();
  let request = intent(
    "join",
    ExperimentSelection::JoinRun {
      run_id: a.run_id.clone(),
      origin: None,
    },
  );
  let receipt = store.assign_run_experiment(&child, &request).unwrap();
  assert_eq!(receipt.experiment_id, before.experiment_id);
  let after = load(&store.db().unwrap(), "project", &receipt.experiment_id)
    .unwrap()
    .unwrap();
  assert_eq!(after.name, before.name);
  assert_eq!(after.description, before.description);
  assert_eq!(after.lineage, before.lineage);
  assert_eq!(after.members.len(), 3);
  assert_ne!(after.revision, before.revision);
  assert_eq!(
    store.assign_run_experiment(&child, &request).unwrap(),
    receipt
  );
  assert_eq!(
    load(&store.db().unwrap(), "project", &receipt.experiment_id)
      .unwrap()
      .unwrap()
      .revision,
    after.revision
  );
}

#[test]
fn simultaneous_joins_to_an_ungrouped_anchor_converge_atomically() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let parent = scope("first", "parent");
  document(&store, &parent);
  let children = [scope("second", "a"), scope("third", "b")];
  for child in &children {
    document(&store, child);
  }
  let barrier = std::sync::Barrier::new(2);
  let receipts = std::thread::scope(|threads| {
    let handles: Vec<_> = children
      .iter()
      .map(|child| {
        threads.spawn(|| {
          let request = intent(
            &child.run_id,
            ExperimentSelection::JoinRun {
              run_id: parent.run_id.clone(),
              origin: None,
            },
          );
          barrier.wait();
          store.assign_run_experiment(child, &request).unwrap()
        })
      })
      .collect();
    handles
      .into_iter()
      .map(|handle| handle.join().unwrap())
      .collect::<Vec<_>>()
  });
  assert_eq!(receipts[0].experiment_id, receipts[1].experiment_id);
  let catalog = store.experiments("project").unwrap();
  assert_eq!(catalog["experiments"].as_array().unwrap().len(), 1);
  assert_eq!(catalog["experiments"][0]["membership_count"], 3);
}

#[test]
fn run_origin_is_unambiguous_and_expired_or_unpublished_runs_are_rejected() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let a = scope("first", "same");
  let b = scope("second", "same");
  for run in [&a, &b] {
    document(&store, run);
  }
  let selection = ExperimentSelection::JoinRun {
    run_id: "same".into(),
    origin: None,
  };
  assert_eq!(
    store
      .validate_experiment_selection("project", &selection)
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .validate_experiment_selection("other-project", &selection)
      .unwrap_err()
      .status,
    404
  );
  store
    .validate_experiment_selection(
      "project",
      &ExperimentSelection::JoinRun {
        run_id: "same".into(),
        origin: Some("first".into()),
      },
    )
    .unwrap();
  store.db().unwrap().execute("INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after,status) VALUES('project','second','same',0,0,'expired')", []).unwrap();
  store
    .validate_experiment_selection("project", &selection)
    .unwrap();
  assert_eq!(
    store
      .validate_experiment_selection(
        "project",
        &ExperimentSelection::JoinRun {
          run_id: "same".into(),
          origin: Some("second".into())
        }
      )
      .unwrap_err()
      .status,
    404
  );
  assert_eq!(
    store
      .assign_run_experiment(&scope("third", "not-yet-published"), &new("missing"))
      .unwrap_err()
      .status,
    404
  );
  assert!(
    store.experiments("project").unwrap()["experiments"]
      .as_array()
      .unwrap()
      .is_empty()
  );
}

#[test]
fn a_conflicting_membership_rolls_back_the_group_and_request_ledger() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let a = scope("first", "a");
  document(&store, &a);
  let original = store.assign_run_experiment(&a, &new("original")).unwrap();
  let before = store.experiments("project").unwrap();
  assert_eq!(
    store
      .assign_run_experiment(&a, &new("conflict"))
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(store.experiments("project").unwrap(), before);
  let count: u64 = store
    .db()
    .unwrap()
    .query_row(
      "SELECT COUNT(*) FROM experiment_assignment_intents",
      [],
      |row| row.get(0),
    )
    .unwrap();
  assert_eq!(count, 1);
  assert_eq!(
    store.dashboard_run_experiment(&a).unwrap()["experiment_id"],
    original.experiment_id
  );
}

#[test]
fn assignment_receipts_survive_reopen_and_project_deletion_removes_the_ledger() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  let run = scope("first", "a");
  document(&store, &run);
  let request = new("stable");
  let receipt = store.assign_run_experiment(&run, &request).unwrap();
  let before = store.experiments("project").unwrap();
  drop(store);
  let store = Store::open(directory.path(), storage).unwrap();
  assert_eq!(
    store.assign_run_experiment(&run, &request).unwrap(),
    receipt
  );
  assert_eq!(store.experiments("project").unwrap(), before);
  let preview = store.preview_project_delete("project").unwrap();
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  assert_eq!(
    store
      .assign_run_experiment(&run, &request)
      .unwrap_err()
      .status,
    410
  );
  for table in [
    "experiment_assignment_intents",
    "experiment_assignment_receipts",
  ] {
    let count: u64 = store
      .db()
      .unwrap()
      .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
      })
      .unwrap();
    assert_eq!(count, 0);
  }
}

#[test]
fn assignment_respects_member_capacity_without_partial_changes() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let members: Vec<_> = (0..MEMBER_LIMIT)
    .map(|index| scope("first", &format!("run-{index}")))
    .collect();
  for member in &members {
    document(&store, member);
  }
  let group = store.save_experiment(selection(members, vec![])).unwrap();
  let selection = ExperimentSelection::Existing {
    experiment_id: group.experiment_id.clone(),
  };
  assert_eq!(
    store
      .validate_experiment_selection("project", &selection)
      .unwrap_err()
      .status,
    409
  );
  let child = scope("other", "new-run");
  document(&store, &child);
  let before = store.experiments("project").unwrap();
  assert_eq!(
    store
      .assign_run_experiment(&child, &intent("overflow", selection))
      .unwrap_err()
      .status,
    400
  );
  assert_eq!(store.experiments("project").unwrap(), before);
  assert!(store.dashboard_run_experiment(&child).unwrap().is_null());
}

#[test]
fn intent_preflight_reuses_a_resolved_group_at_the_project_creation_limit() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let run = scope("first", "a");
  document(&store, &run);
  let request = new("queued-launch");
  store.assign_run_experiment(&run, &request).unwrap();
  for _ in 1..EXPERIMENT_LIMIT {
    store.save_experiment(selection(vec![], vec![])).unwrap();
  }
  store
    .validate_experiment_intent("project", &request)
    .unwrap();
  assert_eq!(
    store
      .validate_experiment_intent("project", &new("another-launch"))
      .unwrap_err()
      .status,
    409
  );
  let conflicting = intent(
    "queued-launch",
    ExperimentSelection::New {
      name: "Changed".into(),
    },
  );
  assert_eq!(
    store
      .validate_experiment_intent("project", &conflicting)
      .unwrap_err()
      .status,
    409
  );
  let child = scope("other", "second-attempt");
  document(&store, &child);
  store.assign_run_experiment(&child, &request).unwrap();
  assert_eq!(
    store.experiments("project").unwrap()["experiments"]
      .as_array()
      .unwrap()
      .len(),
    EXPERIMENT_LIMIT
  );
}
