use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use super::*;

fn fixture() -> (tempfile::TempDir, StoragePaths, PreparedJob) {
  let temporary = tempfile::Builder::new()
    .prefix("expri-service-")
    .tempdir()
    .unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let run = root.join("source/.expri/runs/run-fixture");
  fs::create_dir_all(&run).unwrap();
  fs::write(
    run.join("run-state.json"),
    br#"{"run_id":"run-fixture","status":"running"}"#,
  )
  .unwrap();
  let paths = StoragePaths::from_root(root.join("storage")).unwrap();
  let job = serde_json::from_value(json!({
    "schema_version":1,"client":{"endpoint":"https://example.invalid/v1/request","token":"fixture-private-token"},
    "run_dir":run,"scope":{"project_id":"project","origin":"worker","run_id":"run-fixture"},
    "queue_dir":root.join("queue"),"artifacts":[],"watch":true,"automatic":false,
    "repo_root":null,"dashboard_url":null,
  })).unwrap();
  (temporary, paths, job)
}

fn independent_job_with_same_id(job: &PreparedJob, copy_root: &std::path::Path) -> PreparedJob {
  let run = copy_root.join("run-fixture");
  fs::create_dir_all(&run).unwrap();
  fs::write(
    run.join("run-state.json"),
    br#"{"run_id":"run-fixture","status":"running"}"#,
  )
  .unwrap();
  let mut value = serde_json::to_value(job).unwrap();
  value["run_dir"] = json!(run.canonicalize().unwrap());
  serde_json::from_value(value).unwrap()
}

fn wait_ready(paths: &StoragePaths) {
  let deadline = Instant::now() + Duration::from_secs(3);
  loop {
    if status(paths).unwrap()["collection_ready"] == true {
      return;
    }
    assert!(Instant::now() < deadline, "service never became ready");
    thread::sleep(Duration::from_millis(20));
  }
}

fn collector(context: ServiceContext) -> Result<()> {
  context.collection_ready("machine-fixture".into(), 1);
  while !context.wait(Duration::from_secs(60)) {}
  Ok(())
}

fn wait_component(paths: &StoragePaths, component: ServiceComponent, phase: &str) -> Value {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    let report = status(paths).unwrap();
    if report["components"][component.as_str()]["phase"] == phase {
      return report;
    }
    assert!(
      Instant::now() < deadline,
      "component never became {phase}: {report}"
    );
    thread::sleep(Duration::from_millis(10));
  }
}

fn simple_components() -> ComponentRunners {
  let mut runners = ComponentRunners::new();
  for component in ServiceComponent::ALL {
    if component == ServiceComponent::Publishing {
      continue;
    }
    runners.insert(
      component,
      Arc::new(move |context| {
        if component == ServiceComponent::Collection {
          context.collection_ready("machine-fixture".into(), 1);
        } else {
          context.component_status(json!({"status": "running"}));
        }
        while !context.wait(Duration::from_secs(60)) {}
        Ok(())
      }),
    );
  }
  runners
}

#[test]
fn component_names_accept_cli_spelling_and_serialize_wire_spelling() {
  for component in ServiceComponent::ALL {
    assert_eq!(
      component.to_string().parse::<ServiceComponent>().unwrap(),
      component
    );
    assert_eq!(
      component.as_str().parse::<ServiceComponent>().unwrap(),
      component
    );
    assert_eq!(serde_json::to_value(component).unwrap(), component.as_str());
  }
  assert!("publisher".parse::<ServiceComponent>().is_err());
}

#[test]
fn manual_pause_is_durable_and_lazy_registration_does_not_resume_components() {
  let (_temporary, paths, job) = fixture();
  let service_paths = paths.clone();
  let runner = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(true),
  );
  let first_runner = runner.clone();
  let service = thread::spawn(move || {
    unix::run_components(&service_paths, simple_components(), first_runner, false)
  });
  wait_ready(&paths);
  let pid = status(&paths).unwrap()["pid"].clone();
  stop_component(&paths, ServiceComponent::Publishing).unwrap();
  wait_component(&paths, ServiceComponent::Publishing, "stopped");
  stop_component(&paths, ServiceComponent::Collection).unwrap();
  wait_component(&paths, ServiceComponent::Collection, "stopped");
  assert_eq!(status(&paths).unwrap()["pid"], pid);
  assert!(
    ensure_collection_ready(&paths)
      .unwrap_err()
      .to_string()
      .contains("paused")
  );
  let receipt = register(&paths, job).unwrap();
  let id = receipt["job_id"].as_str().unwrap();
  assert!(receipt["warning"].as_str().unwrap().contains("paused"));
  assert_eq!(observe(&paths, id, false).unwrap()["status"], "paused");
  assert_eq!(job_status(&paths, id).unwrap()["status"], "queued");
  assert_eq!(
    ensure_started(&paths).unwrap()["components"]["publishing"]["desired_running"],
    false
  );
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();

  let service_paths = paths.clone();
  let service =
    thread::spawn(move || unix::run_components(&service_paths, simple_components(), runner, false));
  wait_component(&paths, ServiceComponent::QueueExecution, "running");
  let current = status(&paths).unwrap();
  for component in ["collection", "publishing"] {
    assert_eq!(current["components"][component]["desired_running"], false);
    assert_eq!(current["components"][component]["generation"], 0);
  }
  start_component(&paths, ServiceComponent::Collection).unwrap();
  ensure_collection_ready(&paths).unwrap();
  assert_eq!(
    status(&paths).unwrap()["components"]["publishing"]["desired_running"],
    false
  );
  assert_eq!(job_status(&paths, id).unwrap()["status"], "queued");
  start_component(&paths, ServiceComponent::Publishing).unwrap();
  let deadline = Instant::now() + Duration::from_secs(3);
  while job_status(&paths, id).unwrap()["status"] != "published" {
    assert!(Instant::now() < deadline);
    thread::sleep(Duration::from_millis(10));
  }
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn restarting_component_waits_for_old_generation_while_control_stays_responsive() {
  let (_temporary, paths, _) = fixture();
  let release = Arc::new(AtomicBool::new(false));
  let (entered, started) = mpsc::channel();
  let mut runners = simple_components();
  let release_worker = release.clone();
  runners.insert(
    ServiceComponent::QueueExecution,
    Arc::new(move |context| {
      context.component_status(
        json!({"status": "running", "token": "secret-token", "command": ["private-argument"]}),
      );
      entered.send(context.stop_flag()).unwrap();
      while !context.wait(Duration::from_secs(60)) {}
      while !release_worker.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(10));
      }
      Ok(())
    }),
  );
  let service_paths = paths.clone();
  let service = thread::spawn(move || {
    unix::run_components(
      &service_paths,
      runners,
      Arc::new(|_, _, _| Ok(false)),
      false,
    )
  });
  wait_ready(&paths);
  let first = started.recv_timeout(Duration::from_secs(2)).unwrap();
  stop_component(&paths, ServiceComponent::QueueExecution).unwrap();
  wait_component(&paths, ServiceComponent::QueueExecution, "stopping");
  // The acknowledged mutation is authoritative. Passive status intentionally
  // falls back to an older durable snapshot if its best-effort RPC times out.
  let current = start_component(&paths, ServiceComponent::QueueExecution).unwrap();
  assert_eq!(current["components"]["queue_execution"]["generation"], 1);
  assert_eq!(
    current["components"]["queue_execution"]["desired_running"],
    true
  );
  assert_eq!(current["components"]["collection"]["phase"], "running");
  assert!(started.recv_timeout(Duration::from_millis(100)).is_err());
  assert!(!current.to_string().contains("secret-token"));
  assert!(!current.to_string().contains("private-argument"));
  release.store(true, Ordering::Release);
  let second = started.recv_timeout(Duration::from_secs(2)).unwrap();
  wait_component(&paths, ServiceComponent::QueueExecution, "running");
  assert!(first.load(Ordering::Acquire));
  assert!(!second.load(Ordering::Acquire));
  assert!(!Arc::ptr_eq(&first, &second));
  assert_eq!(
    status(&paths).unwrap()["components"]["queue_execution"]["generation"],
    2
  );
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
  assert!(second.load(Ordering::Acquire));
}

