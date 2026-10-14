use super::*;

fn archive(directory: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
  let path = directory.join("source.zip");
  let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
  for (name, bytes) in entries {
    writer
      .start_file(*name, SimpleFileOptions::default())
      .unwrap();
    writer.write_all(bytes).unwrap();
  }
  writer.finish().unwrap();
  path
}

#[test]
fn extraction_rejects_escape_and_owned_metadata_and_is_atomic() {
  for name in [
    "../escape",
    "/escape",
    "C:/escape",
    "a\\escape",
    ".expri/checkout.manifest",
    ".git/config",
  ] {
    let temporary = tempfile::tempdir().unwrap();
    let source = archive(temporary.path(), &[(name, b"unsafe")]);
    let output = temporary.path().join("output");
    assert!(unpack_source(&source, &output).is_err(), "accepted {name}");
    assert!(!output.exists());
  }
}

#[test]
fn extraction_writes_checkout_manifest_and_does_not_replace_destination() {
  let temporary = tempfile::tempdir().unwrap();
  let source = archive(temporary.path(), &[("src/train.py", b"print(1)")]);
  let output = temporary.path().join("output");
  unpack_source(&source, &output).unwrap();
  assert_eq!(fs::read(output.join("src/train.py")).unwrap(), b"print(1)");
  assert_eq!(
    fs::read_to_string(output.join(".expri/checkout.manifest")).unwrap(),
    "src/train.py\n"
  );
  assert!(unpack_source(&source, &output).is_err());
}

#[cfg(unix)]
#[test]
fn extraction_preserves_leaf_symlinks_and_executable_bits_and_rejects_escape() {
  use std::os::unix::fs::PermissionsExt;
  let temporary = tempfile::tempdir().unwrap();
  let path = temporary.path().join("source.zip");
  let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
  writer
    .start_file(
      "train.sh",
      SimpleFileOptions::default().unix_permissions(0o755),
    )
    .unwrap();
  writer.write_all(b"#!/bin/sh\n").unwrap();
  writer
    .add_symlink("entry", "train.sh", SimpleFileOptions::default())
    .unwrap();
  writer.finish().unwrap();
  let output = temporary.path().join("output");
  unpack_source(&path, &output).unwrap();
  assert_eq!(
    fs::read_link(output.join("entry")).unwrap(),
    Path::new("train.sh")
  );
  assert_ne!(
    fs::metadata(output.join("train.sh"))
      .unwrap()
      .permissions()
      .mode()
      & 0o111,
    0
  );
  assert_eq!(
    fs::metadata(output.join("train.sh"))
      .unwrap()
      .permissions()
      .mode()
      & 0o7000,
    0
  );
  assert!(validate_symlink(Path::new("entry"), Path::new("../escape")).is_err());
  assert!(validate_symlink(Path::new("sub/entry"), Path::new("../train.sh")).is_ok());
}

#[test]
fn freezing_retains_submission_contents_and_does_not_allocate_a_run() {
  let temporary = tempfile::tempdir().unwrap();
  fs::create_dir(temporary.path().join(".expri")).unwrap();
  fs::write(
    temporary.path().join(".expri/checkout.manifest"),
    "train.py\n",
  )
  .unwrap();
  fs::write(temporary.path().join("train.py"), b"submitted").unwrap();
  let bundle = freeze_source(temporary.path(), &[]).unwrap();
  assert_eq!(
    bundle.path,
    fs::canonicalize(&bundle.path).unwrap(),
    "source upload must receive a canonical path without temporary-directory aliases"
  );
  let retry = freeze_source(temporary.path(), &[]).unwrap();
  assert_eq!(
    bundle.sha256, retry.sha256,
    "unchanged source must produce an identical content-addressed ZIP"
  );
  assert_eq!(bundle.size, retry.size);
  fs::write(temporary.path().join("train.py"), b"later edit").unwrap();
  let output = temporary.path().join("extracted");
  unpack_source(&bundle.path, &output).unwrap();
  assert_eq!(fs::read(output.join("train.py")).unwrap(), b"submitted");
  assert!(!temporary.path().join(".expri/runs").exists());
  assert_eq!(
    crate::archive::sha256_file(&bundle.path).unwrap(),
    (bundle.sha256.clone(), bundle.size)
  );
}

#[test]
fn source_limits_and_symlink_parent_policy_fail_closed() {
  assert!(bounded_total(MAX_SOURCE_BYTES, 1).is_err());
  assert!(bounded_total(u64::MAX, 1).is_err());
  let temporary = tempfile::tempdir().unwrap();
  let path = temporary.path().join("source.zip");
  let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
  writer
    .add_symlink("parent", "elsewhere", SimpleFileOptions::default())
    .unwrap();
  writer
    .start_file("parent/child", SimpleFileOptions::default())
    .unwrap();
  writer.write_all(b"child").unwrap();
  writer.finish().unwrap();
  assert!(unpack_source(&path, &temporary.path().join("output")).is_err());
  let links = BTreeMap::from([
    (PathBuf::from("a"), PathBuf::from("b/..")),
    (PathBuf::from("b"), PathBuf::from(".")),
  ]);
  assert!(
    validate_link_graph(&links).is_err(),
    "symlink expansion must not escape despite lexical containment"
  );
}
