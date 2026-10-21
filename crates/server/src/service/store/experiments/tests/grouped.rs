use super::*;
use crate::dashboard::table::{ListQuery, TableOptions};
use crate::service::dashboard_data::HostedDashboard;

fn state(store: &Store<MockStorage>, scope: &RunScope, hour: u32, status: &str, task: &str) {
  let bytes = serde_json::to_vec(&json!({"schema_version":1,"run_id":scope.run_id,"task":task,"status":status,
    "started_at":format!("2026-10-20T{hour:02}:00:00Z"),"finished_at":if status == "completed" {Some("2026-10-21T00:00:00Z")} else {None},"exit_code":if status == "completed" {Some(0)} else {None}})).unwrap();
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

fn query() -> ListQuery<'static> {
  ListQuery {
    group_experiments: true,
    archived: false,
    origin: None,
    experiment_id: None,
    search: None,
    task: None,
    status: None,
    limit: 20,
    offset: 0,
    table: None,
  }
}

fn named(store: &Store<MockStorage>, name: &str, members: Vec<RunScope>) -> Experiment {
  let mut request = selection(members, vec![]);
  request.name = name.into();
  store.save_experiment(request).unwrap()
}

fn metrics(store: &Store<MockStorage>, scope: &RunScope, value: f64) {
  let bytes = format!("{}\n", json!({"step":1,"metrics":{"loss":value}}));
  store
    .execute(Request::AppendTracking {
      scope: scope.clone(),
      path: "outputs/metrics.jsonl".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

#[test]
fn grouped_pages_count_experiments_and_pick_latest_across_origins_before_metric_sort() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let a_old = scope("worker-a", "shared");
  let a_new = scope("worker-b", "shared");
  let a_middle = scope("worker-a", "middle");
  let b = scope("worker-a", "other");
  let standalone = scope("worker-a", "standalone");
  state(&store, &a_old, 1, "running", "train");
  state(&store, &a_middle, 2, "waiting", "train");
  state(&store, &a_new, 3, "completed", "train");
  state(&store, &b, 2, "paused", "other");
  state(&store, &standalone, 4, "completed", "standalone");
  metrics(&store, &a_old, 100.);
  metrics(&store, &a_middle, 90.);
  metrics(&store, &a_new, 1.);
  metrics(&store, &b, 10.);
  metrics(&store, &standalone, 0.);
  let experiment = named(&store, "A", vec![a_old.clone(), a_middle, a_new.clone()]);
  named(&store, "B", vec![b]);
  let dashboard = HostedDashboard::new(&store).unwrap();
  let options = TableOptions::parse(
    vec![],
    vec!["loss".into()],
    "last",
    Some("metric:loss"),
    Some("desc"),
  )
  .unwrap();
  let first_query = ListQuery {
    limit: 1,
    table: Some(&options),
    ..query()
  };
  let first = dashboard
    .list_table("hosted-project:project", &first_query)
    .unwrap();
  assert_eq!(first["total_count"], 3);
  assert_eq!(first["next_offset"], 1);
  assert_eq!(first["experiment_groups"][0]["experiment"]["name"], "B");
  let second_query = ListQuery {
    offset: 1,
    ..first_query
  };
  let second = dashboard
    .list_table("hosted-project:project", &second_query)
    .unwrap();
  let group = &second["experiment_groups"][0];
  assert_eq!(
    group["experiment"]["experiment_id"],
    experiment.experiment_id
  );
  assert_eq!(group["run_key"], "worker-b:shared");
  assert_eq!(group["membership_count"], 3);
  assert_eq!(group["matching_count"], 3);
  assert_eq!(group["active_count"], 2);
  assert_eq!(second["runs"][0]["table_values"]["metrics"]["loss"], 1.);
  let flat = dashboard
    .list_table(
      "hosted-project:project",
      &ListQuery {
        group_experiments: false,
        ..query()
      },
    )
    .unwrap();
  assert_eq!(flat["total_count"], 5);
  assert!(flat.get("experiment_groups").is_none());
  let history = dashboard
    .list_table(
      "hosted-project:project",
      &ListQuery {
        group_experiments: false,
        experiment_id: Some(&experiment.experiment_id),
        ..query()
      },
    )
    .unwrap();
  assert_eq!(history["total_count"], 3);
  assert_eq!(history["runs"][0]["run_key"], "worker-b:shared");
}

#[test]
fn member_filters_choose_latest_matching_run_and_leave_full_membership_counts() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let old = scope("worker-a", "old");
  let new = scope("worker-b", "new");
  state(&store, &old, 1, "running", "train");
  state(&store, &new, 2, "completed", "evaluate");
  let experiment = named(&store, "Work", vec![old.clone(), new]);
  named(&store, "Empty", vec![]);
  let dashboard = HostedDashboard::new(&store).unwrap();
  for selection in [
    ListQuery {
      status: Some("running"),
      ..query()
    },
    ListQuery {
      task: Some("train"),
      ..query()
    },
    ListQuery {
      search: Some("old"),
      ..query()
    },
    ListQuery {
      origin: Some("worker-a"),
      ..query()
    },
  ] {
    let page = dashboard
      .list_table("hosted-project:project", &selection)
      .unwrap();
    assert_eq!(page["total_count"], 1);
    assert_eq!(page["runs"][0]["run_key"], "worker-a:old");
    assert_eq!(page["experiment_groups"][0]["membership_count"], 2);
    assert_eq!(page["experiment_groups"][0]["matching_count"], 1);
    assert_eq!(page["experiment_groups"][0]["active_count"], 1);
  }
  let machine = dashboard
    .list_table("hosted:project:worker-a", &query())
    .unwrap();
  assert_eq!(machine["total_count"], 1);
  assert_eq!(machine["experiment_groups"][0]["run_key"], "old");
  assert_eq!(
    machine["experiment_groups"][0]["experiment"]["experiment_id"],
    experiment.experiment_id
  );
}

#[test]
fn empty_unavailable_and_archived_groups_keep_truthful_counts() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let current = scope("worker-a", "current");
  let archived = scope("worker-b", "archived");
  let expired = scope("worker-b", "expired");
  state(&store, &current, 1, "waiting", "train");
  state(&store, &archived, 2, "completed", "train");
  state(&store, &expired, 3, "completed", "train");
  named(&store, "Mixed", vec![current, archived.clone()]);
  named(&store, "Unavailable", vec![expired.clone()]);
  named(&store, "Empty", vec![]);
  store.archive_run(&archived).unwrap();
  store.archive_run(&expired).unwrap();
  store
    .db()
    .unwrap()
    .execute(
      "UPDATE run_retention SET delete_after=0 WHERE project_id=?1 AND origin=?2 AND run_id=?3",
      params![expired.project_id, expired.origin, expired.run_id],
    )
    .unwrap();
  let dashboard = HostedDashboard::new(&store).unwrap();
  let page = dashboard
    .list_table("hosted-project:project", &query())
    .unwrap();
  assert_eq!(page["total_count"], 3);
  assert_eq!(page["runs"].as_array().unwrap().len(), 1);
  assert_eq!(page["experiment_groups"][0]["run_key"], "worker-a:current");
  assert_eq!(page["experiment_groups"][0]["matching_count"], 1);
  let unavailable = page["experiment_groups"]
    .as_array()
    .unwrap()
    .iter()
    .find(|group| group["experiment"]["name"] == "Unavailable")
    .unwrap();
  assert!(unavailable["run_key"].is_null());
  assert_eq!(unavailable["membership_count"], 1);
  assert_eq!(unavailable["unavailable_count"], 1);
  let archived = dashboard
    .list_table(
      "hosted-project:project",
      &ListQuery {
        archived: true,
        ..query()
      },
    )
    .unwrap();
  assert_eq!(archived["total_count"], 1);
  assert_eq!(archived["runs"][0]["run_key"], "worker-b:archived");
  assert_eq!(archived["experiment_groups"][0]["membership_count"], 2);
}