#[test]
fn failed_component_can_restart_without_stopping_its_peers() {
  let (_temporary, paths, _) = fixture();
  let attempts = Arc::new(AtomicUsize::new(0));
  let attempt_worker = attempts.clone();
  let mut runners = simple_components();
  runners.insert(
    ServiceComponent::QueueExecution,
    Arc::new(move |context| {
      if attempt_worker.fetch_add(1, Ordering::SeqCst) == 0 {
        return Err(error("private-credential must never enter status"));
      }
      context.execution_status(json!({"status": "waiting", "online": false}));
      while !context.wait(Duration::from_secs(60)) {}
      Ok(())
    }),
  );
  let service_paths = paths.clone();
  let service = thread::spawn(move || {
    unix::run_components(
      &service_paths,
      runners,
      Arc::new(|_, _, _| Ok(false)),
      false,
    )
  });
  let failed = wait_component(&paths, ServiceComponent::QueueExecution, "failed");
  assert_eq!(failed["ready"], true);
  assert!(!failed.to_string().contains("private-credential"));
  wait_ready(&paths);
  start_component(&paths, ServiceComponent::QueueExecution).unwrap();
  wait_component(&paths, ServiceComponent::QueueExecution, "running");
  assert_eq!(attempts.load(Ordering::SeqCst), 2);
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn all_components_can_stop_without_removing_the_control_process() {
  let (_temporary, paths, _) = fixture();
  let service_paths = paths.clone();
  let service = thread::spawn(move || {
    unix::run_components(
      &service_paths,
      simple_components(),
      Arc::new(|_, _, _| Ok(false)),
      false,
    )
  });
  wait_ready(&paths);
  for component in ServiceComponent::ALL {
    stop_component(&paths, component).unwrap();
    wait_component(&paths, component, "stopped");
  }
  let current = ensure_started(&paths).unwrap();
  assert_eq!(current["running"], true);
  assert_eq!(current["ready"], true);
  assert_eq!(current["collection_ready"], false);
  start_component(&paths, ServiceComponent::Collection).unwrap();
  wait_ready(&paths);
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn offline_pause_saves_only_control_preferences_without_launching_a_process() {
  let (_temporary, paths, _) = fixture();
  let report = stop_component(&paths, ServiceComponent::QueueExecution).unwrap();
  assert_eq!(report["running"], false);
  assert_eq!(
    report["components"]["queue_execution"]["desired_running"],
    false
  );
  let directory = files::existing(&paths).unwrap().unwrap();
  assert!(!directory.join("service.lock").exists());
  assert!(!directory.join("service.log").exists());
  assert!(!directory.join("jobs").exists());
  let saved: Value =
    serde_json::from_slice(&fs::read(paths.root().join("service.json")).unwrap()).unwrap();
  assert_eq!(saved, json!({"components": {"queue_execution": "disable"}}));
  assert!(!directory.join("components.json").exists());
}

#[test]
fn readonly_status_honors_service_json_without_creating_worker_service_storage() {
  use std::os::unix::fs::PermissionsExt;
  let (_temporary, paths, _) = fixture();
  paths.ensure_private().unwrap();
  let path = paths.root().join("service.json");
  fs::write(&path, br#"{"components":{"publishing":"disable"}}"#).unwrap();
  fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
  let before = fs::read(&path).unwrap();
  let report = status(&paths).unwrap();
  assert_eq!(report["components"]["publishing"]["desired_running"], false);
  assert_eq!(report["components"]["collection"]["desired_running"], true);
  assert_eq!(
    report["components"]["queue_execution"]["desired_running"],
    true
  );
  assert!(!paths.root().join("worker-service").exists());
  assert_eq!(fs::read(&path).unwrap(), before);
  assert_eq!(fs::read_dir(paths.root()).unwrap().count(), 1);
  assert_eq!(
    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
    0o644
  );
  stop_component(&paths, ServiceComponent::Collection).unwrap();
  let updated: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
  assert_eq!(
    updated,
    json!({"components": {"collection": "disable", "publishing": "disable"}})
  );
  assert_eq!(
    fs::metadata(path).unwrap().permissions().mode() & 0o777,
    0o600
  );
}

#[test]
fn saved_service_choices_require_enable_disable_strings_and_known_components() {
  let (_temporary, paths, _) = fixture();
  paths.ensure_private().unwrap();
  for value in [
    json!({"components": {"publishing": false}}),
    json!({"components": {"publishing": "disabled"}}),
    json!({"components": {"invalid": "disable"}}),
    json!({"schema_version": 1, "components": {}}),
  ] {
    files::write(&paths.root().join("service.json"), &value).unwrap();
    assert!(status(&paths).is_err());
    assert!(!paths.root().join("worker-service").exists());
  }
}

#[test]
fn explicit_component_restart_forces_healthy_generation_and_enables_a_disabled_group() {
  let (_temporary, paths, _) = fixture();
  let service_paths = paths.clone();
  let service = thread::spawn(move || {
    unix::run_components(
      &service_paths,
      simple_components(),
      Arc::new(|_, _, _| Ok(false)),
      false,
    )
  });
  wait_ready(&paths);
  stop_component(&paths, ServiceComponent::Publishing).unwrap();
  wait_component(&paths, ServiceComponent::Publishing, "stopped");
  let initial = status(&paths).unwrap();
  let generation = initial["components"]["collection"]["generation"]
    .as_u64()
    .unwrap();
  let report = restart_component(&paths, ServiceComponent::Collection).unwrap();
  assert_eq!(report["restarting"], true);
  let deadline = Instant::now() + Duration::from_secs(3);
  loop {
    let report = status(&paths).unwrap();
    if report["components"]["collection"]["generation"] == generation + 1
      && report["collection_ready"] == true
    {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "collection did not restart: {report}"
    );
    thread::sleep(Duration::from_millis(10));
  }
  let current = status(&paths).unwrap();
  assert_eq!(current["pid"], initial["pid"]);
  assert_eq!(
    current["components"]["queue_execution"]["generation"],
    initial["components"]["queue_execution"]["generation"]
  );
  assert_eq!(
    current["components"]["publishing"]["desired_running"],
    false
  );
  restart_component(&paths, ServiceComponent::Publishing).unwrap();
  wait_component(&paths, ServiceComponent::Publishing, "running");
  let saved: Value =
    serde_json::from_slice(&fs::read(paths.root().join("service.json")).unwrap()).unwrap();
  assert_eq!(
    saved,
    json!({"components": {"collection": "enable", "publishing": "enable"}})
  );
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn whole_restart_waits_for_old_lease_and_preserves_choices_jobs_and_training() {
  let (temporary, paths, job) = fixture();
  let receipt = registry::register(&paths, job).unwrap();
  let id = receipt["job_id"].as_str().unwrap();
  let release = Arc::new(AtomicBool::new(false));
  let stopped = Arc::new(AtomicBool::new(false));
  let mut runners = simple_components();
  let release_old = release.clone();
  let stopped_old = stopped.clone();
  runners.insert(
    ServiceComponent::Collection,
    Arc::new(move |context| {
      context.collection_ready("machine-fixture".into(), 1);
      while !context.wait(Duration::from_secs(60)) {}
      while !release_old.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(10));
      }
      stopped_old.store(true, Ordering::Release);
      Ok(())
    }),
  );
  let runner = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(false),
  );
  let service_paths = paths.clone();
  let service = thread::spawn(move || unix::run_components(&service_paths, runners, runner, false));
  wait_ready(&paths);
  stop_component(&paths, ServiceComponent::Publishing).unwrap();
  wait_component(&paths, ServiceComponent::Publishing, "stopped");
  let before = fs::read(paths.root().join("service.json")).unwrap();
  let training_dir = temporary.path().canonicalize().unwrap().join("training");
  fs::create_dir(&training_dir).unwrap();
  let _training = expri_core::lock::run_lock(&training_dir).unwrap();
  let launches = Arc::new(AtomicUsize::new(0));
  let new_service = Arc::new(std::sync::Mutex::new(None));
  let restart_paths = paths.clone();
  let observed_launches = launches.clone();
  let new_service_handle = new_service.clone();
  let stopped_old = stopped.clone();
  let restarting = thread::spawn(move || {
    unix::restart_with_launcher(
      &restart_paths,
      move |_, paths| {
        assert!(stopped_old.load(Ordering::Acquire));
        assert!(matches!(
          expri_core::lock::try_lock_file_readonly(
            &paths.root().join("worker-service/service.lock")
          )
          .unwrap(),
          expri_core::lock::LockAttempt::Acquired(_)
        ));
        observed_launches.fetch_add(1, Ordering::SeqCst);
        let new_paths = paths.clone();
        *new_service_handle.lock().unwrap() = Some(thread::spawn(move || {
          unix::run_components(
            &new_paths,
            simple_components(),
            Arc::new(|_, _, _| Ok(false)),
            false,
          )
        }));
        Ok(None)
      },
      Duration::from_secs(5),
    )
  });
  wait_component(&paths, ServiceComponent::Collection, "stopping");
  assert_eq!(launches.load(Ordering::SeqCst), 0);
  let lazy_paths = paths.clone();
  let lazy = thread::spawn(move || {
    unix::ensure_with_launcher(&lazy_paths, |_, _| {
      panic!("restart owns the only permitted launch")
    })
  });
  release.store(true, Ordering::Release);
  let restarted = restarting.join().unwrap().unwrap();
  assert_eq!(restarted["restarted"], true);
  assert_eq!(lazy.join().unwrap().unwrap()["ready"], true);
  service.join().unwrap().unwrap();
  assert_eq!(launches.load(Ordering::SeqCst), 1);
  assert_eq!(fs::read(paths.root().join("service.json")).unwrap(), before);
  assert_eq!(
    status(&paths).unwrap()["components"]["publishing"]["desired_running"],
    false
  );
  assert_eq!(job_status(&paths, id).unwrap()["status"], "queued");
  assert!(matches!(
    expri_core::lock::try_lock_file_readonly(&training_dir.join(".run.lock")).unwrap(),
    expri_core::lock::LockAttempt::Busy
  ));
  stop(&paths).unwrap();
  new_service
    .lock()
    .unwrap()
    .take()
    .unwrap()
    .join()
    .unwrap()
    .unwrap();
}

#[test]
fn whole_restart_timeout_does_not_launch_while_old_component_is_still_draining() {
  let (_temporary, paths, _) = fixture();
  let release = Arc::new(AtomicBool::new(false));
  let old_release = release.clone();
  let mut runners = simple_components();
  runners.insert(
    ServiceComponent::Collection,
    Arc::new(move |context| {
      context.collection_ready("machine-fixture".into(), 1);
      while !context.wait(Duration::from_secs(60)) {}
      while !old_release.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(10));
      }
      Ok(())
    }),
  );
  let service_paths = paths.clone();
  let service = thread::spawn(move || {
    unix::run_components(
      &service_paths,
      runners,
      Arc::new(|_, _, _| Ok(false)),
      false,
    )
  });
  wait_ready(&paths);
  let failure = unix::restart_with_launcher(
    &paths,
    |_, _| panic!("a draining process must never be replaced"),
    Duration::from_millis(100),
  )
  .unwrap_err();
  assert!(
    failure
      .to_string()
      .contains("timed out waiting for shutdown")
  );
  assert!(matches!(
    expri_core::lock::try_lock_file_readonly(&paths.root().join("worker-service/service.lock"))
      .unwrap(),
    expri_core::lock::LockAttempt::Busy
  ));
  release.store(true, Ordering::Release);
  service.join().unwrap().unwrap();
}

