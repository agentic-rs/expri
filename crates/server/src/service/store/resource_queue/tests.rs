use super::*;
use crate::service::store::tests::MockStorage;

fn resources(cpus: u32, memory_bytes: u64, gpus: u32) -> ResourceRequest {
  ResourceRequest {
    cpus,
    memory_bytes,
    gpus,
    gpu_ids: Vec::new(),
    gpu_model: None,
    gpu_memory_bytes: None,
  }
}

fn capacity() -> WorkerCapacity {
  WorkerCapacity {
    cpus: 4,
    memory_bytes: 8_000,
    gpus: vec![WorkerGpu {
      device_id: "GPU-one".into(),
      model: "NVIDIA A100".into(),
      memory_bytes: Some(80_000),
      vendor: GpuVendor::Nvidia,
      selector: Some("0".into()),
    }],
  }
}

fn submission(id: &str, resources: ResourceRequest) -> JobSubmission {
  let sha256 = "a".repeat(64);
  JobSubmission {
    submission_id: id.into(),
    project_id: "project".into(),
    task: "train".into(),
    command: vec!["python".into(), "train.py".into()],
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
    resources,
    worker_id: None,
    source: SourceRef {
      input_id: format!("queue-source-{sha256}"),
      size: 100,
      sha256,
    },
  }
}

fn source(store: &Store<MockStorage>, submission: &JobSubmission) {
  let target = FileTarget::Input {
    project_id: submission.project_id.clone(),
    input_id: submission.source.input_id.clone(),
  };
  let record = FileRecord {
    target: target.clone(),
    size: submission.source.size,
    sha256: Some(submission.source.sha256.clone()),
    storage: FileStorage::Object,
    provenance: None,
  };
  store.db().unwrap().execute("INSERT OR IGNORE INTO files(target,record,object_key,sequence) VALUES(?1,?2,'pinned-source-object',1)", params![target_json(&target).unwrap(), encode(&record).unwrap()]).unwrap();
}

fn register_at(
  store: &Store<MockStorage>,
  worker_id: &str,
  session_id: &str,
  now: i64,
) -> (WorkerSummary, Vec<AttemptSummary>) {
  let QueueResponse::Worker { worker, attempts } = store
    .resource_queue_at(
      QueueRequest::RegisterWorker {
        worker_id: worker_id.into(),
        session_id: session_id.into(),
        project_ids: vec!["project".into()],
        capacity: capacity(),
      },
      now,
    )
    .unwrap()
  else {
    panic!("expected worker")
  };
  (worker, attempts)
}

fn submit_at(store: &Store<MockStorage>, submission: JobSubmission, now: i64) -> JobSummary {
  source(store, &submission);
  let QueueResponse::Job { job } = store
    .resource_queue_at(
      QueueRequest::Submit {
        submission: Box::new(submission),
      },
      now,
    )
    .unwrap()
  else {
    panic!("expected job")
  };
  *job
}

fn claim_at(
  store: &Store<MockStorage>,
  worker_id: &str,
  session_id: &str,
  claim_id: &str,
  now: i64,
) -> Option<JobAssignment> {
  let QueueResponse::Claimed { assignment } = store
    .resource_queue_at(
      QueueRequest::Claim {
        worker_id: worker_id.into(),
        session_id: session_id.into(),
        claim_id: claim_id.into(),
      },
      now,
    )
    .unwrap()
  else {
    panic!("expected claim")
  };
  assignment.map(|assignment| *assignment)
}

#[test]
fn submission_and_claim_retries_do_not_duplicate_jobs_or_reservations() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let original = submission("submission-one", resources(2, 4_000, 1));
  let first = submit_at(&store, original.clone(), 1_000);
  let retry = submit_at(&store, original.clone(), 1_001);
  assert_eq!(first.job_id, retry.job_id);
  let mut conflict = original;
  conflict.command.push("different".into());
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Submit {
          submission: Box::new(conflict)
        },
        1_002
      )
      .unwrap_err()
      .status,
    409
  );
  register_at(&store, "worker-one", "session-one", 1_003);
  let first = claim_at(&store, "worker-one", "session-one", "claim-one", 1_004).unwrap();
  let retry = claim_at(&store, "worker-one", "session-one", "claim-one", 1_005).unwrap();
  assert_eq!(first.attempt, retry.attempt);
  assert_eq!(first.attempt.allocation.gpus[0].device_id, "GPU-one");
  assert_eq!(
    outstanding(&store.db().unwrap(), "worker-one")
      .unwrap()
      .len(),
    1
  );
}

