use super::*;
use expri_core::global_config::DEFAULT_GPU_BUSY_MEMORY_THRESHOLD_BYTES;
use expri_core::resource_queue::{
  AttemptIdentity, JobAssignment, JobStatus, JobSubmission, JobSummary, RESOURCE_QUEUE_CAPABILITY,
  RESOURCE_QUEUE_GPU_AVAILABILITY_CAPABILITY, ResourceRequest, SourceRef, WorkerCapacity,
  WorkerGpu, WorkerSummary,
};
use expri_core::service::{Request as ServiceRequest, Response as ServiceResponse};
use serde_json::json;

fn control_mock(
  request_count: usize,
  mut handler: impl FnMut(ServiceRequest) -> Option<ServiceResponse> + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
  use std::io::{BufRead, Read, Write};

  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  let server = thread::spawn(move || {
    for _ in 0..request_count {
      let deadline = Instant::now() + Duration::from_secs(5);
      let mut stream = loop {
        match listener.accept() {
          Ok((stream, _)) => break stream,
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            assert!(
              Instant::now() < deadline,
              "queue control client did not connect"
            );
            thread::sleep(Duration::from_millis(5));
          }
          Err(error) => panic!("queue control accept: {error}"),
        }
      };
      stream.set_nonblocking(false).unwrap();
      stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
      stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
      let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
      let mut first = String::new();
      reader.read_line(&mut first).unwrap();
      assert!(first.starts_with("POST "));
      let mut length = None;
      loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "incomplete queue control request");
        if line == "\r\n" {
          break;
        }
        if let Some((name, value)) = line.split_once(':')
          && name.eq_ignore_ascii_case("content-length")
        {
          length = Some(value.trim().parse::<usize>().unwrap());
        }
      }
      let length = length.expect("queue control request requires content length");
      assert!(length <= 4096);
      let mut body = vec![0; length];
      reader.read_exact(&mut body).unwrap();
      let request = serde_json::from_slice(&body).unwrap();
      let Some(response) = handler(request) else {
        // Simulate a response lost after the control operation was committed.
        continue;
      };
      let body = serde_json::to_vec(&response).unwrap();
      write!(
        stream,
        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
      )
      .unwrap();
      stream.write_all(&body).unwrap();
    }
  });
  (url, server)
}

fn capabilities() -> ServiceResponse {
  ServiceResponse::Capabilities {
    features: vec![
      RESOURCE_QUEUE_CAPABILITY.into(),
      RESOURCE_QUEUE_GPU_AVAILABILITY_CAPABILITY.into(),
    ],
  }
}

fn apple_assignment() -> JobAssignment {
  let mut assignment = assignment();
  assignment.attempt.allocation.gpus[0] = WorkerGpu {
    device_id: "apple-fixture".into(),
    model: "Apple GPU".into(),
    memory_bytes: None,
    vendor: GpuVendor::Apple,
    selector: None,
  };
  assignment.job.attempt = Some(assignment.attempt.clone());
  assignment
}

fn executor_fixture(root: &std::path::Path, url: &str) -> Executor {
  let client_config = root.join("client.toml");
  fs::write(
    &client_config,
    format!("url='{url}'\ntoken='fixture-token'\n"),
  )
  .unwrap();
  let directory = root.join("worker-a");
  private_directory(&directory).unwrap();
  private_directory(&directory.join("attempts")).unwrap();
  let capacity = WorkerCapacity {
    cpus: 4,
    memory_bytes: 8192,
    gpus: apple_assignment().attempt.allocation.gpus,
  };
  Executor {
    options: ExecutorOptions {
      storage: StoragePaths::from_root(root.join("storage")).unwrap(),
      worker: ResolvedQueueWorkerConfig {
        worker_id: "worker-a".into(),
        project_ids: vec![],
        gpu_busy_memory_threshold_bytes: DEFAULT_GPU_BUSY_MEMORY_THRESHOLD_BYTES,
        capacity: capacity.clone(),
      },
      client_config: client_config.clone(),
      poll_interval: Duration::from_secs(2),
    },
    client: ResourceQueueClient::new(&client_config).unwrap(),
    client_config,
    directory,
    session: Arc::new(Mutex::new("session-a".into())),
    attempts: vec![apple_assignment().attempt],
    stop: Arc::new(AtomicBool::new(false)),
    worker: WorkerSummary {
      worker_id: "worker-a".into(),
      session_id: "session-a".into(),
      project_ids: vec![],
      capacity,
      available: ResourceAllocation {
        cpus: 2,
        memory_bytes: 4096,
        gpus: vec![],
      },
      reserved: apple_assignment().attempt.allocation,
      unavailable_gpus: vec![],
      gpu_availability_observed_at_ms: Some(1),
      online: true,
      last_seen_at_ms: 1,
      lease_expires_at_ms: 60_001,
      quarantined: false,
      outstanding_attempts: 1,
      assigned_jobs: 0,
    },
  }
}