#[test]
fn inactive_profile_does_not_disable_or_restart_a_healthy_publishing_group() {
  let (_temporary, paths, job) = fixture();
  let receipt = registry::register(&paths, job).unwrap();
  let id = receipt["job_id"].as_str().unwrap();
  let mut runners = simple_components();
  runners.insert(
    ServiceComponent::Publishing,
    Arc::new(|context| {
      context.component_status(json!({"status": "unconfigured"}));
      Ok(())
    }),
  );
  let (entered, started) = mpsc::channel();
  let runner = Arc::new(
    move |_: &PreparedJob, stop: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| {
      entered.send(stop.clone()).unwrap();
      while !stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(10));
      }
      Ok(false)
    },
  );
  let service_paths = paths.clone();
  let service = thread::spawn(move || unix::run_components(&service_paths, runners, runner, false));
  let token = started.recv_timeout(Duration::from_secs(3)).unwrap();
  wait_component(&paths, ServiceComponent::Publishing, "running");
  let report = start_component(&paths, ServiceComponent::Publishing).unwrap();
  assert_eq!(report["components"]["publishing"]["generation"], 1);
  assert_eq!(
    report["components"]["publishing"]["parts"]["publishing"]["phase"],
    "unconfigured"
  );
  assert!(!token.load(Ordering::Acquire));
  stop_component(&paths, ServiceComponent::Publishing).unwrap();
  wait_component(&paths, ServiceComponent::Publishing, "stopped");
  assert!(token.load(Ordering::Acquire));
  assert_eq!(job_status(&paths, id).unwrap()["status"], "queued");
  assert_eq!(status(&paths).unwrap()["collection_ready"], true);
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn failed_publishing_part_preserves_peers_and_explicit_start_retries_the_group() {
  let (_temporary, paths, _) = fixture();
  let attempts = Arc::new(AtomicUsize::new(0));
  let observed = attempts.clone();
  let mut runners = simple_components();
  runners.insert(
    ServiceComponent::Publishing,
    Arc::new(move |context| {
      if observed.fetch_add(1, Ordering::SeqCst) == 0 {
        return Err(error("private token"));
      }
      context.component_status(json!({"status": "running"}));
      while !context.wait(Duration::from_secs(60)) {}
      Ok(())
    }),
  );
  let service_paths = paths.clone();
  let service = thread::spawn(move || {
    unix::run_components(
      &service_paths,
      runners,
      Arc::new(|_, _, _| Ok(false)),
      false,
    )
  });
  let degraded = wait_component(&paths, ServiceComponent::Publishing, "degraded");
  assert_eq!(
    degraded["components"]["publishing"]["parts"]["run_publishing"]["phase"],
    "running"
  );
  assert!(!degraded.to_string().contains("private token"));
  start_component(&paths, ServiceComponent::Publishing).unwrap();
  let running = wait_component(&paths, ServiceComponent::Publishing, "running");
  assert_eq!(running["components"]["publishing"]["generation"], 2);
  assert_eq!(attempts.load(Ordering::SeqCst), 2);
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn collection_control_cancels_and_restarts_both_sampler_and_export_threads() {
  let (_temporary, paths, _) = fixture();
  let mut runners = simple_components();
  let (entered, started) = mpsc::channel();
  runners.add(
    ServiceComponent::Collection,
    "capture_export",
    Arc::new(move |context| {
      context.component_status(json!({"status": "running"}));
      entered.send(context.stop_flag()).unwrap();
      while !context.wait(Duration::from_secs(60)) {}
      Ok(())
    }),
  );
  // Replacing the sampler must preserve the named exporter hook.
  runners.insert(ServiceComponent::Collection, Arc::new(collector));
  let service_paths = paths.clone();
  let service = thread::spawn(move || {
    unix::run_components(
      &service_paths,
      runners,
      Arc::new(|_, _, _| Ok(false)),
      false,
    )
  });
  wait_ready(&paths);
  let first = started.recv_timeout(Duration::from_secs(3)).unwrap();
  stop_component(&paths, ServiceComponent::Collection).unwrap();
  let stopped = wait_component(&paths, ServiceComponent::Collection, "stopped");
  for part in ["collection", "capture_export"] {
    assert_eq!(
      stopped["components"]["collection"]["parts"][part]["phase"],
      "stopped"
    );
  }
  assert!(first.load(Ordering::Acquire));
  assert_eq!(stopped["collection_ready"], false);
  assert_eq!(stopped["components"]["publishing"]["phase"], "running");
  start_component(&paths, ServiceComponent::Collection).unwrap();
  let second = started.recv_timeout(Duration::from_secs(3)).unwrap();
  ensure_collection_ready(&paths).unwrap();
  assert!(!Arc::ptr_eq(&first, &second));
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn offline_pause_racing_foreground_bootstrap_is_never_lost() {
  for _ in 0..8 {
    let (_temporary, paths, _) = fixture();
    files::ensure(&paths).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let boot = barrier.clone();
    let service_paths = paths.clone();
    let service = thread::spawn(move || {
      boot.wait();
      unix::run_components(
        &service_paths,
        simple_components(),
        Arc::new(|_, _, _| Ok(false)),
        false,
      )
    });
    barrier.wait();
    stop_component(&paths, ServiceComponent::Collection).unwrap();
    wait_component(&paths, ServiceComponent::QueueExecution, "running");
    let paused = wait_component(&paths, ServiceComponent::Collection, "stopped");
    assert_eq!(paused["components"]["collection"]["desired_running"], false);
    assert!(
      ensure_collection_ready(&paths)
        .unwrap_err()
        .to_string()
        .contains("paused")
    );
    stop(&paths).unwrap();
    service.join().unwrap().unwrap();
  }
}

#[test]
fn executor_status_is_independent_of_collection_and_excludes_credentials() {
  let context = ServiceContext::new();
  context.collection_ready("machine-fixture".into(), 1);
  context.execution_status(json!({
    "status": "waiting", "worker_id": "worker-fixture", "online": false,
    "gpu_busy_memory_threshold_bytes": 67108864,
    "unavailable_gpus": [{"device_id": "GPU-fixture", "reason": "busy"}],
    "gpu_availability_observed_at_ms": 123,
    "last_error": "server_unavailable", "token": "private-token",
    "client_config": "/private/config.toml", "command": ["private-task-argument"],
  }));
  let snapshot = unix::snapshot(&context, 0, 0);
  assert_eq!(snapshot["ready"], true);
  assert_eq!(snapshot["collection"]["machine_id"], "machine-fixture");
  assert_eq!(snapshot["execution"]["status"], "waiting");
  assert_eq!(snapshot["execution"]["online"], false);
  assert_eq!(
    snapshot["execution"]["gpu_busy_memory_threshold_bytes"],
    67108864
  );
  assert_eq!(
    snapshot["execution"]["unavailable_gpus"][0]["device_id"],
    "GPU-fixture"
  );
  assert_eq!(
    snapshot["execution"]["gpu_availability_observed_at_ms"],
    123
  );
  let encoded = snapshot.to_string();
  for secret in [
    "private-token",
    "/private/config.toml",
    "private-task-argument",
  ] {
    assert!(!encoded.contains(secret));
  }
}

#[test]
fn status_is_readonly_and_does_not_create_storage() {
  let (_temporary, paths, _) = fixture();
  assert_eq!(status(&paths).unwrap()["running"], false);
  assert_eq!(stop(&paths).unwrap()["already_stopped"], true);
  assert!(!paths.root().exists());
}

#[test]
fn crashed_process_snapshot_cannot_report_running_or_ready_component_parts() {
  let (_temporary, paths, _) = fixture();
  let directory = files::ensure(&paths).unwrap();
  let snapshot = json!({
    "running": true, "ready": true, "collection_ready": true, "status": "running",
    "collection": {"status": "collecting", "machine_id": "machine-fixture"},
    "execution": {"status": "waiting", "online": true},
    "components": {
      "collection": {"generation": 3, "phase": "running", "ready": true, "parts": {
        "collection": {"phase": "running", "status": "collecting", "ready": true},
        "capture_export": {"phase": "failed", "status": "failed", "last_error": "saved export error"}
      }},
      "publishing": {"generation": 2, "phase": "starting", "parts": {
        "publishing": {"phase": "unconfigured", "status": "unconfigured", "ready": false},
        "run_publishing": {"phase": "starting", "status": "starting", "ready": true}
      }},
      "queue_execution": {"generation": 1, "phase": "stopping", "parts": {
        "queue_execution": {"phase": "stopping", "status": "waiting", "ready": true, "online": true}
      }}
    }
  });
  let status_path = directory.join("status.json");
  files::write(&status_path, &snapshot).unwrap();
  let before = fs::read(&status_path).unwrap();
  let report = status(&paths).unwrap();
  assert_eq!(report["running"], false);
  assert_eq!(report["ready"], false);
  assert_eq!(report["collection_ready"], false);
  assert_eq!(report["collection"]["status"], "stopped");
  assert_eq!(report["execution"]["online"], false);
  for component in ServiceComponent::ALL {
    assert_eq!(report["components"][component.as_str()]["phase"], "stopped");
    for part in report["components"][component.as_str()]["parts"]
      .as_object()
      .unwrap()
      .values()
    {
      assert_eq!(part["ready"], false);
      assert!(!matches!(
        part["phase"].as_str(),
        Some("starting" | "running" | "stopping")
      ));
    }
  }
  assert_eq!(
    report["components"]["collection"]["parts"]["capture_export"]["phase"],
    "failed"
  );
  assert_eq!(
    report["components"]["collection"]["parts"]["capture_export"]["last_error"],
    "saved export error"
  );
  assert_eq!(
    report["components"]["publishing"]["parts"]["publishing"]["phase"],
    "unconfigured"
  );
  assert_eq!(
    report["components"]["queue_execution"]["parts"]["queue_execution"]["online"],
    false
  );
  assert_eq!(fs::read(status_path).unwrap(), before);
  assert!(!directory.join("service.lock").exists());
}

#[test]
fn held_process_lease_cannot_make_cached_status_ready_without_a_live_handshake() {
  let (_temporary, paths, _) = fixture();
  let directory = files::ensure(&paths).unwrap();
  let lease_path = directory.join("service.lock");
  files::private_file(&lease_path).unwrap();
  let expri_core::lock::LockAttempt::Acquired(_lease) =
    expri_core::lock::try_lock_file(&lease_path, false).unwrap()
  else {
    panic!("fixture needs its lease");
  };
  let path = directory.join("status.json");
  files::write(
    &path,
    &json!({"pid": 1234567, "running": true, "ready": true,
    "collection_ready": true, "status": "running", "collection": {"machine_id": "cached-machine"}}),
  )
  .unwrap();
  let before = fs::read(&path).unwrap();
  let report = status(&paths).unwrap();
  assert_eq!(report["running"], true);
  assert_eq!(report["ready"], false);
  assert_eq!(report["collection_ready"], false);
  assert_eq!(report["collection"]["machine_id"], "cached-machine");
  assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn registration_is_durable_private_idempotent_and_conflicts_cannot_replace_credentials() {
  use std::os::unix::fs::PermissionsExt;
  let (_temporary, paths, job) = fixture();
  let first = registry::register(&paths, job.clone()).unwrap();
  let repeated = registry::register(&paths, job.clone()).unwrap();
  assert_eq!(first["job_id"], repeated["job_id"]);
  assert_eq!(repeated["already_registered"], true);
  let id = first["job_id"].as_str().unwrap();
  let directory = files::existing(&paths)
    .unwrap()
    .unwrap()
    .join("jobs")
    .join(id);
  assert_eq!(
    fs::metadata(directory.join("request.json"))
      .unwrap()
      .permissions()
      .mode()
      & 0o777,
    0o600
  );
  assert_eq!(
    fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
    0o700
  );
  assert!(!first.to_string().contains("fixture-private-token"));
  assert!(
    !job_status(&paths, id)
      .unwrap()
      .to_string()
      .contains("fixture-private-token")
  );
  let request_before = fs::read(directory.join("request.json")).unwrap();
  let mut changed = serde_json::to_value(&job).unwrap();
  changed["client"]["token"] = json!("different-private-token");
  assert!(registry::register(&paths, serde_json::from_value(changed).unwrap()).is_err());
  assert_eq!(
    fs::read(directory.join("request.json")).unwrap(),
    request_before
  );
  assert!(registry::read_job(&directory).unwrap() == job);
}

#[test]
fn continuation_rearm_keeps_the_job_and_rejects_an_old_slices_final_result() {
  let (_temporary, paths, job) = fixture();
  let registered = registry::register(&paths, job.clone()).unwrap();
  let id = registered["job_id"].as_str().unwrap();
  let registry = files::existing(&paths).unwrap().unwrap();
  let directory = registry.join("jobs").join(id);
  let mut old = registry::read_state(&directory).unwrap();
  old.status = "publishing".into();
  old.save(&directory).unwrap();
  fs::write(
    job.run_dir().join("run-state.json"),
    br#"{"run_id":"run-fixture","status":"preparing","publication_epoch":1}"#,
  )
  .unwrap();
  registry::rearm(&paths, &job).unwrap();
  old.status = "published".into();
  old.progress = json!({"stale":true});
  assert!(!old.save_if_current(&directory).unwrap());
  let new = registry::read_state(&directory).unwrap();
  assert_eq!(new.status, "queued");
  assert_eq!(new.publication_epoch, 1);
  assert!(new.progress.get("stale").is_none());
  assert!(registry::read_job(&directory).unwrap() == job);
  assert_eq!(registry::pending(&registry).unwrap(), vec![directory]);
}

#[test]
fn credentials_never_enter_a_source_checkout() {
  let (temporary, _, job) = fixture();
  let repo = temporary.path().join("source");
  fs::create_dir(repo.join(".git")).unwrap();
  let paths = StoragePaths::from_root(repo.join("private-storage")).unwrap();
  assert!(
    registry::register(&paths, job)
      .unwrap_err()
      .to_string()
      .contains("outside")
  );
  assert!(!paths.root().join("worker-service/jobs").exists());
}

#[test]
fn missing_public_state_is_rebuilt_from_durable_registration_without_exposing_secrets() {
  let (_temporary, paths, job) = fixture();
  let report = registry::register(&paths, job).unwrap();
  let directory = files::existing(&paths).unwrap().unwrap();
  let path = directory
    .join("jobs")
    .join(report["job_id"].as_str().unwrap());
  fs::remove_file(path.join("state.json")).unwrap();
  assert_eq!(registry::pending(&directory).unwrap(), vec![path]);
  assert_eq!(
    job_status(&paths, report["job_id"].as_str().unwrap()).unwrap()["status"],
    "queued"
  );
}

#[test]
fn concurrent_jobs_have_independent_bounded_slots_and_stop_preserves_training() {
  let (temporary, paths, job) = fixture();
  let mut ids = Vec::new();
  for index in 0..7 {
    let run = temporary
      .path()
      .canonicalize()
      .unwrap()
      .join(format!("source/.expri/runs/run-{index}"));
    fs::create_dir_all(&run).unwrap();
    fs::write(
      run.join("run-state.json"),
      format!("{{\"run_id\":\"run-{index}\",\"status\":\"running\"}}"),
    )
    .unwrap();
    let mut value = serde_json::to_value(&job).unwrap();
    value["run_dir"] = json!(run);
    value["scope"]["run_id"] = json!(format!("run-{index}"));
    let report = registry::register(&paths, serde_json::from_value(value).unwrap()).unwrap();
    ids.push(report["job_id"].as_str().unwrap().to_owned());
  }
  let active = Arc::new(AtomicUsize::new(0));
  let peak = Arc::new(AtomicUsize::new(0));
  let completed = Arc::new(AtomicUsize::new(0));
  let (active_job, peak_job, completed_job) = (active.clone(), peak.clone(), completed.clone());
  let runner = Arc::new(
    move |_: &PreparedJob, _: Arc<AtomicBool>, progress: &mut dyn FnMut(Value) -> Result<()>| {
      let count = active_job.fetch_add(1, Ordering::SeqCst) + 1;
      peak_job.fetch_max(count, Ordering::SeqCst);
      thread::sleep(Duration::from_millis(200));
      progress(json!({"confirmed": true}))?;
      active_job.fetch_sub(1, Ordering::SeqCst);
      completed_job.fetch_add(1, Ordering::SeqCst);
      Ok(true)
    },
  );
  let training_dir = temporary
    .path()
    .canonicalize()
    .unwrap()
    .join("live-training");
  fs::create_dir(&training_dir).unwrap();
  let _training = expri_core::lock::run_lock(&training_dir).unwrap();
  let training_before = fs::read(training_dir.join(".run.lock")).unwrap();
  let service_paths = paths.clone();
  let service = thread::spawn(move || unix::run_runtime(&service_paths, collector, runner, false));
  wait_ready(&paths);
  let deadline = Instant::now() + Duration::from_secs(5);
  while completed.load(Ordering::SeqCst) != ids.len() {
    assert!(Instant::now() < deadline, "queued jobs were starved");
    thread::sleep(Duration::from_millis(20));
  }
  assert!(peak.load(Ordering::SeqCst) >= 2);
  assert!(peak.load(Ordering::SeqCst) <= 4);
  assert_eq!(stop(&paths).unwrap()["training_preserved"], true);
  service.join().unwrap().unwrap();
  assert_eq!(status(&paths).unwrap()["running"], false);
  for id in ids {
    assert_eq!(job_status(&paths, &id).unwrap()["status"], "published");
  }
  assert_eq!(
    fs::read(training_dir.join(".run.lock")).unwrap(),
    training_before
  );
  assert!(matches!(
    expri_core::lock::try_lock_file(&training_dir.join(".run.lock"), false).unwrap(),
    expri_core::lock::LockAttempt::Busy
  ));
}

#[test]
fn duplicate_foreground_refuses_and_existing_ready_start_is_concurrent_idempotent() {
  let (_temporary, paths, _) = fixture();
  let service_paths = paths.clone();
  let runner = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(false),
  );
  let service = thread::spawn(move || unix::run_runtime(&service_paths, collector, runner, false));
  wait_ready(&paths);
  let duplicate_runner = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(false),
  );
  let failure = unix::run_runtime(&paths, collector, duplicate_runner, false).unwrap_err();
  assert!(failure.to_string().contains("already running"));
  let starts: Vec<_> = (0..8)
    .map(|_| {
      let paths = paths.clone();
      thread::spawn(move || ensure_started(&paths))
    })
    .collect();
  for start in starts {
    assert_eq!(start.join().unwrap().unwrap()["ready"], true);
  }
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn stopped_inflight_job_remains_durable_and_resumes_after_restart() {
  let (_temporary, paths, job) = fixture();
  let report = registry::register(&paths, job).unwrap();
  let id = report["job_id"].as_str().unwrap().to_owned();
  let (entered, started) = mpsc::channel();
  let runner = Arc::new(
    move |_: &PreparedJob, stop: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| {
      entered.send(()).unwrap();
      while !stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(10));
      }
      Ok(false)
    },
  );
  let service_paths = paths.clone();
  let service = thread::spawn(move || unix::run_runtime(&service_paths, collector, runner, false));
  wait_ready(&paths);
  started.recv_timeout(Duration::from_secs(2)).unwrap();
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
  assert_eq!(job_status(&paths, &id).unwrap()["status"], "queued");
  let service_paths = paths.clone();
  let done = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(true),
  );
  let service = thread::spawn(move || unix::run_runtime(&service_paths, collector, done, false));
  wait_ready(&paths);
  let deadline = Instant::now() + Duration::from_secs(3);
  while job_status(&paths, &id).unwrap()["status"] != "published" {
    assert!(Instant::now() < deadline);
    thread::sleep(Duration::from_millis(20));
  }
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn long_storage_paths_use_a_private_short_socket() {
  let (temporary, _, _) = fixture();
  let paths = StoragePaths::from_root(
    temporary
      .path()
      .canonicalize()
      .unwrap()
      .join("a-long-custom-storage-name".repeat(5)),
  )
  .unwrap();
  let service_paths = paths.clone();
  let runner = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(false),
  );
  let service = thread::spawn(move || unix::run_runtime(&service_paths, collector, runner, false));
  wait_ready(&paths);
  assert_eq!(
    status(&paths).unwrap()["collection"]["machine_id"],
    "machine-fixture"
  );
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn concurrent_cold_start_launches_one_service_after_committed_collection() {
  let (_temporary, paths, _) = fixture();
  let launches = Arc::new(AtomicUsize::new(0));
  let service_thread = Arc::new(std::sync::Mutex::new(None));
  let starts: Vec<_> = (0..8)
    .map(|_| {
      let paths = paths.clone();
      let launches = launches.clone();
      let service_thread = service_thread.clone();
      thread::spawn(move || {
        unix::ensure_with_launcher(&paths, move |_, paths| {
          launches.fetch_add(1, Ordering::SeqCst);
          let service_paths = paths.clone();
          let runner = Arc::new(
            |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(false),
          );
          *service_thread.lock().unwrap() = Some(thread::spawn(move || {
            unix::run_runtime(
              &service_paths,
              |context| {
                thread::sleep(Duration::from_millis(100));
                collector(context)
              },
              runner,
              false,
            )
          }));
          Ok(None)
        })
      })
    })
    .collect();
  for start in starts {
    assert_eq!(start.join().unwrap().unwrap()["ready"], true);
  }
  assert_eq!(launches.load(Ordering::SeqCst), 1);
  stop(&paths).unwrap();
  service_thread
    .lock()
    .unwrap()
    .take()
    .unwrap()
    .join()
    .unwrap()
    .unwrap();
}