#[test]
fn oldest_fitting_job_obeys_cpu_memory_gpu_and_worker_pins() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker-one", "session-one", 1_000);
  let oversized = submit_at(
    &store,
    submission("oversized", resources(5, 1_000, 0)),
    1_001,
  );
  let mut pinned = submission("pinned", resources(1, 1_000, 0));
  pinned.worker_id = Some("worker-two".into());
  submit_at(&store, pinned, 1_002);
  let gpu = submit_at(&store, submission("gpu", resources(2, 4_000, 1)), 1_003);
  let another_gpu = submit_at(
    &store,
    submission("another-gpu", resources(1, 1_000, 1)),
    1_004,
  );
  let cpu = submit_at(&store, submission("cpu", resources(2, 4_000, 0)), 1_005);
  let claimed = claim_at(&store, "worker-one", "session-one", "claim-one", 1_006).unwrap();
  assert_eq!(claimed.job.job_id, gpu.job_id);
  assert_eq!(
    claim_at(&store, "worker-one", "session-one", "claim-two", 1_007)
      .unwrap()
      .job
      .job_id,
    cpu.job_id
  );
  assert!(claim_at(&store, "worker-one", "session-one", "claim-three", 1_008).is_none());
  let db = store.db().unwrap();
  assert_eq!(
    job(&db, &oversized.job_id, 1_009).unwrap().status,
    JobStatus::Queued
  );
  assert_eq!(
    job(&db, &another_gpu.job_id, 1_009).unwrap().status,
    JobStatus::Queued
  );
}

#[test]
fn lease_loss_survives_restart_and_requires_recovery_before_new_claims() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker-one", "session-one", 1_000);
  submit_at(&store, submission("first", resources(4, 8_000, 1)), 1_001);
  let first = claim_at(&store, "worker-one", "session-one", "claim-one", 1_002).unwrap();
  submit_at(&store, submission("second", resources(4, 8_000, 1)), 1_003);
  let expired = 1_000 + QUEUE_LEASE_MS;
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Heartbeat {
          worker_id: "worker-one".into(),
          session_id: "session-one".into()
        },
        expired
      )
      .unwrap_err()
      .status,
    409
  );
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let (worker, attempts) = register_at(&store, "worker-one", "session-two", expired + 1);
  assert!(worker.quarantined);
  assert_eq!(attempts[0].status, AttemptStatus::Unknown);
  assert_eq!(attempts[0].allocation, first.attempt.allocation);
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Claim {
          worker_id: "worker-one".into(),
          session_id: "session-two".into(),
          claim_id: "claim-two".into()
        },
        expired + 2
      )
      .unwrap_err()
      .status,
    409
  );
  let mut current = first.attempt.identity();
  current.session_id = "session-two".into();
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Start {
          attempt: current,
          run_id: "run-one".into()
        },
        expired + 3
      )
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Recover {
          attempt_id: first.attempt.attempt_id.clone(),
          confirmed_stopped: false
        },
        expired + 4
      )
      .unwrap_err()
      .status,
    400
  );
  let recovered = store
    .resource_queue_at(
      QueueRequest::Recover {
        attempt_id: first.attempt.attempt_id,
        confirmed_stopped: true,
      },
      expired + 5,
    )
    .unwrap();
  assert!(matches!(
    recovered,
    QueueResponse::Attempt {
      attempt: AttemptSummary {
        status: AttemptStatus::Lost,
        ..
      }
    }
  ));
  assert!(
    claim_at(
      &store,
      "worker-one",
      "session-two",
      "claim-three",
      expired + 6
    )
    .is_some()
  );
}

