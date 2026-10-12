use super::*;

use super::super::download::mock;

#[test]
fn system_stream_capability_is_negotiated_and_saved_separately() {
  for supported in [false, true] {
    let (url, task) = mock::mock(1, move |request, _| {
      assert!(matches!(
        serde_json::from_slice::<Request>(&request.body).unwrap(),
        Request::Capabilities
      ));
      let mut features = vec!["tracking-v1".into()];
      if supported {
        features.push(expri_core::run_system::RUN_SYSTEM_CAPABILITY.into());
      }
      (
        200,
        Vec::new(),
        serde_json::to_vec(&Response::Capabilities { features }).unwrap(),
      )
    });
    let (root, api, scope) = fixture(&url);
    let directory = root.path().join("queue");
    let owner = json!({"endpoint": api.endpoint, "scope": scope});
    let mut queue = Queue::new(directory.clone(), owner.clone()).unwrap();
    assert_eq!(negotiate(&api, &mut queue).unwrap(), Protocol::TrackingV1);
    assert_eq!(system_supported(&api, &mut queue).unwrap(), supported);
    drop(queue);
    let mut queue = Queue::new(directory, owner).unwrap();
    assert_eq!(system_supported(&api, &mut queue).unwrap(), supported);
    task.join().unwrap();
  }
}

#[test]
fn capture_upload_excludes_uncheckpointed_tail_across_recovery() {
  let committed = b"{\"kind\":\"sample\"}\n";
  let expected = committed.to_vec();
  let (url, task) = mock::mock(1, move |request, _| {
    let Request::AppendTracking {
      path,
      offset,
      data_base64,
      ..
    } = serde_json::from_slice(&request.body).unwrap()
    else {
      panic!("tracking capture expected")
    };
    assert_eq!(path, RUN_SYSTEM_PATH);
    assert_eq!(offset, 0);
    assert_eq!(STANDARD.decode(data_base64).unwrap(), expected);
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::Acknowledged {
        offset: committed.len() as u64,
      })
      .unwrap(),
    )
  });
  let (root, api, scope) = fixture(&url);
  let source = root.path().join("capture.jsonl");
  std::fs::write(
    &source,
    [committed.as_slice(), b"uncommitted complete line\n"].concat(),
  )
  .unwrap();
  let directory = root.path().join("queue");
  let owner = json!({"endpoint": api.endpoint, "scope": scope});
  let mut queue = Queue::new(directory.clone(), owner.clone()).unwrap();
  queue.state.protocol = Some(Protocol::TrackingV1);
  upload::sync_stream_up_to(
    &api,
    &mut queue,
    &scope,
    RUN_SYSTEM_PATH,
    &source,
    false,
    Some(committed.len() as u64),
  )
  .unwrap();
  drop(queue);
  // Recovery discards the tail that was never durably checkpointed.
  std::fs::write(&source, committed).unwrap();
  let mut queue = Queue::new(directory, owner).unwrap();
  upload::sync_stream_up_to(
    &api,
    &mut queue,
    &scope,
    RUN_SYSTEM_PATH,
    &source,
    false,
    Some(committed.len() as u64),
  )
  .unwrap();
  assert_eq!(queue.state.streams[RUN_SYSTEM_PATH], committed.len() as u64);
  assert!(
    upload::sync_stream_up_to(
      &api,
      &mut queue,
      &scope,
      RUN_SYSTEM_PATH,
      &source,
      false,
      Some(committed.len() as u64 + 1)
    )
    .is_err()
  );
  task.join().unwrap();
}