#[test]
fn stopped_status_exposes_bounded_jobs_and_errors_without_reading_secret_requests() {
  use std::os::unix::fs::PermissionsExt;
  let (temporary, paths, job) = fixture();
  let mut ids = Vec::new();
  for index in 0..35 {
    let copy = independent_job_with_same_id(
      &job,
      &temporary
        .path()
        .canonicalize()
        .unwrap()
        .join(format!("copy-{index}")),
    );
    let mut value = serde_json::to_value(copy).unwrap();
    value["scope"]["origin"] = json!(format!("worker-{index}"));
    let registered = registry::register(&paths, serde_json::from_value(value).unwrap()).unwrap();
    ids.push(registered["job_id"].as_str().unwrap().to_owned());
  }
  let registry = files::existing(&paths).unwrap().unwrap();
  let failed_path = registry.join("jobs").join(&ids[34]);
  let mut failed = registry::read_state(&failed_path).unwrap();
  failed.status = "error".into();
  failed.last_error = Some("Publishing permission was rejected".into());
  failed.save(&failed_path).unwrap();
  let state_bytes = fs::read(failed_path.join("state.json")).unwrap();
  fs::set_permissions(
    failed_path.join("state.json"),
    fs::Permissions::from_mode(0o400),
  )
  .unwrap();
  // Status uses only safe public state, so it works even when the private
  // request is unreadable/corrupt and cannot rewrite read-only job state.
  fs::write(
    failed_path.join("request.json"),
    b"private request deliberately unavailable",
  )
  .unwrap();
  let report = status(&paths).unwrap();
  assert_eq!(report["running"], false);
  assert_eq!(report["jobs_total"], 35);
  assert_eq!(report["jobs"].as_array().unwrap().len(), 32);
  assert_eq!(report["jobs_truncated"], 3);
  assert_eq!(report["job_counts"]["error"], 1);
  assert_eq!(report["job_counts"]["queued"], 34);
  assert!(
    report["jobs"]
      .as_array()
      .unwrap()
      .iter()
      .any(|job| job["status"] == "error")
  );
  assert!(!report.to_string().contains("fixture-private-token"));
  assert_eq!(
    fs::read(failed_path.join("state.json")).unwrap(),
    state_bytes
  );
  assert_eq!(
    fs::metadata(failed_path.join("state.json"))
      .unwrap()
      .permissions()
      .mode()
      & 0o777,
    0o400
  );
  assert!(!registry.join("service.lock").exists());
}