#[test]
fn known_running_supervisor_adoption_rotates_fence_and_retries_are_safe() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker-one", "session-one", 1_000);
  submit_at(&store, submission("first", resources(2, 4_000, 1)), 1_001);
  let claimed = claim_at(&store, "worker-one", "session-one", "claim-one", 1_002).unwrap();
  store
    .resource_queue_at(
      QueueRequest::Start {
        attempt: claimed.attempt.identity(),
        run_id: "run-one".into(),
      },
      1_003,
    )
    .unwrap();
  let expired = 1_000 + QUEUE_LEASE_MS;
  let (_, attempts) = register_at(&store, "worker-one", "session-two", expired);
  let mut identity = attempts[0].identity();
  identity.session_id = "session-two".into();
  let start = QueueRequest::Start {
    attempt: identity.clone(),
    run_id: "run-one".into(),
  };
  let QueueResponse::Attempt { attempt: adopted } =
    store.resource_queue_at(start.clone(), expired + 1).unwrap()
  else {
    panic!("expected attempt")
  };
  assert_ne!(adopted.fencing_token, identity.fencing_token);
  let QueueResponse::Attempt { attempt: retried } =
    store.resource_queue_at(start, expired + 2).unwrap()
  else {
    panic!("expected attempt")
  };
  assert_eq!(retried.fencing_token, adopted.fencing_token);
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Finish {
          attempt: claimed.attempt.identity(),
          status: AttemptTerminalStatus::Completed,
          exit_code: Some(0),
          message: None
        },
        expired + 3
      )
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Finish {
          attempt: identity,
          status: AttemptTerminalStatus::Completed,
          exit_code: Some(0),
          message: None
        },
        expired + 3
      )
      .unwrap_err()
      .status,
    409
  );
  store
    .resource_queue_at(
      QueueRequest::Finish {
        attempt: adopted.identity(),
        status: AttemptTerminalStatus::Completed,
        exit_code: Some(0),
        message: None,
      },
      expired + 4,
    )
    .unwrap();
  assert!(
    outstanding(&store.db().unwrap(), "worker-one")
      .unwrap()
      .is_empty()
  );
}

#[test]
fn current_session_can_reconcile_only_its_already_linked_supervisor() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker-one", "session-one", 1_000);
  submit_at(&store, submission("first", resources(2, 4_000, 1)), 1_001);
  let claimed = claim_at(&store, "worker-one", "session-one", "claim-one", 1_002).unwrap();
  let identity = claimed.attempt.identity();
  store
    .resource_queue_at(
      QueueRequest::Start {
        attempt: identity.clone(),
        run_id: "run-one".into(),
      },
      1_003,
    )
    .unwrap();
  store
    .resource_queue_at(
      QueueRequest::ReportUncertain {
        attempt: identity.clone(),
        message: "journal verification was interrupted".into(),
      },
      1_004,
    )
    .unwrap();
  assert!(
    worker(&store.db().unwrap(), "worker-one")
      .unwrap()
      .quarantined
  );
  assert_eq!(
    outstanding(&store.db().unwrap(), "worker-one")
      .unwrap()
      .len(),
    1
  );
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Finish {
          attempt: identity.clone(),
          status: AttemptTerminalStatus::Completed,
          exit_code: Some(0),
          message: None
        },
        1_005
      )
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Start {
          attempt: identity.clone(),
          run_id: "different-run".into()
        },
        1_006
      )
      .unwrap_err()
      .status,
    409
  );
  let start = QueueRequest::Start {
    attempt: identity.clone(),
    run_id: "run-one".into(),
  };
  let QueueResponse::Attempt { attempt: adopted } =
    store.resource_queue_at(start.clone(), 1_007).unwrap()
  else {
    panic!("expected adopted attempt")
  };
  assert_ne!(adopted.fencing_token, identity.fencing_token);
  let QueueResponse::Attempt { attempt: retry } = store.resource_queue_at(start, 1_008).unwrap()
  else {
    panic!("expected adoption retry")
  };
  assert_eq!(retry.fencing_token, adopted.fencing_token);
  assert!(
    !worker(&store.db().unwrap(), "worker-one")
      .unwrap()
      .quarantined
  );
}

#[test]
fn cuda_placement_requires_nvidia_even_when_another_vendor_matches_the_model() {
  let request = resources(1, 1_000, 1);
  let mut capacity = capacity();
  capacity.gpus[0].vendor = GpuVendor::Amd;
  assert!(allocate(&capacity, &request, &[], true).is_none());
  assert!(allocate(&capacity, &request, &[], false).is_some());
  capacity.gpus[0].vendor = GpuVendor::Nvidia;
  assert!(allocate(&capacity, &request, &[], true).is_some());
}