fn allocation() -> ResourceAllocation {
  ResourceAllocation {
    cpus: 2,
    memory_bytes: 4096,
    gpus: vec![WorkerGpu {
      device_id: "GPU-a".into(),
      model: "fixture".into(),
      memory_bytes: Some(8192),
      vendor: GpuVendor::Nvidia,
      selector: Some("GPU-a".into()),
    }],
  }
}

fn assignment() -> JobAssignment {
  let resources = ResourceRequest {
    cpus: 2,
    memory_bytes: 4096,
    gpus: 1,
    gpu_ids: vec![],
    gpu_model: None,
    gpu_memory_bytes: None,
  };
  let attempt = AttemptSummary {
    attempt_id: "attempt-a".into(),
    job_id: "job-a".into(),
    worker_id: "worker-a".into(),
    session_id: "session-a".into(),
    claim_id: "claim-a".into(),
    fencing_token: "fence-a".into(),
    allocation: allocation(),
    run_id: None,
    started_at_ms: None,
    status: AttemptStatus::Claimed,
    cancel_requested: false,
    created_at_ms: 0,
    updated_at_ms: 0,
    exit_code: None,
    message: None,
  };
  let digest = "a".repeat(64);
  JobAssignment {
    job: JobSummary {
      job_id: "job-a".into(),
      project_id: "project-a".into(),
      task: "train".into(),
      resources: resources.clone(),
      worker_id: None,
      assigned_worker_id: Some("worker-a".into()),
      status: JobStatus::Starting,
      submitted_at_ms: 0,
      updated_at_ms: 0,
      attempt: Some(attempt.clone()),
      pending_reason: None,
    },
    submission: JobSubmission {
      submission_id: "submission-a".into(),
      project_id: "project-a".into(),
      task: "train".into(),
      command: vec!["python".into(), "train.py".into()],
      environment: crate::config::EnvironmentConfig {
        base_python: None,
        reuse_packages: vec![],
        require_cuda: false,
        env: BTreeMap::new(),
      },
      extras: vec![],
      sync_args: vec![],
      remote_managed: vec![],
      smoke: false,
      publish: false,
      resources,
      worker_id: None,
      source: SourceRef {
        input_id: format!("queue-source-{digest}"),
        size: 10,
        sha256: digest,
      },
    },
    attempt,
  }
}

#[test]
fn allocation_hints_hide_unallocated_devices_and_use_assigned_driver_selectors() {
  let mut allocated = allocation();
  let env = allocation_env(&allocated);
  assert_eq!(env["OMP_NUM_THREADS"], "2");
  assert_eq!(env["CUDA_VISIBLE_DEVICES"], "GPU-a");
  assert_eq!(env["HIP_VISIBLE_DEVICES"], "");
  assert_eq!(env["ROCR_VISIBLE_DEVICES"], "");
  assert_eq!(env["EXPRI_ALLOCATED_MEMORY_BYTES"], "4096");
  allocated.gpus.clear();
  let env = allocation_env(&allocated);
  for key in [
    "CUDA_VISIBLE_DEVICES",
    "HIP_VISIBLE_DEVICES",
    "ROCR_VISIBLE_DEVICES",
  ] {
    assert_eq!(env[key], "");
  }
}

