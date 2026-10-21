use super::*;
use crate::service::store::tests::MockStorage;

mod grouped;

fn scope(origin: &str, run_id: &str) -> RunScope {
  RunScope {
    project_id: "project".into(),
    origin: origin.into(),
    run_id: run_id.into(),
  }
}

fn document(store: &Store<MockStorage>, scope: &RunScope) {
  let bytes = serde_json::to_vec(&json!({"schema_version":1,"run_id":scope.run_id,"task":"train","status":"completed","started_at":"2026-10-20T00:00:00Z","finished_at":"2026-10-20T00:01:00Z","exit_code":0})).unwrap();
  store
    .execute(Request::PutDocument {
      scope: scope.clone(),
      path: "run-state.json".into(),
      revision: 1,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

fn selection(members: Vec<RunScope>, lineage: Vec<Lineage>) -> ExperimentSave {
  static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
  ExperimentSave {
    project_id: "project".into(),
    experiment_id: None,
    expected_revision: None,
    create_request_id: Some(format!(
      "create-{}",
      NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )),
    name: "  scaling  ".into(),
    description: "  Two workers\nSame goal.  ".into(),
    members,
    lineage,
  }
}

fn edit(experiment: &Experiment) -> ExperimentSave {
  ExperimentSave {
    project_id: "project".into(),
    experiment_id: Some(experiment.experiment_id.clone()),
    expected_revision: Some(experiment.revision.clone()),
    create_request_id: None,
    name: experiment.name.clone(),
    description: experiment.description.clone(),
    members: experiment.members.clone(),
    lineage: experiment.lineage.clone(),
  }
}

fn edge(parent: &RunScope, child: &RunScope) -> Lineage {
  Lineage {
    parent: parent.clone(),
    child: child.clone(),
    kind: LineageKind::Continued,
    checkpoint: Some("outputs/checkpoint.pt".into()),
    step: Some("18446744073709551615".into()),
  }
}

#[test]
fn full_run_scopes_lineage_and_worker_bytes_survive_reopen() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  let a = scope("worker-a", "same-run");
  let b = scope("worker-b", "same-run");
  document(&store, &a);
  document(&store, &b);
  let original = store
    .dashboard_artifact(&a, "run-state.json")
    .unwrap()
    .unwrap();
  let experiment = store
    .save_experiment(selection(vec![a.clone(), b.clone()], vec![edge(&a, &b)]))
    .unwrap();
  assert_eq!(experiment.name, "scaling");
  assert_eq!(experiment.membership_count, 2);
  assert_eq!(
    experiment.lineage[0].step.as_deref(),
    Some("18446744073709551615")
  );
  assert!(experiment.unavailable_runs.is_empty());
  assert_eq!(
    store.dashboard_run_experiment(&a).unwrap()["experiment_id"],
    experiment.experiment_id
  );
  assert_eq!(
    store.dashboard_run_experiment(&b).unwrap()["experiment_id"],
    experiment.experiment_id
  );
  let other = RunScope {
    project_id: "other".into(),
    ..a.clone()
  };
  document(&store, &other);
  assert!(store.dashboard_run_experiment(&other).unwrap().is_null());
  assert_eq!(
    serde_json::to_value(
      store
        .dashboard_artifact(&a, "run-state.json")
        .unwrap()
        .unwrap()
    )
    .unwrap(),
    serde_json::to_value(original).unwrap()
  );
  drop(store);
  let reopened = Store::open(directory.path(), storage).unwrap();
  let snapshot = reopened.experiments("project").unwrap();
  assert_eq!(
    snapshot["experiments"][0],
    serde_json::to_value(experiment).unwrap()
  );
  assert_eq!(
    reopened
      .db()
      .unwrap()
      .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
      .unwrap(),
    METADATA_SCHEMA_VERSION
  );
}

#[test]
fn schema_eleven_migrates_experiments_without_changing_runs_or_annotations() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  let selected = scope("worker-a", "existing-run");
  document(&store, &selected);
  let annotations = store
    .set_run_annotations(
      &selected,
      Some("Existing baseline".into()),
      vec!["keep".into(), "history".into()],
    )
    .unwrap();
  let original = serde_json::to_value(
    store
      .dashboard_artifact(&selected, "run-state.json")
      .unwrap()
      .unwrap(),
  )
  .unwrap();
  store
    .db()
    .unwrap()
    .execute_batch(
      "DROP TABLE experiment_members;
      DROP TABLE experiments;
      DROP TABLE experiment_revisions;
      DROP TABLE experiment_creates;
      PRAGMA user_version=11;",
    )
    .unwrap();
  drop(store);

  let reopened = Store::open(directory.path(), storage).unwrap();
  assert_eq!(
    reopened
      .db()
      .unwrap()
      .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
      .unwrap(),
    METADATA_SCHEMA_VERSION
  );
  assert_eq!(
    serde_json::to_value(
      reopened
        .dashboard_artifact(&selected, "run-state.json")
        .unwrap()
        .unwrap(),
    )
    .unwrap(),
    original
  );
  assert_eq!(reopened.run_annotations(&selected).unwrap(), annotations);
  assert!(
    reopened.experiments("project").unwrap()["experiments"]
      .as_array()
      .unwrap()
      .is_empty()
  );
  let experiment = reopened
    .save_experiment(selection(vec![selected], vec![]))
    .unwrap();
  assert_eq!(
    reopened.experiments("project").unwrap()["experiments"][0],
    serde_json::to_value(experiment).unwrap()
  );
}

#[test]
fn membership_cycles_cross_scope_and_stale_revisions_are_atomic() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let a = scope("worker-a", "a");
  let b = scope("worker-b", "b");
  let c = scope("worker-a", "c");
  for scope in [&a, &b, &c] {
    document(&store, scope);
  }
  let experiment = store
    .save_experiment(selection(vec![a.clone(), b.clone()], vec![edge(&a, &b)]))
    .unwrap();
  let before = store.experiments("project").unwrap();
  let mut cycle = edit(&experiment);
  cycle.lineage.push(edge(&b, &a));
  assert_eq!(store.save_experiment(cycle).unwrap_err().status, 400);
  let mut dangling = edit(&experiment);
  dangling.members.pop();
  assert_eq!(store.save_experiment(dangling).unwrap_err().status, 400);
  let mut duplicate = edit(&experiment);
  duplicate.members.push(a.clone());
  assert_eq!(store.save_experiment(duplicate).unwrap_err().status, 400);
  let mut cross_project = edit(&experiment);
  cross_project.members.push(RunScope {
    project_id: "other".into(),
    ..c.clone()
  });
  assert_eq!(
    store.save_experiment(cross_project).unwrap_err().status,
    400
  );
  assert_eq!(
    store
      .save_experiment(selection(vec![scope("worker-a", "missing")], vec![]))
      .unwrap_err()
      .status,
    404
  );
  assert_eq!(
    store
      .save_experiment(selection(vec![a.clone()], vec![]))
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(before, store.experiments("project").unwrap());
  let mut changed = edit(&experiment);
  changed.name = "updated".into();
  let updated = store.save_experiment(changed).unwrap();
  assert_ne!(updated.revision, experiment.revision);
  assert_eq!(
    store.save_experiment(edit(&experiment)).unwrap_err().status,
    409
  );
  assert_eq!(
    store
      .delete_experiment("project", &experiment.experiment_id, &experiment.revision)
      .unwrap_err()
      .status,
    409
  );
  let mut removed = edit(&updated);
  removed.members = vec![b.clone()];
  removed.lineage.clear();
  store.save_experiment(removed).unwrap();
  assert!(store.dashboard_run_experiment(&a).unwrap().is_null());
  store.save_experiment(selection(vec![a], vec![])).unwrap();
}

#[test]
fn metadata_edits_refresh_cached_views_without_changing_metrics_and_delete_keeps_runs() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let a = scope("worker-a", "a");
  document(&store, &a);
  let updates = || {
    store
      .dashboard_project_updates("project", &[("worker-a:a".into(), a.clone())])
      .unwrap()
  };
  let before = updates();
  let preview = store.preview_project_delete("project").unwrap();
  let experiment = store
    .save_experiment(selection(vec![a.clone()], vec![]))
    .unwrap();
  let after = updates();
  for field in ["catalog_revision", "source_revision"] {
    assert_ne!(before[field], after[field]);
  }
  assert_ne!(
    before["runs"][0]["metadata_revision"],
    after["runs"][0]["metadata_revision"]
  );
  assert_eq!(
    before["runs"][0]["metrics_revision"],
    after["runs"][0]["metrics_revision"]
  );
  assert_ne!(
    preview.revision,
    store.preview_project_delete("project").unwrap().revision
  );
  assert_eq!(
    experiment,
    store.save_experiment(edit(&experiment)).unwrap()
  );
  assert_eq!(after, updates());
  let dashboard = crate::service::dashboard_data::HostedDashboard::new(&store).unwrap();
  let first = dashboard
    .list("hosted:project:worker-a", None, None, None, 20, 0)
    .unwrap();
  assert_eq!(first["runs"][0]["experiment"]["name"], "scaling");
  let mut changed = edit(&experiment);
  changed.name = "new name".into();
  let updated = store.save_experiment(changed).unwrap();
  assert_eq!(
    dashboard.detail("hosted:project:worker-a", "a").unwrap()["run"]["experiment"]["name"],
    "new name"
  );
  let second = dashboard
    .list("hosted:project:worker-a", None, None, None, 20, 0)
    .unwrap();
  assert_eq!(second["runs"][0]["experiment"]["name"], "new name");
  store
    .delete_experiment("project", &updated.experiment_id, &updated.revision)
    .unwrap();
  assert!(
    store
      .dashboard_artifact(&a, "run-state.json")
      .unwrap()
      .is_some()
  );
  assert!(store.dashboard_run_experiment(&a).unwrap().is_null());
}

