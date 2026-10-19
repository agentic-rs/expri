use super::*;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  std::fs::create_dir(root.join("outputs")).unwrap();
  fs::atomic_json(
    &root.join("run-state.json"),
    &json!({"run_id":"run-fixture","status":"running"}),
  )
  .unwrap();
  (temporary, root)
}

fn attempt_state(current: u64, total: u64) -> Value {
  json!({
    "run_id":"run-fixture", "status":"running",
    "task_attempts": {
      "protocol_version":1, "current_attempt":current,
      "attempts":(1..=total).map(|attempt| json!({"attempt":attempt})).collect::<Vec<_>>(),
      "total_count":total, "truncated":false,
    },
  })
}

fn forget_attempt(root: &Path, path: &str) {
  let mut registry: Registry =
    serde_json::from_slice(&std::fs::read(root.join(RECORD)).unwrap()).unwrap();
  registry.files.get_mut(path).unwrap().task_attempt = None;
  save(root, &registry).unwrap();
}

#[test]
fn finalized_checkpoint_verification_refuses_unregistered_or_replaced_files() {
  let (_temporary, root) = fixture();
  let path = root.join("outputs/state.bin");
  std::fs::write(&path, "checkpoint").unwrap();
  assert!(verify_registered(&root, "outputs/state.bin").is_err());
  register(&root, "outputs/state.bin").unwrap();
  verify_registered(&root, "outputs/state.bin").unwrap();
  let replacement = root.join("outputs/replacement.bin");
  std::fs::write(&replacement, "checkpoint").unwrap();
  std::fs::rename(replacement, &path).unwrap();
  assert!(verify_registered(&root, "outputs/state.bin").is_err());
  assert!(verify_registered(&root, "../state.bin").is_err());
}

#[test]
fn checkpoint_attempt_is_captured_once_and_survives_later_registrations() {
  let (_temporary, root) = fixture();
  std::fs::write(root.join("outputs/old.pt"), "old").unwrap();
  let old = register(&root, "outputs/old.pt").unwrap();
  assert_eq!(old["task_attempt"], 1);
  fs::atomic_json(&root.join("run-state.json"), &attempt_state(2, 2)).unwrap();
  assert_eq!(register(&root, "outputs/old.pt").unwrap(), old);
  std::fs::write(root.join("outputs/new.pt"), "new").unwrap();
  let new = register(&root, "outputs/new.pt").unwrap();
  assert_eq!(new["task_attempt"], 2);
  let files = list(&root).unwrap();
  assert_eq!(files["files"][0]["task_attempt"], 2);
  assert_eq!(files["files"][1]["task_attempt"], 1);
}

#[test]
fn new_registration_waits_for_the_launch_state_lease_before_capturing_attempt() {
  let (_temporary, root) = fixture();
  std::fs::write(root.join("outputs/new.pt"), "checkpoint").unwrap();
  let lease = crate::lock::run_state_lock(&root).unwrap();
  let child_root = root.clone();
  let (sender, receiver) = std::sync::mpsc::channel();
  let child = std::thread::spawn(move || {
    sender
      .send(register(&child_root, "outputs/new.pt"))
      .unwrap();
  });
  assert!(matches!(
    receiver.recv_timeout(Duration::from_millis(50)),
    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
  ));
  fs::atomic_json(&root.join("run-state.json"), &attempt_state(2, 2)).unwrap();
  drop(lease);
  let registration = receiver
    .recv_timeout(Duration::from_secs(2))
    .unwrap()
    .unwrap();
  assert_eq!(registration["task_attempt"], 2);
  child.join().unwrap();
}

#[test]
fn only_the_first_explicit_reopen_migrates_legacy_checkpoint_attempts() {
  let (_temporary, root) = fixture();
  std::fs::write(root.join("outputs/legacy.pt"), "old").unwrap();
  register(&root, "outputs/legacy.pt").unwrap();
  forget_attempt(&root, "outputs/legacy.pt");
  assert!(register(&root, "outputs/legacy.pt").unwrap()["task_attempt"].is_null());
  // Failed preflight attempts did not launch a task or change legacy ownership.
  fs::atomic_json(&root.join("run-state.json"), &attempt_state(1, 3)).unwrap();
  reopen_for_attempt(&root).unwrap();
  assert_eq!(list(&root).unwrap()["files"][0]["task_attempt"], 1);
  fs::atomic_json(&root.join("run-state.json"), &attempt_state(3, 3)).unwrap();
  std::fs::write(root.join("outputs/unknown.pt"), "unknown").unwrap();
  register(&root, "outputs/unknown.pt").unwrap();
  forget_attempt(&root, "outputs/unknown.pt");
  reopen_for_attempt(&root).unwrap();
  let files = list(&root).unwrap();
  assert_eq!(files["files"][0]["task_attempt"], 1);
  assert!(files["files"][1]["task_attempt"].is_null());
}

