use std::sync::atomic::{AtomicUsize, Ordering};

use expri_core::service_catalog::{RunFileAvailability, validate_file_entry};
use serde_json::{Value, json};

use super::*;
use crate::service::storage::ObjectMetadata;
use crate::service::store::tests::{MockStorage, scope};

#[derive(Clone, Default)]
struct MetadataOnlyStorage {
  backend: MockStorage,
  reads: Arc<AtomicUsize>,
}

impl ObjectStorage for MetadataOnlyStorage {
  fn begin_upload(&self, key: &str, content_type: &str) -> crate::error::Result<String> {
    self.backend.begin_upload(key, content_type)
  }

  fn presign_part(
    &self,
    key: &str,
    upload_id: &str,
    number: u32,
    expires: u32,
  ) -> crate::error::Result<String> {
    self.backend.presign_part(key, upload_id, number, expires)
  }

  fn complete_upload(
    &self,
    key: &str,
    upload_id: &str,
    parts: &[CompletedPart],
  ) -> crate::error::Result<ObjectMetadata> {
    self.backend.complete_upload(key, upload_id, parts)
  }

  fn head(&self, key: &str) -> crate::error::Result<Option<ObjectMetadata>> {
    self.backend.head(key)
  }

  fn presign_get(&self, _: &str, _: u32) -> crate::error::Result<String> {
    self.reads.fetch_add(1, Ordering::Relaxed);
    Err(crate::error::ExpriError::Message(
      "fixture object storage is unavailable".into(),
    ))
  }
}

fn query(prefix: Option<&str>, limit: usize) -> RunFilesQuery {
  RunFilesQuery {
    scope: scope(),
    prefix: prefix.map(str::to_string),
    limit,
    cursor: None,
  }
}

fn object<S: ObjectStorage>(
  store: &Store<S>,
  selected: &RunScope,
  path: &str,
  size: u64,
  sha256: Option<&str>,
) {
  let record = FileRecord {
    target: FileTarget::Run {
      scope: selected.clone(),
      path: path.into(),
    },
    size,
    sha256: sha256.map(str::to_string),
    storage: FileStorage::Object,
    provenance: None,
  };
  let encoded = target_json(&record.target).unwrap();
  store
    .db()
    .unwrap()
    .execute(
      "INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,?3,1)",
      params![
        encoded,
        serde_json::to_string(&record).unwrap(),
        format!("objects/{path}")
      ],
    )
    .unwrap();
}

fn legacy_stream<S: ObjectStorage>(store: &Store<S>, path: &str, size: u64) {
  let target = FileTarget::Run {
    scope: scope(),
    path: path.into(),
  };
  store
    .db()
    .unwrap()
    .execute(
      "INSERT INTO streams(target,size) VALUES(?1,?2)",
      params![target_json(&target).unwrap(), size],
    )
    .unwrap();
}

fn document<S: ObjectStorage>(store: &Store<S>, path: &str, revision: u64, value: &Value) {
  let bytes = serde_json::to_vec(value).unwrap();
  for (index, chunk) in bytes.chunks(STREAM_BATCH).enumerate() {
    store
      .execute(Request::PutDocument {
        scope: scope(),
        path: path.into(),
        revision,
        offset: (index * STREAM_BATCH) as u64,
        total_size: bytes.len() as u64,
        data_base64: base64::engine::general_purpose::STANDARD.encode(chunk),
      })
      .unwrap();
  }
}

fn inventory<S: ObjectStorage>(store: &Store<S>, revision: u64, files: Value, truncated: bool) {
  document(
    store,
    crate::run_artifacts::INVENTORY_PATH,
    revision,
    &json!({
      "schema_version": 1,
      "recorded_at": "2026-10-12T01:02:03Z",
      "files": files,
      "truncated": truncated,
    }),
  );
}

fn file<'a>(page: &'a RunFilesPage, path: &str) -> &'a RunFileEntry {
  page.files.iter().find(|file| file.path == path).unwrap()
}

