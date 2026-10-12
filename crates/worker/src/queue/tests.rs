use super::*;
use expri_core::resource_queue::{
  AttemptIdentity, JobAssignment, JobStatus, JobSubmission, JobSummary, ResourceRequest, SourceRef,
  WorkerCapacity, WorkerGpu,
};
use serde_json::json;

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
  assert_eq!(env["EXPRI_ALLOCATED_MEMORY_BYTES"], "4096");
  allocated.gpus.clear();
  assert_eq!(allocation_env(&allocated)["CUDA_VISIBLE_DEVICES"], "");
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
  let worker = QueueWorkerConfig {
    worker_id: "worker-a".into(),
    project_ids: vec!["project-a".into()],
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
  assert!(!marker.exists());
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