#[test]
fn retained_unavailable_runs_keep_lineage_but_cannot_be_newly_assigned() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let a = scope("worker-a", "a");
  let b = scope("worker-b", "b");
  document(&store, &a);
  document(&store, &b);
  let experiment = store
    .save_experiment(selection(vec![a.clone(), b.clone()], vec![edge(&a, &b)]))
    .unwrap();
  store.archive_run(&a).unwrap();
  store
    .db()
    .unwrap()
    .execute(
      "UPDATE run_retention SET delete_after=0 WHERE project_id=?1 AND origin=?2 AND run_id=?3",
      params![a.project_id, a.origin, a.run_id],
    )
    .unwrap();
  let snapshot = store.experiments("project").unwrap();
  assert_eq!(
    snapshot["experiments"][0]["unavailable_runs"],
    json!([a.clone()])
  );
  assert_eq!(
    snapshot["experiments"][0]["lineage"],
    json!(experiment.lineage)
  );
  let mut changed = edit(&experiment);
  changed.description = "Still historical".into();
  assert_eq!(
    store.save_experiment(changed).unwrap().unavailable_runs,
    vec![a.clone()]
  );
  assert_eq!(
    store
      .save_experiment(selection(vec![a], vec![]))
      .unwrap_err()
      .status,
    404
  );
}