#[test]
fn cancel_running_job_keeps_reservation_until_terminal_acknowledgement() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker-one", "session-one", 1_000);
  let queued = submit_at(&store, submission("queued", resources(1, 1_000, 0)), 1_001);
  let queued_cancel = store
    .resource_queue_at(
      QueueRequest::Cancel {
        job_id: queued.job_id,
      },
      1_002,
    )
    .unwrap();
  assert!(matches!(
    queued_cancel,
    QueueResponse::Job { job } if job.status == JobStatus::Cancelled
  ));
  let running = submit_at(&store, submission("running", resources(4, 8_000, 1)), 1_003);
  let claimed = claim_at(&store, "worker-one", "session-one", "claim", 1_004).unwrap();
  store
    .resource_queue_at(
      QueueRequest::Start {
        attempt: claimed.attempt.identity(),
        run_id: "run-one".into(),
      },
      1_005,
    )
    .unwrap();
  store
    .resource_queue_at(
      QueueRequest::Cancel {
        job_id: running.job_id,
      },
      1_006,
    )
    .unwrap();
  let attempts = outstanding(&store.db().unwrap(), "worker-one").unwrap();
  assert!(attempts[0].cancel_requested);
  assert_eq!(attempts[0].allocation.cpus, 4);
  store
    .resource_queue_at(
      QueueRequest::Finish {
        attempt: claimed.attempt.identity(),
        status: AttemptTerminalStatus::Cancelled,
        exit_code: Some(130),
        message: None,
      },
      1_007,
    )
    .unwrap();
  assert!(
    outstanding(&store.db().unwrap(), "worker-one")
      .unwrap()
      .is_empty()
  );
}

#[test]
fn source_url_uses_pinned_object_and_project_deletion_retains_active_ledger() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let now = now_ms();
  register_at(&store, "worker-one", "session-one", now);
  let submitted = submit_at(&store, submission("first", resources(4, 8_000, 1)), now + 1);
  let claimed = claim_at(&store, "worker-one", "session-one", "claim", now + 2).unwrap();
  store
    .db()
    .unwrap()
    .execute("UPDATE files SET object_key='replacement-object'", [])
    .unwrap();
  let QueueResponse::SourceUrl { url, .. } = store
    .resource_queue_at(
      QueueRequest::SourceUrl {
        attempt: claimed.attempt.identity(),
      },
      now + 3,
    )
    .unwrap()
  else {
    panic!("expected source URL")
  };
  assert!(url.ends_with("/pinned-source-object"));
  let queued = submit_at(
    &store,
    submission("second", resources(1, 1_000, 0)),
    now + 4,
  );
  let preview = store.preview_project_delete("project").unwrap();
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  let db = store.db().unwrap();
  assert_eq!(
    job(&db, &queued.job_id, now + 5).unwrap().status,
    JobStatus::Cancelled
  );
  assert_eq!(
    job(&db, &submitted.job_id, now + 5).unwrap().status,
    JobStatus::CancelRequested
  );
  assert!(outstanding(&db, "worker-one").unwrap()[0].cancel_requested);
  drop(db);
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::SourceUrl {
          attempt: claimed.attempt.identity()
        },
        now + 6
      )
      .unwrap_err()
      .status,
    409
  );
  store
    .resource_queue_at(
      QueueRequest::Finish {
        attempt: claimed.attempt.identity(),
        status: AttemptTerminalStatus::Cancelled,
        exit_code: Some(130),
        message: None,
      },
      now + 7,
    )
    .unwrap();
  assert!(
    outstanding(&store.db().unwrap(), "worker-one")
      .unwrap()
      .is_empty()
  );
}