#[test]
fn experiment_latest_member_is_not_limited_to_recent_five_hundred_run_activity() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let old = scope("worker-a", "old");
  let latest = scope("worker-b", "latest");
  state(&store, &old, 1, "completed", "train");
  state(&store, &latest, 9, "running", "train");
  let experiment = named(&store, "Full history", vec![old, latest]);
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
  let dashboard = HostedDashboard::new(&store).unwrap();
  let page = dashboard
    .list_table(
      "hosted-project:project",
      &ListQuery {
        limit: 1,
        ..query()
      },
    )
    .unwrap();
  assert_eq!(page["total_count"], 501);
  assert_eq!(
    page["experiment_groups"][0]["experiment"]["experiment_id"],
    experiment.experiment_id
  );
  assert_eq!(page["runs"][0]["run_key"], "worker-b:latest");
  assert_eq!(page["experiment_groups"][0]["matching_count"], 2);
  assert!(page["warnings"].as_array().unwrap().iter().any(|warning| {
    warning["message"]
      .as_str()
      .unwrap_or_default()
      .contains("ungrouped runs")
  }));
}

#[test]
fn incomplete_member_overview_fails_instead_of_claiming_an_arbitrary_latest_run() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let valid = scope("worker-a", "valid");
  let invalid = scope("worker-b", "invalid");
  state(&store, &valid, 1, "completed", "train");
  let bytes = br#"{"status":"running","task":"train"}"#;
  store
    .execute(Request::PutDocument {
      scope: invalid.clone(),
      path: "run-state.json".into(),
      revision: 1,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
  named(&store, "Incomplete", vec![valid, invalid]);
  let dashboard = HostedDashboard::new(&store).unwrap();
  assert!(
    dashboard
      .list_table("hosted-project:project", &query())
      .unwrap_err()
      .to_string()
      .contains("Cannot determine the latest")
  );
  assert_eq!(
    dashboard
      .list_table(
        "hosted-project:project",
        &ListQuery {
          group_experiments: false,
          ..query()
        }
      )
      .unwrap()["total_count"],
    2
  );
}

#[test]
fn grouped_overview_bound_rejects_incomplete_sampling_of_large_projects() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let mut scopes = Vec::new();
  {
    let mut db = store.db().unwrap();
    let tx = db.transaction().unwrap();
    for n in 0..2049 {
      let scope = scope("worker-a", &format!("run-{n}"));
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
      scopes.push(scope);
    }
    tx.commit().unwrap();
  }
  for (n, group) in scopes.chunks(256).enumerate() {
    named(&store, &format!("Group {n}"), group.to_vec());
  }
  let dashboard = HostedDashboard::new(&store).unwrap();
  assert!(
    dashboard
      .list_table("hosted-project:project", &query())
      .unwrap_err()
      .to_string()
      .contains("2048 run overviews")
  );
}