#[test]
fn selected_experiment_reads_old_members_before_recent_run_window_and_pagination() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let a = scope("worker-a", "a");
  let b = scope("worker-b", "a");
  document(&store, &a);
  document(&store, &b);
  let experiment = store
    .save_experiment(selection(vec![a.clone(), b.clone()], vec![]))
    .unwrap();
  {
    let mut db = store.db().unwrap();
    let tx = db.transaction().unwrap();
    for n in 0..501 {
      let scope = scope("worker-a", &format!("new-{n}"));
      let target = FileTarget::Run {
        scope: scope.clone(),
        path: "logs/stdout.log".into(),
      };
      tx.execute(
        "INSERT INTO streams(target,size) VALUES(?1,0)",
        [target_json(&target).unwrap()],
      )
      .unwrap();
      record_run_activity(&tx, &scope).unwrap();
    }
    tx.commit().unwrap();
  }
  let dashboard = crate::service::dashboard_data::HostedDashboard::new(&store).unwrap();
  let query = crate::dashboard::table::ListQuery {
    group_experiments: false,
    archived: false,
    origin: None,
    experiment_id: Some(&experiment.experiment_id),
    search: None,
    task: None,
    status: None,
    limit: 1,
    offset: 0,
    table: None,
  };
  let first = dashboard
    .list_table("hosted-project:project", &query)
    .unwrap();
  assert_eq!(first["total_count"], 2);
  assert_eq!(first["runs"].as_array().unwrap().len(), 1);
  let next_query = crate::dashboard::table::ListQuery { offset: 1, ..query };
  let next = dashboard
    .list_table("hosted-project:project", &next_query)
    .unwrap();
  assert_ne!(first["runs"][0]["origin"], next["runs"][0]["origin"]);
  assert!(first["warnings"].as_array().unwrap().iter().all(|warning| {
    !warning["message"]
      .as_str()
      .unwrap_or_default()
      .contains("500 runs")
  }));
}