#[test]
fn permanent_project_deletion_erases_stopped_payloads_and_active_payloads_after_ack() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let now = now_ms() - 1_000;
  let private_spec = |id: &str, resources| {
    let mut spec = submission(id, resources);
    spec.task = "private-task-marker".into();
    spec.command.push("private-command-marker".into());
    spec.environment.env.insert(
      "PRIVATE_TEST_SECRET".into(),
      "private-environment-marker".into(),
    );
    spec
  };
  register_at(&store, "worker-one", "session-one", now);
  let terminal = submit_at(
    &store,
    private_spec("terminal", resources(1, 1_000, 0)),
    now + 1,
  );
  let terminal_attempt = claim_at(
    &store,
    "worker-one",
    "session-one",
    "terminal-claim",
    now + 2,
  )
  .unwrap();
  store
    .resource_queue_at(
      QueueRequest::Finish {
        attempt: terminal_attempt.attempt.identity(),
        status: AttemptTerminalStatus::Failed,
        exit_code: Some(1),
        message: Some("private-command-marker private-environment-marker".into()),
      },
      now + 3,
    )
    .unwrap();
  let active_spec = private_spec("active", resources(4, 8_000, 1));
  let active = submit_at(&store, active_spec.clone(), now + 4);
  let active_attempt =
    claim_at(&store, "worker-one", "session-one", "active-claim", now + 5).unwrap();
  store
    .resource_queue_at(
      QueueRequest::Start {
        attempt: active_attempt.attempt.identity(),
        run_id: "run-active".into(),
      },
      now + 6,
    )
    .unwrap();
  let queued = submit_at(
    &store,
    private_spec("queued", resources(1, 1_000, 0)),
    now + 7,
  );
  let preview = store.preview_project_delete("project").unwrap();
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  let after = now_ms();
  let payload = |job_id: &str| {
    store
      .db()
      .unwrap()
      .query_row(
        "SELECT spec,source_key,record FROM queue_jobs WHERE job_id=?1",
        [job_id],
        |row| {
          Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
          ))
        },
      )
      .unwrap()
  };
  for stopped in [&terminal, &queued] {
    let (spec, key, record) = payload(&stopped.job_id);
    assert!(spec.is_empty());
    assert!(key.is_empty());
    assert!(!record.contains("private-task-marker"));
  }
  let (spec, key, _) = payload(&active.job_id);
  assert!(spec.contains("private-command-marker"));
  assert!(spec.contains("private-environment-marker"));
  assert!(!key.is_empty());
  assert_eq!(
    outstanding(&store.db().unwrap(), "worker-one")
      .unwrap()
      .len(),
    1
  );
  assert!(claim_at(&store, "worker-one", "session-one", "active-claim", after).is_none());
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::SourceUrl {
          attempt: active_attempt.attempt.identity()
        },
        after
      )
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Submit {
          submission: Box::new(active_spec)
        },
        after
      )
      .unwrap_err()
      .status,
    410
  );
  let completion = QueueRequest::Finish {
    attempt: active_attempt.attempt.identity(),
    status: AttemptTerminalStatus::Cancelled,
    exit_code: Some(130),
    message: Some("private-command-marker private-environment-marker".into()),
  };
  store
    .resource_queue_at(completion.clone(), after + 1)
    .unwrap();
  store.resource_queue_at(completion, after + 2).unwrap();
  let db = store.db().unwrap();
  assert_eq!(db.query_row("SELECT COUNT(*) FROM queue_jobs WHERE spec<>'' OR source_key<>'' OR record LIKE '%private-%'", [], |row| row.get::<_, usize>(0)).unwrap(), 0);
  assert_eq!(db.query_row("SELECT COUNT(*) FROM queue_attempts WHERE record LIKE '%private-command-marker%' OR record LIKE '%private-environment-marker%'", [], |row| row.get::<_, usize>(0)).unwrap(), 0);
  assert!(outstanding(&db, "worker-one").unwrap().is_empty());
}