#[test]
fn amd_uuid_allocations_use_rocr_selectors_and_relative_hip_ordinals() {
  let mut allocated = allocation();
  allocated.gpus = ["GPU-000000000000000a", "GPU-000000000000000b"]
    .map(|selector| WorkerGpu {
      device_id: selector.into(),
      model: "AMD GPU".into(),
      memory_bytes: Some(8192),
      vendor: GpuVendor::Amd,
      selector: Some(selector.into()),
    })
    .into();
  let env = allocation_env(&allocated);
  assert_eq!(env["CUDA_VISIBLE_DEVICES"], "");
  assert_eq!(
    env["ROCR_VISIBLE_DEVICES"],
    "GPU-000000000000000a,GPU-000000000000000b"
  );
  assert_eq!(env["HIP_VISIBLE_DEVICES"], "0,1");
  assert_eq!(env["OMP_NUM_THREADS"], "2");
}

#[test]
fn mixed_vendor_allocations_keep_nonzero_amd_selector_out_of_hip_mask() {
  let mut allocated = allocation();
  allocated.gpus.push(WorkerGpu {
    device_id: "amd-a".into(),
    model: "AMD GPU".into(),
    memory_bytes: Some(8192),
    vendor: GpuVendor::Amd,
    selector: Some("4".into()),
  });
  let env = allocation_env(&allocated);
  assert_eq!(env["CUDA_VISIBLE_DEVICES"], "GPU-a");
  assert_eq!(env["ROCR_VISIBLE_DEVICES"], "4");
  assert_eq!(env["HIP_VISIBLE_DEVICES"], "0");
}

#[test]
fn launch_journal_survives_restart_and_rejects_foreign_run_paths() {
  let temporary = tempfile::tempdir().unwrap();
  let directory = temporary.path().join("attempt-a");
  private_directory(&directory).unwrap();
  let mut journal = Journal::claimed(assignment());
  journal.stage = Stage::LaunchPending;
  journal.save(&directory).unwrap();
  let recovered = Journal::load(&directory).unwrap();
  assert_eq!(recovered.stage, Stage::LaunchPending);
  assert_eq!(
    recovered.attempt.identity(),
    AttemptIdentity {
      attempt_id: "attempt-a".into(),
      worker_id: "worker-a".into(),
      session_id: "session-a".into(),
      fencing_token: "fence-a".into()
    }
  );
  // Recovery only observes; an uncertain prelaunch journal is never terminal.
  assert!(!local_terminal(&recovered).unwrap());
  journal.repo_root = Some(temporary.path().join("foreign"));
  journal.save(&directory).unwrap();
  assert!(Journal::load(&directory).is_err());
}

#[test]
fn repeated_unverified_gpu_probes_park_the_same_durable_attempt_without_download() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let (url, server) = control_mock(1, |request| {
    assert!(matches!(request, ServiceRequest::Capabilities));
    Some(capabilities())
  });
  let mut executor = executor_fixture(&root, &url);
  let directory = executor.directory.join("attempts/attempt-a");
  private_directory(&directory).unwrap();
  let source = directory.join("source");
  private_directory(&source).unwrap();
  fs::write(source.join("frozen.txt"), "submitted source").unwrap();
  let mut journal = Journal::claimed(apple_assignment());
  journal.repo_root = Some(source.clone());
  journal.save(&directory).unwrap();
  let attempt = journal.attempt.clone();
  executor
    .dispatch_or_finish(&mut journal, &directory)
    .unwrap();
  for _ in 0..2 {
    let persisted = Journal::load(&directory).unwrap();
    assert_eq!(persisted.stage, Stage::WaitingForGpu);
    assert_eq!(persisted.attempt, attempt);
    assert_eq!(persisted.repo_root.as_ref(), Some(&source));
    assert!(persisted.run_dir.is_none());
    assert!(prelaunch_without_supervisor(&persisted));
    assert!(!directory.join("source.zip").exists());
    assert!(!source.join(".expri/runs").exists());
    executor.reconcile(attempt.clone()).unwrap();
  }
  assert_eq!(executor.worker.reserved, attempt.allocation);
  assert_eq!(
    fs::read_to_string(source.join("frozen.txt")).unwrap(),
    "submitted source"
  );
  server.join().unwrap();
}

