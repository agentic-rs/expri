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
  register_capacity_at(store, worker_id, session_id, capacity(), now)
}

fn register_capacity_at(
  store: &Store<MockStorage>,
  worker_id: &str,
  session_id: &str,
  capacity: WorkerCapacity,
  now: i64,
) -> (WorkerSummary, Vec<AttemptSummary>) {
  let QueueResponse::Worker { worker, attempts } = store
    .resource_queue_at(
      QueueRequest::RegisterWorker {
        worker_id: worker_id.into(),
        session_id: session_id.into(),
        project_ids: vec!["project".into()],
        capacity,
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
    JobStatus::Pending
  );
  assert_eq!(
    job(&db, &another_gpu.job_id, 1_009).unwrap().status,
    JobStatus::Pending
  );
}

#[test]
fn coordinator_places_cpu_and_gpu_jobs_before_the_gpu_worker_polls_first() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "a-gpu-worker", "gpu-session", 1_000);
  let mut cpu_capacity = capacity();
  cpu_capacity.gpus.clear();
  register_capacity_at(&store, "z-cpu-worker", "cpu-session", cpu_capacity, 1_000);
  let cpu = submit_at(&store, submission("cpu", resources(4, 8_000, 0)), 1_001);
  let gpu = submit_at(&store, submission("gpu", resources(4, 8_000, 1)), 1_002);
  assert_eq!(cpu.assigned_worker_id.as_deref(), Some("z-cpu-worker"));
  assert_eq!(gpu.assigned_worker_id.as_deref(), Some("a-gpu-worker"));
  assert_eq!(cpu.status, JobStatus::Pending);
  assert!(cpu.attempt.is_none());
  assert!(
    cpu.worker_id.is_none(),
    "the optional user pin remains distinct"
  );
  let heartbeat = QueueRequest::Heartbeat {
    worker_id: "z-cpu-worker".into(),
    session_id: "cpu-session".into(),
  };
  let response = store.resource_queue_at(heartbeat.clone(), 1_003).unwrap();
  response.validate_for(&heartbeat).unwrap();
  let QueueResponse::Worker {
    worker: reported,
    attempts,
  } = response
  else {
    panic!("expected worker")
  };
  assert!(
    attempts.is_empty(),
    "an offer has no accepted execution journal"
  );
  assert_eq!(reported.assigned_jobs, 1);
  assert_eq!(reported.outstanding_attempts, 0);
  assert_eq!(reported.available.cpus, 0);
  assert_eq!(reported.available.memory_bytes, 0);
  assert_eq!(reported.reserved.cpus, 4);
  assert!(reported.online);
  assert_eq!(
    claim_at(&store, "a-gpu-worker", "gpu-session", "gpu-claim", 1_004)
      .unwrap()
      .job
      .job_id,
    gpu.job_id
  );
  let accepted = claim_at(&store, "z-cpu-worker", "cpu-session", "cpu-claim", 1_005).unwrap();
  assert_eq!(accepted.job.job_id, cpu.job_id);
  let accepted_worker = worker(&store.db().unwrap(), "z-cpu-worker", 1_005).unwrap();
  accepted_worker.validate().unwrap();
  assert_eq!(accepted_worker.assigned_jobs, 0);
  assert_eq!(accepted_worker.outstanding_attempts, 1);
  assert_eq!(accepted_worker.available.cpus, 0);
  store
    .resource_queue_at(
      QueueRequest::Finish {
        attempt: accepted.attempt.identity(),
        status: AttemptTerminalStatus::Completed,
        exit_code: Some(0),
        message: None,
      },
      1_006,
    )
    .unwrap();
  let worker = worker(&store.db().unwrap(), "z-cpu-worker", 1_006).unwrap();
  assert_eq!(worker.available.cpus, 4);
  assert_eq!(worker.reserved.cpus, 0);
}