#[test]
fn merges_all_reported_versions_without_reading_checkpoint_objects() {
  let directory = tempfile::tempdir().unwrap();
  let backend = MetadataOnlyStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  let digest = "a".repeat(64);
  let newer = "b".repeat(64);
  for path in [
    "outputs/matching.pt",
    "outputs/stale.pt",
    "outputs/legacy.pt",
  ] {
    object(&store, &scope(), path, 10, Some(&digest));
  }
  object(
    &store,
    &scope(),
    "outputs/size-changed.pt",
    8,
    Some(&digest),
  );
  object(
    &store,
    &scope(),
    "outputs/received-only.pt",
    42,
    Some(&digest),
  );
  inventory(
    &store,
    1,
    json!([
      {"path":"outputs/matching.pt","size":10,"sha256":digest,"labels":["best","latest"],"sync_status":"uploading","sync_error":"Retrying"},
      {"path":"outputs/stale.pt","size":10,"sha256":newer,"labels":["best"],"sync_status":"cloud"},
      {"path":"outputs/size-changed.pt","size":9,"sha256":digest,"labels":["latest"],"sync_status":"registered"},
      {"path":"outputs/legacy.pt","size":10,"labels":["latest"]},
      {"path":"outputs/worker-only.pt","size":12,"sha256":newer,"labels":["latest"],"sync_status":"needs_attention","sync_error":"Changed checkpoint"}
    ]),
    false,
  );
  let page = store
    .browse_run_files(&query(Some("outputs/"), 100))
    .unwrap();
  assert_eq!(
    page.inventory_recorded_at.as_deref(),
    Some("2026-10-12T01:02:03Z")
  );
  assert!(!page.reported_inventory_truncated);
  assert!(page.warnings.is_empty());
  let matching = file(&page, "outputs/matching.pt");
  assert_eq!(matching.availability, RunFileAvailability::Received);
  assert!(!matching.labels_pending);
  // Published worker state stays reported state even when its version is confirmed.
  assert_eq!(
    matching.reported.as_ref().unwrap().sync_status.as_deref(),
    Some("uploading")
  );
  assert_eq!(
    matching.reported.as_ref().unwrap().labels,
    ["best", "latest"]
  );
  for path in ["outputs/stale.pt", "outputs/size-changed.pt"] {
    let stale = file(&page, path);
    assert_eq!(
      stale.availability,
      RunFileAvailability::ReceivedDifferentVersion
    );
    assert!(stale.labels_pending);
  }
  let legacy = file(&page, "outputs/legacy.pt");
  assert_eq!(legacy.availability, RunFileAvailability::Received);
  assert!(legacy.labels_pending);
  let worker = file(&page, "outputs/worker-only.pt");
  assert_eq!(worker.availability, RunFileAvailability::ReportedOnly);
  assert!(worker.received.is_none());
  assert!(worker.labels_pending);
  let received = file(&page, "outputs/received-only.pt");
  assert_eq!(received.availability, RunFileAvailability::Received);
  assert!(received.reported.is_none());
  assert!(!received.labels_pending);
  for row in &page.files {
    validate_file_entry(row).unwrap();
  }
  let encoded = serde_json::to_value(&page).unwrap();
  assert!(encoded["files"].as_array().unwrap().iter().all(|row| {
    row.get("download_url").is_none()
      && row.get("provenance").is_none()
      && row["received"].get("provenance").is_none()
  }));
  assert_eq!(backend.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn actual_catalog_includes_tracking_streams_and_object_shadowing() {
  let directory = tempfile::tempdir().unwrap();
  let backend = MetadataOnlyStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  document(&store, "run-state.json", 7, &json!({"status":"running"}));
  store
    .execute(Request::AppendTracking {
      scope: scope(),
      path: "logs/stdout.log".into(),
      offset: 0,
      data_base64: base64::engine::general_purpose::STANDARD.encode(b"recorded\n"),
    })
    .unwrap();
  legacy_stream(&store, "logs/stderr.log", 22);
  legacy_stream(&store, "outputs/metrics.jsonl", 999);
  object(&store, &scope(), "outputs/metrics.jsonl", 10, None);
  object(&store, &scope(), "result.zip", 123, Some(&"f".repeat(64)));
  let page = store.browse_run_files(&query(None, 100)).unwrap();
  assert_eq!(page.files.len(), 5);
  assert!(matches!(
    file(&page, "run-state.json")
      .received
      .as_ref()
      .unwrap()
      .storage,
    FileStorage::Tracking {
      revision: 7,
      sealed: true
    }
  ));
  assert!(matches!(
    file(&page, "logs/stdout.log")
      .received
      .as_ref()
      .unwrap()
      .storage,
    FileStorage::Tracking {
      revision: 1,
      sealed: false
    }
  ));
  assert!(matches!(
    file(&page, "logs/stderr.log")
      .received
      .as_ref()
      .unwrap()
      .storage,
    FileStorage::Stream
  ));
  let shadowed = file(&page, "outputs/metrics.jsonl")
    .received
    .as_ref()
    .unwrap();
  assert_eq!(shadowed.size, 10);
  assert!(matches!(shadowed.storage, FileStorage::Object));
  assert_eq!(backend.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn literal_prefix_and_raw_path_keyset_merge_reported_and_received_names() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MetadataOnlyStorage::default()).unwrap();
  for path in [
    "outputs/a_%/z.pt",
    "outputs/a_%/a\".pt",
    "outputs/a_%/a .pt",
    "outputs/axxx/z.pt",
    "outputs/β/z.pt",
  ] {
    object(&store, &scope(), path, 1, None);
  }
  inventory(
    &store,
    1,
    json!([
      {"path":"outputs/a_%/a#.pt","size":2},
      {"path":"outputs/a_%/β.pt","size":3},
      {"path":"outputs/axxx/reported.pt","size":4}
    ]),
    false,
  );
  let mut selected = query(Some("outputs/a_%/"), 2);
  let mut paths = Vec::new();
  loop {
    let page = store.browse_run_files(&selected).unwrap();
    assert!(page.files.len() <= 2);
    paths.extend(page.files.iter().map(|file| file.path.clone()));
    let Some(cursor) = page.next_cursor else {
      break;
    };
    assert_eq!(page.files.last().unwrap().path, cursor);
    selected.cursor = Some(cursor);
  }
  assert_eq!(
    paths,
    [
      "outputs/a_%/a .pt",
      "outputs/a_%/a\".pt",
      "outputs/a_%/a#.pt",
      "outputs/a_%/z.pt",
      "outputs/a_%/β.pt"
    ]
  );
  assert_eq!(
    store
      .browse_run_files(&query(Some("outputs/β/"), 100))
      .unwrap()
      .files
      .len(),
    1
  );
  selected.cursor = Some("outputs/a_%/zzzz.pt".into());
  let after = store.browse_run_files(&selected).unwrap();
  assert_eq!(after.files.len(), 1);
  assert_eq!(after.files[0].path, "outputs/a_%/β.pt");
}

#[test]
fn keyset_pages_exceed_the_legacy_thousand_file_limit_and_isolate_scopes() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MetadataOnlyStorage::default()).unwrap();
  {
    let mut db = store.db().unwrap();
    let tx = db.transaction().unwrap();
    for index in 0..1070 {
      let record = FileRecord {
        target: FileTarget::Run {
          scope: scope(),
          path: format!("outputs/models/{:04}.pt", index * 2),
        },
        size: index,
        sha256: None,
        storage: FileStorage::Object,
        provenance: None,
      };
      tx.execute(
        "INSERT INTO files(target,record,object_key,sequence) VALUES(?1,?2,'unused',1)",
        params![
          target_json(&record.target).unwrap(),
          serde_json::to_string(&record).unwrap()
        ],
      )
      .unwrap();
    }
    tx.commit().unwrap();
  }
  let reported: Vec<_> = (0..200)
    .map(|index| json!({"path":format!("outputs/models/{:04}.pt", index * 2 + 1),"size":index}))
    .collect();
  inventory(&store, 1, json!(reported), false);
  for other in [
    RunScope {
      project_id: "other".into(),
      ..scope()
    },
    RunScope {
      origin: "other".into(),
      ..scope()
    },
    RunScope {
      run_id: "other".into(),
      ..scope()
    },
  ] {
    object(&store, &other, "outputs/models/hidden.pt", 1, None);
  }
  assert_eq!(store.list_files(&scope()).unwrap_err().status, 413);
  let mut selected = query(Some("outputs/models/"), 100);
  let mut paths = Vec::new();
  let mut pages = 0;
  loop {
    let page = store.browse_run_files(&selected).unwrap();
    assert!(page.files.len() <= 100);
    assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_CATALOG_PAGE_BYTES);
    paths.extend(page.files.iter().map(|file| file.path.clone()));
    pages += 1;
    let Some(cursor) = page.next_cursor else {
      break;
    };
    selected.cursor = Some(cursor);
  }
  assert_eq!(pages, 13);
  assert_eq!(paths.len(), 1270);
  assert!(paths.windows(2).all(|paths| paths[0] < paths[1]));
  assert!(paths.iter().all(|path| path != "outputs/models/hidden.pt"));
}

