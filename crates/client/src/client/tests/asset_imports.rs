use super::*;

fn scope() -> RunScope {
  RunScope {
    project_id: "example-project".into(),
    origin: "worker-1".into(),
    run_id: "run-abc123".into(),
  }
}

fn provenance() -> AssetProvenance {
  AssetProvenance {
    producer: OutputProducer {
      origin: "worker-1".into(),
      run_id: "run-abc123".into(),
      path: "outputs/best.pt".into(),
    },
    captured_at: "2026-01-01T00:00:00Z".into(),
    state: Value::Null,
    snapshot: Value::Null,
    environment: Value::Null,
  }
}

#[test]
fn metadata_and_output_import_use_only_control_requests_without_downloading() {
  let (_temporary, root) = root();
  let target = run_target(&scope(), "outputs/best.pt");
  let expected = target.clone();
  let mut requests = 0;
  let (url, task) = mock(3, move |request, _| {
    assert_eq!(request.path, "/v1/request");
    let target = match serde_json::from_slice::<Request>(&request.body).unwrap() {
      Request::Capabilities => {
        return (
          200,
          vec![],
          serde_json::to_vec(&Response::Capabilities { features: vec![] }).unwrap(),
        );
      }
      Request::GetFile { target } => {
        assert_eq!(requests, 0);
        assert_eq!(target, expected);
        target
      }
      Request::ImportOutput {
        scope: imported,
        path,
        input_id,
        size,
        sha256,
      } => {
        assert_eq!(requests, 1);
        assert_eq!(imported, scope());
        assert_eq!(path, "outputs/best.pt");
        assert!(input_id.is_none());
        assert_eq!(size, 7);
        assert_eq!(sha256, "a".repeat(64));
        FileTarget::Input {
          project_id: imported.project_id,
          input_id: "asset-opaque-id".into(),
        }
      }
      _ => panic!("asset import requested a download or upload"),
    };
    let file = FileRecord {
      target,
      size: 7,
      sha256: Some("a".repeat(64)),
      storage: FileStorage::Object,
      provenance: (requests == 1).then(provenance),
    };
    requests += 1;
    (
      200,
      vec![],
      serde_json::to_vec(&Response::File { file }).unwrap(),
    )
  });
  let config = config(&root, &url);
  let mut contents = std::fs::read_to_string(&config).unwrap();
  contents.push_str("project_id='example-project'\n");
  std::fs::write(&config, contents).unwrap();
  let metadata = file_metadata(&config, &target).unwrap();
  assert_eq!(metadata.size, 7);
  let imported = import_output(
    &config,
    &scope(),
    "outputs/best.pt",
    None,
    metadata.size,
    metadata.sha256.as_deref().unwrap(),
  )
  .unwrap();
  assert_eq!(imported.provenance.unwrap().producer, provenance().producer);
  assert!(!root.join("models/base.pt").exists());
  task.join().unwrap();
}

#[test]
fn output_import_rejects_an_inconsistent_or_foreign_canonical_record() {
  for change in ["project", "identity", "producer", "input_id", "provenance"] {
    let (_temporary, root) = root();
    let (url, task) = mock(2, move |request, _| {
      if matches!(
        serde_json::from_slice::<Request>(&request.body).unwrap(),
        Request::Capabilities
      ) {
        return (
          200,
          vec![],
          serde_json::to_vec(&Response::Capabilities { features: vec![] }).unwrap(),
        );
      }
      let mut file = FileRecord {
        target: FileTarget::Input {
          project_id: "example-project".into(),
          input_id: "release".into(),
        },
        size: 7,
        sha256: Some("a".repeat(64)),
        storage: FileStorage::Object,
        provenance: Some(provenance()),
      };
      match change {
        "project" => {
          file.target = FileTarget::Input {
            project_id: "other-project".into(),
            input_id: "release".into(),
          }
        }
        "identity" => file.size = 8,
        "producer" => file.provenance.as_mut().unwrap().producer.run_id = "another-run".into(),
        "input_id" => {
          file.target = FileTarget::Input {
            project_id: "example-project".into(),
            input_id: "another-input".into(),
          }
        }
        "provenance" => file.provenance = None,
        _ => unreachable!(),
      }
      (
        200,
        vec![],
        serde_json::to_vec(&Response::File { file }).unwrap(),
      )
    });
    let error = import_output(
      &config(&root, &url),
      &scope(),
      "outputs/best.pt",
      Some("release"),
      7,
      &"a".repeat(64),
    )
    .unwrap_err();
    assert!(error.to_string().contains("inconsistent imported output"));
    task.join().unwrap();
  }
}