#[test]
fn expiration_reassigns_only_unaccepted_offers_and_retains_unknown_execution() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker-one", "session-one", 1_000);
  register_at(&store, "worker-two", "session-two", 2_000);
  submit_at(
    &store,
    submission("accepted", resources(2, 4_000, 1)),
    2_001,
  );
  let accepted = claim_at(&store, "worker-one", "session-one", "claim", 2_002).unwrap();
  let offered = submit_at(&store, submission("offered", resources(2, 4_000, 0)), 2_003);
  assert_eq!(offered.assigned_worker_id.as_deref(), Some("worker-one"));
  let after = 1_000 + QUEUE_LEASE_MS + 1;
  let response = store
    .resource_queue_at(
      QueueRequest::GetJob {
        job_id: offered.job_id.clone(),
      },
      after,
    )
    .unwrap();
  let QueueResponse::Job { job: offered } = response else {
    panic!("expected offered job")
  };
  assert_eq!(offered.assigned_worker_id.as_deref(), Some("worker-two"));
  assert_eq!(offered.status, JobStatus::Pending);
  assert!(offered.attempt.is_none());
  let db = store.db().unwrap();
  let expired = worker(&db, "worker-one", after).unwrap();
  expired.validate().unwrap();
  assert!(!expired.online);
  assert!(expired.quarantined);
  assert_eq!(expired.assigned_jobs, 0);
  assert_eq!(expired.outstanding_attempts, 1);
  assert_eq!(expired.reserved.cpus, 2);
  assert_eq!(expired.available.cpus, 2);
  assert!(expired.available.gpus.is_empty());
  assert_eq!(
    attempt(&db, &accepted.attempt.attempt_id).unwrap().status,
    AttemptStatus::Unknown
  );
  drop(db);
  let heartbeat = QueueRequest::Heartbeat {
    worker_id: "worker-two".into(),
    session_id: "session-two".into(),
  };
  let response = store.resource_queue_at(heartbeat.clone(), after).unwrap();
  response.validate_for(&heartbeat).unwrap();
  let QueueResponse::Worker { worker, attempts } = response else {
    panic!("expected worker")
  };
  assert!(attempts.is_empty());
  assert_eq!(worker.assigned_jobs, 1);
  let claimed = claim_at(&store, "worker-two", "session-two", "claim-two", after + 1).unwrap();
  assert_eq!(claimed.job.job_id, offered.job_id);
}

#[test]
fn cancelling_a_planned_offer_releases_capacity_for_the_next_queued_job() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker-one", "session-one", 1_000);
  let offered = submit_at(&store, submission("offered", resources(4, 8_000, 1)), 1_001);
  let next = submit_at(&store, submission("next", resources(4, 8_000, 1)), 1_002);
  assert!(next.assigned_worker_id.is_none());
  let QueueResponse::Job { job: cancelled } = store
    .resource_queue_at(
      QueueRequest::Cancel {
        job_id: offered.job_id,
      },
      1_003,
    )
    .unwrap()
  else {
    panic!("expected cancelled job")
  };
  assert_eq!(cancelled.status, JobStatus::Cancelled);
  assert!(cancelled.assigned_worker_id.is_none());
  assert!(cancelled.attempt.is_none());
  assert_eq!(
    claim_at(&store, "worker-one", "session-one", "claim", 1_004)
      .unwrap()
      .job
      .job_id,
    next.job_id
  );
}

#[test]
fn concurrent_acceptance_retries_convert_one_offer_to_one_attempt() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker-one", "session-one", 1_000);
  submit_at(&store, submission("offered", resources(4, 8_000, 1)), 1_001);
  std::thread::scope(|scope| {
    let first =
      scope.spawn(|| claim_at(&store, "worker-one", "session-one", "same-claim", 1_002).unwrap());
    let second =
      scope.spawn(|| claim_at(&store, "worker-one", "session-one", "same-claim", 1_002).unwrap());
    assert_eq!(
      first.join().unwrap().attempt,
      second.join().unwrap().attempt
    );
  });
  let db = store.db().unwrap();
  assert_eq!(outstanding(&db, "worker-one").unwrap().len(), 1);
  assert_eq!(
    db.query_row("SELECT COUNT(*) FROM queue_offers", [], |row| row
      .get::<_, usize>(0))
      .unwrap(),
    0
  );
}

