use super::*;
use crate::service::store::{
  Store,
  tests::{MockStorage, scope},
};
use crate::service::types::{FileStorage, Request, Response};
use base64::Engine;

fn target(path: &str) -> FileTarget {
  FileTarget::Run {
    scope: scope(),
    path: path.into(),
  }
}

fn append_bytes(store: &Store<MockStorage>, path: &str, offset: u64, bytes: &[u8]) {
  for (index, chunk) in bytes.chunks(64 * 1024).enumerate() {
    store
      .execute(Request::AppendTracking {
        scope: scope(),
        path: path.into(),
        offset: offset + (index * 64 * 1024) as u64,
        data_base64: base64::engine::general_purpose::STANDARD.encode(chunk),
      })
      .unwrap();
  }
}

fn compact_fixture(store: &Store<MockStorage>, count: u64) {
  let target = target(RUN_SYSTEM_PATH);
  let mut raw = open_path(
    &store.directory,
    &raw_path(&store.directory, &target, 1).unwrap(),
    false,
    true,
  )
  .unwrap();
  let mut db = store.db().unwrap();
  for index in 0..count {
    compact_block(
      &store.directory,
      &mut db,
      &target,
      &mut raw,
      index * BLOCK_SIZE,
    )
    .unwrap();
    // Emulate holes on every test platform. The production CLI uses fallocate.
    raw.seek(SeekFrom::Start(index * BLOCK_SIZE)).unwrap();
    raw.write_all(&vec![0; BLOCK_SIZE as usize]).unwrap();
    raw.sync_all().unwrap();
  }
}

fn fixture_bytes() -> Vec<u8> {
  (0..(2 * BLOCK_SIZE + 9173))
    .map(|index| ((index * 7 + index / 19) % 251) as u8)
    .collect()
}

#[test]
fn logical_ranges_and_retries_cross_blocks_and_plain_tail_after_restart() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let expected = fixture_bytes();
  append_bytes(&store, RUN_SYSTEM_PATH, 0, &expected);
  append_bytes(&store, "logs/stdout.log", 0, b"ordinary stream stays raw\n");
  let record = store.file(&target(RUN_SYSTEM_PATH)).unwrap();
  compact_fixture(&store, 2);
  assert_eq!(record.size, expected.len() as u64);
  assert_eq!(
    std::fs::metadata(raw_path(directory.path(), &record.target, 1).unwrap())
      .unwrap()
      .len(),
    record.size
  );
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  for (offset, length) in [
    (0, 123),
    (BLOCK_SIZE - 11, 38),
    (2 * BLOCK_SIZE - 17, 66),
    (2 * BLOCK_SIZE + 19, 300),
  ] {
    assert_eq!(
      store.tracking_range(&record, offset, length).unwrap(),
      expected[offset as usize..offset as usize + length]
    );
    append_bytes(
      &store,
      RUN_SYSTEM_PATH,
      offset,
      &expected[offset as usize..offset as usize + length],
    );
  }
  let offset = BLOCK_SIZE - 17;
  assert_eq!(
    store
      .execute(Request::AppendTracking {
        scope: scope(),
        path: RUN_SYSTEM_PATH.into(),
        offset,
        data_base64: base64::engine::general_purpose::STANDARD.encode(b"conflicting bytes")
      })
      .unwrap_err()
      .status,
    409
  );
  // A retry overlapping the old plain tail may append new bytes, preserving offsets.
  let suffix = [
    expected[expected.len() - 13..].to_vec(),
    b"new attempt bytes".to_vec(),
  ]
  .concat();
  append_bytes(&store, RUN_SYSTEM_PATH, expected.len() as u64 - 13, &suffix);
  let updated = store.file(&record.target).unwrap();
  let mut actual = Vec::new();
  store
    .tracking_open(&updated)
    .unwrap()
    .read_to_end(&mut actual)
    .unwrap();
  assert_eq!(actual, [expected, b"new attempt bytes".to_vec()].concat());
  assert_eq!(
    store
      .tracking_range(&store.file(&target("logs/stdout.log")).unwrap(), 0, 26)
      .unwrap(),
    b"ordinary stream stays raw\n"
  );
  let mut reader = store.tracking_open(&updated).unwrap();
  assert_eq!(reader.seek(SeekFrom::End(-3)).unwrap(), updated.size - 3);
  assert!(reader.seek(SeekFrom::Current(-i64::MAX)).is_err());
}

#[test]
fn compressed_corruption_and_missing_pack_fail_closed() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  append_bytes(&store, RUN_SYSTEM_PATH, 0, &fixture_bytes());
  compact_fixture(&store, 1);
  let record = store.file(&target(RUN_SYSTEM_PATH)).unwrap();
  let pack_path = pack_path(directory.path(), &record.target).unwrap();
  let mut pack = open_path(directory.path(), &pack_path, false, true).unwrap();
  pack.write_all(b"invalid compressed bytes").unwrap();
  pack.sync_all().unwrap();
  assert!(store.tracking_range(&record, 0, 20).is_err());
  drop(pack);
  drop(store);
  std::fs::remove_file(pack_path).unwrap();
  assert!(Store::open(directory.path(), MockStorage::default()).is_err());
}