#[test]
fn inventory_truncation_and_invalid_metadata_are_explicit_and_receipts_remain_readable() {
  let directory = tempfile::tempdir().unwrap();
  let backend = MetadataOnlyStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  object(&store, &scope(), "outputs/model.pt", 10, None);
  inventory(
    &store,
    1,
    json!([{"path":"outputs/worker.pt","size":20}]),
    true,
  );
  let page = store
    .browse_run_files(&query(Some("outputs/"), 100))
    .unwrap();
  assert!(page.reported_inventory_truncated);
  assert_eq!(page.warnings, [INVENTORY_TRUNCATED]);
  assert_eq!(
    file(&page, "outputs/worker.pt").availability,
    RunFileAvailability::ReportedOnly
  );
  for (revision, value) in [
    (2, json!({"schema_version":99,"files":[],"truncated":false})),
    (
      3,
      json!({"schema_version":1,"files":[{"path":"outputs/x","size":1},{"path":"outputs/x","size":2}],"truncated":false}),
    ),
    (
      4,
      json!({"schema_version":1,"files":[],"truncated":false,"padding":"x".repeat(crate::run_artifacts::INVENTORY_LIMIT)}),
    ),
  ] {
    document(
      &store,
      crate::run_artifacts::INVENTORY_PATH,
      revision,
      &value,
    );
    let page = store
      .browse_run_files(&query(Some("outputs/"), 100))
      .unwrap();
    assert_eq!(page.warnings, [INVENTORY_UNAVAILABLE]);
    assert!(page.inventory_recorded_at.is_none());
    assert!(!page.reported_inventory_truncated);
    assert!(page.files.iter().all(|file| file.reported.is_none()));
    assert_eq!(
      file(&page, "outputs/model.pt")
        .received
        .as_ref()
        .unwrap()
        .size,
      10
    );
  }
  assert_eq!(backend.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn manifest_storage_outage_warns_without_hiding_received_objects_or_leaking_provenance() {
  let directory = tempfile::tempdir().unwrap();
  let backend = MetadataOnlyStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  object(
    &store,
    &scope(),
    crate::run_artifacts::INVENTORY_PATH,
    20,
    None,
  );
  object(&store, &scope(), "outputs/model.pt", 100, None);
  // A catalog does not parse or return provenance. Even an oversized legacy
  // provenance payload cannot inflate a received metadata row or page.
  let target = target_json(&FileTarget::Run {
    scope: scope(),
    path: "outputs/model.pt".into(),
  })
  .unwrap();
  store
    .db()
    .unwrap()
    .execute(
      "UPDATE files SET record=json_set(record,'$.provenance',?2) WHERE target=?1",
      params![target, "private lineage metadata ".repeat(100_000)],
    )
    .unwrap();
  let page = store
    .browse_run_files(&query(Some("outputs/"), 100))
    .unwrap();
  assert_eq!(page.warnings, [INVENTORY_UNAVAILABLE]);
  assert_eq!(page.files.len(), 2);
  assert!(serde_json::to_vec(&page).unwrap().len() < 1024);
  assert_eq!(backend.reads.load(Ordering::Relaxed), 1);
}

#[test]
fn missing_expired_and_deleted_runs_and_invalid_queries_have_specific_errors() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MetadataOnlyStorage::default()).unwrap();
  assert_eq!(
    store
      .browse_run_files(&query(None, 100))
      .unwrap_err()
      .status,
    404
  );
  object(&store, &scope(), "outputs/model.pt", 100, None);
  for invalid in [
    RunFilesQuery {
      limit: 0,
      ..query(None, 100)
    },
    RunFilesQuery {
      limit: 101,
      ..query(None, 100)
    },
    query(Some("outputs/../private"), 100),
    RunFilesQuery {
      cursor: Some("../private".into()),
      ..query(None, 100)
    },
  ] {
    assert_eq!(store.browse_run_files(&invalid).unwrap_err().status, 400);
  }
  assert!(
    store
      .browse_run_files(&query(Some("outputs/missing"), 100))
      .unwrap()
      .files
      .is_empty()
  );
  let at = run_retention::now();
  store.db().unwrap().execute(
    "INSERT INTO run_retention(project_id,origin,run_id,archived_at,delete_after) VALUES(?1,?2,?3,?4,?5)",
    params![scope().project_id, scope().origin, scope().run_id, at, at + 100],
  ).unwrap();
  assert_eq!(
    store
      .browse_run_files(&query(None, 100))
      .unwrap()
      .files
      .len(),
    1
  );
  store
    .db()
    .unwrap()
    .execute("UPDATE run_retention SET delete_after=0", [])
    .unwrap();
  assert_eq!(
    store
      .browse_run_files(&query(None, 100))
      .unwrap_err()
      .status,
    410
  );
  store
    .db()
    .unwrap()
    .execute("DELETE FROM run_retention", [])
    .unwrap();
  store
    .db()
    .unwrap()
    .execute(
      "INSERT INTO project_deletions(project_id,revision,status) VALUES(?1,'fixture','deleted')",
      [scope().project_id],
    )
    .unwrap();
  assert_eq!(
    store
      .browse_run_files(&query(None, 100))
      .unwrap_err()
      .status,
    410
  );
}