#[test]
fn prelaunch_terminal_confirmation_requires_no_launch_intent_or_run_identity() {
  let mut journal = Journal::claimed(apple_assignment());
  for stage in [Stage::Claimed, Stage::WaitingForGpu] {
    journal.stage = stage;
    assert!(prelaunch_without_supervisor(&journal));
  }
  for stage in [
    Stage::Prepared,
    Stage::LaunchPending,
    Stage::Active,
    Stage::Quarantined,
    Stage::Terminal,
  ] {
    journal.stage = stage;
    assert!(!prelaunch_without_supervisor(&journal));
  }
  journal.stage = Stage::WaitingForGpu;
  journal.attempt.run_id = Some("run-a".into());
  assert!(!prelaunch_without_supervisor(&journal));
  journal.attempt.run_id = None;
  journal.run_dir = Some(PathBuf::from("source/.expri/runs/run-a"));
  assert!(!prelaunch_without_supervisor(&journal));
}

#[test]
fn cancelling_a_gpu_wait_finishes_without_launching_or_releasing_it_locally() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let identity = apple_assignment().attempt.identity();
  let mut cancelled = apple_assignment().attempt;
  cancelled.status = AttemptStatus::Cancelled;
  cancelled.cancel_requested = true;
  cancelled.exit_code = Some(130);
  cancelled.updated_at_ms = 1;
  let expected = cancelled.clone();
  let (url, server) = control_mock(2, move |request| {
    let response = match request {
      ServiceRequest::Capabilities => capabilities(),
      ServiceRequest::Queue {
        request:
          QueueRequest::Finish {
            attempt,
            status,
            exit_code,
            ..
          },
      } => {
        assert_eq!(attempt, identity);
        assert_eq!(status, AttemptTerminalStatus::Cancelled);
        assert_eq!(exit_code, Some(130));
        ServiceResponse::Queue {
          response: QueueResponse::Attempt {
            attempt: cancelled.clone(),
          },
        }
      }
      other => panic!("unexpected GPU wait control request: {other:?}"),
    };
    Some(response)
  });
  let mut executor = executor_fixture(&root, &url);
  let directory = executor.directory.join("attempts/attempt-a");
  private_directory(&directory).unwrap();
  let mut journal = Journal::claimed(apple_assignment());
  journal.stage = Stage::WaitingForGpu;
  journal.save(&directory).unwrap();
  let mut attempt = journal.attempt.clone();
  attempt.cancel_requested = true;
  executor.reconcile(attempt).unwrap();
  let persisted = Journal::load(&directory).unwrap();
  assert_eq!(persisted.stage, Stage::Terminal);
  assert_eq!(persisted.attempt, expected);
  assert!(persisted.run_dir.is_none());
  assert!(!directory.join("source").exists());
  server.join().unwrap();
}

