use super::*;
use crate::config::RunInputConfig;

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::thread;

use crate::service::types::{FileRecord, FileStorage, FileTarget, Request, Response};

fn report(bytes: &[u8]) -> Value {
  json!({"size":bytes.len(),"sha256":Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect::<String>()})
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, RunServiceConfig) {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let repo = root.join("project");
  let run = repo.join(".expri/runs/run-1");
  fs::directories(&run).unwrap();
  let config = RunServiceConfig {
    client_config: root.join("private-client.toml"),
    project_id: "project".into(),
    origin: "worker".into(),
    dashboard_url: None,
    inputs: vec![RunInputConfig {
      input_id: "dataset".into(),
      destination: "data/train.bin".into(),
    }],
    publish: true,
  };
  (temporary, repo, run, config)
}

#[test]
fn hardlinks_pin_cache_identity_across_atomic_refresh_for_other_runs() {
  let (_temporary, repo, run, _config) = fixture();
  let cache = repo.join(".expri/inputs/project/dataset/file");
  fs::directories(cache.parent().unwrap()).unwrap();
  std::fs::write(&cache, b"original-dataset").unwrap();
  let first = run.join("inputs/train.bin");
  fs::directories(first.parent().unwrap()).unwrap();
  pin_verified(&cache, &first, &report(b"original-dataset"), &mut || {
    Ok(false)
  })
  .unwrap();
  assert_read_only(&cache);
  assert_read_only(&first);
  assert!(fs::unchanged(
    &fs::open(&cache).unwrap().metadata().unwrap(),
    &fs::open(&first).unwrap().metadata().unwrap()
  ));
  let replacement = cache.with_extension("new");
  std::fs::write(&replacement, b"updated-dataset").unwrap();
  std::fs::rename(&replacement, &cache).unwrap();
  let other = repo.join(".expri/runs/run-2/inputs/train.bin");
  fs::directories(other.parent().unwrap()).unwrap();
  pin_verified(&cache, &other, &report(b"updated-dataset"), &mut || {
    Ok(false)
  })
  .unwrap();
  assert_read_only(&cache);
  assert_read_only(&other);
  assert_eq!(std::fs::read(&first).unwrap(), b"original-dataset");
  assert_eq!(std::fs::read(&other).unwrap(), b"updated-dataset");
  assert_eq!(std::fs::read(&cache).unwrap(), b"updated-dataset");
}

#[test]
fn existing_binding_is_never_replaced_or_silently_adopted() {
  let (_temporary, repo, run, config) = fixture();
  let source = repo.join("source");
  std::fs::write(&source, b"verified").unwrap();
  let binding = run.join("inputs/data/train.bin");
  fs::directories(binding.parent().unwrap()).unwrap();
  std::fs::write(&binding, b"existing").unwrap();
  assert!(pin_verified(&source, &binding, &report(b"verified"), &mut || Ok(false)).is_err());
  assert_eq!(std::fs::read(&binding).unwrap(), b"existing");
  // The preparation guard runs before reading credentials or requesting the service.
  assert!(
    prepare(&repo, &run, &config, &mut || Ok(false))
      .unwrap_err()
      .to_string()
      .contains("binding already exists")
  );
}

#[test]
fn cross_filesystem_copy_verifies_existing_digest_and_preserves_existing_destination() {
  let (_temporary, repo, run, _config) = fixture();
  let source = repo.join("source");
  std::fs::write(&source, b"dataset").unwrap();
  let binding = run.join("inputs/data.bin");
  fs::directories(binding.parent().unwrap()).unwrap();
  copy_verified(
    &mut fs::open(&source).unwrap(),
    &binding,
    7,
    report(b"dataset")["sha256"].as_str().unwrap(),
    &mut || Ok(false),
  )
  .unwrap();
  assert_eq!(std::fs::read(&binding).unwrap(), b"dataset");
  assert_read_only(&binding);
  assert!(
    copy_verified(
      &mut fs::open(&source).unwrap(),
      &binding,
      7,
      report(b"dataset")["sha256"].as_str().unwrap(),
      &mut || Ok(false)
    )
    .is_err()
  );
  assert_eq!(std::fs::read(&binding).unwrap(), b"dataset");
  let bad = run.join("inputs/bad.bin");
  assert!(
    copy_verified(
      &mut fs::open(&source).unwrap(),
      &bad,
      7,
      &"0".repeat(64),
      &mut || Ok(false)
    )
    .is_err()
  );
  assert!(!bad.exists());
  assert_eq!(
    std::fs::read_dir(binding.parent().unwrap())
      .unwrap()
      .count(),
    1
  );
}

fn assert_read_only(path: &Path) {
  let metadata = std::fs::symlink_metadata(path).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    assert_eq!(metadata.mode() & 0o7777, 0o400);
    if unsafe { libc::geteuid() } != 0 {
      assert!(std::fs::OpenOptions::new().write(true).open(path).is_err());
    }
  }
  #[cfg(not(unix))]
  assert!(metadata.permissions().readonly());
}

