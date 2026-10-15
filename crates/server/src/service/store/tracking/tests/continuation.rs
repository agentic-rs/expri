use super::*;

fn at(epoch: u64, request: Request) -> Request {
  Request::AtRunEpoch {
    epoch,
    request: Box::new(request),
  }
}

fn snapshot(store: &Store<MockStorage>) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
  let Response::Files { files } = store.list_files(&scope()).unwrap() else {
    panic!("catalog");
  };
  let mut documents = BTreeMap::new();
  let mut streams = BTreeMap::new();
  for file in files {
    if let FileStorage::Tracking { revision, .. } = file.storage {
      let FileTarget::Run { path, .. } = file.target else {
        panic!("scope");
      };
      if document_path(&path) {
        documents.insert(path, revision);
      } else if stream_path(&path) {
        streams.insert(path, file.size);
      }
    }
  }
  (documents, streams)
}

fn begin(store: &Store<MockStorage>, id: &str, epoch: u64, attempt: u64) -> Request {
  let (documents, streams) = snapshot(store);
  Request::BeginRunAttempt {
    scope: scope(),
    request_id: id.into(),
    expected_epoch: epoch,
    attempt,
    documents,
    streams,
  }
}

fn state(store: &Store<MockStorage>, epoch: u64, revision: u64, value: Value) {
  let bytes = serde_json::to_vec(&value).unwrap();
  store
    .execute(at(
      epoch,
      Request::PutDocument {
        scope: scope(),
        path: "run-state.json".into(),
        revision,
        offset: 0,
        total_size: bytes.len() as u64,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
      },
    ))
    .unwrap();
}

fn append_at(store: &Store<MockStorage>, epoch: u64, path: &str, offset: u64, bytes: &[u8]) {
  store
    .execute(at(
      epoch,
      Request::AppendTracking {
        scope: scope(),
        path: path.into(),
        offset,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
      },
    ))
    .unwrap();
}

fn seal_at(store: &Store<MockStorage>, epoch: u64) -> Response {
  let (documents, streams) = snapshot(store);
  store
    .execute(at(
      epoch,
      Request::SealRun {
        scope: scope(),
        documents,
        streams,
        incomplete: false,
      },
    ))
    .unwrap()
}