#[test]
fn lost_gpu_wait_finish_reply_is_settled_from_server_confirmation_on_next_tick() {
  use expri_core::resource_queue::{GpuUnavailableReason, UnavailableGpu};

  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let mut cancelled = apple_assignment().attempt;
  cancelled.status = AttemptStatus::Cancelled;
  cancelled.cancel_requested = true;
  cancelled.exit_code = Some(130);
  cancelled.updated_at_ms = 1;
  let mut job = apple_assignment().job;
  job.status = JobStatus::Cancelled;
  job.updated_at_ms = 1;
  job.attempt = Some(cancelled.clone());
  let capacity = WorkerCapacity {
    cpus: 4,
    memory_bytes: 8192,
    gpus: cancelled.allocation.gpus.clone(),
  };
  let released_worker = WorkerSummary {
    worker_id: "worker-a".into(),
    session_id: "session-a".into(),
    project_ids: vec![],
    capacity,
    available: ResourceAllocation {
      cpus: 4,
      memory_bytes: 8192,
      gpus: vec![],
    },
    reserved: ResourceAllocation {
      cpus: 0,
      memory_bytes: 0,
      gpus: vec![],
    },
    unavailable_gpus: vec![UnavailableGpu {
      device_id: "apple-fixture".into(),
      reason: GpuUnavailableReason::Unverified,
    }],
    gpu_availability_observed_at_ms: Some(1),
    online: true,
    last_seen_at_ms: 1,
    lease_expires_at_ms: 60_001,
    quarantined: false,
    outstanding_attempts: 0,
    assigned_jobs: 0,
  };
  let identity = cancelled.identity();
  let mut finish_committed = false;
  let (url, server) = control_mock(5, move |request| {
    Some(match request {
      ServiceRequest::Capabilities => capabilities(),
      ServiceRequest::Queue {
        request: QueueRequest::Finish {
          attempt, status, ..
        },
      } => {
        assert!(!finish_committed);
        assert_eq!(attempt, identity);
        assert_eq!(status, AttemptTerminalStatus::Cancelled);
        finish_committed = true;
        return None;
      }
      ServiceRequest::Queue {
        request: QueueRequest::Heartbeat { .. },
      } => {
        assert!(finish_committed);
        ServiceResponse::Queue {
          response: QueueResponse::Worker {
            worker: released_worker.clone(),
            attempts: vec![],
          },
        }
      }
      ServiceRequest::Queue {
        request: QueueRequest::GetJob { job_id },
      } => {
        assert_eq!(job_id, "job-a");
        ServiceResponse::Queue {
          response: QueueResponse::Job {
            job: Box::new(job.clone()),
          },
        }
      }
      ServiceRequest::Queue {
        request: QueueRequest::Claim { worker_id, .. },
      } => {
        assert_eq!(worker_id, "worker-a");
        ServiceResponse::Queue {
          response: QueueResponse::Claimed { assignment: None },
        }
      }
      other => panic!("unexpected GPU wait control request: {other:?}"),
    })
  });
  let mut executor = executor_fixture(&root, &url);
  let directory = executor.directory.join("attempts/attempt-a");
  private_directory(&directory).unwrap();
  let mut journal = Journal::claimed(apple_assignment());
  journal.stage = Stage::WaitingForGpu;
  journal.save(&directory).unwrap();
  assert!(
    executor
      .finish(
        &mut journal,
        &directory,
        AttemptTerminalStatus::Cancelled,
        Some(130),
        None
      )
      .is_err()
  );
  assert_eq!(
    Journal::load(&directory).unwrap().stage,
    Stage::WaitingForGpu
  );
  executor.tick().unwrap();
  let persisted = Journal::load(&directory).unwrap();
  assert_eq!(persisted.stage, Stage::Terminal);
  assert_eq!(persisted.attempt, cancelled);
  assert!(!executor.worker.quarantined);
  assert!(persisted.run_dir.is_none());
  assert!(!directory.join("source").exists());
  server.join().unwrap();
}

#[test]
fn an_unlocked_running_record_is_lost_and_cannot_release_resources() {
  let temporary = tempfile::tempdir().unwrap();
  let directory = temporary.path().canonicalize().unwrap().join("attempt-a");
  private_directory(&directory).unwrap();
  let repo = directory.join("source");
  fs::create_dir_all(repo.join(".expri/runs/run-a/code")).unwrap();
  fs::create_dir(repo.join(".expri/runs/run-a/outputs")).unwrap();
  let run = repo.join(".expri/runs/run-a");
  let mut journal = Journal::claimed(assignment());
  journal.repo_root = Some(repo);
  journal.run_dir = Some(run.clone());
  journal.stage = Stage::Active;
  write_json(
    &run.join("run-state.json"),
    &json!({
      "schema_version": 1, "run_id": "run-a", "status": "running", "detached": true,
      "code_dir": run.join("code"), "output_dir": run.join("outputs"),
    }),
  )
  .unwrap();
  assert_eq!(local_status(&journal).unwrap().unwrap()["status"], "lost");
  assert!(!local_terminal(&journal).unwrap());
}