#[test]
fn collection_failure_is_isolated_and_preserved_after_service_shutdown() {
  let (_temporary, paths, _) = fixture();
  let runner = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(false),
  );
  let service_paths = paths.clone();
  let service = thread::spawn(move || {
    unix::run_runtime(
      &service_paths,
      |context| {
        context.collection_status(
          json!({"status": "failed", "last_error": "Local profile store is unavailable"}),
        );
        context.stop_flag().store(true, Ordering::Release);
        Err(error("local profile store failure"))
      },
      runner,
      false,
    )
  });
  wait_component(&paths, ServiceComponent::Collection, "failed");
  let running = status(&paths).unwrap();
  assert_eq!(running["running"], true);
  assert_eq!(running["ready"], true);
  assert_eq!(running["collection_ready"], false);
  wait_component(&paths, ServiceComponent::Publishing, "running");
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
  let report = status(&paths).unwrap();
  assert_eq!(report["running"], false);
  assert_eq!(report["ready"], false);
  assert_eq!(report["status"], "stopped");
  assert_eq!(
    report["collection"]["last_error"],
    "Local profile store is unavailable"
  );
}

#[test]
fn corrupt_saved_job_retries_slowly_while_other_jobs_continue() {
  let (temporary, paths, job) = fixture();
  let corrupt = registry::register(&paths, job.clone()).unwrap();
  let id = corrupt["job_id"].as_str().unwrap();
  let directory = files::existing(&paths).unwrap().unwrap();
  fs::write(
    directory.join("jobs").join(id).join("request.json"),
    b"invalid private request",
  )
  .unwrap();
  let healthy_copy = independent_job_with_same_id(
    &job,
    &temporary
      .path()
      .canonicalize()
      .unwrap()
      .join("healthy-copy"),
  );
  let mut healthy = serde_json::to_value(healthy_copy).unwrap();
  healthy["scope"]["origin"] = json!("healthy-worker");
  let healthy = registry::register(&paths, serde_json::from_value(healthy).unwrap()).unwrap();
  let healthy_id = healthy["job_id"].as_str().unwrap();
  let runner = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(true),
  );
  let service_paths = paths.clone();
  let service = thread::spawn(move || unix::run_runtime(&service_paths, collector, runner, false));
  wait_ready(&paths);
  let deadline = Instant::now() + Duration::from_secs(3);
  while job_status(&paths, healthy_id).unwrap()["status"] != "published"
    || job_status(&paths, id).unwrap()["status"] != "retrying"
  {
    assert!(Instant::now() < deadline);
    thread::sleep(Duration::from_millis(20));
  }
  thread::sleep(Duration::from_millis(250));
  let corrupt_state = registry::read_state(&directory.join("jobs").join(id)).unwrap();
  assert_eq!(corrupt_state.failures, 1);
  assert!(
    corrupt_state
      .last_error
      .unwrap()
      .contains("Saved publishing registration")
  );
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn resume_uses_saved_credentials_and_rejects_ambiguous_destinations() {
  let (_temporary, paths, job) = fixture();
  let run_dir = job.run_dir().to_path_buf();
  let report = registry::register(&paths, job.clone()).unwrap();
  let id = report["job_id"].as_str().unwrap();
  let directory = files::existing(&paths)
    .unwrap()
    .unwrap()
    .join("jobs")
    .join(id);
  let mut state = registry::read_state(&directory).unwrap();
  state.status = "error".into();
  state.last_error = Some("Transient caller context disappeared".into());
  state.save(&directory).unwrap();
  let service_paths = paths.clone();
  let runner = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(false),
  );
  let service = thread::spawn(move || unix::run_runtime(&service_paths, collector, runner, false));
  wait_ready(&paths);
  let resumed = resume_registered_run(&paths, &run_dir).unwrap().unwrap();
  assert_eq!(resumed["job_id"], report["job_id"]);
  assert_eq!(resumed["accepted"], true);
  assert!(registry::read_job(&directory).unwrap() == job);
  let mut alternate = serde_json::to_value(job).unwrap();
  alternate["scope"]["origin"] = json!("second-worker");
  let alternate: PreparedJob = serde_json::from_value(alternate).unwrap();
  let alternate_id = alternate.identity().unwrap();
  let alternate_root = directory.parent().unwrap().join(&alternate_id);
  files::directory(&alternate_root).unwrap();
  files::write(&alternate_root.join("request.json"), &alternate).unwrap();
  registry::JobState {
    schema_version: 1,
    job_id: alternate_id,
    status: "queued".into(),
    updated_at_ms: 0,
    retry_at_ms: 0,
    failures: 0,
    last_error: None,
    report: alternate.safe_report(),
    progress: json!({}),
    publication_epoch: 0,
  }
  .save(&alternate_root)
  .unwrap();
  assert!(
    resume_registered_run(&paths, &run_dir)
      .unwrap_err()
      .to_string()
      .contains("multiple saved")
  );
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn idle_service_does_not_fsync_unchanged_status_each_tick() {
  let (_temporary, paths, _) = fixture();
  let service_paths = paths.clone();
  let runner = Arc::new(
    |_: &PreparedJob, _: Arc<AtomicBool>, _: &mut dyn FnMut(Value) -> Result<()>| Ok(false),
  );
  let service = thread::spawn(move || unix::run_runtime(&service_paths, collector, runner, false));
  wait_ready(&paths);
  // Allow the initial ready transition to reach its durable public snapshot.
  thread::sleep(Duration::from_millis(150));
  let directory = files::existing(&paths).unwrap().unwrap();
  let path = directory.join("status.json");
  let before = fs::read(&path).unwrap();
  let modified = fs::metadata(&path).unwrap().modified().unwrap();
  thread::sleep(Duration::from_millis(300));
  assert_eq!(fs::read(&path).unwrap(), before);
  assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn saved_run_scan_isolates_unrelated_corruption_but_rejects_selected_job_corruption() {
  let (temporary, paths, job) = fixture();
  let selected_run = job.run_dir().to_path_buf();
  let selected = registry::register(&paths, job.clone()).unwrap();
  let other_run = temporary
    .path()
    .canonicalize()
    .unwrap()
    .join("source/.expri/runs/run-other");
  fs::create_dir_all(&other_run).unwrap();
  fs::write(
    other_run.join("run-state.json"),
    br#"{"run_id":"run-other","status":"running"}"#,
  )
  .unwrap();
  let mut other = serde_json::to_value(&job).unwrap();
  other["run_dir"] = json!(other_run);
  other["scope"]["run_id"] = json!("run-other");
  let unrelated = registry::register(&paths, serde_json::from_value(other).unwrap()).unwrap();
  let root = files::existing(&paths).unwrap().unwrap().join("jobs");
  let unrelated_root = root.join(unrelated["job_id"].as_str().unwrap());
  fs::write(
    unrelated_root.join("state.json"),
    b"corrupt unrelated public state",
  )
  .unwrap();
  assert!(registry::saved_run(&paths, &selected_run).unwrap().unwrap() == job);
  fs::write(
    unrelated_root.join("request.json"),
    b"corrupt unrelated private request",
  )
  .unwrap();
  assert!(registry::saved_run(&paths, &selected_run).unwrap().unwrap() == job);
  let selected_root = root.join(selected["job_id"].as_str().unwrap());
  fs::write(
    selected_root.join("state.json"),
    b"corrupt selected public state",
  )
  .unwrap();
  assert!(
    registry::saved_run(&paths, &selected_run)
      .err()
      .unwrap()
      .to_string()
      .contains("selected run is invalid")
  );
  fs::remove_file(selected_root.join("state.json")).unwrap();
  fs::write(
    selected_root.join("request.json"),
    b"corrupt selected private request",
  )
  .unwrap();
  // A valid matching public record identifies a broken selected private binding.
  registry::JobState {
    schema_version: 1,
    job_id: selected["job_id"].as_str().unwrap().to_owned(),
    status: "queued".into(),
    updated_at_ms: 0,
    retry_at_ms: 0,
    failures: 0,
    last_error: None,
    report: job.safe_report(),
    progress: json!({}),
    publication_epoch: 0,
  }
  .save(&selected_root)
  .unwrap();
  assert!(registry::saved_run(&paths, &selected_run).is_err());
}

#[test]
fn one_local_run_has_one_destination_even_after_publication_or_error() {
  let (temporary, paths, job) = fixture();
  let first = registry::register(&paths, job.clone()).unwrap();
  let root = files::existing(&paths).unwrap().unwrap();
  let first_path = root.join("jobs").join(first["job_id"].as_str().unwrap());
  for status in ["queued", "published", "error"] {
    let mut state = registry::read_state(&first_path).unwrap();
    state.status = status.into();
    state.save(&first_path).unwrap();
    for (field, value) in [
      ("endpoint", "https://another.invalid/v1/request"),
      ("origin", "second-origin"),
      ("project_id", "second-project"),
    ] {
      let mut alternate = serde_json::to_value(&job).unwrap();
      if field == "endpoint" {
        alternate["client"][field] = json!(value);
      } else {
        alternate["scope"][field] = json!(value);
      }
      let alternate: PreparedJob = serde_json::from_value(alternate).unwrap();
      let id = alternate.identity().unwrap();
      let failure = register(&paths, alternate).unwrap_err();
      assert!(
        failure
          .to_string()
          .contains("another publishing destination")
      );
      assert!(!root.join("jobs").join(id).exists());
      assert!(!root.join("service.lock").exists());
      assert!(!root.join("service.log").exists());
    }
  }
  assert_eq!(
    registry::register(&paths, job.clone()).unwrap()["already_registered"],
    true
  );
  let copy = independent_job_with_same_id(
    &job,
    &temporary
      .path()
      .canonicalize()
      .unwrap()
      .join("different-local-copy"),
  );
  let mut copy = serde_json::to_value(copy).unwrap();
  copy["scope"]["project_id"] = json!("second-project");
  copy["scope"]["origin"] = json!("second-origin");
  copy["client"]["endpoint"] = json!("https://another.invalid/v1/request");
  let accepted = registry::register(&paths, serde_json::from_value(copy).unwrap()).unwrap();
  assert_eq!(accepted["accepted"], true);
  assert_ne!(accepted["job_id"], first["job_id"]);
}

#[test]
fn destination_guard_uses_validated_public_binding_and_private_fallback() {
  let (_temporary, paths, job) = fixture();
  let first = registry::register(&paths, job.clone()).unwrap();
  let root = files::existing(&paths).unwrap().unwrap();
  let first_path = root.join("jobs").join(first["job_id"].as_str().unwrap());
  let mut alternate = serde_json::to_value(&job).unwrap();
  alternate["scope"]["origin"] = json!("another-origin");
  let alternate: PreparedJob = serde_json::from_value(alternate).unwrap();
  // A corrupt public record is still protected by the valid private binding.
  fs::write(first_path.join("state.json"), b"corrupt public record").unwrap();
  assert!(registry::register(&paths, alternate.clone()).is_err());
  // A validated public record still protects the run when the private request
  // cannot be read; neither diagnostic needs to reveal its credential.
  registry::JobState {
    schema_version: 1,
    job_id: first["job_id"].as_str().unwrap().into(),
    status: "published".into(),
    updated_at_ms: 0,
    retry_at_ms: 0,
    failures: 0,
    last_error: None,
    report: job.safe_report(),
    progress: json!({}),
    publication_epoch: 0,
  }
  .save(&first_path)
  .unwrap();
  fs::write(first_path.join("request.json"), b"corrupt private record").unwrap();
  let failure = registry::register(&paths, alternate)
    .unwrap_err()
    .to_string();
  assert!(failure.contains("another publishing destination"));
  assert!(!failure.contains("fixture-private-token"));
}

#[test]
fn run_binding_pins_destination_across_storage_ledgers_without_credentials() {
  use std::os::unix::fs::PermissionsExt;
  let (temporary, first_paths, job) = fixture();
  let second_paths = StoragePaths::from_root(
    temporary
      .path()
      .canonicalize()
      .unwrap()
      .join("second-storage"),
  )
  .unwrap();
  let first = registry::register(&first_paths, job.clone()).unwrap();
  let binding_path = job.run_dir().join("publishing-binding.json");
  let before = fs::read(&binding_path).unwrap();
  assert!(!String::from_utf8_lossy(&before).contains("fixture-private-token"));
  assert_eq!(
    fs::metadata(&binding_path).unwrap().permissions().mode() & 0o777,
    0o600
  );
  assert_eq!(
    fs::metadata(job.run_dir().join(".publishing-binding.lock"))
      .unwrap()
      .permissions()
      .mode()
      & 0o777,
    0o600
  );
  let second = registry::register(&second_paths, job.clone()).unwrap();
  assert_eq!(first["job_id"], second["job_id"]);
  assert_eq!(fs::read(&binding_path).unwrap(), before);
  let third_paths = StoragePaths::from_root(
    temporary
      .path()
      .canonicalize()
      .unwrap()
      .join("third-storage"),
  )
  .unwrap();
  for (field, value) in [
    ("endpoint", "https://another.invalid/v1/request"),
    ("origin", "new-origin"),
    ("project_id", "new-project"),
  ] {
    let mut different = serde_json::to_value(&job).unwrap();
    if field == "endpoint" {
      different["client"][field] = json!(value);
    } else {
      different["scope"][field] = json!(value);
    }
    let different: PreparedJob = serde_json::from_value(different).unwrap();
    let identity = different.identity().unwrap();
    let failure = register(&third_paths, different).unwrap_err().to_string();
    assert!(failure.contains("another publishing destination"));
    let root = files::existing(&third_paths).unwrap().unwrap();
    assert!(!root.join("jobs").join(identity).exists());
    assert!(!root.join("service.lock").exists());
    assert_eq!(fs::read(&binding_path).unwrap(), before);
  }
}

#[test]
fn copied_run_binding_rejects_transplant_until_independent_handoffs_are_reset() {
  let (temporary, paths, job) = fixture();
  registry::register(&paths, job.clone()).unwrap();
  let independent = independent_job_with_same_id(
    &job,
    &temporary
      .path()
      .canonicalize()
      .unwrap()
      .join("independent-run"),
  );
  fs::copy(
    job.run_dir().join("publishing-binding.json"),
    independent.run_dir().join("publishing-binding.json"),
  )
  .unwrap();
  let failure = registry::register(&paths, independent.clone())
    .unwrap_err()
    .to_string();
  assert!(failure.contains("belongs to another run"));
  let id = independent.identity().unwrap();
  assert!(
    !files::existing(&paths)
      .unwrap()
      .unwrap()
      .join("jobs")
      .join(&id)
      .exists()
  );
  // This fixture has fresh, empty handoff state, so removing a transplanted
  // marker restores an independent run rather than reusing cloud receipts.
  fs::remove_file(independent.run_dir().join("publishing-binding.json")).unwrap();
  fs::remove_file(independent.run_dir().join(".publishing-binding.lock")).unwrap();
  assert_eq!(
    registry::register(&paths, independent).unwrap()["accepted"],
    true
  );
}

#[test]
fn acknowledged_backlog_continues_quickly_without_starving_idle_or_failed_jobs() {
  let (temporary, paths, job) = fixture();
  let mut ids = std::collections::BTreeMap::new();
  for name in [
    "backlog-0",
    "backlog-1",
    "backlog-2",
    "backlog-3",
    "backlog-4",
    "idle",
    "error",
    "stale",
  ] {
    let independent =
      independent_job_with_same_id(&job, &temporary.path().canonicalize().unwrap().join(name));
    let report = registry::register(&paths, independent).unwrap();
    ids.insert(
      name.to_owned(),
      report["job_id"].as_str().unwrap().to_owned(),
    );
  }
  let root = files::existing(&paths).unwrap().unwrap().join("jobs");
  let mut stale = registry::read_state(&root.join(&ids["stale"])).unwrap();
  stale.progress = json!({"backlog_remaining": true});
  stale.save(&root.join(&ids["stale"])).unwrap();
  let calls = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::<
    String,
    usize,
  >::new()));
  let observed = calls.clone();
  let runner = Arc::new(
    move |job: &PreparedJob, _: Arc<AtomicBool>, progress: &mut dyn FnMut(Value) -> Result<()>| {
      let name = job
        .run_dir()
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
      let count = {
        let mut calls = observed.lock().unwrap();
        let count = calls.entry(name.clone()).or_default();
        *count += 1;
        *count
      };
      if name.starts_with("backlog-") {
        progress(
          json!({"backlog_remaining": count < 4, "stream_offsets": {"outputs/metrics.jsonl": count * 262_144}}),
        )?;
        return Ok(count >= 4);
      }
      if name == "error" {
        // An error must never adopt even a fresh continuation hint.
        progress(json!({"backlog_remaining": true}))?;
        return Err(expri_core::error::ExpriError::ServiceUnavailable {
          reading_response: false,
        });
      }
      // No new progress models idle work or a held legacy publishing lease.
      Ok(false)
    },
  );
  let service_paths = paths.clone();
  let start = Instant::now();
  let service = thread::spawn(move || unix::run_runtime(&service_paths, collector, runner, false));
  wait_ready(&paths);
  let deadline = start + Duration::from_secs(4);
  loop {
    let snapshot = calls.lock().unwrap().clone();
    let backlog_done = (0..5).all(|index| {
      snapshot
        .get(&format!("backlog-{index}"))
        .copied()
        .unwrap_or(0)
        >= 4
    });
    let older_jobs_ran = ["idle", "error", "stale"]
      .iter()
      .all(|name| snapshot.get(*name) == Some(&1));
    if backlog_done && older_jobs_ran {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "useful backlog was throttled or older jobs were starved: {snapshot:?}"
    );
    thread::sleep(Duration::from_millis(20));
  }
  for index in 0..5 {
    let id = &ids[&format!("backlog-{index}")];
    let deadline = start + Duration::from_secs(4);
    while job_status(&paths, id).unwrap()["status"] != "published" {
      assert!(Instant::now() < deadline);
      thread::sleep(Duration::from_millis(10));
    }
  }
  for name in ["idle", "stale"] {
    let state = registry::read_state(&root.join(&ids[name])).unwrap();
    assert_eq!(state.status, "queued");
    assert!(state.retry_at_ms - state.updated_at_ms > 4_000);
  }
  let mut failed = registry::read_state(&root.join(&ids["error"])).unwrap();
  assert_eq!(failed.status, "retrying");
  assert_eq!(failed.failures, 1);
  assert!(failed.retry_at_ms - failed.updated_at_ms > 4_000);
  // Make the saved first retry due without waiting for five wall-clock seconds.
  // The next failure must preserve the existing exponential cooldown.
  failed.retry_at_ms = 0;
  failed.save(&root.join(&ids["error"])).unwrap();
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let state = registry::read_state(&root.join(&ids["error"])).unwrap();
    if state.failures == 2 {
      assert!(state.retry_at_ms - state.updated_at_ms > 9_000);
      break;
    }
    assert!(Instant::now() < deadline);
    thread::sleep(Duration::from_millis(20));
  }
  assert_eq!(calls.lock().unwrap().get("idle"), Some(&1));
  assert_eq!(calls.lock().unwrap().get("stale"), Some(&1));
  stop(&paths).unwrap();
  service.join().unwrap().unwrap();
}