#[test]
fn terminal_continuation_fences_all_old_mutations_and_keeps_prefixes_and_seals() {
  let directory = tempfile::tempdir().unwrap();
  let backend = MockStorage::default();
  let store = Store::open(directory.path(), backend.clone()).unwrap();
  state(&store, 0, 1, json!({"status":"completed"}));
  put(&store, "outputs/params.json", 1, b"{\"steps\":1}");
  for attempt in 1..=2 {
    for name in [
      "snapshot.json",
      "environment-state.json",
      "params.json",
      "attempt-state.json",
    ] {
      put(
        &store,
        &format!("outputs/attempts/{attempt}/{name}"),
        1,
        b"{}",
      );
    }
  }
  let old = b"{\"schema_version\":1,\"step\":7,\"metrics\":{\"loss\":2}}\n";
  append(&store, "outputs/metrics.jsonl", 0, old).unwrap();
  store
    .execute(Request::BeginUpload {
      upload_id: "old-upload".into(),
      target: run_target("outputs/old.pt"),
      size: 1,
      sha256: "a".repeat(64),
    })
    .unwrap();
  store
    .execute(Request::RecordPart {
      upload_id: "old-upload".into(),
      part: CompletedPart {
        part_number: 1,
        etag: "old-part".into(),
      },
    })
    .unwrap();
  backend.stage("old-upload", 1);
  seal_at(&store, 0);
  let original_archive: String = store
    .db()
    .unwrap()
    .query_row(
      "SELECT snapshot FROM result_archives WHERE id=1",
      [],
      |row| row.get(0),
    )
    .unwrap();
  assert_eq!(
    serde_json::from_str::<Value>(&original_archive).unwrap()["files"]
      .as_array()
      .unwrap()
      .len(),
    11
  );
  let request = begin(&store, "continue-three", 0, 3); // Attempt two failed before cutover.
  let mut wrong = request.clone();
  if let Request::BeginRunAttempt { streams, .. } = &mut wrong {
    streams.insert("outputs/metrics.jsonl".into(), 0);
  }
  assert_eq!(store.execute(wrong).unwrap_err().status, 409);
  assert!(matches!(
    store.execute(request.clone()).unwrap(),
    Response::RunAttempt { epoch: 1 }
  ));
  assert!(matches!(
    store.execute(request.clone()).unwrap(),
    Response::RunAttempt { epoch: 1 }
  ));
  for stale in [
    Request::AppendTracking {
      scope: scope(),
      path: "outputs/metrics.jsonl".into(),
      offset: old.len() as u64,
      data_base64: "Cg==".into(),
    },
    Request::AppendStream {
      scope: scope(),
      path: "logs/stdout.log".into(),
      offset: 0,
      data_base64: "Cg==".into(),
    },
    Request::PutDocument {
      scope: scope(),
      path: "run-state.json".into(),
      revision: 2,
      offset: 0,
      total_size: 2,
      data_base64: "e30=".into(),
    },
    Request::SealRun {
      scope: scope(),
      documents: BTreeMap::new(),
      streams: BTreeMap::new(),
      incomplete: true,
    },
    Request::BeginUpload {
      upload_id: "stale-new".into(),
      target: run_target("outputs/new.pt"),
      size: 1,
      sha256: "b".repeat(64),
    },
    Request::PartUrl {
      upload_id: "old-upload".into(),
      part_number: 1,
    },
    Request::RecordPart {
      upload_id: "old-upload".into(),
      part: CompletedPart {
        part_number: 1,
        etag: "old-part".into(),
      },
    },
    Request::CompleteUpload {
      upload_id: "old-upload".into(),
    },
    Request::ReferenceFile {
      source: run_target("outputs/missing.pt"),
      target: run_target("outputs/reference.pt"),
      size: 1,
      sha256: "a".repeat(64),
    },
    Request::ImportOutput {
      scope: scope(),
      path: "outputs/imported.pt".into(),
      input_id: None,
      size: 1,
      sha256: "a".repeat(64),
    },
  ] {
    assert_eq!(store.execute(stale).unwrap_err().status, 409);
  }
  assert_eq!(store.archive_status(&scope()).unwrap().status, "none");
  assert!(!store.archive_cycle().unwrap());
  state(
    &store,
    1,
    2,
    json!({"status":"running","publication_epoch":1}),
  );
  let new = b"{\"schema_version\":1,\"step\":8,\"metrics\":{\"loss\":1}}\n";
  append_at(&store, 1, "outputs/metrics.jsonl", old.len() as u64, new);
  assert_eq!(metrics(&store, true).metrics["loss"].points.len(), 2);
  assert!(matches!(
    store.execute(request).unwrap(),
    Response::RunAttempt { epoch: 1 }
  ));
  let params = Request::PutDocument {
    scope: scope(),
    path: "outputs/params.json".into(),
    revision: 2,
    offset: 0,
    total_size: 2,
    data_base64: "e30=".into(),
  };
  assert_eq!(store.execute(at(1, params)).unwrap_err().status, 409);
  let mut next_params = Request::PutDocument {
    scope: scope(),
    path: "outputs/attempts/3/params.json".into(),
    revision: 1,
    offset: 0,
    total_size: 2,
    data_base64: "e30=".into(),
  };
  store.execute(at(1, next_params.clone())).unwrap();
  if let Request::PutDocument { revision, .. } = &mut next_params {
    *revision = 2;
  }
  assert_eq!(store.execute(at(1, next_params)).unwrap_err().status, 409);
  state(
    &store,
    1,
    3,
    json!({"status":"completed","publication_epoch":1}),
  );
  seal_at(&store, 1);
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row("SELECT COUNT(*) FROM result_archives", [], |row| row
        .get::<_, u64>(0))
      .unwrap(),
    2
  );
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row("SELECT COUNT(*) FROM tracking_attempt_history", [], |row| {
        row.get::<_, u64>(0)
      })
      .unwrap(),
    1
  );
  assert_eq!(
    store
      .db()
      .unwrap()
      .query_row(
        "SELECT snapshot FROM result_archives WHERE id=1",
        [],
        |row| row.get::<_, String>(0)
      )
      .unwrap(),
    original_archive
  );
  drop(store);
  let store = Store::open(directory.path(), backend).unwrap();
  assert_eq!(
    append(&store, "outputs/metrics.jsonl", 0, old)
      .unwrap_err()
      .status,
    409
  );
  let Response::Stream { data_base64, .. } = store
    .execute(Request::ReadStream {
      scope: scope(),
      path: "outputs/metrics.jsonl".into(),
      offset: 0,
      limit: STREAM_BATCH,
    })
    .unwrap()
  else {
    panic!("raw stream");
  };
  assert_eq!(
    base64::engine::general_purpose::STANDARD
      .decode(data_base64)
      .unwrap(),
    [old.as_slice(), new.as_slice()].concat()
  );
}

