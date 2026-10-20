use super::*;
use crate::resource_guard::{Filesystem, Guard};
use expri_core::resource_requirements::DiskRequirement;
use expri_core::run_state::{MAX_STATE_BYTES, update_state, write_state};
use serde_json::json;
use std::fs;

fn policy(path: &str) -> ResourceRequirements {
  ResourceRequirements {
    disk: vec![DiskRequirement {
      path: path.into(),
      min_free_bytes: 100,
      resume_free_bytes: 120,
    }],
    recovery_seconds: 0,
  }
}

fn budget(limit: usize) -> Arc<Budget> {
  Arc::new(Budget {
    active: AtomicUsize::new(0),
    limit,
  })
}

fn fixture(policy: &ResourceRequirements) -> tempfile::TempDir {
  let directory = tempfile::tempdir().unwrap();
  fs::create_dir(directory.path().join("outputs")).unwrap();
  write_state(
    &directory.path().join("run-state.json"),
    &json!({
      "schema_version":1,"run_id":"test","status":"preparing","requirements":policy,
    }),
  )
  .unwrap();
  directory
}

fn reading() -> io::Result<Reading> {
  Ok(Reading {
    available: 130,
    filesystem: Filesystem {
      device: 1,
      mount: 1,
    },
  })
}

fn ready(outcome: Outcome) -> Readings {
  match outcome {
    Outcome::Ready(readings) => readings,
    Outcome::Pending { .. } => panic!("expected a completed observation"),
  }
}

#[test]
fn a_stalled_observation_is_bounded_and_does_not_hold_the_state_lease() {
  let policy = policy("outputs");
  let directory = fixture(&policy);
  let (sender, receiver) = mpsc::channel();
  let mut guard = Guard {
    sampler: Sampler {
      pending: vec![Sampling {
        paths: paths(&policy),
        started_at: Instant::now(),
        receiver,
        completed: None,
      }],
      budget: budget(2),
    },
    ..Default::default()
  };
  let started = Instant::now();
  assert!(guard.tick(directory.path(), None).unwrap());
  assert!(started.elapsed() < Duration::from_millis(500));
  assert_eq!(guard.sampler.pending.len(), 1);
  update_state(&directory.path().join("run-state.json"), |state| {
    state["manual_paused"] = json!(true);
    Ok(())
  })
  .unwrap();
  let state =
    super::super::read_json(&directory.path().join("run-state.json"), MAX_STATE_BYTES).unwrap();
  assert_eq!(state["status"], "pending");
  assert_eq!(state["resource_status"]["phase"], "unavailable");
  drop(sender);
}

#[test]
fn replacing_a_stalled_path_can_probe_healthy_new_requirements() {
  let old_policy = policy("/mnt/stalled");
  let healthy_policy = policy("outputs");
  let directory = fixture(&healthy_policy);
  let (release, blocked) = mpsc::channel();
  let mut sampler = Sampler {
    pending: Vec::new(),
    budget: budget(2),
  };
  sampler
    .start(paths(&old_policy), directory.path(), move |_| {
      blocked.recv().unwrap();
      reading()
    })
    .unwrap();
  let mut guard = Guard {
    sampler,
    ..Default::default()
  };
  assert!(
    !guard.tick(directory.path(), None).unwrap(),
    "the former path must not monopolize the new policy's observation"
  );
  assert_eq!(
    guard.sampler.pending.len(),
    1,
    "the former pending read remains bounded"
  );
  let state =
    super::super::read_json(&directory.path().join("run-state.json"), MAX_STATE_BYTES).unwrap();
  assert_eq!(state["resource_status"]["disk"][0]["path"], "outputs");
  assert_eq!(state["resource_status"]["phase"], "ready");
  release.send(()).unwrap();
}

