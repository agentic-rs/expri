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
      .contains("file-put")
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