#[test]
fn project_deletion_erases_uncertain_payload_only_after_confirmed_recovery() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let now = now_ms() - 1_000;
  register_at(&store, "worker-one", "session-one", now);
  let mut spec = submission("uncertain", resources(4, 8_000, 1));
  spec.environment.env.insert(
    "PRIVATE_TEST_SECRET".into(),
    "private-environment-marker".into(),
  );
  submit_at(&store, spec, now + 1);
  let active = claim_at(&store, "worker-one", "session-one", "active-claim", now + 2).unwrap();
  store
    .resource_queue_at(
      QueueRequest::ReportUncertain {
        attempt: active.attempt.identity(),
        message: "supervisor cannot be verified".into(),
      },
      now + 3,
    )
    .unwrap();
  let preview = store.preview_project_delete("project").unwrap();
  store
    .delete_project("project", &preview.revision, "project")
    .unwrap();
  assert_eq!(
    outstanding(&store.db().unwrap(), "worker-one")
      .unwrap()
      .len(),
    1
  );
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row(
        "SELECT COUNT(*) FROM queue_jobs WHERE spec LIKE '%private-environment-marker%'",
        [],
        |row| row.get::<_, usize>(0)
      )
      .unwrap(),
    1
  );
  store
    .resource_queue_at(
      QueueRequest::Recover {
        attempt_id: active.attempt.attempt_id,
        confirmed_stopped: true,
      },
      now_ms(),
    )
    .unwrap();
  let db = store.db().unwrap();
  assert_eq!(
    db.query_row(
      "SELECT COUNT(*) FROM queue_jobs WHERE spec<>'' OR source_key<>''",
      [],
      |row| row.get::<_, usize>(0)
    )
    .unwrap(),
    0
  );
  assert!(outstanding(&db, "worker-one").unwrap().is_empty());
}

#[test]
fn large_job_and_worker_pages_fit_the_transport_and_do_not_skip_entries() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let gpu_ids: Vec<String> = (0..64)
    .map(|id| format!("{}{:03}", "\"".repeat(253), id))
    .collect();
  let gpus: Vec<WorkerGpu> = gpu_ids
    .iter()
    .map(|device_id| WorkerGpu {
      device_id: device_id.clone(),
      model: "\"".repeat(256),
      memory_bytes: Some(i64::MAX as u64),
      vendor: GpuVendor::Nvidia,
      selector: Some(device_id.clone()),
    })
    .collect();
  let large_capacity = WorkerCapacity {
    cpus: 128,
    memory_bytes: 128,
    gpus,
  };
  large_capacity.validate().unwrap();
  let mut project_ids = vec!["project".to_string()];
  for id in 1..100 {
    let prefix = format!("project-{id:03}");
    project_ids.push(format!("{prefix}{}", "x".repeat(96 - prefix.len())));
  }
  let mut expected_workers = Vec::new();
  let mut expected_jobs = Vec::new();
  for id in 0..12 {
    let prefix = format!("worker-page-{id:03}");
    let worker_id = format!("{prefix}{}", "x".repeat(96 - prefix.len()));
    expected_workers.push(worker_id.clone());
    store
      .resource_queue_at(
        QueueRequest::RegisterWorker {
          worker_id: worker_id.clone(),
          session_id: "s".repeat(96),
          project_ids: project_ids.clone(),
          capacity: large_capacity.clone(),
        },
        1_000,
      )
      .unwrap();
    let mut spec = submission(
      &format!("submission-page-{id}"),
      ResourceRequest {
        cpus: 1,
        memory_bytes: 1,
        gpus: 64,
        gpu_ids: gpu_ids.clone(),
        gpu_model: None,
        gpu_memory_bytes: None,
      },
    );
    spec.worker_id = Some(worker_id.clone());
    spec.task = "\"".repeat(256);
    spec.validate().unwrap();
    let submitted = submit_at(&store, spec, 1_001);
    expected_jobs.push(submitted.job_id);
    claim_at(
      &store,
      &worker_id,
      &"s".repeat(96),
      &format!("claim-page-{id}"),
      1_002,
    )
    .unwrap();
  }
  expected_jobs.sort();
  expected_workers.sort();
  let mut cursor = None;
  let mut seen_jobs = Vec::new();
  let mut job_pages = 0;
  loop {
    let request = QueueRequest::ListJobs {
      project_id: None,
      status: None,
      cursor: cursor.clone(),
      limit: 100,
    };
    let response = store.resource_queue_at(request.clone(), 1_003).unwrap();
    response.validate_for(&request).unwrap();
    let encoded = serde_json::to_vec(&Response::Queue { response }).unwrap();
    assert!(encoded.len() < MAX_REQUEST);
    let Response::Queue {
      response: QueueResponse::Jobs { jobs, next_cursor },
    } = serde_json::from_slice(&encoded).unwrap()
    else {
      panic!("expected job page")
    };
    assert!(!jobs.is_empty());
    seen_jobs.extend(jobs.into_iter().map(|job| job.job_id));
    job_pages += 1;
    cursor = next_cursor;
    if cursor.is_none() {
      break;
    }
  }
  assert!(
    job_pages > 1,
    "large job summaries must split before the count limit"
  );
  assert_eq!(seen_jobs, expected_jobs);
  let mut cursor = None;
  let mut seen_workers = Vec::new();
  let mut worker_pages = 0;
  loop {
    let request = QueueRequest::ListWorkers {
      cursor: cursor.clone(),
      limit: 100,
    };
    let response = store.resource_queue_at(request.clone(), 1_003).unwrap();
    response.validate_for(&request).unwrap();
    let encoded = serde_json::to_vec(&Response::Queue { response }).unwrap();
    assert!(encoded.len() < MAX_REQUEST);
    let Response::Queue {
      response: QueueResponse::Workers {
        workers,
        next_cursor,
      },
    } = serde_json::from_slice(&encoded).unwrap()
    else {
      panic!("expected worker page")
    };
    assert!(!workers.is_empty());
    seen_workers.extend(workers.into_iter().map(|worker| worker.worker_id));
    worker_pages += 1;
    cursor = next_cursor;
    if cursor.is_none() {
      break;
    }
  }
  assert!(
    worker_pages > 1,
    "large worker metadata must split before the count limit"
  );
  assert_eq!(seen_workers, expected_workers);
}