#[test]
fn successful_slow_observations_are_consumed_before_the_next_poll_deadline() {
  let policy = policy("outputs");
  let directory = fixture(&policy);
  let (release, blocked) = mpsc::channel();
  let (done, finished) = mpsc::channel();
  let mut sampler = Sampler {
    pending: Vec::new(),
    budget: budget(1),
  };
  sampler
    .start(paths(&policy), directory.path(), move |_| {
      blocked.recv().unwrap();
      let result = reading();
      done.send(()).unwrap();
      result
    })
    .unwrap();
  let mut guard = Guard {
    sampler,
    ..Default::default()
  };
  assert!(guard.tick(directory.path(), None).unwrap());
  let deadline = guard.next_check.unwrap();
  release.send(()).unwrap();
  finished.recv_timeout(Duration::from_secs(2)).unwrap();
  let timeout = Instant::now() + Duration::from_secs(2);
  while !guard.sampler.has_completed(Some(&policy)) {
    assert!(Instant::now() < timeout);
    thread::yield_now();
  }
  assert!(Instant::now() < deadline);
  assert!(!guard.tick(directory.path(), None).unwrap());
  let state =
    super::super::read_json(&directory.path().join("run-state.json"), MAX_STATE_BYTES).unwrap();
  assert_eq!(state["resource_status"]["phase"], "ready");
}

#[test]
fn repeated_or_reordered_policies_do_not_accumulate_stalled_reads() {
  let mut policy = policy("outputs");
  policy.disk.push(DiskRequirement {
    path: "/other".into(),
    min_free_bytes: 100,
    resume_free_bytes: 120,
  });
  let (sender, receiver) = mpsc::channel();
  let mut sampler = Sampler {
    pending: vec![Sampling {
      paths: paths(&policy),
      started_at: Instant::now(),
      receiver,
      completed: None,
    }],
    budget: budget(2),
  };
  policy.disk.reverse();
  policy.disk[0].min_free_bytes = 101;
  for _ in 0..3 {
    assert!(matches!(
      sampler.sample(&policy, Path::new("/unused")),
      Outcome::Pending { .. }
    ));
    assert_eq!(sampler.pending.len(), 1);
  }
  drop(sender);
}

#[test]
fn obsolete_stalled_reads_have_a_fixed_limit_and_an_explicit_error() {
  let mut sampler = Sampler {
    pending: Vec::new(),
    budget: budget(8),
  };
  let mut senders = Vec::new();
  for number in 0..MAX_GUARD_SAMPLERS {
    let (sender, receiver) = mpsc::channel();
    senders.push(sender);
    sampler.pending.push(Sampling {
      paths: vec![format!("/stalled/{number}")],
      started_at: Instant::now(),
      receiver,
      completed: None,
    });
  }
  for _ in 0..3 {
    let mut readings = ready(sampler.sample(&policy("outputs"), Path::new("/unused")));
    assert!(
      readings
        .remove("outputs")
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("capacity is exhausted")
    );
    assert_eq!(sampler.pending.len(), MAX_GUARD_SAMPLERS);
  }
}

#[test]
fn the_thread_budget_survives_replacing_a_guard_or_sampler() {
  let budget = budget(1);
  let directory = tempfile::tempdir().unwrap();
  let (release, blocked) = mpsc::channel();
  let mut sampler = Sampler {
    pending: Vec::new(),
    budget: budget.clone(),
  };
  sampler
    .start(vec!["/stalled".into()], directory.path(), move |_| {
      blocked.recv().unwrap();
      reading()
    })
    .unwrap();
  drop(sampler);
  let mut replacement = Sampler {
    pending: Vec::new(),
    budget: budget.clone(),
  };
  let mut readings = ready(replacement.sample(&policy("outputs"), directory.path()));
  assert!(
    readings
      .remove("outputs")
      .unwrap()
      .unwrap_err()
      .to_string()
      .contains("capacity is exhausted")
  );
  release.send(()).unwrap();
  let deadline = Instant::now() + Duration::from_secs(2);
  while budget.active.load(Ordering::Acquire) > 0 {
    assert!(Instant::now() < deadline);
    thread::yield_now();
  }
  let mut readings = ready(replacement.sample(&policy("outputs"), directory.path()));
  assert!(readings.remove("outputs").unwrap().is_ok());
}

fn wait_for_completion(sampler: &mut Sampler, policy: &ResourceRequirements) {
  let deadline = Instant::now() + Duration::from_secs(2);
  while !sampler.has_completed(Some(policy)) {
    assert!(Instant::now() < deadline);
    thread::yield_now();
  }
}