#[test]
fn archived_run_requires_restore_before_preflight_or_epoch_change() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  state(&store, 0, 1, json!({"status":"completed"}));
  store.archive_run(&scope()).unwrap();
  assert_eq!(
    store
      .execute(Request::CheckRunContinuation { scope: scope() })
      .unwrap_err()
      .status,
    409
  );
  assert_eq!(
    store
      .execute(begin(&store, "archived", 0, 2))
      .unwrap_err()
      .status,
    409
  );
  store.restore_run(&scope()).unwrap();
  assert!(matches!(
    store
      .execute(Request::CheckRunContinuation { scope: scope() })
      .unwrap(),
    Response::RunAttempt { epoch: 0 }
  ));
  assert!(matches!(
    store.execute(begin(&store, "restored", 0, 2)).unwrap(),
    Response::RunAttempt { epoch: 1 }
  ));
}

#[test]
fn reconciliation_fences_a_delayed_abandoned_cas_and_resolves_committed_acks() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  state(&store, 0, 1, json!({"status":"failed"}));
  let delayed = begin(&store, "uncertain", 0, 2);
  let reconcile = |id: &str| Request::ReconcileRunAttempt {
    scope: scope(),
    request_id: id.into(),
    expected_epoch: 0,
  };
  for _ in 0..2 {
    assert!(matches!(
      store.execute(reconcile("uncertain")).unwrap(),
      Response::ReconciledRunAttempt {
        committed_epoch: None
      }
    ));
  }
  assert_eq!(store.execute(delayed).unwrap_err().status, 409);
  assert!(matches!(
    store.execute(begin(&store, "fresh", 0, 3)).unwrap(),
    Response::RunAttempt { epoch: 1 }
  ));
  for _ in 0..2 {
    assert!(matches!(
      store.execute(reconcile("fresh")).unwrap(),
      Response::ReconciledRunAttempt {
        committed_epoch: Some(1)
      }
    ));
    assert!(matches!(
      store.execute(reconcile("uncertain")).unwrap(),
      Response::ReconciledRunAttempt {
        committed_epoch: None
      }
    ));
  }
  drop(store);
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  assert!(matches!(
    store.execute(reconcile("uncertain")).unwrap(),
    Response::ReconciledRunAttempt {
      committed_epoch: None
    }
  ));
  assert_eq!(
    store
      .execute(begin(&store, "uncertain", 0, 2))
      .unwrap_err()
      .status,
    409
  );
}

fn capture(id: &str, start: i64, end: i64) -> Vec<u8> {
  format!("{{\"kind\":\"capture\",\"schema_version\":1,\"capture_id\":\"{id}\",\"machine_id\":\"machine\",\"start_ms\":{start}}}\n{{\"kind\":\"end\",\"end_ms\":{end},\"complete\":true,\"warnings\":[]}}\n").into_bytes()
}