#[test]
fn cancelled_cross_filesystem_copy_never_publishes_partial_input() {
  let (_temporary, repo, run, _config) = fixture();
  let source = repo.join("source");
  let bytes = vec![b'x'; 3 * 64 * 1024];
  std::fs::write(&source, &bytes).unwrap();
  let binding = run.join("inputs/data.bin");
  fs::directories(binding.parent().unwrap()).unwrap();
  let mut checks = 0;
  let failure = copy_verified(
    &mut fs::open(&source).unwrap(),
    &binding,
    bytes.len() as u64,
    report(&bytes)["sha256"].as_str().unwrap(),
    &mut || {
      checks += 1;
      Ok(checks >= 3)
    },
  )
  .unwrap_err();
  assert!(failure.to_string().contains("cancelled"));
  assert_eq!(checks, 3);
  assert!(!binding.exists());
  assert_eq!(
    std::fs::read_dir(binding.parent().unwrap())
      .unwrap()
      .count(),
    0
  );
  assert_eq!(std::fs::read(source).unwrap(), bytes);
}

#[test]
fn preparation_checks_cancellation_before_network_or_credentials() {
  let (_temporary, repo, run, config) = fixture();
  assert!(
    prepare(&repo, &run, &config, &mut || Ok(true))
      .unwrap_err()
      .to_string()
      .contains("cancelled")
  );
  assert!(!repo.join(".expri/inputs").exists());
  let mut empty = config;
  empty.inputs.clear();
  assert!(
    prepare(&repo, &run, &empty, &mut || panic!(
      "no inputs need no network or cancellation wait"
    ))
    .unwrap()
    .is_none()
  );
}

#[cfg(unix)]
#[test]
fn symlink_sources_bindings_and_binding_parents_are_rejected() {
  use std::os::unix::fs::symlink;
  let (_temporary, repo, run, _config) = fixture();
  let source = repo.join("source");
  std::fs::write(&source, b"dataset").unwrap();
  let linked_source = repo.join("linked");
  symlink(&source, &linked_source).unwrap();
  let binding = run.join("inputs/data.bin");
  fs::directories(binding.parent().unwrap()).unwrap();
  assert!(
    pin_verified(&linked_source, &binding, &report(b"dataset"), &mut || Ok(
      false
    ))
    .is_err()
  );
  symlink(&source, &binding).unwrap();
  assert!(pin_verified(&source, &binding, &report(b"dataset"), &mut || Ok(false)).is_err());
  assert_eq!(std::fs::read(&source).unwrap(), b"dataset");
  let outside = tempfile::tempdir().unwrap();
  symlink(outside.path(), run.join("linked-inputs")).unwrap();
  assert!(
    pin_verified(
      &source,
      &run.join("linked-inputs/dataset.bin"),
      &report(b"dataset"),
      &mut || Ok(false)
    )
    .is_err()
  );
  assert!(!outside.path().join("dataset.bin").exists());
}

#[test]
fn changing_input_id_keeps_same_task_destination_and_uses_separate_project_caches() {
  let (_temporary, repo, first_run, mut config) = fixture();
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  let task = thread::spawn(move || {
    for (id, bytes) in [
      ("dataset-v1", b"first".as_slice()),
      ("dataset-v2", b"second".as_slice()),
    ] {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
      let mut reader = BufReader::new(stream.try_clone().unwrap());
      let mut line = String::new();
      reader.read_line(&mut line).unwrap();
      assert!(line.starts_with("POST /v1/request "));
      let mut length = 0;
      loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
          break;
        }
        if let Some((key, value)) = line.split_once(':')
          && key.eq_ignore_ascii_case("content-length")
        {
          length = value.trim().parse::<usize>().unwrap();
        }
      }
      assert!(length <= crate::service::types::MAX_REQUEST);
      let mut request = vec![0; length];
      reader.read_exact(&mut request).unwrap();
      let Request::GetFile { target } = serde_json::from_slice(&request).unwrap() else {
        panic!("input cache should not download already verified bytes")
      };
      assert_eq!(
        target,
        FileTarget::Input {
          project_id: "project".into(),
          input_id: id.into()
        }
      );
      let response = Response::File {
        file: FileRecord {
          target,
          size: bytes.len() as u64,
          sha256: Some(report(bytes)["sha256"].as_str().unwrap().into()),
          storage: FileStorage::Object,
        },
      };
      let response = serde_json::to_vec(&response).unwrap();
      write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
      )
      .unwrap();
      stream.write_all(&response).unwrap();
    }
  });
  std::fs::write(
    &config.client_config,
    format!("url={url:?}\ntoken_env='PATH'\n"),
  )
  .unwrap();
  for (id, bytes) in [
    ("dataset-v1", b"first".as_slice()),
    ("dataset-v2", b"second".as_slice()),
  ] {
    let cached = repo.join(".expri/inputs/project").join(id).join("file");
    fs::directories(cached.parent().unwrap()).unwrap();
    std::fs::write(cached, bytes).unwrap();
  }
  config.inputs[0].input_id = "dataset-v1".into();
  let (first_inputs, first_receipts) = prepare(&repo, &first_run, &config, &mut || Ok(false))
    .unwrap()
    .unwrap();
  assert_eq!(first_inputs, first_run.join("inputs"));
  assert_eq!(first_receipts[0]["input_id"], "dataset-v1");
  let second_run = repo.join(".expri/runs/run-2");
  fs::directories(&second_run).unwrap();
  config.inputs[0].input_id = "dataset-v2".into();
  let (second_inputs, second_receipts) = prepare(&repo, &second_run, &config, &mut || Ok(false))
    .unwrap()
    .unwrap();
  assert_eq!(second_receipts[0]["input_id"], "dataset-v2");
  assert_eq!(
    std::fs::read(first_inputs.join("data/train.bin")).unwrap(),
    b"first"
  );
  assert_eq!(
    std::fs::read(second_inputs.join("data/train.bin")).unwrap(),
    b"second"
  );
  task.join().unwrap();
}