#[test]
fn already_continued_legacy_registry_keeps_unknown_attempts_unknown() {
  let (_temporary, root) = fixture();
  std::fs::write(root.join("outputs/legacy.pt"), "unknown").unwrap();
  register(&root, "outputs/legacy.pt").unwrap();
  forget_attempt(&root, "outputs/legacy.pt");
  fs::atomic_json(&root.join("run-state.json"), &attempt_state(2, 2)).unwrap();
  reopen_for_attempt(&root).unwrap();
  assert!(list(&root).unwrap()["files"][0]["task_attempt"].is_null());
  let registry: Value = serde_json::from_slice(&std::fs::read(root.join(RECORD)).unwrap()).unwrap();
  assert_eq!(registry["continuation_reopened"], true);
  assert!(
    registry["files"]["outputs/legacy.pt"]
      .get("task_attempt")
      .is_none()
  );
}

#[test]
fn registration_is_durable_idempotent_and_does_not_read_large_file() {
  let (_temporary, root) = fixture();
  let file = std::fs::File::create(root.join("outputs/checkpoint.pt")).unwrap();
  file.set_len(16 * 1024 * 1024 * 1024).unwrap();
  let started = Instant::now();
  let first = register(&root, "outputs/checkpoint.pt").unwrap();
  assert!(started.elapsed() < Duration::from_secs(1));
  assert_eq!(first["sync_status"], "registered");
  assert_eq!(first["size"], 16 * 1024 * 1024 * 1024u64);
  let bytes = std::fs::read(root.join(RECORD)).unwrap();
  assert!(!String::from_utf8_lossy(&bytes).contains("sha256"));
  assert_eq!(register(&root, "outputs/checkpoint.pt").unwrap(), first);
  assert_eq!(std::fs::read(root.join(RECORD)).unwrap(), bytes);
  assert_eq!(list(&root).unwrap()["files"][0], first);
  assert_eq!(records(&root).unwrap().len(), 1);
}

#[test]
fn labels_move_without_changing_upload_state_or_identity() {
  let (_temporary, root) = fixture();
  for path in ["outputs/1.pt", "outputs/2.pt"] {
    std::fs::write(root.join(path), "checkpoint").unwrap();
    register(&root, path).unwrap();
  }
  let before = records(&root).unwrap()[0].clone();
  update(&root, &before, "cloud", None).unwrap();
  register_with_labels(&root, "outputs/1.pt", &["best".into(), "latest".into()]).unwrap();
  register_with_labels(&root, "outputs/2.pt", &["latest".into()]).unwrap();
  let files = records(&root).unwrap();
  assert_eq!(files[0].labels, ["best"]);
  assert_eq!(files[0].sync_status, "cloud");
  assert_eq!(files[0].identity, before.identity);
  assert_eq!(files[1].labels, ["latest"]);
  assert!(register_with_labels(&root, "outputs/2.pt", &["other".into()]).is_err());
}

#[test]
fn changes_and_replacements_are_rejected_even_with_same_size() {
  let (_temporary, root) = fixture();
  let path = "outputs/checkpoint.pt";
  std::fs::write(root.join(path), "first").unwrap();
  register(&root, path).unwrap();
  let readiness = records(&root).unwrap().remove(0);
  std::fs::write(root.join(path), "other").unwrap();
  assert!(verify(&root, &readiness).is_err());
  assert!(register(&root, path).is_err());
  std::fs::rename(root.join(path), root.join("outputs/old.pt")).unwrap();
  std::fs::write(root.join(path), "first").unwrap();
  assert!(verify(&root, &readiness).is_err());
  assert!(register(&root, path).is_err());
}

#[test]
fn missing_tracking_unsafe_and_foreign_records_are_rejected() {
  let (_temporary, root) = fixture();
  assert!(register(&root, "outputs/missing.pt").is_err());
  for path in [
    "outputs/metrics.jsonl",
    "outputs/params.json",
    "outputs/../secret",
    "code/model.pt",
  ] {
    assert!(register(&root, path).is_err());
  }
  std::fs::write(root.join("outputs/model.pt"), "checkpoint").unwrap();
  register(&root, "outputs/model.pt").unwrap();
  let mut registry: Registry =
    serde_json::from_slice(&std::fs::read(root.join(RECORD)).unwrap()).unwrap();
  registry.run_id = "another-run".into();
  fs::atomic_json(&root.join(RECORD), &registry).unwrap();
  assert!(list(&root).is_err());
}

#[cfg(unix)]
#[test]
fn linked_sources_and_linked_registry_are_rejected() {
  use std::os::unix::fs::symlink;
  let (_temporary, root) = fixture();
  let outside = tempfile::tempdir().unwrap();
  std::fs::write(outside.path().join("model.pt"), "private").unwrap();
  symlink(
    outside.path().join("model.pt"),
    root.join("outputs/model.pt"),
  )
  .unwrap();
  assert!(register(&root, "outputs/model.pt").is_err());
  std::fs::write(root.join("outputs/safe.pt"), "checkpoint").unwrap();
  symlink(outside.path().join("registry.json"), root.join(RECORD)).unwrap();
  assert!(register(&root, "outputs/safe.pt").is_err());
}