#[test]
fn current_capture_completion_can_supersede_only_its_stale_attempt_snapshot() {
  for finished_at in [Value::Null, json!("1970-01-01T00:00:01Z")] {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let mut value = json!({
      "status":"completed", "system_capture_id":"capture-1", "machine_id":"machine",
      "system_capture_status":"complete", "started_at":"1970-01-01T00:00:00Z",
      "finished_at":"1970-01-01T00:00:01Z",
      "system_capture_started_at":"1970-01-01T00:00:00Z",
      "system_capture_finished_at":"1970-01-01T00:00:01Z"
    });
    let mut attempt = value.clone();
    attempt["system_capture_status"] = json!("capturing");
    attempt["finished_at"] = finished_at.clone();
    attempt["system_capture_finished_at"] = finished_at;
    value["task_attempts"] = json!({"attempts":[attempt]});
    append(
      &store,
      expri_core::run_system::RUN_SYSTEM_PATH,
      0,
      &capture("capture-1", 0, 1000),
    )
    .unwrap();
    let mut revision = 0;
    for (field, invalid) in [
      ("machine_id", json!("another-machine")),
      (
        "system_capture_started_at",
        json!("1970-01-01T00:00:00.001Z"),
      ),
      ("system_capture_started_at", json!("invalid")),
      ("system_capture_finished_at", json!("1970-01-01T00:00:02Z")),
      ("system_capture_finished_at", json!("invalid")),
      ("system_capture_status", json!("incomplete")),
      ("system_capture_id", json!("historical-capture")),
    ] {
      let mut bad = value.clone();
      bad["task_attempts"]["attempts"][0][field] = invalid;
      revision += 1;
      state(&store, 0, revision, bad);
      let (documents, streams) = snapshot(&store);
      assert_eq!(
        store
          .execute(Request::SealRun {
            scope: scope(),
            documents,
            streams,
            incomplete: false,
          })
          .unwrap_err()
          .status,
        409,
        "contradictory {field} must not be superseded"
      );
    }
    state(&store, 0, revision + 1, value);
    seal_at(&store, 0);
  }
}

#[test]
fn consecutive_system_segments_keep_the_gap_and_require_every_attempt_identity() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path(), MockStorage::default()).unwrap();
  let first = json!({"system_capture_id":"capture-1","machine_id":"machine","system_capture_status":"complete","started_at":"1970-01-01T00:00:00Z","system_capture_started_at":"1970-01-01T00:00:00Z","system_capture_finished_at":"1970-01-01T00:00:01Z"});
  let mut value = first.clone();
  value["status"] = json!("preparing");
  value["finished_at"] = Value::Null;
  value["task_attempts"] = json!({"attempts":[first.clone()]});
  state(&store, 0, 1, value.clone());
  let old = capture("capture-1", 0, 1000);
  append(&store, expri_core::run_system::RUN_SYSTEM_PATH, 0, &old).unwrap();
  store.execute(begin(&store, "second", 0, 2)).unwrap();
  let second = capture("capture-2", 2000, 3000);
  append_at(
    &store,
    1,
    expri_core::run_system::RUN_SYSTEM_PATH,
    old.len() as u64,
    &second,
  );
  value["status"] = json!("completed");
  value["publication_epoch"] = json!(1);
  value["finished_at"] = json!("1970-01-01T00:00:03Z");
  value["system_capture_id"] = json!("capture-2");
  value["system_capture_started_at"] = json!("1970-01-01T00:00:02Z");
  value["system_capture_finished_at"] = json!("1970-01-01T00:00:03Z");
  let current = value.clone();
  value["task_attempts"]["attempts"]
    .as_array_mut()
    .unwrap()
    .push(current);
  let mut bad = value.clone();
  bad["task_attempts"]["attempts"][0]["machine_id"] = json!("wrong-machine");
  state(&store, 1, 2, bad);
  let (documents, streams) = snapshot(&store);
  assert_eq!(
    store
      .execute(at(
        1,
        Request::SealRun {
          scope: scope(),
          documents,
          streams,
          incomplete: false
        }
      ))
      .unwrap_err()
      .status,
    409
  );
  state(&store, 1, 3, value);
  seal_at(&store, 1);
}