#[test]
fn a_terminal_task_with_a_draining_capture_publishes_state_without_sealing() {
  let (url, task) = mock::mock(4, move |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec!["tracking-v1".into()],
      },
      Request::PutDocument {
        revision,
        offset,
        total_size,
        ..
      } => {
        assert_eq!(offset, 0);
        Response::DocumentAcknowledged {
          revision,
          offset: total_size,
          complete: true,
        }
      }
      other => panic!("draining capture must not seal or use an unsupported stream: {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let (root, api, scope) = fixture(&url);
  let run = root.path().join("run");
  fs::directories(&run.join("outputs")).unwrap();
  std::fs::write(run.join("snapshot.json"), b"{}").unwrap();
  std::fs::write(run.join("run-state.json"), serde_json::to_vec(&json!({
    "run_id": "run", "status": "completed", "started_at": "1970-01-01T00:00:00Z",
    "machine_id": "machine-1", "system_capture_id": "capture-1", "system_capture_status": "capturing"
  })).unwrap()).unwrap();
  std::fs::write(run.join(RUN_SYSTEM_PATH), b"{\"kind\":\"capture\",\"schema_version\":1,\"capture_id\":\"capture-1\",\"machine_id\":\"machine-1\",\"start_ms\":0}\n").unwrap();
  let mut queue = Queue::new(
    root.path().join("queue"),
    json!({"endpoint": api.endpoint, "scope": scope}),
  )
  .unwrap();
  assert!(
    !publish_cycle(
      &api,
      &mut queue,
      &scope,
      &run,
      &BTreeSet::new(),
      true,
      &mut |_| Ok(())
    )
    .unwrap()
  );
  assert_eq!(system_upload_report(&queue)["status"], "unsupported");
  assert!(queue.state.archive.is_none());
  task.join().unwrap();
}

#[test]
fn capture_identity_failure_keeps_logs_and_terminal_state_publication_alive() {
  let (url, task) = mock::mock(5, move |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec![
          "tracking-v1".into(),
          expri_core::run_system::RUN_SYSTEM_CAPABILITY.into(),
        ],
      },
      Request::PutDocument {
        revision,
        total_size,
        ..
      } => Response::DocumentAcknowledged {
        revision,
        offset: total_size,
        complete: true,
      },
      Request::AppendTracking {
        path,
        offset,
        data_base64,
        ..
      } => {
        assert_eq!(path, "logs/stdout.log");
        assert_eq!(offset, 0);
        assert_eq!(STANDARD.decode(data_base64).unwrap(), b"task finished\n");
        Response::Acknowledged { offset: 14 }
      }
      other => panic!("invalid capture cannot upload or seal: {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let (root, api, scope) = fixture(&url);
  let run = root.path().join("run");
  fs::directories(&run.join("outputs")).unwrap();
  fs::directories(&run.join("logs")).unwrap();
  std::fs::write(run.join("snapshot.json"), b"{}").unwrap();
  std::fs::write(run.join("logs/stdout.log"), b"task finished\n").unwrap();
  std::fs::write(run.join("run-state.json"), serde_json::to_vec(&json!({
    "run_id": "run", "status": "completed", "started_at": "1970-01-01T00:00:00Z",
    "machine_id": "machine-1", "system_capture_id": "capture-1", "system_capture_status": "complete"
  })).unwrap()).unwrap();
  let unrelated = b"{\"kind\":\"capture\",\"schema_version\":1,\"capture_id\":\"capture-1\",\"machine_id\":\"machine-other\",\"start_ms\":0}\n";
  std::fs::write(run.join(RUN_SYSTEM_PATH), unrelated).unwrap();
  let mut queue = Queue::new(
    root.path().join("queue"),
    json!({"endpoint": api.endpoint, "scope": scope}),
  )
  .unwrap();
  assert!(
    publish_cycle(
      &api,
      &mut queue,
      &scope,
      &run,
      &BTreeSet::new(),
      true,
      &mut |_| Ok(())
    )
    .is_err()
  );
  assert!(queue.state.documents["run-state.json"].complete);
  assert_eq!(queue.state.streams["logs/stdout.log"], 14);
  assert!(!queue.state.streams.contains_key(RUN_SYSTEM_PATH));
  assert!(queue.state.archive.is_none());
  assert_eq!(std::fs::read(run.join(RUN_SYSTEM_PATH)).unwrap(), unrelated);
  task.join().unwrap();
}

#[test]
fn unrelated_output_named_like_a_capture_is_not_automatically_uploaded() {
  let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
  fs::directories(&root.path().join("outputs")).unwrap();
  std::fs::write(root.path().join(RUN_SYSTEM_PATH), b"user data").unwrap();
  assert!(!system_capture_owned(root.path(), &json!({"run_id": "run"})).unwrap());
}

#[test]
fn a_legacy_queue_reports_retained_local_even_after_the_service_upgrades() {
  let (root, api, scope) = fixture("http://127.0.0.1:1");
  let mut queue = Queue::new(
    root.path().join("queue"),
    json!({"endpoint": api.endpoint, "scope": scope}),
  )
  .unwrap();
  queue.state.protocol = Some(Protocol::Legacy);
  queue.state.run_system_supported = Some(true);
  queue.state.run_system_capture = true;
  let report = system_upload_report(&queue);
  assert_eq!(report["status"], "unsupported");
  assert_eq!(report["retained_local"], true);
  assert!(
    report["message"]
      .as_str()
      .unwrap()
      .contains("legacy publishing queue")
  );
}

#[cfg(unix)]
#[test]
fn supported_capture_is_published_at_its_checkpoint_and_included_in_final_seal() {
  use std::os::unix::fs::MetadataExt;
  let bytes = b"{\"kind\":\"capture\",\"schema_version\":1,\"capture_id\":\"capture-1\",\"machine_id\":\"machine-1\",\"start_ms\":0}\n{\"kind\":\"end\",\"end_ms\":1000,\"complete\":true,\"warnings\":[]}\n";
  let (url, task) = mock::mock(6, move |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec![
          "tracking-v1".into(),
          expri_core::run_system::RUN_SYSTEM_CAPABILITY.into(),
        ],
      },
      Request::PutDocument {
        revision,
        offset,
        total_size,
        ..
      } => {
        assert_eq!(offset, 0);
        Response::DocumentAcknowledged {
          revision,
          offset: total_size,
          complete: true,
        }
      }
      Request::AppendTracking {
        path,
        offset,
        data_base64,
        ..
      } => {
        assert_eq!(path, RUN_SYSTEM_PATH);
        assert_eq!(offset, 0);
        assert_eq!(STANDARD.decode(data_base64).unwrap(), bytes);
        Response::Acknowledged {
          offset: bytes.len() as u64,
        }
      }
      Request::SealRun {
        streams,
        incomplete,
        ..
      } => {
        assert_eq!(
          streams,
          BTreeMap::from([(RUN_SYSTEM_PATH.into(), bytes.len() as u64)])
        );
        assert!(!incomplete);
        Response::Archive {
          archive: ArchiveRecord {
            status: "pending".into(),
            incomplete,
            file: None,
            last_error: None,
          },
        }
      }
      other => panic!("unexpected capture publication: {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let (root, api, scope) = fixture(&url);
  let run = root.path().join("run");
  fs::directories(&run.join("outputs")).unwrap();
  std::fs::write(run.join("snapshot.json"), b"{}").unwrap();
  std::fs::write(run.join("run-state.json"), serde_json::to_vec(&json!({
    "schema_version": 1, "run_id": "run", "status": "completed", "started_at": "1970-01-01T00:00:00Z", "finished_at": "1970-01-01T00:00:01Z",
    "machine_id": "machine-1", "system_capture_id": "capture-1", "system_capture_status": "complete"
  })).unwrap()).unwrap();
  std::fs::write(run.join(RUN_SYSTEM_PATH), bytes).unwrap();
  let metadata = std::fs::metadata(run.join(RUN_SYSTEM_PATH)).unwrap();
  std::fs::write(run.join(".system-profile-checkpoint.json"), serde_json::to_vec(&json!({
    "capture_id": "capture-1", "file_identity": format!("{}:{}", metadata.dev(), metadata.ino()), "committed_bytes": bytes.len(), "closed": true
  })).unwrap()).unwrap();
  let mut queue = Queue::new(
    root.path().join("queue"),
    json!({"endpoint": api.endpoint, "scope": scope}),
  )
  .unwrap();
  assert!(
    publish_cycle(
      &api,
      &mut queue,
      &scope,
      &run,
      &BTreeSet::new(),
      true,
      &mut |_| Ok(())
    )
    .unwrap()
  );
  assert_eq!(system_upload_report(&queue)["status"], "published");
  assert_eq!(queue.state.streams[RUN_SYSTEM_PATH], bytes.len() as u64);
  task.join().unwrap();
}

fn fixture(url: &str) -> (tempfile::TempDir, Api, RunScope) {
  let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
  let config = root.path().join("client.toml");
  std::fs::write(&config, format!("url={url:?}\ntoken_env='PATH'\n")).unwrap();
  let scope = RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: "run".into(),
  };
  (root, Api::new(&config).unwrap(), scope)
}

#[test]
fn failed_capability_probe_does_not_pin_legacy_but_explicit_unsupported_does() {
  for status in [503, 401, 400] {
    let (url, task) = mock::mock(1, move |request, _| {
      assert_eq!(request.path, "/v1/request");
      assert!(request.headers.contains_key("authorization"));
      assert!(matches!(
        serde_json::from_slice::<Request>(&request.body).unwrap(),
        Request::Capabilities
      ));
      (status, Vec::new(), br#"{"error":"not available"}"#.to_vec())
    });
    let (root, api, scope) = fixture(&url);
    let directory = root.path().join("queue");
    let owner = json!({"endpoint":api.endpoint,"scope":scope});
    let mut queue = Queue::new(directory.clone(), owner.clone()).unwrap();
    let result = negotiate(&api, &mut queue);
    assert_eq!(result.is_ok(), status == 400);
    drop(queue);
    let queue = Queue::new(directory, owner).unwrap();
    assert_eq!(
      queue.state.protocol,
      if status == 400 {
        Some(Protocol::Legacy)
      } else {
        None
      }
    );
    task.join().unwrap();
  }
}

#[test]
fn existing_stream_queue_stays_legacy_without_a_capability_request() {
  let (root, api, scope) = fixture("http://127.0.0.1:1");
  let directory = root.path().join("queue");
  let owner = json!({"endpoint":api.endpoint,"scope":scope});
  let mut queue = Queue::new(directory.clone(), owner.clone()).unwrap();
  queue.state.streams.insert("logs/stdout.log".into(), 123);
  queue.save().unwrap();
  drop(queue);
  let mut queue = Queue::new(directory, owner).unwrap();
  assert_eq!(negotiate(&api, &mut queue).unwrap(), Protocol::Legacy);
}

#[test]
fn lost_document_ack_reopens_original_snapshot_before_sending_new_revision() {
  let original = vec![b'a'; STREAM_BATCH + 17];
  let expected = original.clone();
  let mut calls = 0;
  let (url, task) = mock::mock(4, move |request, _| {
    let Request::PutDocument {
      revision,
      offset,
      total_size,
      data_base64,
      ..
    } = serde_json::from_slice(&request.body).unwrap()
    else {
      panic!("tracking document expected")
    };
    let bytes = STANDARD.decode(data_base64).unwrap();
    calls += 1;
    if calls <= 3 {
      assert_eq!(revision, 1);
      assert_eq!(total_size as usize, expected.len());
      assert_eq!(
        bytes,
        expected[offset as usize..offset as usize + bytes.len()]
      );
    } else {
      assert_eq!(revision, 2);
      assert_eq!(offset, 0);
      assert_eq!(bytes, b"new document");
    }
    if calls == 1 {
      return (
        503,
        Vec::new(),
        br#"{"error":"lost successful ACK"}"#.to_vec(),
      );
    }
    let acknowledged = offset + bytes.len() as u64;
    (
      200,
      Vec::new(),
      serde_json::to_vec(&Response::DocumentAcknowledged {
        offset: acknowledged,
        revision,
        complete: acknowledged == total_size,
      })
      .unwrap(),
    )
  });
  let (root, api, scope) = fixture(&url);
  let source = root.path().join("snapshot.json");
  std::fs::write(&source, original).unwrap();
  let directory = root.path().join("queue");
  let owner = json!({"endpoint":api.endpoint,"scope":scope});
  let mut queue = Queue::new(directory.clone(), owner.clone()).unwrap();
  queue.state.protocol = Some(Protocol::TrackingV1);
  assert!(sync_document(&api, &mut queue, &scope, "snapshot.json", &source).is_err());
  assert_eq!(queue.state.documents["snapshot.json"].offset, 0);
  drop(queue);
  std::fs::write(&source, b"new document").unwrap();
  let mut queue = Queue::new(directory, owner).unwrap();
  sync_document(&api, &mut queue, &scope, "snapshot.json", &source).unwrap();
  let saved = &queue.state.documents["snapshot.json"];
  assert_eq!(saved.revision, 2);
  assert!(saved.complete);
  assert!(saved.snapshot.is_none());
  task.join().unwrap();
}

#[test]
fn terminal_tracking_seals_original_bytes_without_uploading_them_to_s3() {
  let metrics = b"{\"step\":1,\"timestamp\":\"123456789.123456789\",\"metrics\":{\"loss\":1}}";
  let mut documents = BTreeMap::new();
  let mut streams = BTreeMap::new();
  let (url, task) = mock::mock(7, move |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec!["tracking-v1".into()],
      },
      Request::PutDocument {
        path,
        revision,
        offset,
        total_size,
        data_base64,
        ..
      } => {
        assert_eq!(offset, 0);
        assert_eq!(
          STANDARD.decode(data_base64).unwrap().len() as u64,
          total_size
        );
        documents.insert(path, revision);
        Response::DocumentAcknowledged {
          offset: total_size,
          revision,
          complete: true,
        }
      }
      Request::AppendTracking {
        path,
        offset,
        data_base64,
        ..
      } => {
        assert_eq!(offset, 0);
        let bytes = STANDARD.decode(data_base64).unwrap();
        assert_eq!(
          bytes,
          if path == "outputs/metrics.jsonl" {
            &metrics[..]
          } else {
            b"done\n"
          }
        );
        let size = bytes.len() as u64;
        streams.insert(path, size);
        Response::Acknowledged { offset: size }
      }
      Request::SealRun {
        documents: sealed_docs,
        streams: sealed_streams,
        incomplete,
        ..
      } => {
        assert_eq!(sealed_docs, documents);
        assert_eq!(sealed_streams, streams);
        assert!(sealed_docs.contains_key("run-state.json"));
        assert!(sealed_docs.contains_key(crate::run_artifacts::INVENTORY_PATH));
        assert!(!incomplete);
        Response::Archive {
          archive: ArchiveRecord {
            status: "pending".into(),
            incomplete,
            file: None,
            last_error: None,
          },
        }
      }
      other => panic!("tracking bytes unexpectedly used object transport: {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let (root, api, scope) = fixture(&url);
  let run = root.path().join("run");
  fs::directories(&run.join("outputs")).unwrap();
  fs::directories(&run.join("logs")).unwrap();
  std::fs::write(run.join("snapshot.json"), b"{}").unwrap();
  std::fs::write(
    run.join("run-state.json"),
    br#"{"run_id":"run","status":"completed"}"#,
  )
  .unwrap();
  std::fs::write(run.join("outputs/metrics.jsonl"), metrics).unwrap();
  std::fs::write(run.join("logs/stdout.log"), b"done\n").unwrap();
  let mut queue = Queue::new(
    root.path().join("queue"),
    json!({"endpoint":api.endpoint,"scope":scope}),
  )
  .unwrap();
  assert!(
    publish_cycle(
      &api,
      &mut queue,
      &scope,
      &run,
      &BTreeSet::new(),
      true,
      &mut |_| Ok(())
    )
    .unwrap()
  );
  assert!(queue.state.files.is_empty());
  assert_eq!(queue.state.archive.as_ref().unwrap().status, "pending");
  task.join().unwrap();
}

#[test]
fn lost_tracking_append_ack_retries_the_prefix_and_new_tail_after_reopen() {
  let mut requests = 0;
  let (url, task) = mock::mock(2, move |request, _| {
    let Request::AppendTracking {
      offset,
      data_base64,
      ..
    } = serde_json::from_slice(&request.body).unwrap()
    else {
      panic!("append tracking expected")
    };
    assert_eq!(offset, 0);
    let bytes = STANDARD.decode(data_base64).unwrap();
    requests += 1;
    if requests == 1 {
      assert_eq!(bytes, b"first");
      (
        503,
        Vec::new(),
        br#"{"error":"lost successful ACK"}"#.to_vec(),
      )
    } else {
      assert_eq!(bytes, b"first second");
      (
        200,
        Vec::new(),
        serde_json::to_vec(&Response::Acknowledged {
          offset: bytes.len() as u64,
        })
        .unwrap(),
      )
    }
  });
  let (root, api, scope) = fixture(&url);
  let source = root.path().join("stdout.log");
  std::fs::write(&source, b"first").unwrap();
  let directory = root.path().join("queue");
  let owner = json!({"endpoint":api.endpoint,"scope":scope});
  let mut queue = Queue::new(directory.clone(), owner.clone()).unwrap();
  queue.state.protocol = Some(Protocol::TrackingV1);
  assert!(sync_stream(&api, &mut queue, &scope, "logs/stdout.log", &source, false).is_err());
  queue.save().unwrap();
  drop(queue);
  std::fs::write(&source, b"first second").unwrap();
  let mut queue = Queue::new(directory, owner).unwrap();
  sync_stream(&api, &mut queue, &scope, "logs/stdout.log", &source, false).unwrap();
  assert_eq!(queue.state.streams["logs/stdout.log"], 12);
  task.join().unwrap();
}

#[test]
fn explicit_upload_captures_exact_tracking_revisions_and_partial_intent() {
  for partial in [false, true] {
    let (url, task) = mock::mock(3, move |request, _| {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::Capabilities => Response::Capabilities {
          features: vec!["tracking-v1".into()],
        },
        Request::ListFiles { scope } => Response::Files {
          files: vec![
            FileRecord {
              target: run_target(&scope, "run-state.json"),
              size: 40,
              sha256: None,
              provenance: None,
              storage: FileStorage::Tracking {
                revision: 3,
                sealed: true,
              },
            },
            FileRecord {
              target: run_target(&scope, "logs/stdout.log"),
              size: 71,
              sha256: None,
              provenance: None,
              storage: FileStorage::Tracking {
                revision: 1,
                sealed: false,
              },
            },
            FileRecord {
              target: run_target(&scope, "outputs/checkpoint.pt"),
              size: 100,
              sha256: Some("a".repeat(64)),
              provenance: None,
              storage: FileStorage::Object,
            },
          ],
        },
        Request::SealRun {
          documents,
          streams,
          incomplete,
          ..
        } => {
          assert_eq!(documents, BTreeMap::from([("run-state.json".into(), 3)]));
          assert_eq!(streams, BTreeMap::from([("logs/stdout.log".into(), 71)]));
          assert_eq!(incomplete, partial);
          Response::Archive {
            archive: ArchiveRecord {
              status: "pending".into(),
              incomplete,
              file: None,
              last_error: None,
            },
          }
        }
        other => panic!("unexpected upload action: {other:?}"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    });
    let (root, _api, scope) = fixture(&url);
    let result = upload(&root.path().join("client.toml"), &scope, partial).unwrap();
    assert_eq!(result["status"], "pending");
    assert_eq!(result["incomplete"], partial);
    task.join().unwrap();
  }
}

#[test]
fn explicit_upload_rejects_another_run_before_sealing() {
  let (url, task) = mock::mock(2, |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec!["tracking-v1".into()],
      },
      Request::ListFiles { mut scope } => {
        scope.run_id = "different".into();
        Response::Files {
          files: vec![FileRecord {
            target: run_target(&scope, "run-state.json"),
            size: 40,
            sha256: None,
            provenance: None,
            storage: FileStorage::Tracking {
              revision: 1,
              sealed: true,
            },
          }],
        }
      }
      other => panic!("unsafe upload action: {other:?}"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let (root, _api, scope) = fixture(&url);
  assert!(upload(&root.path().join("client.toml"), &scope, false).is_err());
  task.join().unwrap();
}