#[test]
fn production_claim_checks_lease_after_waiting_for_the_metadata_lock() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let now = now_ms();
  register_at(&store, "worker-one", "session-one", now);
  submit_at(&store, submission("waiting", resources(1, 1_000, 0)), now);
  let db = store.db().unwrap();
  std::thread::scope(|scope| {
    let (entered, waiting) = std::sync::mpsc::channel();
    let store_ref = &store;
    let blocked = scope.spawn(move || {
      entered.send(()).unwrap();
      store_ref.resource_queue(QueueRequest::Claim {
        worker_id: "worker-one".into(),
        session_id: "session-one".into(),
        claim_id: "blocked-claim".into(),
      })
    });
    waiting.recv().unwrap();
    // The request has arrived, but its authoritative lease decision must wait
    // for this transaction and use the clock after the lock becomes available.
    std::thread::sleep(std::time::Duration::from_millis(100));
    db.execute(
      "UPDATE queue_workers SET lease_expires_at_ms=?1 WHERE worker_id='worker-one'",
      [now_ms() - 1],
    )
    .unwrap();
    drop(db);
    assert_eq!(blocked.join().unwrap().unwrap_err().status, 409);
  });
  assert!(
    outstanding(&store.db().unwrap(), "worker-one")
      .unwrap()
      .is_empty()
  );
}

#[test]
fn oversized_page_item_fails_clearly_instead_of_returning_an_empty_cursor_loop() {
  let error = page(vec!["item".into()], 1, |_| Ok("x".repeat(PAGE_ITEM_BYTES))).unwrap_err();
  assert_eq!(error.status, 413);
  assert!(error.message.contains("queue item exceeds"));
}

#[test]
fn source_mismatch_rolls_back_and_schema_seven_upgrade_preserves_records() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("metadata.sqlite3");
  let db = Connection::open(&path).unwrap();
  db.execute_batch("CREATE TABLE preservation_marker(value TEXT); INSERT INTO preservation_marker VALUES('retained'); PRAGMA user_version=7;").unwrap();
  drop(db);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert_eq!(
    store
      .db()
      .unwrap()
      .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
      .unwrap(),
    8
  );
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row("SELECT value FROM preservation_marker", [], |row| row
        .get::<_, String>(0))
      .unwrap(),
    "retained"
  );
  let mut submitted = submission("wrong", resources(1, 1_000, 0));
  source(&store, &submitted);
  submitted.source.size += 1;
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::Submit {
          submission: Box::new(submitted)
        },
        1_000
      )
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row("SELECT COUNT(*) FROM queue_jobs", [], |row| row
        .get::<_, usize>(0))
      .unwrap(),
    0
  );
}