#[test]
fn queued_jobs_are_standalone_until_native_metadata_and_keep_context_when_grouped() {
  use crate::service::types::FileStorage;
  use expri_core::resource_queue::{
    JobSubmission, QueueRequest, QueueResponse, ResourceRequest, SourceRef, WorkerCapacity,
  };

  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let run = scope("worker-a", "running");
  state(&store, &run, 1, "running", "train");
  let experiment = named(&store, "Experiment", vec![run]);
  let source = SourceRef {
    input_id: format!("queue-source-{}", "a".repeat(64)),
    size: 100,
    sha256: "a".repeat(64),
  };
  let target = FileTarget::Input {
    project_id: "project".into(),
    input_id: source.input_id.clone(),
  };
  let record = FileRecord {
    target: target.clone(),
    size: source.size,
    sha256: Some(source.sha256.clone()),
    storage: FileStorage::Object,
    provenance: None,
  };
  store
    .db()
    .unwrap()
    .execute(
      "INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,'queue-source-object',1)",
      params![
        target_json(&target).unwrap(),
        serde_json::to_string(&record).unwrap()
      ],
    )
    .unwrap();
  let submission = JobSubmission {
    submission_id: "grouped-pending".into(),
    project_id: "project".into(),
    task: "queued-task".into(),
    command: vec!["echo".into(), "ok".into()],
    environment: expri_core::config::EnvironmentConfig {
      base_python: None,
      reuse_packages: Vec::new(),
      require_cuda: false,
      env: BTreeMap::new(),
    },
    extras: Vec::new(),
    sync_args: Vec::new(),
    remote_managed: Vec::new(),
    smoke: false,
    publish: false,
    resources: ResourceRequest {
      cpus: 1,
      memory_bytes: 1,
      gpus: 0,
      gpu_ids: Vec::new(),
      gpu_model: None,
      gpu_memory_bytes: None,
    },
    worker_id: None,
    source,
  };
  let QueueResponse::Job { job } = store
    .resource_queue(QueueRequest::Submit {
      submission: Box::new(submission),
    })
    .unwrap()
  else {
    panic!("expected pending job")
  };
  let dashboard = HostedDashboard::new(&store).unwrap();
  let page = dashboard
    .list_table("hosted-project:project", &query())
    .unwrap();
  assert_eq!(page["total_count"], 2);
  let queued = page["experiment_groups"]
    .as_array()
    .unwrap()
    .iter()
    .find(|entry| entry["experiment"].is_null())
    .unwrap();
  assert_eq!(queued["run_key"], format!("queue::{}", job.job_id));
  assert_eq!(queued["membership_count"], 1);
  assert_eq!(queued["matching_count"], 1);
  assert_eq!(queued["active_count"], 1);
  let ungrouped = dashboard
    .list_table(
      "hosted-project:project",
      &ListQuery {
        experiment_id: Some("ungrouped"),
        ..query()
      },
    )
    .unwrap();
  assert_eq!(ungrouped["total_count"], 1);
  assert_eq!(ungrouped["runs"][0]["queue_only"], true);
  store
    .resource_queue(QueueRequest::RegisterWorker {
      worker_id: "worker-a".into(),
      session_id: "grouped-session".into(),
      project_ids: vec!["project".into()],
      capacity: WorkerCapacity {
        cpus: 1,
        memory_bytes: 1024,
        gpus: Vec::new(),
      },
      gpu_availability: None,
    })
    .unwrap();
  let QueueResponse::Claimed {
    assignment: Some(assignment),
  } = store
    .resource_queue(QueueRequest::Claim {
      worker_id: "worker-a".into(),
      session_id: "grouped-session".into(),
      claim_id: "grouped-claim".into(),
    })
    .unwrap()
  else {
    panic!("expected assignment")
  };
  let native = scope("worker-a", "queued-native");
  store
    .resource_queue(QueueRequest::Start {
      attempt: assignment.attempt.identity(),
      run_id: native.run_id.clone(),
    })
    .unwrap();
  state(&store, &native, 2, "running", "queued-task");
  let mut changed = edit(&experiment);
  changed.members.push(native);
  store.save_experiment(changed).unwrap();
  let grouped = dashboard
    .list_table("hosted-project:project", &query())
    .unwrap();
  assert_eq!(grouped["total_count"], 1);
  assert_eq!(grouped["runs"][0]["run_key"], "worker-a:queued-native");
  assert_eq!(grouped["runs"][0]["queue"]["job_id"], job.job_id);
  assert_eq!(grouped["experiment_groups"][0]["matching_count"], 2);
  assert_eq!(grouped["experiment_groups"][0]["active_count"], 2);
  let flat = dashboard
    .list_table(
      "hosted-project:project",
      &ListQuery {
        group_experiments: false,
        ..query()
      },
    )
    .unwrap();
  assert_eq!(grouped["runs"][0]["queue"], flat["runs"][0]["queue"]);
}