#[test]
fn preview_is_live_readonly_and_apply_requires_exclusive_lease_and_new_schema() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  append_bytes(&store, RUN_SYSTEM_PATH, 0, &fixture_bytes());
  let preview = compact_system_captures(directory.path(), false).unwrap();
  assert_eq!(preview["eligible_bytes"], 2 * BLOCK_SIZE);
  assert_eq!(preview["logical_bytes"], 2 * BLOCK_SIZE + 9173);
  assert!(
    compact_system_captures(directory.path(), true)
      .unwrap_err()
      .to_string()
      .contains("stop the service")
  );
  store.db().unwrap().execute_batch("DROP TABLE tracking_system_blocks; DROP TABLE tracking_system_packs; PRAGMA user_version=12;").unwrap();
  assert_eq!(
    compact_system_captures(directory.path(), false).unwrap()["compressed_blocks"],
    0
  );
  drop(store);
  assert!(
    compact_system_captures(directory.path(), true)
      .unwrap_err()
      .to_string()
      .contains("upgraded service")
  );
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert_eq!(
    store.file(&target(RUN_SYSTEM_PATH)).unwrap().size,
    2 * BLOCK_SIZE + 9173
  );
  assert_eq!(
    store
      .db()
      .unwrap()
      .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
      .unwrap(),
    crate::service::store::METADATA_SCHEMA_VERSION
  );
}

#[test]
fn recovery_discards_uncommitted_pack_tail_but_keeps_exact_mapped_prefix() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let bytes = fixture_bytes();
  append_bytes(&store, RUN_SYSTEM_PATH, 0, &bytes);
  compact_fixture(&store, 1);
  let target = target(RUN_SYSTEM_PATH);
  let length = blocks(&store.db().unwrap(), &target).unwrap()[0].packed_size;
  let mut pack = open_path(
    directory.path(),
    &pack_path(directory.path(), &target).unwrap(),
    false,
    true,
  )
  .unwrap();
  pack.seek(SeekFrom::End(0)).unwrap();
  pack.write_all(b"uncommitted compression attempt").unwrap();
  pack.sync_all().unwrap();
  drop(pack);
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert_eq!(
    std::fs::metadata(pack_path(directory.path(), &target).unwrap())
      .unwrap()
      .len(),
    length
  );
  assert_eq!(
    store
      .tracking_range(&store.file(&target).unwrap(), BLOCK_SIZE - 3, 9)
      .unwrap(),
    bytes[BLOCK_SIZE as usize - 3..BLOCK_SIZE as usize + 6]
  );
}

#[test]
fn interrupted_first_mapping_recovers_plaintext_without_ever_punching() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let bytes = fixture_bytes();
  append_bytes(&store, RUN_SYSTEM_PATH, 0, &bytes);
  let target = target(RUN_SYSTEM_PATH);
  let mut raw = open_path(
    directory.path(),
    &raw_path(directory.path(), &target, 1).unwrap(),
    false,
    true,
  )
  .unwrap();
  store
    .db()
    .unwrap()
    .execute_batch(
      "CREATE TRIGGER reject_system_block BEFORE INSERT ON tracking_system_blocks
    BEGIN SELECT RAISE(ABORT,'injected mapping failure'); END;",
    )
    .unwrap();
  assert!(
    compact_block(
      directory.path(),
      &mut store.db().unwrap(),
      &target,
      &mut raw,
      0
    )
    .is_err()
  );
  assert_eq!(
    frontier(&store.db().unwrap(), &target).unwrap(),
    Some((0, 0))
  );
  assert!(
    std::fs::metadata(pack_path(directory.path(), &target).unwrap())
      .unwrap()
      .len()
      > 0
  );
  assert_eq!(
    std::fs::read(raw_path(directory.path(), &target, 1).unwrap()).unwrap(),
    bytes
  );
  drop(raw);
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert_eq!(
    std::fs::metadata(pack_path(directory.path(), &target).unwrap())
      .unwrap()
      .len(),
    0
  );
  assert_eq!(
    store
      .tracking_range(&store.file(&target).unwrap(), 0, bytes.len())
      .unwrap(),
    bytes
  );
}