#[test]
fn repeated_slow_successful_probes_keep_a_nonzero_recovery_dwell() {
  let mut policy = policy("outputs");
  policy.recovery_seconds = 30;
  let directory = fixture(&policy);
  update_state(&directory.path().join("run-state.json"), |state| {
    state["resource_paused"] = json!(true);
    state["status"] = json!("pending");
    Ok(())
  })
  .unwrap();
  let mut guard = Guard {
    policy: Some(policy.clone()),
    held: true,
    launch_held: true,
    recovering_since: Some((
      Instant::now() - Duration::from_secs(20),
      (chrono::Utc::now() - chrono::Duration::seconds(20)).to_rfc3339(),
    )),
    sampler: Sampler {
      pending: Vec::new(),
      budget: budget(1),
    },
    ..Default::default()
  };
  for round in 0..3 {
    let (release, blocked) = mpsc::channel();
    guard
      .sampler
      .start(paths(&policy), directory.path(), move |_| {
        blocked.recv().unwrap();
        reading()
      })
      .unwrap();
    guard.force_check();
    let before = guard.recovering_since.clone();
    assert!(guard.tick(directory.path(), None).unwrap());
    assert_eq!(
      guard.recovering_since, before,
      "a pending probe is not an observed failure"
    );
    release.send(()).unwrap();
    wait_for_completion(&mut guard.sampler, &policy);
    if round == 2 {
      guard.recovering_since = Some((
        Instant::now() - Duration::from_secs(31),
        (chrono::Utc::now() - chrono::Duration::seconds(31)).to_rfc3339(),
      ));
    }
    assert_eq!(guard.tick(directory.path(), None).unwrap(), round != 2);
    if round != 2 {
      assert_eq!(guard.recovering_since, before);
    }
  }
}

#[test]
fn a_pending_probe_keeps_ready_state_but_a_real_timeout_resets_recovery() {
  let policy = policy("outputs");
  let directory = fixture(&policy);
  update_state(&directory.path().join("run-state.json"), |state| {
    state["status"] = json!("running");
    state["resource_paused"] = json!(false);
    Ok(())
  })
  .unwrap();
  let (sender, receiver) = mpsc::channel();
  let mut guard = Guard {
    policy: Some(policy.clone()),
    sampler: Sampler {
      pending: vec![Sampling {
        paths: paths(&policy),
        started_at: Instant::now(),
        receiver,
        completed: None,
      }],
      budget: budget(1),
    },
    ..Default::default()
  };
  let before = fs::read(directory.path().join("run-state.json")).unwrap();
  // No real child is needed: a pending probe must never signal this value.
  assert!(!guard.tick(directory.path(), Some(u32::MAX)).unwrap());
  assert_eq!(
    fs::read(directory.path().join("run-state.json")).unwrap(),
    before
  );
  guard.recovering_since = Some((Instant::now(), chrono::Utc::now().to_rfc3339()));
  guard.sampler.pending[0].started_at = Instant::now() - CHECK_INTERVAL;
  guard.force_check();
  assert!(guard.tick(directory.path(), None).unwrap());
  assert!(guard.recovering_since.is_none());
  let state =
    super::super::read_json(&directory.path().join("run-state.json"), MAX_STATE_BYTES).unwrap();
  assert_eq!(state["resource_status"]["phase"], "unavailable");
  assert!(
    state["resource_status"]["disk"][0]["error"]
      .as_str()
      .unwrap()
      .contains("timed out")
  );
  drop(sender);
}

#[test]
fn a_new_policy_cannot_launch_using_the_previous_policy_snapshot() {
  let old = policy("/previous");
  let policy = policy("outputs");
  let directory = fixture(&policy);
  let (sender, receiver) = mpsc::channel();
  let mut guard = Guard {
    policy: Some(old),
    sampler: Sampler {
      pending: vec![Sampling {
        paths: paths(&policy),
        started_at: Instant::now(),
        receiver,
        completed: None,
      }],
      budget: budget(1),
    },
    ..Default::default()
  };
  assert!(guard.tick(directory.path(), None).unwrap());
  assert!(
    guard.tick(directory.path(), None).unwrap(),
    "cached pending state must still block a launch"
  );
  let state =
    super::super::read_json(&directory.path().join("run-state.json"), MAX_STATE_BYTES).unwrap();
  assert_eq!(state["status"], "pending");
  assert_eq!(state["resource_status"]["disk"][0]["path"], "outputs");
  assert!(
    !guard.held,
    "pending observation does not latch an actual resource shortage"
  );
  drop(sender);
}