#[cfg(unix)]
#[test]
fn queue_storage_rejects_symlink_journals_and_directory_aliases() {
  let temporary = tempfile::tempdir().unwrap();
  let outside = temporary.path().join("outside.json");
  fs::write(&outside, "\"foreign\"").unwrap();
  let alias = temporary.path().join("journal.json");
  std::os::unix::fs::symlink(&outside, &alias).unwrap();
  assert!(read_json::<String>(&alias).is_err());
  assert!(write_json(&alias, &"changed").is_err());
  assert_eq!(fs::read_to_string(&outside).unwrap(), "\"foreign\"");
}

#[test]
fn allocations_cannot_inject_an_unregistered_gpu_selector() {
  let assignment = assignment();
  let worker = ResolvedQueueWorkerConfig {
    worker_id: "worker-a".into(),
    project_ids: vec!["project-a".into()],
    gpu_busy_memory_threshold_bytes: DEFAULT_GPU_BUSY_MEMORY_THRESHOLD_BYTES,
    capacity: WorkerCapacity {
      cpus: 4,
      memory_bytes: 8192,
      gpus: allocation().gpus,
    },
  };
  validate_allocation(&allocation(), &worker, &assignment.submission.resources).unwrap();
  let mut foreign = allocation();
  foreign.gpus[0].selector = Some("GPU-foreign".into());
  assert!(validate_allocation(&foreign, &worker, &assignment.submission.resources).is_err());
}

#[test]
fn project_restrictions_are_optional_and_do_not_change_worker_assignment_identity() {
  let mut journal = Journal::claimed(assignment());
  let mut worker = ResolvedQueueWorkerConfig {
    worker_id: "worker-a".into(),
    project_ids: vec![],
    gpu_busy_memory_threshold_bytes: DEFAULT_GPU_BUSY_MEMORY_THRESHOLD_BYTES,
    capacity: WorkerCapacity {
      cpus: 4,
      memory_bytes: 8192,
      gpus: allocation().gpus,
    },
  };
  validate_worker_assignment(&journal, &worker).unwrap();
  journal.assignment.submission.project_id = "project-b".into();
  validate_worker_assignment(&journal, &worker).unwrap();
  worker.project_ids.push("project-a".into());
  assert!(validate_worker_assignment(&journal, &worker).is_err());
  journal.assignment.submission.project_id = "project-a".into();
  validate_worker_assignment(&journal, &worker).unwrap();
  worker.project_ids.clear();
  journal.attempt.worker_id = "other-worker".into();
  assert!(validate_worker_assignment(&journal, &worker).is_err());
}

#[test]
fn preparing_a_run_records_its_path_without_launching_and_can_cancel_before_launch() {
  let temporary = tempfile::tempdir().unwrap();
  fs::create_dir(temporary.path().join(".expri")).unwrap();
  fs::write(
    temporary.path().join(".expri/checkout.manifest"),
    "train.py\n",
  )
  .unwrap();
  fs::write(temporary.path().join("train.py"), "print('source')").unwrap();
  let marker = temporary.path().join("must-not-start");
  let request = RunRequest {
    name: "train".into(),
    command: vec!["touch".into(), marker.to_string_lossy().into_owned()],
    environment: crate::config::EnvironmentConfig {
      base_python: None,
      reuse_packages: vec![],
      require_cuda: false,
      env: BTreeMap::new(),
    },
    service: None,
    detach: true,
    smoke: false,
    remote_managed: vec![],
    extras: vec![],
    sync_args: vec![],
    expected_sync: None,
    requirements: Default::default(),
  };
  let prepared = crate::node::run::prepare_request_at(&request, temporary.path()).unwrap();
  let run = prepared.snapshot.run_dir.clone();
  let state: serde_json::Value = read_json(&run.join("run-state.json")).unwrap();
  assert_eq!(state["status"], "preparing");
  assert!(run.join("run-request.json").is_file());
  assert!(!run.join(".worker-ready").exists());
  assert!(!marker.exists());
  crate::node::run::abort_prepared(prepared, true, "cancelled before launch").unwrap();
  let state: serde_json::Value = read_json(&run.join("run-state.json")).unwrap();
  assert_eq!(state["status"], "cancelled");
  assert_eq!(state["exit_code"], 130);
  assert!(state["task_started_at"].is_null());
  assert!(!marker.exists());
}