#[test]
fn stop_handles_delayed_or_lost_control_acknowledgments() {
  use std::io::{BufRead, BufReader, Write};
  use std::os::unix::fs::PermissionsExt;
  use std::os::unix::net::UnixListener;

  for outcome in ["delayed", "stopping", "stopped", "unacknowledged"] {
    let (_temporary, paths, _) = fixture();
    let directory = files::ensure(&paths).unwrap();
    let lease_path = directory.join("service.lock");
    files::private_file(&lease_path).unwrap();
    let expri_core::lock::LockAttempt::Acquired(lease) =
      expri_core::lock::try_lock_file(&lease_path, false).unwrap()
    else {
      panic!("fixture service lease must be available");
    };
    let socket = unix::socket(&directory).unwrap();
    files::directory(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let (finish, finished) = mpsc::channel();
    let control = thread::spawn(move || {
      let mut lease = Some(lease);
      for operation in ["status", "stop"] {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream).read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["operation"], operation);
        if operation == "stop" && outcome != "delayed" {
          files::write(
            &directory.join("status.json"),
            &json!({
              "ready": false,
              "status": if outcome == "unacknowledged" { "running" } else { "stopping" },
            }),
          )
          .unwrap();
          if outcome == "stopped" {
            drop(lease.take());
          }
          // Drop the accepted request without sending its acknowledgment. The
          // caller must establish the accepted stop from the lease or status.
          break;
        }
        let value = if operation == "stop" {
          // Exceed the best-effort status timeout, as a loaded runtime can while
          // finishing local durable writes before reading its control IPC.
          thread::sleep(Duration::from_millis(350));
          json!({"stopping": true, "jobs_preserved": true, "training_preserved": true})
        } else {
          json!({"running": true, "ready": true, "status": "running"})
        };
        let _ = writeln!(stream, "{}", json!({"ok": true, "value": value}));
      }
      drop(listener);
      // Keep the stopping lease until the caller finishes its state probe.
      finished.recv_timeout(Duration::from_secs(5)).unwrap();
      drop(lease);
    });
    let report = stop(&paths);
    finish.send(()).unwrap();
    control.join().unwrap();
    fs::remove_file(socket).unwrap();
    if outcome == "unacknowledged" {
      assert!(report.is_err(), "an active service must acknowledge stop");
      continue;
    }
    let report = report.unwrap_or_else(|failure| panic!("{outcome}: {failure}"));
    assert_eq!(report["jobs_preserved"], true, "{outcome}");
    if outcome == "stopped" {
      assert_eq!(report["stopped"], true);
    } else {
      assert_eq!(report["stopping"], true, "{outcome}");
      assert_eq!(report["training_preserved"], true, "{outcome}");
    }
  }
}