#[test]
fn planned_offers_survive_restart_without_becoming_accepted_attempts() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker-one", "session-one", 1_000);
  let offered = submit_at(&store, submission("offered", resources(4, 8_000, 1)), 1_001);
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let before = worker(&store.db().unwrap(), "worker-one", 1_002).unwrap();
  assert_eq!(before.assigned_jobs, 1);
  assert_eq!(before.outstanding_attempts, 0);
  assert_eq!(before.available.cpus, 0);
  assert_eq!(
    claim_at(&store, "worker-one", "session-one", "claim", 1_003)
      .unwrap()
      .job
      .job_id,
    offered.job_id
  );
}

#[test]
fn outstanding_limit_counts_both_offers_and_accepted_attempts() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let capacity = WorkerCapacity {
    cpus: MAX_OUTSTANDING_ATTEMPTS as u32 + 1,
    memory_bytes: MAX_OUTSTANDING_ATTEMPTS as u64 + 1,
    gpus: Vec::new(),
  };
  register_capacity_at(&store, "worker-one", "session-one", capacity, 1_000);
  let mut last = None;
  for index in 0..=MAX_OUTSTANDING_ATTEMPTS {
    last = Some(submit_at(
      &store,
      submission(&format!("job-{index}"), resources(1, 1, 0)),
      1_001 + index as i64,
    ));
  }
  let last = last.unwrap();
  assert!(last.assigned_worker_id.is_none());
  assert_eq!(
    last.pending_reason.as_deref(),
    Some("resources_unavailable")
  );
  let reported = worker(&store.db().unwrap(), "worker-one", 2_000).unwrap();
  assert_eq!(reported.assigned_jobs as usize, MAX_OUTSTANDING_ATTEMPTS);
  assert_eq!(reported.available.cpus, 1);
  let accepted = claim_at(&store, "worker-one", "session-one", "claim", 2_001).unwrap();
  let reported = worker(&store.db().unwrap(), "worker-one", 2_001).unwrap();
  reported.validate().unwrap();
  assert_eq!(
    reported.assigned_jobs as usize,
    MAX_OUTSTANDING_ATTEMPTS - 1
  );
  assert_eq!(reported.outstanding_attempts, 1);
  store
    .resource_queue_at(
      QueueRequest::Finish {
        attempt: accepted.attempt.identity(),
        status: AttemptTerminalStatus::Completed,
        exit_code: Some(0),
        message: None,
      },
      2_002,
    )
    .unwrap();
  let reported = worker(&store.db().unwrap(), "worker-one", 2_002).unwrap();
  assert_eq!(reported.assigned_jobs as usize, MAX_OUTSTANDING_ATTEMPTS);
  assert_eq!(reported.outstanding_attempts, 0);
  assert_eq!(
    job(&store.db().unwrap(), &last.job_id, 2_003)
      .unwrap()
      .assigned_worker_id
      .as_deref(),
    Some("worker-one")
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
    worker(&store.db().unwrap(), "worker-one", 1_004)
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
    !worker(&store.db().unwrap(), "worker-one", 1_008)
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

#[test]
fn canonical_pending_queries_read_legacy_queued_records_after_restart() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let pending = submit_at(
    &store,
    submission("legacy-pending", resources(1, 1_000, 0)),
    now_ms(),
  );
  let stored: (String, String) = store
    .db()
    .unwrap()
    .query_row(
      "SELECT status,json_extract(record,'$.status') FROM queue_jobs WHERE job_id=?1",
      [&pending.job_id],
      |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap();
  assert_eq!(stored, ("queued".into(), "pending".into()));
  store
    .db()
    .unwrap()
    .execute(
      "UPDATE queue_jobs SET record=json_set(record,'$.status','queued') WHERE job_id=?1",
      [&pending.job_id],
    )
    .unwrap();
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let QueueResponse::Jobs { jobs, .. } = store
    .resource_queue(QueueRequest::ListJobs {
      project_id: Some("project".into()),
      status: Some(JobStatus::Pending),
      cursor: None,
      limit: 20,
    })
    .unwrap()
  else {
    panic!("jobs expected");
  };
  assert_eq!(jobs.len(), 1);
  assert_eq!(jobs[0].status, JobStatus::Pending);
  assert_eq!(serde_json::to_value(&jobs[0]).unwrap()["status"], "pending");
}

#[test]
fn pending_history_uses_job_identity_and_public_resource_context_without_runtime_records() {
  use crate::service::dashboard_data::HostedDashboard;
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let mut spec = submission("pending-history", resources(2, 4_000, 1));
  spec.command.push("private-source-command".into());
  spec
    .environment
    .env
    .insert("PRIVATE_TOKEN".into(), "fixture-secret-token".into());
  let pending = submit_at(&store, spec, now_ms());
  let dashboard = HostedDashboard::new(&store).unwrap();
  let source = "hosted-project:project";
  let key = format!("queue::{}", pending.job_id);
  let list = dashboard
    .list(source, None, Some("train"), Some("pending"), 20, 0)
    .unwrap();
  assert_eq!(list["total_count"], 1);
  let row = &list["runs"][0];
  assert_eq!(row["run_id"], pending.job_id);
  assert_eq!(row["run_key"], key);
  assert_eq!(row["status"], "pending");
  assert_eq!(row["queue"]["pending_reason"], "no_eligible_worker");
  assert_eq!(row["queue"]["resources"]["gpus"], 1);
  assert!(row["started_at"].is_null());
  assert!(row["system_capture_id"].is_null());
  assert!(list["warnings"].as_array().unwrap().is_empty());
  let detail = dashboard.detail(source, &key).unwrap();
  assert!(detail["state"].is_null());
  assert_eq!(detail["metric_count"], 0);
  assert!(detail["warnings"].as_array().unwrap().is_empty());
  assert!(
    dashboard.artifacts(source, &key).unwrap()["files"]
      .as_array()
      .unwrap()
      .is_empty()
  );
  assert_eq!(
    dashboard.log(source, &key, "stdout", 100).unwrap()["content"],
    ""
  );
  let updates = dashboard
    .updates(source, std::slice::from_ref(&key))
    .unwrap();
  assert_eq!(updates["runs"][0]["run_id"], key);
  assert_eq!(updates["runs"][0]["missing"], false);
  assert!(updates["runs"][0]["redirect"].is_null());
  let serialized = serde_json::to_string(&(list, detail, updates)).unwrap();
  for private in [
    "fixture-secret-token",
    "PRIVATE_TOKEN",
    "private-source-command",
    "fencing_token",
    "source_key",
    "queue-source-",
  ] {
    assert!(
      !serialized.contains(private),
      "public projection leaked {private}"
    );
  }
  assert!(dashboard.detail("hosted-project:other", &key).is_err());
  assert!(
    dashboard
      .list("hosted-project:project", None, None, Some("pending"), 1, 1)
      .unwrap()["runs"]
      .as_array()
      .unwrap()
      .is_empty()
  );
  let QueueResponse::Job { job } = store
    .resource_queue(QueueRequest::Cancel {
      job_id: pending.job_id,
    })
    .unwrap()
  else {
    panic!("job expected");
  };
  assert_eq!(job.status, JobStatus::Cancelled);
  let terminal = dashboard.detail(source, &key).unwrap();
  assert_eq!(terminal["run"]["status"], "cancelled");
  assert!(terminal["run"]["started_at"].is_null());
  assert!(
    terminal["run"]["queue_message"]
      .as_str()
      .unwrap()
      .contains("No published")
  );
  assert!(terminal["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn queue_history_handoff_keeps_selection_live_without_duplicate_or_unpublished_run_gap() {
  use crate::service::dashboard_data::HostedDashboard;
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let now = now_ms();
  register_at(&store, "queue", "session-one", now);
  let pending = submit_at(
    &store,
    submission("handoff", resources(1, 1_000, 0)),
    now + 1,
  );
  let dashboard = HostedDashboard::new(&store).unwrap();
  let source = "hosted-project:project";
  let key = format!("queue::{}", pending.job_id);
  let initial = dashboard
    .updates(source, std::slice::from_ref(&key))
    .unwrap();
  let assignment = claim_at(&store, "queue", "session-one", "claim-one", now + 2).unwrap();
  let claimed = dashboard.detail(source, &key).unwrap();
  assert_eq!(claimed["run"]["status"], "preparing");
  assert_eq!(claimed["run"]["queue"]["status"], "starting");
  assert!(claimed["run"]["started_at"].is_null());
  store
    .resource_queue_at(
      QueueRequest::Start {
        attempt: assignment.attempt.identity(),
        run_id: "run-native".into(),
      },
      now + 3,
    )
    .unwrap();
  let unpublished = dashboard.detail(source, &key).unwrap();
  assert_eq!(unpublished["run"]["run_id"], pending.job_id);
  assert_eq!(unpublished["run"]["status"], "preparing");
  assert_eq!(unpublished["run"]["queue"]["run_id"], "run-native");
  assert!(unpublished["redirect"].is_null());
  let scope = RunScope {
    project_id: "project".into(),
    origin: "queue".into(),
    run_id: "run-native".into(),
  };
  store
    .execute(Request::AppendTracking {
      scope: scope.clone(),
      path: "logs/stdout.log".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"first startup log\n"),
    })
    .unwrap();
  let logs_first = dashboard.list(source, None, None, None, 20, 0).unwrap();
  assert_eq!(logs_first["total_count"], 1);
  assert_eq!(logs_first["catalog_total_count"], 1);
  assert_eq!(logs_first["runs"][0]["run_id"], pending.job_id);
  assert_eq!(logs_first["runs"][0]["status"], "preparing");
  assert!(logs_first["runs"][0]["started_at"].is_null());
  assert!(logs_first["warnings"].as_array().unwrap().is_empty());
  assert!(dashboard.detail(source, &key).unwrap()["redirect"].is_null());
  let prepublication = dashboard
    .updates(source, std::slice::from_ref(&key))
    .unwrap();
  assert_eq!(prepublication["runs"][0]["missing"], false);
  assert!(prepublication["runs"][0]["redirect"].is_null());
  assert_ne!(
    initial["source_revision"],
    prepublication["source_revision"]
  );
  assert_ne!(
    initial["runs"][0]["metadata_revision"],
    prepublication["runs"][0]["metadata_revision"]
  );
  let malformed = serde_json::to_vec(&serde_json::json!({"schema_version":1,"run_id":"run-native","task":"train","status":"waiting"})).unwrap();
  store
    .execute(Request::PutDocument {
      scope: scope.clone(),
      path: "run-state.json".into(),
      revision: 1,
      offset: 0,
      total_size: malformed.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(malformed),
    })
    .unwrap();
  let native_error = dashboard.list(source, None, None, None, 20, 0).unwrap();
  assert_eq!(native_error["total_count"], 1);
  assert_eq!(native_error["runs"][0]["run_id"], "run-native");
  assert!(native_error["runs"][0]["queue_only"].is_null());
  assert!(
    native_error["warnings"]
      .as_array()
      .unwrap()
      .iter()
      .any(|warning| warning["message"]
        .as_str()
        .is_some_and(|message| message.contains("started_at")))
  );
  assert_eq!(
    dashboard.detail(source, &key).unwrap()["redirect"]["run_id"],
    "queue:run-native"
  );
  let bytes = serde_json::to_vec(&serde_json::json!({"schema_version":1,"run_id":"run-native","task":"train","status":"waiting","started_at":"2026-10-14T00:00:00Z"})).unwrap();
  store
    .execute(Request::PutDocument {
      scope: scope.clone(),
      path: "run-state.json".into(),
      revision: 2,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
  let list = dashboard.list(source, None, None, None, 20, 0).unwrap();
  assert_eq!(list["total_count"], 1);
  assert_eq!(list["runs"][0]["run_id"], "run-native");
  assert_eq!(list["runs"][0]["status"], "waiting");
  assert_eq!(list["runs"][0]["queue"]["job_id"], pending.job_id);
  assert!(list["runs"][0]["queue_only"].is_null());
  let redirected = dashboard.detail(source, &key).unwrap();
  assert_eq!(redirected["run"]["run_id"], "run-native");
  assert_eq!(redirected["redirect"]["source_id"], source);
  assert_eq!(redirected["redirect"]["run_id"], "queue:run-native");
  let updates = dashboard
    .updates(source, &[key.clone(), "queue:run-native".into()])
    .unwrap();
  assert_eq!(updates["runs"].as_array().unwrap().len(), 2);
  assert_eq!(updates["runs"][0]["run_id"], key);
  assert_eq!(updates["runs"][0]["missing"], false);
  assert_eq!(updates["runs"][0]["redirect"]["run_id"], "queue:run-native");
  assert_eq!(updates["runs"][1]["queue"]["job_id"], pending.job_id);
  let serialized = serde_json::to_string(&(list, redirected, updates)).unwrap();
  for private in [
    &assignment.attempt.fencing_token,
    &assignment.attempt.session_id,
    &assignment.submission.source.input_id,
  ] {
    assert!(!serialized.contains(private));
  }
}

#[test]
fn startup_confirmation_is_fenced_linked_immutable_and_legacy_attempts_default_unconfirmed() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker", "session", 1_000);
  submit_at(&store, submission("startup", resources(1, 1_000, 0)), 1_001);
  let assignment = claim_at(&store, "worker", "session", "claim", 1_002).unwrap();
  let identity = assignment.attempt.identity();
  let request = QueueRequest::MarkStarted {
    attempt: identity.clone(),
    run_id: "run-one".into(),
  };
  assert_eq!(
    store
      .resource_queue_at(request.clone(), 1_003)
      .unwrap_err()
      .status,
    409
  );
  let QueueResponse::Attempt { attempt: linked } = store
    .resource_queue_at(
      QueueRequest::Start {
        attempt: identity.clone(),
        run_id: "run-one".into(),
      },
      1_004,
    )
    .unwrap()
  else {
    panic!("attempt expected");
  };
  assert_eq!(linked.started_at_ms, None);
  let mut old = serde_json::to_value(&linked).unwrap();
  old.as_object_mut().unwrap().remove("started_at_ms");
  let legacy: AttemptSummary = serde_json::from_value(old).unwrap();
  assert_eq!(legacy.started_at_ms, None);
  assert!(
    QueueResponse::Attempt { attempt: legacy }
      .validate_for(&request)
      .is_err()
  );
  let mut forged = identity.clone();
  forged.fencing_token = "wrong-fence".into();
  assert_eq!(
    store
      .resource_queue_at(
        QueueRequest::MarkStarted {
          attempt: forged,
          run_id: "run-one".into()
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
        QueueRequest::MarkStarted {
          attempt: identity.clone(),
          run_id: "wrong-run".into()
        },
        1_005
      )
      .unwrap_err()
      .status,
    409
  );
  let confirmed = store.resource_queue_at(request.clone(), 1_006).unwrap();
  confirmed.validate_for(&request).unwrap();
  let QueueResponse::Attempt { attempt: confirmed } = confirmed else {
    panic!("attempt expected");
  };
  assert_eq!(confirmed.started_at_ms, Some(1_006));
  let QueueResponse::Attempt { attempt: repeated } =
    store.resource_queue_at(request, 1_010).unwrap()
  else {
    panic!("attempt expected");
  };
  assert_eq!(repeated, confirmed);
}

#[test]
fn terminal_startup_confirmation_never_revives_the_task_or_its_released_allocation() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  register_at(&store, "worker", "session", 1_000);
  submit_at(
    &store,
    submission("short-task", resources(1, 1_000, 0)),
    1_001,
  );
  let assignment = claim_at(&store, "worker", "session", "claim", 1_002).unwrap();
  let identity = assignment.attempt.identity();
  store
    .resource_queue_at(
      QueueRequest::Start {
        attempt: identity.clone(),
        run_id: "run-fast".into(),
      },
      1_003,
    )
    .unwrap();
  store
    .resource_queue_at(
      QueueRequest::Finish {
        attempt: identity.clone(),
        status: AttemptTerminalStatus::Completed,
        exit_code: Some(0),
        message: None,
      },
      1_004,
    )
    .unwrap();
  let QueueResponse::Attempt { attempt } = store
    .resource_queue_at(
      QueueRequest::MarkStarted {
        attempt: identity,
        run_id: "run-fast".into(),
      },
      1_005,
    )
    .unwrap()
  else {
    panic!("attempt expected");
  };
  assert_eq!(attempt.status, AttemptStatus::Completed);
  assert_eq!(attempt.started_at_ms, Some(1_005));
  assert_eq!(attempt.exit_code, Some(0));
  let db = store.db().unwrap();
  assert!(
    db.query_row(
      "SELECT released FROM queue_attempts WHERE attempt_id=?1",
      [&attempt.attempt_id],
      |row| row.get::<_, bool>(0)
    )
    .unwrap()
  );
  assert_eq!(
    job(&db, &attempt.job_id, 1_005).unwrap().status,
    JobStatus::Completed
  );
  assert_eq!(worker(&db, "worker", 1_005).unwrap().reserved.cpus, 0);
}

#[test]
fn queue_update_revisions_ignore_private_inputs_and_track_shared_worker_resources() {
  use crate::service::dashboard_data::HostedDashboard;
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let now = now_ms();
  store
    .resource_queue_at(
      QueueRequest::RegisterWorker {
        worker_id: "worker".into(),
        session_id: "session".into(),
        project_ids: vec!["project".into(), "other".into()],
        capacity: capacity(),
      },
      now,
    )
    .unwrap();
  let mut other = submission("other-task", resources(2, 4_000, 0));
  other.project_id = "other".into();
  other.task = "private-other-project-task".into();
  submit_at(&store, other, now + 1);
  let assignment = claim_at(&store, "worker", "session", "claim", now + 2).unwrap();
  store
    .resource_queue_at(
      QueueRequest::Start {
        attempt: assignment.attempt.identity(),
        run_id: "private-other-run".into(),
      },
      now + 3,
    )
    .unwrap();
  let pending = submit_at(
    &store,
    submission("pending-shared", resources(4, 8_000, 1)),
    now + 4,
  );
  let dashboard = HostedDashboard::new(&store).unwrap();
  let key = format!("queue::{}", pending.job_id);
  let source_id = "hosted-project:project";
  let before = dashboard
    .updates(source_id, std::slice::from_ref(&key))
    .unwrap();
  assert_eq!(
    before["runs"][0]["queue"]["pending_reason"],
    "resources_unavailable"
  );
  let mut private_input = submission("private-input", resources(1, 1_000, 0));
  private_input.source.sha256 = "b".repeat(64);
  private_input.source.input_id = format!("queue-source-{}", private_input.source.sha256);
  source(&store, &private_input);
  let after_input = dashboard
    .updates(source_id, std::slice::from_ref(&key))
    .unwrap();
  assert_eq!(before["source_revision"], after_input["source_revision"]);
  assert_eq!(before["queue_revision"], after_input["queue_revision"]);
  // Inspect immediately after another project's allocation release, before
  // the scheduler creates a new offer or mutates this project's own job.
  let project_revision: i64;
  {
    let db = store.db().unwrap();
    project_revision = db
      .query_row(
        "SELECT revision FROM queue_change_revisions WHERE project_id='project'",
        [],
        |row| row.get(0),
      )
      .unwrap();
    finish(
      &db,
      &assignment.attempt.identity(),
      AttemptTerminalStatus::Completed,
      Some(0),
      None,
      now + 5,
    )
    .unwrap();
    assert_eq!(
      project_revision,
      db.query_row(
        "SELECT revision FROM queue_change_revisions WHERE project_id='project'",
        [],
        |row| row.get::<_, i64>(0)
      )
      .unwrap()
    );
  }
  let released = dashboard
    .updates(source_id, std::slice::from_ref(&key))
    .unwrap();
  assert_eq!(
    released["runs"][0]["queue"]["pending_reason"],
    "waiting_for_worker_claim"
  );
  assert_ne!(released["source_revision"], before["source_revision"]);
  assert_ne!(released["queue_revision"], before["queue_revision"]);
  let serialized = serde_json::to_string(&released).unwrap();
  for private in [
    "private-other-project-task",
    "private-other-run",
    "fencing_token",
    "session_id",
  ] {
    assert!(!serialized.contains(private));
  }
}

#[test]
fn merged_native_and_pending_history_pages_reach_every_row_beyond_offset_six_hundred() {
  use crate::service::dashboard_data::HostedDashboard;
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let now = now_ms();
  let native_count = 320;
  let pending_count = 321;
  for index in 0..native_count {
    let run_id = format!("native-{index:04}");
    let scope = RunScope {
      project_id: "project".into(),
      origin: "worker".into(),
      run_id: run_id.clone(),
    };
    let bytes = serde_json::to_vec(&serde_json::json!({"schema_version":1,"run_id":run_id,"task":"train","status":"completed","started_at":"2026-10-14T00:00:00Z","finished_at":"2026-10-14T00:00:01Z","exit_code":0})).unwrap();
    store
      .execute(Request::PutDocument {
        scope,
        path: "run-state.json".into(),
        revision: 1,
        offset: 0,
        total_size: bytes.len() as u64,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
      })
      .unwrap();
  }
  for index in 0..pending_count {
    submit_at(
      &store,
      submission(&format!("pending-{index:04}"), resources(1, 1_000, 0)),
      now + index as i64,
    );
  }
  let dashboard = HostedDashboard::new(&store).unwrap();
  let mut offset = 0;
  let mut references = BTreeSet::new();
  let mut statuses = BTreeMap::<String, usize>::new();
  loop {
    let page = dashboard
      .list("hosted-project:project", None, None, None, 100, offset)
      .unwrap();
    assert_eq!(page["total_count"], native_count + pending_count);
    assert_eq!(page["catalog_total_count"], native_count + pending_count);
    assert!(page["warnings"].as_array().unwrap().is_empty());
    for row in page["runs"].as_array().unwrap() {
      assert!(references.insert(row["run_key"].as_str().unwrap().to_owned()));
      *statuses
        .entry(row["status"].as_str().unwrap().to_owned())
        .or_default() += 1;
    }
    let Some(next) = page["next_offset"].as_u64() else {
      break;
    };
    assert!(next > offset as u64);
    offset = next as usize;
  }
  assert_eq!(offset, 600);
  assert_eq!(references.len(), native_count + pending_count);
  assert_eq!(statuses["completed"], native_count);
  assert_eq!(statuses["pending"], pending_count);
  assert!(
    dashboard
      .list("hosted-project:project", None, None, None, 100, 1001)
      .is_err()
  );
}