#[test]
fn allocated_run_identity_is_not_task_start_evidence_but_fast_terminal_marker_is() {
  let mut attempt = assignment().attempt;
  attempt.run_id = Some("run-a".into());
  let prepared = json!({"status":"preparing","state":{"run_id":"run-a"}});
  assert!(task_start_request(&attempt, &prepared).unwrap().is_none());
  let aborted = json!({"status":"cancelled","state":{"run_id":"run-a","exit_code":130}});
  assert!(task_start_request(&attempt, &aborted).unwrap().is_none());
  let completed = json!({"status":"completed","state":{"run_id":"run-a",
    "task_started_at":"2026-10-14T01:00:00Z","exit_code":0}});
  let request = task_start_request(&attempt, &completed).unwrap().unwrap();
  assert!(matches!(request, QueueRequest::MarkStarted { run_id, .. } if run_id == "run-a"));
  attempt.started_at_ms = Some(1);
  assert!(task_start_request(&attempt, &completed).unwrap().is_none());
}

#[test]
fn task_start_evidence_rejects_unlinked_runs_and_invalid_timestamps() {
  let mut attempt = assignment().attempt;
  attempt.run_id = Some("run-a".into());
  for (run_id, marker) in [("run-other", "2026-10-14T01:00:00Z"), ("run-a", "invalid")] {
    let status = json!({"state":{"run_id":run_id,"task_started_at":marker}});
    assert!(task_start_request(&attempt, &status).is_err());
  }
}

#[test]
fn heartbeat_cancellation_uses_owned_live_supervisor_without_rewriting_journal() {
  let temporary = tempfile::tempdir().unwrap();
  let worker = temporary.path().canonicalize().unwrap();
  private_directory(&worker.join("attempts")).unwrap();
  let directory = worker.join("attempts/attempt-a");
  private_directory(&directory).unwrap();
  let repo = directory.join("source");
  fs::create_dir_all(repo.join(".expri/runs/run-a/code")).unwrap();
  fs::create_dir(repo.join(".expri/runs/run-a/outputs")).unwrap();
  let run = repo.join(".expri/runs/run-a");
  let mut journal = Journal::claimed(assignment());
  journal.repo_root = Some(repo);
  journal.run_dir = Some(run.clone());
  journal.attempt.run_id = Some("run-a".into());
  journal.stage = Stage::Active;
  journal.save(&directory).unwrap();
  write_json(
    &run.join("run-state.json"),
    &json!({
      "schema_version": 1, "run_id": "run-a", "status": "running", "detached": true,
      "code_dir": run.join("code"), "output_dir": run.join("outputs"),
    }),
  )
  .unwrap();
  let _lease = crate::lock::run_lock(&run).unwrap();
  let before = fs::read(directory.join("journal.json")).unwrap();
  let mut cancellation = journal.attempt.clone();
  cancellation.cancel_requested = true;
  cancellation.run_id = Some("different-run".into());
  assert!(cancel_known_attempt(&worker, &cancellation).is_err());
  assert!(!run.join(".cancel-request").exists());
  cancellation.run_id = Some("run-a".into());
  cancel_known_attempt(&worker, &cancellation).unwrap();
  assert!(run.join(".cancel-request").is_file());
  assert_eq!(fs::read(directory.join("journal.json")).unwrap(), before);
}

#[test]
fn queue_diagnostics_are_control_free_and_bounded_in_bytes() {
  assert_eq!(
    queue_message("provider\nfailed\tbad", 2048),
    "provider failed bad"
  );
  let bounded = queue_message(&"😀".repeat(1000), 511);
  assert!(bounded.len() <= 511);
  assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
}

