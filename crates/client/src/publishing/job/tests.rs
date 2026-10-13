use super::*;
use crate::client::tests::mock;
use crate::types::{
  ArchiveRecord, FileRecord, FileStorage, FileTarget, Request, Response, UploadState,
};
use base64::{Engine, engine::general_purpose::STANDARD};

fn fixture(url: &str) -> (tempfile::TempDir, PublishOptions) {
  let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
  let run = root.path().join("run-fixture");
  std::fs::create_dir_all(run.join("outputs")).unwrap();
  let config = root.path().join("client.toml");
  std::fs::write(
    &config,
    format!("url={url:?}\ntoken='bound-worker-token'\n"),
  )
  .unwrap();
  fs::atomic_json(
    &run.join("run-state.json"),
    &json!({"run_id":"run-fixture", "status":"completed"}),
  )
  .unwrap();
  std::fs::write(run.join("snapshot.json"), b"{}").unwrap();
  let options = PublishOptions {
    config,
    run_dir: run,
    project_id: "project".into(),
    origin: "worker".into(),
    artifacts: Vec::new(),
    watch: true,
    queue_dir: root.path().join("queue"),
  };
  (root, options)
}

#[test]
fn registration_pins_options_credentials_and_canonical_target_without_public_secrets() {
  let (_root, mut options) = fixture("http://127.0.0.1:1");
  options.artifacts = vec![
    "outputs/b.pt".into(),
    "outputs/a.pt".into(),
    "outputs/a.pt".into(),
  ];
  let job = prepare_manual(&options).unwrap();
  assert_eq!(job.artifacts, vec!["outputs/a.pt", "outputs/b.pt"]);
  let saved = serde_json::to_vec(&job).unwrap();
  let recovered: PreparedJob = serde_json::from_slice(&saved).unwrap();
  assert!(job == recovered);
  assert!(
    job
      .safe_report()
      .to_string()
      .find("bound-worker-token")
      .is_none()
  );
  assert_eq!(
    job.redact("bound-worker-token rejected"),
    "[redacted] rejected"
  );
  options.artifacts.clear();
  let changed = prepare_manual(&options).unwrap();
  assert_eq!(job.identity().unwrap(), changed.identity().unwrap());
  assert!(
    job != changed,
    "changed artifact intent must conflict at the same identity"
  );
  std::fs::write(
    &options.config,
    "url='http://127.0.0.1:1'\ntoken='another-worker-token'\n",
  )
  .unwrap();
  let credential = prepare_manual(&options).unwrap();
  assert_eq!(changed.identity().unwrap(), credential.identity().unwrap());
  assert!(
    changed != credential,
    "credential replacement must be explicit"
  );
  assert!(job.validate_storage(&job.run_dir.join("private")).is_err());
}

#[test]
fn automatic_preparation_accepts_http_and_https_dashboard_roots_with_scope_queries() {
  for dashboard_root in ["http://127.0.0.1:8787/", "https://dashboard.example.test/"] {
    let (root, options) = fixture("http://127.0.0.1:1");
    let repo = root.path().join("source");
    let run_dir = repo.join(".expri/runs/run-fixture");
    std::fs::create_dir_all(&run_dir).unwrap();
    fs::atomic_json(
      &run_dir.join("run-state.json"),
      &json!({"run_id":"run-fixture", "status":"running"}),
    )
    .unwrap();
    let intent = Intent {
      schema_version: 1,
      repo_root: repo,
      run_dir,
      config: crate::config::RunServiceConfig {
        client_config: options.config,
        project_id: "project".into(),
        origin: "worker".into(),
        dashboard_url: Some(dashboard_root.into()),
        publish: true,
      },
    };
    intent.config.validate().unwrap();
    let prepared = prepare_automatic(&intent).unwrap();
    let link = reqwest::Url::parse(prepared.dashboard_url.as_deref().unwrap()).unwrap();
    assert_eq!(
      link.origin(),
      reqwest::Url::parse(dashboard_root).unwrap().origin()
    );
    assert_eq!(
      link
        .query_pairs()
        .into_owned()
        .collect::<std::collections::BTreeMap<_, _>>(),
      std::collections::BTreeMap::from([
        ("project_id".into(), "project".into()),
        ("origin".into(), "worker".into()),
        ("run_id".into(), "run-fixture".into()),
      ])
    );
    for invalid in [
      "https://embedded-secret@dashboard.example.test/",
      "http:///",
      "ftp://dashboard.example.test/",
      "https://dashboard.example.test/path",
      "https://dashboard.example.test/#fragment",
    ] {
      let mut changed = prepared.clone();
      changed.dashboard_url = Some(invalid.into());
      let failure = changed.validate().unwrap_err().to_string();
      assert_eq!(failure, "invalid job dashboard URL");
    }
  }
}