#[test]
fn compression_crash_child() {
  let Ok(directory) = std::env::var("EXPRI_SYSTEM_COMPRESSION_CRASH_DIRECTORY") else {
    return;
  };
  let store = Store::open(Path::new(&directory), MockStorage::default()).unwrap();
  let target = target(RUN_SYSTEM_PATH);
  let mut raw = open_path(
    &store.directory,
    &raw_path(&store.directory, &target, 1).unwrap(),
    false,
    true,
  )
  .unwrap();
  compact_block(
    &store.directory,
    &mut store.db().unwrap(),
    &target,
    &mut raw,
    0,
  )
  .unwrap();
}

#[test]
fn process_kills_before_and_after_mapping_commit_preserve_bytes_and_resume_compaction() {
  use std::process::{Command, Stdio};
  use std::time::{Duration, Instant};
  for phase in ["system_pack_synced", "system_mapping_committed"] {
    let directory = tempfile::tempdir().unwrap();
    let bytes = fixture_bytes();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    append_bytes(&store, RUN_SYSTEM_PATH, 0, &bytes);
    drop(store);
    let mut child = Command::new(std::env::current_exe().unwrap())
      .args([
        "--exact",
        "service::store::tracking::system_compression::tests::compression_crash_child",
        "--nocapture",
      ])
      .env("EXPRI_SYSTEM_COMPRESSION_CRASH_DIRECTORY", directory.path())
      .env("EXPRI_TRACKING_CRASH_DIRECTORY", directory.path())
      .env("EXPRI_TRACKING_CRASH_PHASE", phase)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .unwrap();
    let started = Instant::now();
    while !directory.path().join("tracking-test-ready").exists() {
      if child.try_wait().unwrap().is_some() || started.elapsed() > Duration::from_secs(10) {
        let _ = child.kill();
        let _ = child.wait();
        panic!("checkpoint {phase} not reached");
      }
      std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    assert_eq!(
      store
        .tracking_range(
          &store.file(&target(RUN_SYSTEM_PATH)).unwrap(),
          0,
          bytes.len()
        )
        .unwrap(),
      bytes
    );
    let next = if phase == "system_mapping_committed" {
      BLOCK_SIZE
    } else {
      0
    };
    let target = target(RUN_SYSTEM_PATH);
    let mut raw = open_path(
      &store.directory,
      &raw_path(&store.directory, &target, 1).unwrap(),
      false,
      true,
    )
    .unwrap();
    compact_block(
      &store.directory,
      &mut store.db().unwrap(),
      &target,
      &mut raw,
      next,
    )
    .unwrap();
    assert_eq!(
      store
        .tracking_range(&store.file(&target).unwrap(), 0, bytes.len())
        .unwrap(),
      bytes
    );
  }
}

#[test]
fn missing_frontier_missing_rows_gaps_and_truncated_pack_reject_restart() {
  for corruption in ["first", "last", "rows", "frontier", "all", "truncated"] {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    append_bytes(&store, RUN_SYSTEM_PATH, 0, &fixture_bytes());
    compact_fixture(&store, 2);
    match corruption {
      "first" => {
        store
          .db()
          .unwrap()
          .execute("DELETE FROM tracking_system_blocks WHERE raw_offset=0", [])
          .unwrap();
      }
      "last" => {
        store
          .db()
          .unwrap()
          .execute(
            "DELETE FROM tracking_system_blocks WHERE raw_offset=?1",
            [BLOCK_SIZE],
          )
          .unwrap();
      }
      "rows" => {
        store
          .db()
          .unwrap()
          .execute("DELETE FROM tracking_system_blocks", [])
          .unwrap();
      }
      "frontier" => {
        store
          .db()
          .unwrap()
          .execute("DELETE FROM tracking_system_packs", [])
          .unwrap();
      }
      "all" => {
        store
          .db()
          .unwrap()
          .execute_batch("DELETE FROM tracking_system_blocks; DELETE FROM tracking_system_packs;")
          .unwrap();
      }
      "truncated" => {
        let pack = open_path(
          directory.path(),
          &pack_path(directory.path(), &target(RUN_SYSTEM_PATH)).unwrap(),
          false,
          true,
        )
        .unwrap();
        pack.set_len(pack.metadata().unwrap().len() - 1).unwrap();
        pack.sync_all().unwrap();
      }
      _ => unreachable!(),
    }
    drop(store);
    assert!(
      Store::open(directory.path(), MockStorage::default()).is_err(),
      "{corruption}"
    );
  }
}

fn put(store: &Store<MockStorage>, path: &str, revision: u64, value: Value) {
  let bytes = serde_json::to_vec(&value).unwrap();
  store
    .execute(Request::PutDocument {
      scope: scope(),
      path: path.into(),
      revision,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

#[test]
fn compressed_capture_seals_continues_and_builds_the_original_zip_bytes() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let inventory = json!({"kind":"inventory","inventory":{"revision":1,"inventory":{
    "observed_at_ms":0,"hostname":"fixture","os_name":null,"os_version":null,"kernel_version":null,
    "architecture":"x86_64","cpu":{"brand":"fixture","vendor":"fixture","logical_cpu_count":1,"physical_core_count":1},
    "memory_total_bytes":1024,"measurement_scope":"host","devices":[],"warnings":[]}}});
  let sample = json!({"kind":"sample","sample":{"sequence":1,"session_id":"session-1","observed_at_ms":1000,
    "interval_ms":1000,"collection_duration_ms":1,"inventory_revision":1,"metrics":{},"counters":{},"warnings":[]}});
  let mut bytes = Vec::new();
  for value in [
    json!({"kind":"capture","schema_version":1,"capture_id":"capture-1","machine_id":"machine-1","start_ms":0}),
    inventory,
  ] {
    serde_json::to_writer(&mut bytes, &value).unwrap();
    bytes.push(b'\n');
  }
  while bytes.len() < BLOCK_SIZE as usize + 100 {
    serde_json::to_writer(&mut bytes, &sample).unwrap();
    bytes.push(b'\n');
  }
  serde_json::to_writer(
    &mut bytes,
    &json!({"kind":"end","end_ms":2000,"complete":true,"warnings":[]}),
  )
  .unwrap();
  bytes.push(b'\n');
  append_bytes(&store, RUN_SYSTEM_PATH, 0, &bytes);
  compact_fixture(&store, 1);
  for path in crate::service::types::TRACKING_DOCUMENT_PATHS {
    put(
      &store,
      path,
      1,
      if path == "run-state.json" {
        json!({"status":"completed","system_capture_id":"capture-1",
      "system_capture_status":"complete","machine_id":"machine-1","started_at":"1970-01-01T00:00:00Z","finished_at":"1970-01-01T00:00:02Z"})
      } else {
        json!({})
      },
    );
  }
  for path in crate::service::types::TRACKING_STREAM_PATHS {
    if path != RUN_SYSTEM_PATH {
      append_bytes(&store, path, 0, b"fixture\n");
    }
  }
  let Response::Files { files } = store.list_files(&scope()).unwrap() else {
    panic!("files");
  };
  let mut documents = std::collections::BTreeMap::new();
  let mut streams = std::collections::BTreeMap::new();
  for file in files {
    if let (FileTarget::Run { path, .. }, FileStorage::Tracking { revision, .. }) =
      (file.target, file.storage)
    {
      if super::super::document_path(&path) {
        documents.insert(path, revision);
      } else {
        streams.insert(path, file.size);
      }
    }
  }
  store
    .execute(Request::SealRun {
      scope: scope(),
      documents: documents.clone(),
      streams: streams.clone(),
      incomplete: false,
    })
    .unwrap();
  // Archive upload cannot reach the fixture object store, but staging must finish first.
  assert!(store.archive_cycle().is_err());
  let archive_path = directory.path().join("archives/1/result.zip");
  let original_zip = std::fs::read(&archive_path).unwrap();
  let mut archive = zip::ZipArchive::new(File::open(&archive_path).unwrap()).unwrap();
  let mut archived = Vec::new();
  archive
    .by_name(RUN_SYSTEM_PATH)
    .unwrap()
    .read_to_end(&mut archived)
    .unwrap();
  assert_eq!(archived, bytes);
  // Existing immutable ZIP snapshots stay unchanged when a new attempt unseals the stream.
  store
    .execute(Request::BeginRunAttempt {
      scope: scope(),
      request_id: "continue-1".into(),
      expected_epoch: 0,
      attempt: 2,
      documents,
      streams,
    })
    .unwrap();
  assert_eq!(std::fs::read(archive_path).unwrap(), original_zip);
  assert_eq!(
    store
      .tracking_range(
        &store.file(&target(RUN_SYSTEM_PATH)).unwrap(),
        0,
        bytes.len()
      )
      .unwrap(),
    bytes
  );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_apply_reclaims_allocated_blocks_and_is_idempotent() {
  use std::os::unix::fs::MetadataExt;
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let bytes = fixture_bytes();
  append_bytes(&store, RUN_SYSTEM_PATH, 0, &bytes);
  let raw = raw_path(directory.path(), &target(RUN_SYSTEM_PATH), 1).unwrap();
  let allocated_before = std::fs::metadata(&raw).unwrap().blocks();
  drop(store);
  let result = compact_system_captures(directory.path(), true).unwrap();
  assert_eq!(result["processed_blocks"], 2);
  assert!(std::fs::metadata(&raw).unwrap().blocks() < allocated_before);
  assert_eq!(std::fs::metadata(&raw).unwrap().len(), bytes.len() as u64);
  assert_eq!(
    compact_system_captures(directory.path(), true).unwrap()["processed_blocks"],
    0
  );
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert_eq!(
    store
      .tracking_range(
        &store.file(&target(RUN_SYSTEM_PATH)).unwrap(),
        0,
        bytes.len()
      )
      .unwrap(),
    bytes
  );
}