#[test]
fn empty_experiments_remain_discoverable_and_project_deletion_cleans_metadata() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  store.save_experiment(selection(vec![], vec![])).unwrap();
  let dashboard = crate::service::dashboard_data::HostedDashboard::new(&store).unwrap();
  assert_eq!(
    crate::dashboard::DashboardView::projects(&dashboard)
      .unwrap()
      .unwrap()["sources"][0]["project_id"],
    "project"
  );
  assert_eq!(
    dashboard.catalog().unwrap()["sources"][0]["project_id"],
    "project"
  );
  assert!(
    store
      .dashboard_project_exists_without_runs("project")
      .unwrap()
  );
  let preview = store.preview_project_delete("project").unwrap();
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  assert_eq!(store.experiments("project").unwrap_err().status, 410);
  for table in [
    "experiments",
    "experiment_members",
    "experiment_revisions",
    "experiment_creates",
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
}

#[test]
fn lost_create_responses_replay_once_across_reopen_and_never_recreate_deleted_experiments() {
  let directory = tempfile::tempdir().unwrap();
  let storage = MockStorage::default();
  let store = Store::open(directory.path(), storage.clone()).unwrap();
  let a = scope("worker-a", "a");
  document(&store, &a);
  let request = selection(vec![a], vec![]);
  let experiment = store.save_experiment(request.clone()).unwrap();
  let before = store.experiments("project").unwrap();
  let revision = store.preview_project_delete("project").unwrap().revision;
  assert_eq!(store.save_experiment(request.clone()).unwrap(), experiment);
  assert_eq!(store.experiments("project").unwrap(), before);
  assert_eq!(
    store.preview_project_delete("project").unwrap().revision,
    revision
  );
  let mut changed = request.clone();
  changed.name = "Different request".into();
  assert_eq!(store.save_experiment(changed).unwrap_err().status, 409);
  drop(store);
  let store = Store::open(directory.path(), storage).unwrap();
  assert_eq!(store.save_experiment(request.clone()).unwrap(), experiment);
  let mut update = edit(&experiment);
  update.name = "Edited later".into();
  let updated = store.save_experiment(update).unwrap();
  assert_eq!(store.save_experiment(request.clone()).unwrap(), updated);
  let mut other_project = request.clone();
  other_project.project_id = "other-project".into();
  other_project.members.clear();
  let other = store.save_experiment(other_project).unwrap();
  assert_ne!(other.experiment_id, experiment.experiment_id);
  store
    .delete_experiment("project", &updated.experiment_id, &updated.revision)
    .unwrap();
  assert_eq!(store.save_experiment(request).unwrap_err().status, 410);
  assert!(
    store.experiments("project").unwrap()["experiments"]
      .as_array()
      .unwrap()
      .is_empty()
  );
}

#[test]
fn lineage_validation_preserves_checkpoint_provenance_and_exact_steps_for_all_relationships() {
  let a = scope("worker-a", "a");
  let b = scope("worker-b", "b");
  for step in ["-1", "1.0", "18446744073709551616", "", "١"] {
    let mut lineage = edge(&a, &b);
    lineage.step = Some(step.into());
    assert_eq!(
      normalize(selection(vec![a.clone(), b.clone()], vec![lineage]))
        .unwrap_err()
        .status,
      400
    );
  }
  for kind in [
    LineageKind::Continued,
    LineageKind::Restarted,
    LineageKind::Split,
  ] {
    let mut lineage = edge(&a, &b);
    lineage.kind = kind;
    let normalized =
      normalize(selection(vec![a.clone(), b.clone()], vec![lineage.clone()])).unwrap();
    assert_eq!(normalized.lineage[0], lineage);
  }
  let mut lineage = edge(&a, &b);
  lineage.step = Some("0001".into());
  assert_eq!(
    normalize(selection(vec![a, b], vec![lineage]))
      .unwrap()
      .lineage[0]
      .step
      .as_deref(),
    Some("1")
  );
}