#[test]
fn uncertain_group_cleanup_cannot_release_a_terminal_reservation() {
  let state = json!({"status":"failed","alive":false,"state":{"queue_cleanup_uncertain":true}});
  assert!(!terminal_status(&state));
  let confirmed = json!({"status":"failed","alive":false,"state":{}});
  assert!(terminal_status(&confirmed));
  let malformed =
    json!({"status":"failed","alive":false,"state":{"queue_cleanup_uncertain":"true"}});
  assert!(!terminal_status(&malformed));
}

#[test]
fn disabling_publishing_preserves_private_asset_credentials_in_native_request() {
  let journal = Journal::claimed(assignment());
  assert!(!journal.assignment.submission.publish);
  let config = std::path::Path::new("/owner/private/config.toml");
  let request = queued_run_request(&journal, "worker-a", config);
  let encoded = serde_json::to_vec(&request).unwrap();
  let native: RunRequest = serde_json::from_slice(&encoded).unwrap();
  let service = native
    .service
    .expect("private asset preparation needs worker credentials");
  assert_eq!(service.client_config, config);
  assert_eq!(service.project_id, "project-a");
  assert_eq!(service.origin, "worker-a");
  assert!(
    !service.publish,
    "asset access must not re-enable result publishing"
  );
}

#[test]
fn a_stopped_executor_does_not_create_storage_or_contact_the_server() {
  let temporary = tempfile::tempdir().unwrap();
  let paths =
    StoragePaths::from_root(temporary.path().canonicalize().unwrap().join("storage")).unwrap();
  let options = ExecutorOptions {
    storage: paths.clone(),
    worker: ResolvedQueueWorkerConfig {
      worker_id: "worker-stopped".into(),
      project_ids: vec!["project-a".into()],
      gpu_busy_memory_threshold_bytes: DEFAULT_GPU_BUSY_MEMORY_THRESHOLD_BYTES,
      capacity: WorkerCapacity {
        cpus: 1,
        memory_bytes: 4096,
        gpus: vec![],
      },
    },
    client_config: temporary.path().join("missing-config.toml"),
    poll_interval: Duration::from_secs(20),
  };
  let mut reports = vec![];
  run_executor(options, Arc::new(AtomicBool::new(true)), |report| {
    reports.push(report);
  })
  .unwrap();
  assert_eq!(reports.last().unwrap()["status"], "stopped");
  assert!(!paths.root().exists());
  assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn an_offline_executor_retries_and_stops_without_waiting_for_its_poll_interval() {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  let address = listener.local_addr().unwrap();
  drop(listener);
  let config = root.join("config.toml");
  fs::write(
    &config,
    format!("url='http://{address}'\ntoken='fixture-private-token'\n"),
  )
  .unwrap();
  let options = ExecutorOptions {
    storage: StoragePaths::from_root(root.join("storage")).unwrap(),
    worker: ResolvedQueueWorkerConfig {
      worker_id: "worker-offline".into(),
      project_ids: vec!["project-a".into()],
      gpu_busy_memory_threshold_bytes: DEFAULT_GPU_BUSY_MEMORY_THRESHOLD_BYTES,
      capacity: WorkerCapacity {
        cpus: 1,
        memory_bytes: 4096,
        gpus: vec![],
      },
    },
    client_config: config,
    poll_interval: Duration::from_secs(20),
  };
  let stop = Arc::new(AtomicBool::new(false));
  let thread_stop = stop.clone();
  let (send, receive) = std::sync::mpsc::channel();
  let executor = thread::spawn(move || {
    run_executor(options, thread_stop, |report| {
      send.send(report).unwrap();
    })
  });
  let waiting = receive.recv_timeout(Duration::from_secs(3)).unwrap();
  assert_eq!(waiting["status"], "waiting");
  assert_eq!(waiting["last_error"], "server_unavailable");
  assert!(!waiting.to_string().contains("fixture-private-token"));
  stop.store(true, Ordering::Release);
  let stopped = receive.recv_timeout(Duration::from_secs(1)).unwrap();
  assert_eq!(stopped["status"], "stopped");
  executor.join().unwrap().unwrap();
}
