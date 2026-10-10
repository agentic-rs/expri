use super::*;
use crate::assets::Source;

fn descriptor(bytes: &[u8]) -> Descriptor {
  Descriptor {
    version: 1,
    source: Source::Url {
      url: "https://example.com/asset.bin".into(),
    },
    size: bytes.len() as u64,
    sha256: Sha256::digest(bytes)
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect(),
  }
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  (temporary, root)
}

fn assert_read_only(path: &Path) {
  let metadata = fs::symlink_metadata(path).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o400);
  }
  #[cfg(not(unix))]
  assert!(metadata.permissions().readonly());
}

#[test]
fn immutable_run_binding_survives_cache_replacement_by_another_source() {
  let (_temporary, root) = fixture();
  let cache = root.join("cache");
  fs::write(&cache, b"original").unwrap();
  let binding = root.join("run-1/code/data/train.bin");
  bind_verified(&cache, &binding, &descriptor(b"original"), &mut || {
    Ok(false)
  })
  .unwrap();
  assert_read_only(&cache);
  assert_read_only(&binding);
  assert!(unchanged(
    &open(&cache).unwrap().metadata().unwrap(),
    &open(&binding).unwrap().metadata().unwrap()
  ));
  fs::write(root.join("replacement"), b"replacement").unwrap();
  fs::rename(root.join("replacement"), &cache).unwrap();
  let mut other_descriptor = descriptor(b"replacement");
  other_descriptor.source = Source::Expri {
    url: "https://expri.example".into(),
    project_id: "vision".into(),
    input_id: "new-data".into(),
  };
  let other_binding = root.join("run-2/code/data/train.bin");
  bind_verified(&cache, &other_binding, &other_descriptor, &mut || Ok(false)).unwrap();
  assert_eq!(fs::read(binding).unwrap(), b"original");
  assert_eq!(fs::read(other_binding).unwrap(), b"replacement");
}

#[test]
fn binding_never_adopts_or_overwrites_existing_destinations() {
  let (_temporary, root) = fixture();
  let source = root.join("source");
  fs::write(&source, b"verified").unwrap();
  let binding = root.join("destination");
  fs::write(&binding, b"existing").unwrap();
  assert!(
    bind_verified(&source, &binding, &descriptor(b"verified"), &mut || Ok(
      false
    ))
    .is_err()
  );
  assert_eq!(fs::read(binding).unwrap(), b"existing");
  let new_binding = root.join("new");
  assert!(
    bind_verified(
      &source,
      &new_binding,
      &descriptor(b"wrong-size"),
      &mut || Ok(false)
    )
    .is_err()
  );
  assert!(!new_binding.exists());
}

#[test]
fn verified_copy_is_atomic_and_cannot_replace_existing_destination() {
  let (_temporary, root) = fixture();
  let source = root.join("source");
  fs::write(&source, b"dataset").unwrap();
  let binding = root.join("bound");
  copy_verified(
    &mut open(&source).unwrap(),
    &binding,
    &descriptor(b"dataset"),
    &mut || Ok(false),
  )
  .unwrap();
  assert_eq!(fs::read(&binding).unwrap(), b"dataset");
  assert_read_only(&binding);
  assert!(
    copy_verified(
      &mut open(&source).unwrap(),
      &binding,
      &descriptor(b"dataset"),
      &mut || Ok(false)
    )
    .is_err()
  );
  let bad = root.join("bad");
  let mut mismatch = descriptor(b"dataset");
  mismatch.sha256 = "0".repeat(64);
  assert!(
    copy_verified(&mut open(&source).unwrap(), &bad, &mismatch, &mut || Ok(
      false
    ))
    .is_err()
  );
  assert!(!bad.exists());
  assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
}

#[test]
fn cancellation_never_publishes_partial_copies() {
  let (_temporary, root) = fixture();
  let source = root.join("source");
  let bytes = vec![b'x'; 3 * 64 * 1024];
  fs::write(&source, &bytes).unwrap();
  let binding = root.join("binding");
  let mut checks = 0;
  let error = copy_verified(
    &mut open(&source).unwrap(),
    &binding,
    &descriptor(&bytes),
    &mut || {
      checks += 1;
      Ok(checks >= 3)
    },
  )
  .unwrap_err();
  assert!(matches!(error, ExpriError::DownloadCancelled));
  assert!(!binding.exists());
  assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
  assert!(bind_verified(&source, &binding, &descriptor(&bytes), &mut || Ok(true)).is_err());
  assert!(!binding.exists());
}

#[cfg(unix)]
#[test]
fn binding_rejects_linked_sources_destinations_and_parent_directories() {
  use std::os::unix::fs::symlink;
  let (_temporary, root) = fixture();
  let source = root.join("source");
  fs::write(&source, b"dataset").unwrap();
  let linked_source = root.join("linked-source");
  symlink(&source, &linked_source).unwrap();
  let binding = root.join("binding");
  assert!(
    bind_verified(
      &linked_source,
      &binding,
      &descriptor(b"dataset"),
      &mut || Ok(false)
    )
    .is_err()
  );
  symlink(&source, &binding).unwrap();
  assert!(
    bind_verified(&source, &binding, &descriptor(b"dataset"), &mut || Ok(
      false
    ))
    .is_err()
  );
  let outside = tempfile::tempdir().unwrap();
  symlink(outside.path(), root.join("linked-parent")).unwrap();
  assert!(
    bind_verified(
      &source,
      &root.join("linked-parent/asset.bin"),
      &descriptor(b"dataset"),
      &mut || Ok(false)
    )
    .is_err()
  );
  assert!(!outside.path().join("asset.bin").exists());
  assert_eq!(fs::read(source).unwrap(), b"dataset");
}