#[test]
fn bound_job_recovers_and_seals_capture_after_config_removal() {
  let mut docs = std::collections::BTreeMap::new();
  let mut streams = std::collections::BTreeMap::new();
  let (url, server) = mock(6, move |request, _| {
    assert!(request.headers.contains("Bearer bound-worker-token"));
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec!["tracking-v1".into(), "run-system-metrics-v1".into()],
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
        docs.insert(path, revision);
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
        assert_eq!(path, "outputs/system-profile.jsonl");
        assert_eq!(offset, 0);
        let bytes = STANDARD.decode(data_base64).unwrap();
        assert!(bytes.ends_with(b"\"warnings\":[]}\n"));
        streams.insert(path, bytes.len() as u64);
        Response::Acknowledged {
          offset: bytes.len() as u64,
        }
      }
      Request::SealRun {
        documents,
        streams: sealed,
        incomplete,
        ..
      } => {
        assert_eq!(documents, docs);
        assert_eq!(sealed, streams);
        assert!(!incomplete);
        Response::Archive {
          archive: ArchiveRecord {
            status: "pending".into(),
            incomplete: false,
            file: None,
            last_error: None,
          },
        }
      }
      _ => panic!("unexpected publishing request"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let (_root, options) = fixture(&url);
  fs::atomic_json(
    &options.run_dir.join("run-state.json"),
    &json!({"run_id":"run-fixture", "status":"completed",
    "started_at":"1970-01-01T00:00:00Z", "finished_at":"1970-01-01T00:00:01Z",
    "machine_id":"machine-1", "system_capture_id":"capture-1", "system_capture_status":"complete"}),
  )
  .unwrap();
  std::fs::write(options.run_dir.join("outputs/system-profile.jsonl"), b"{\"kind\":\"capture\",\"schema_version\":1,\"capture_id\":\"capture-1\",\"machine_id\":\"machine-1\",\"start_ms\":0}\n{\"kind\":\"end\",\"end_ms\":1000,\"complete\":true,\"warnings\":[]}\n").unwrap();
  let saved = serde_json::to_vec(&prepare_manual(&options).unwrap()).unwrap();
  std::fs::remove_file(&options.config).unwrap();
  let job: PreparedJob = serde_json::from_slice(&saved).unwrap();
  let mut reports = Vec::new();
  assert!(
    run_job(&job, Arc::new(AtomicBool::new(false)), &mut |value| {
      reports.push(value);
      Ok(())
    })
    .unwrap()
  );
  assert_eq!(
    reports.last().unwrap()["system_capture_upload"]["status"],
    "published"
  );
  assert!(
    !reports
      .iter()
      .any(|value| value.to_string().contains("bound-worker-token"))
  );
  let status = std::fs::read_to_string(job.run_dir.join(STATE)).unwrap();
  assert!(status.contains("published") && !status.contains("bound-worker-token"));
  server.join().unwrap();
}

#[test]
fn cancelled_job_does_not_create_upload_progress_or_touch_training() {
  let (_root, options) = fixture("http://127.0.0.1:1");
  let job = prepare_manual(&options).unwrap();
  let original = std::fs::read(options.run_dir.join("run-state.json")).unwrap();
  assert!(
    !run_job(&job, Arc::new(AtomicBool::new(true)), &mut |_| panic!(
      "cancelled job reported progress"
    ))
    .unwrap()
  );
  assert!(!job.queue_dir.exists());
  assert_eq!(
    std::fs::read(options.run_dir.join("run-state.json")).unwrap(),
    original
  );
}

#[test]
fn manual_run_status_uses_recorded_identity_when_the_directory_has_another_name() {
  let (_root, options) = fixture("http://127.0.0.1:1");
  fs::atomic_json(
    &options.run_dir.join("run-state.json"),
    &json!({"run_id":"recorded-run", "status":"completed"}),
  )
  .unwrap();
  let job = prepare_manual(&options).unwrap();
  let state = PublishingState {
    schema_version: 1,
    run_id: job.scope.run_id.clone(),
    status: "published".into(),
    updated_at: now(),
    last_success_at: None,
    last_error: None,
    retry_after_seconds: None,
    dashboard_url: None,
    progress: json!({}),
  };
  fs::atomic_json(&job.run_dir.join(STATE), &state).unwrap();
  assert_eq!(
    super::super::status(&job.run_dir).unwrap().unwrap()["run_id"],
    "recorded-run"
  );
  let resumed = super::super::resume(&job.run_dir).unwrap();
  assert_eq!(resumed["run_id"], "recorded-run");
  assert_eq!(resumed["started"], false);
}

#[test]
fn bounded_slices_resume_stream_offsets_and_never_seal_a_shorter_prefix() {
  let size = 256 * 1024 + 17;
  let mut offset = 0;
  let mut documents = std::collections::BTreeMap::new();
  let (url, server) = mock(10, move |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec!["tracking-v1".into()],
      },
      Request::PutDocument {
        path,
        revision,
        offset,
        total_size,
        ..
      } => {
        assert_eq!(offset, 0);
        documents.insert(path, revision);
        Response::DocumentAcknowledged {
          revision,
          offset: total_size,
          complete: true,
        }
      }
      Request::AppendTracking {
        path,
        offset: expected,
        data_base64,
        ..
      } => {
        assert_eq!(path, "logs/stdout.log");
        assert_eq!(offset, expected);
        let bytes = STANDARD.decode(data_base64).unwrap();
        assert!(bytes.iter().all(|byte| *byte == b'a'));
        offset += bytes.len() as u64;
        Response::Acknowledged { offset }
      }
      Request::SealRun {
        documents: expected,
        streams,
        ..
      } => {
        assert_eq!(offset, size as u64, "partial backlog was sealed");
        assert_eq!(expected, documents);
        assert_eq!(streams["logs/stdout.log"], offset);
        Response::Archive {
          archive: ArchiveRecord {
            status: "pending".into(),
            incomplete: false,
            file: None,
            last_error: None,
          },
        }
      }
      _ => panic!("unexpected publishing request"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let (_root, options) = fixture(&url);
  std::fs::create_dir(options.run_dir.join("logs")).unwrap();
  std::fs::write(options.run_dir.join("logs/stdout.log"), vec![b'a'; size]).unwrap();
  let job = prepare_manual(&options).unwrap();
  let mut report = Value::Null;
  assert!(
    !run_job(&job, Arc::new(AtomicBool::new(false)), &mut |value| {
      report = value;
      Ok(())
    })
    .unwrap()
  );
  assert_eq!(report["backlog_remaining"], true);
  let queue = job
    .queue_dir
    .join("runs/project/worker/run-fixture/queue.json");
  let saved: Value = serde_json::from_slice(&std::fs::read(queue).unwrap()).unwrap();
  assert_eq!(saved["streams"]["logs/stdout.log"], 256 * 1024);
  assert!(saved["archive"].is_null());
  let recovered: PreparedJob = serde_json::from_value(serde_json::to_value(job).unwrap()).unwrap();
  assert!(
    run_job(&recovered, Arc::new(AtomicBool::new(false)), &mut |value| {
      report = value;
      Ok(())
    })
    .unwrap()
  );
  assert_eq!(report["backlog_remaining"], false);
  server.join().unwrap();
}

#[test]
fn caught_up_and_idle_runs_do_not_hint_backlog_and_direct_reports_stay_unchanged() {
  let (url, server) = mock(5, |request, _| {
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec!["tracking-v1".into()],
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
        offset,
        data_base64,
        ..
      } => Response::Acknowledged {
        offset: offset + STANDARD.decode(data_base64).unwrap().len() as u64,
      },
      _ => panic!("active idle run must not upload objects or seal"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let (_root, options) = fixture(&url);
  fs::atomic_json(
    &options.run_dir.join("run-state.json"),
    &json!({"run_id":"run-fixture", "status":"running"}),
  )
  .unwrap();
  std::fs::write(
    options.run_dir.join("outputs/metrics.jsonl"),
    b"{\"step\":1,\"metrics\":{\"loss\":0.5}}\n",
  )
  .unwrap();
  let job = prepare_manual(&options).unwrap();
  let stop = Arc::new(AtomicBool::new(false));
  for _ in 0..2 {
    let mut report = Value::Null;
    assert!(
      !run_job(&job, Arc::clone(&stop), &mut |value| {
        report = value;
        Ok(())
      })
      .unwrap()
    );
    assert_eq!(
      report["backlog_remaining"], false,
      "caught-up watch must keep the idle cadence"
    );
  }
  let direct = Publisher::new(&options).unwrap();
  assert!(direct.report(false).get("backlog_remaining").is_none());
  assert!(direct.progress(false).get("backlog_remaining").is_none());
  server.join().unwrap();
}

#[test]
fn manual_artifact_transfer_uses_the_bound_credential_after_config_replacement() {
  let mut upload: Option<UploadState> = None;
  let mut target: Option<FileTarget> = None;
  let mut digest = String::new();
  let bytes = b"checkpoint bytes";
  let (url, server) = mock(10, move |request, origin| {
    if request.path == "/part" {
      assert!(
        !request
          .headers
          .to_ascii_lowercase()
          .contains("authorization:")
      );
      assert_eq!(request.body, bytes);
      return (200, vec![("ETag".into(), "part-1".into())], Vec::new());
    }
    assert!(request.headers.contains("Bearer bound-worker-token"));
    assert!(!request.headers.contains("replacement-token"));
    let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => Response::Capabilities {
        features: vec!["tracking-v1".into()],
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
      Request::BeginUpload {
        upload_id,
        target: returned,
        size,
        sha256,
      } => {
        assert_eq!(size, bytes.len() as u64);
        assert!(
          matches!(&returned, FileTarget::Run { path, scope } if path == "outputs/model.pt" && scope.project_id == "project" && scope.origin == "worker")
        );
        target = Some(returned);
        digest = sha256;
        upload = Some(UploadState {
          upload_id,
          part_size: 8 * 1024 * 1024,
          parts: Vec::new(),
          complete: false,
        });
        Response::Upload {
          upload: upload.clone().unwrap(),
        }
      }
      Request::PartUrl { .. } => Response::Url {
        url: format!("{origin}/part"),
      },
      Request::RecordPart { part, .. } => {
        upload.as_mut().unwrap().parts.push(part);
        Response::Upload {
          upload: upload.clone().unwrap(),
        }
      }
      Request::CompleteUpload { .. } => Response::File {
        file: FileRecord {
          target: target.clone().unwrap(),
          size: bytes.len() as u64,
          sha256: Some(digest.clone()),
          storage: FileStorage::Object,
          provenance: None,
        },
      },
      Request::SealRun { incomplete, .. } => Response::Archive {
        archive: ArchiveRecord {
          status: "pending".into(),
          incomplete,
          file: None,
          last_error: None,
        },
      },
      _ => panic!("unexpected artifact publishing request"),
    };
    (200, Vec::new(), serde_json::to_vec(&response).unwrap())
  });
  let (_root, mut options) = fixture(&url);
  std::fs::write(options.run_dir.join("outputs/model.pt"), bytes).unwrap();
  options.artifacts = vec!["outputs/model.pt".into()];
  let job = prepare_manual(&options).unwrap();
  std::fs::write(
    &options.config,
    "url='http://127.0.0.1:1'\ntoken='replacement-token'\n",
  )
  .unwrap();
  let stop = Arc::new(AtomicBool::new(false));
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
  while !run_job(&job, Arc::clone(&stop), &mut |_| Ok(())).unwrap() {
    assert!(std::time::Instant::now() < deadline);
    std::thread::sleep(std::time::Duration::from_millis(5));
  }
  server.join().unwrap();
}