#[test]
fn published_run_without_checkpoint_registry_remains_sealed() {
  let (_temporary, root) = fixture();
  let path = "outputs/late.pt";
  std::fs::write(root.join(path), "late checkpoint").unwrap();
  fs::atomic_json(
    &root.join("run-state.json"),
    &json!({"run_id":"run-fixture","status":"completed"}),
  )
  .unwrap();
  let publisher_state = root.join("publishing-state.json");
  fs::atomic_json(
    &publisher_state,
    &json!({"schema_version":1,"run_id":"run-fixture","status":"published"}),
  )
  .unwrap();
  let saved_publisher = std::fs::read(&publisher_state).unwrap();
  assert!(!root.join(RECORD).exists());
  assert!(is_closed(&root).unwrap());
  assert!(
    register_with_labels(&root, path, &["latest".into()])
      .unwrap_err()
      .to_string()
      .contains("file-upload")
  );
  assert!(!root.join(RECORD).exists());
  assert_eq!(std::fs::read(root.join(path)).unwrap(), b"late checkpoint");
  assert_eq!(std::fs::read(&publisher_state).unwrap(), saved_publisher);
}

#[test]
fn unfinished_publication_without_checkpoint_registry_accepts_handoffs() {
  for status in ["pending", "publishing", "retrying", "error"] {
    let (_temporary, root) = fixture();
    let path = "outputs/ready.pt";
    std::fs::write(root.join(path), "checkpoint").unwrap();
    fs::atomic_json(
      &root.join("publishing-state.json"),
      &json!({"schema_version":1,"run_id":"run-fixture","status":status}),
    )
    .unwrap();
    assert!(!is_closed(&root).unwrap());
    assert_eq!(register(&root, path).unwrap()["sync_status"], "registered");
  }
}

#[test]
fn sealed_inventory_rejects_new_handoffs_and_alias_moves_but_keeps_exact_repeats() {
  let (_temporary, root) = fixture();
  std::fs::write(root.join("outputs/1.pt"), "checkpoint").unwrap();
  let first = register_with_labels(&root, "outputs/1.pt", &["best".into()]).unwrap();
  assert!(!close_if_ready(&root, || panic!("pending checkpoint cannot seal")).unwrap());
  let registration = records(&root).unwrap().remove(0);
  update(&root, &registration, "cloud", None).unwrap();
  assert!(close_if_ready(&root, || Ok(())).unwrap());
  assert!(is_closed(&root).unwrap());
  let repeated = register_with_labels(&root, "outputs/1.pt", &["best".into()]).unwrap();
  assert_eq!(repeated["registered_at"], first["registered_at"]);
  assert_eq!(repeated["sync_status"], "cloud");
  assert!(register_with_labels(&root, "outputs/1.pt", &["latest".into()]).is_err());
  std::fs::write(root.join("outputs/2.pt"), "checkpoint").unwrap();
  assert!(
    register(&root, "outputs/2.pt")
      .unwrap_err()
      .to_string()
      .contains("file-upload")
  );
}

#[test]
fn only_failed_readiness_can_be_discarded_and_original_bytes_are_preserved() {
  let (_temporary, root) = fixture();
  let path = "outputs/bad.pt";
  std::fs::write(root.join(path), "checkpoint").unwrap();
  register(&root, path).unwrap();
  let registration = records(&root).unwrap().remove(0);
  assert!(unregister(&root, path).is_err());
  update(&root, &registration, "uploading", None).unwrap();
  assert!(unregister(&root, path).is_err());
  update(&root, &registration, "cloud", None).unwrap();
  assert!(unregister(&root, path).is_err());
  update(
    &root,
    &registration,
    "needs_attention",
    Some("Checkpoint changed"),
  )
  .unwrap();
  assert_eq!(unregister(&root, path).unwrap()["unregistered"], true);
  assert_eq!(std::fs::read(root.join(path)).unwrap(), b"checkpoint");
  assert!(records(&root).unwrap().is_empty());
  assert!(
    register(&root, path)
      .unwrap_err()
      .to_string()
      .contains("retired")
  );
  std::fs::write(root.join("outputs/corrected.pt"), "new-checkpoint").unwrap();
  register(&root, "outputs/corrected.pt").unwrap();
  let registration = records(&root).unwrap().remove(0);
  update(&root, &registration, "cloud", None).unwrap();
  assert!(close_if_ready(&root, || Ok(())).unwrap());
}

#[test]
fn failed_final_manifest_write_leaves_the_handoff_open_for_retry() {
  let (_temporary, root) = fixture();
  std::fs::write(root.join("outputs/model.pt"), "checkpoint").unwrap();
  register(&root, "outputs/model.pt").unwrap();
  let registration = records(&root).unwrap().remove(0);
  update(&root, &registration, "cloud", None).unwrap();
  assert!(close_if_ready(&root, || Err(message("disk unavailable"))).is_err());
  assert!(!is_closed(&root).unwrap());
  std::fs::write(root.join("outputs/new.pt"), "checkpoint").unwrap();
  assert!(register(&root, "outputs/new.pt").is_ok());
}