#[test]
fn accepted_control_socket_waits_for_a_delayed_request() {
  use std::io::{BufRead, BufReader, Write};
  use std::os::unix::net::UnixStream;

  let (mut server, mut client) = UnixStream::pair().unwrap();
  // Model an accepted socket on platforms that inherit the listener's mode.
  server.set_nonblocking(true).unwrap();
  client
    .set_read_timeout(Some(Duration::from_secs(1)))
    .unwrap();
  let context = ServiceContext::new();
  context.collection_ready("machine-fixture".into(), 1);
  let (entered, handling) = mpsc::channel();
  let handler = thread::spawn(move || {
    entered.send(()).unwrap();
    unix::handle(&mut server, &context, 0, 0)
  });
  handling.recv_timeout(Duration::from_secs(1)).unwrap();
  thread::sleep(Duration::from_millis(30));
  client.write_all(b"{\"operation\":\"status\"}\n").unwrap();
  let mut response = String::new();
  BufReader::new(client).read_line(&mut response).unwrap();
  handler.join().unwrap().unwrap();
  let response: Value = serde_json::from_str(&response).unwrap();
  assert_eq!(response["ok"], true);
  assert_eq!(response["value"]["ready"], true);
  assert_eq!(
    response["value"]["collection"]["machine_id"],
    "machine-fixture"
  );
}
