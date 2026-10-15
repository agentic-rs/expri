use super::*;

fn scope(run_id: &str) -> RunScope {
  RunScope {
    project_id: "project".into(),
    origin: "worker".into(),
    run_id: run_id.into(),
  }
}

fn reply(response: Response) -> (u16, Vec<(String, String)>, Vec<u8>) {
  (200, vec![], serde_json::to_vec(&response).unwrap())
}

fn capabilities() -> Response {
  Response::Capabilities {
    features: vec!["tracking-v1".into(), "run-task-continuation-v1".into()],
  }
}

fn pinned(request: Request) -> Request {
  let Request::AtRunEpoch { epoch, request } = request else {
    panic!("owner mutation did not capture the run epoch");
  };
  assert_eq!(epoch, 3);
  *request
}

#[test]
fn upload_captures_epoch_once_using_its_existing_capability_request() {
  let (_temporary, root) = root();
  let selected = scope("selected");
  let expected = selected.clone();
  let mut index = 0;
  let (url, task) = mock(4, move |request, _| {
    let request: Request = serde_json::from_slice(&request.body).unwrap();
    let response = match index {
      0 => {
        assert!(matches!(request, Request::Capabilities));
        capabilities()
      }
      1 => {
        assert!(matches!(request, Request::RunPublication { scope } if scope == expected));
        Response::RunAttempt { epoch: 3 }
      }
      2 => {
        assert!(matches!(request, Request::ListFiles { scope } if scope == expected));
        Response::Files {
          files: vec![FileRecord {
            target: run_target(&expected, "run-state.json"),
            size: 2,
            sha256: None,
            storage: FileStorage::Tracking {
              revision: 7,
              sealed: false,
            },
            provenance: None,
          }],
        }
      }
      3 => {
        let Request::SealRun {
          scope,
          documents,
          incomplete,
          ..
        } = pinned(request)
        else {
          panic!("expected a fenced seal");
        };
        assert_eq!(scope, expected);
        assert_eq!(documents.get("run-state.json"), Some(&7));
        assert!(incomplete);
        Response::Archive {
          archive: ArchiveRecord {
            status: "pending".into(),
            incomplete: true,
            file: None,
            last_error: None,
          },
        }
      }
      _ => unreachable!(),
    };
    index += 1;
    reply(response)
  });
  let report = upload(&config(&root, &url), &selected, true).unwrap();
  assert_eq!(report["status"], "pending");
  task.join().unwrap();
}

#[test]
fn output_import_captures_the_source_run_epoch() {
  let (_temporary, root) = root();
  let selected = scope("source");
  let expected = selected.clone();
  let mut index = 0;
  let (url, task) = mock(3, move |request, _| {
    let request: Request = serde_json::from_slice(&request.body).unwrap();
    let response = match index {
      0 => {
        assert!(matches!(request, Request::Capabilities));
        capabilities()
      }
      1 => {
        assert!(matches!(request, Request::RunPublication { scope } if scope == expected));
        Response::RunAttempt { epoch: 3 }
      }
      2 => {
        let Request::ImportOutput {
          scope,
          path,
          size,
          sha256,
          ..
        } = pinned(request)
        else {
          panic!("expected a fenced output import");
        };
        assert_eq!(scope, expected);
        Response::File {
          file: FileRecord {
            target: FileTarget::Input {
              project_id: scope.project_id,
              input_id: "imported".into(),
            },
            size,
            sha256: Some(sha256),
            storage: FileStorage::Object,
            provenance: Some(AssetProvenance {
              producer: OutputProducer {
                origin: scope.origin,
                run_id: scope.run_id,
                path,
              },
              captured_at: "2026-01-01T00:00:00Z".into(),
              state: Value::Null,
              snapshot: Value::Null,
              environment: Value::Null,
            }),
          },
        }
      }
      _ => unreachable!(),
    };
    index += 1;
    reply(response)
  });
  let imported = import_output(
    &config(&root, &url),
    &selected,
    "outputs/best.pt",
    None,
    7,
    &"a".repeat(64),
  )
  .unwrap();
  assert_eq!(imported.size, 7);
  task.join().unwrap();
}

#[test]
fn references_capture_only_the_destination_run_epoch() {
  for destination_run in [false, true] {
    let (_temporary, root) = root();
    let source = run_target(&scope("source"), "outputs/best.pt");
    let target = if destination_run {
      run_target(&scope("destination"), "outputs/best.pt")
    } else {
      FileTarget::Input {
        project_id: "project".into(),
        input_id: "input".into(),
      }
    };
    let expected_source = source.clone();
    let expected_target = target.clone();
    let mut index = 0;
    let (url, task) = mock(if destination_run { 3 } else { 1 }, move |request, _| {
      let request: Request = serde_json::from_slice(&request.body).unwrap();
      if destination_run && index < 2 {
        let response = if index == 0 {
          assert!(matches!(request, Request::Capabilities));
          capabilities()
        } else {
          assert!(
            matches!(request, Request::RunPublication { scope: selected } if selected == scope("destination"))
          );
          Response::RunAttempt { epoch: 3 }
        };
        index += 1;
        return reply(response);
      }
      let request = if destination_run {
        pinned(request)
      } else {
        request
      };
      let Request::ReferenceFile {
        source,
        target,
        size,
        sha256,
      } = request
      else {
        panic!("expected a reference without querying the source run epoch");
      };
      assert_eq!(source, expected_source);
      assert_eq!(target, expected_target);
      reply(Response::File {
        file: FileRecord {
          target,
          size,
          sha256: Some(sha256),
          storage: FileStorage::Object,
          provenance: None,
        },
      })
    });
    reference(super::super::super::ReferenceOptions {
      config: config(&root, &url),
      source: serde_json::to_string(&source).unwrap(),
      target: serde_json::to_string(&target).unwrap(),
      size: 7,
      sha256: "a".repeat(64),
    })
    .unwrap();
    task.join().unwrap();
  }
}

#[test]
fn explicit_file_upload_captures_run_epoch_but_leaves_input_uploads_unfenced() {
  for destination_run in [false, true] {
    let (_temporary, root) = root();
    let path = root.join("checkpoint.pt");
    std::fs::write(&path, b"checkpoint").unwrap();
    let target = if destination_run {
      run_target(&scope("destination"), "outputs/best.pt")
    } else {
      FileTarget::Input {
        project_id: "project".into(),
        input_id: "input".into(),
      }
    };
    let expected = target.clone();
    let mut index = 0;
    let (url, task) = mock(if destination_run { 4 } else { 2 }, move |request, _| {
      let request: Request = serde_json::from_slice(&request.body).unwrap();
      if destination_run && index < 2 {
        let response = if index == 0 {
          assert!(matches!(request, Request::Capabilities));
          capabilities()
        } else {
          assert!(
            matches!(request, Request::RunPublication { scope: selected } if selected == scope("destination"))
          );
          Response::RunAttempt { epoch: 3 }
        };
        index += 1;
        return reply(response);
      }
      if matches!(&request, Request::GetFile { target } if target == &expected) {
        return (404, vec![], br#"{"error":"file missing"}"#.to_vec());
      }
      let request = if destination_run {
        pinned(request)
      } else {
        request
      };
      let Request::BeginUpload {
        target,
        upload_id,
        size,
        ..
      } = request
      else {
        panic!("expected a file upload with the selected destination epoch");
      };
      assert_eq!(target, expected);
      assert_eq!(size, 10);
      reply(Response::Upload {
        upload: UploadState {
          upload_id,
          part_size: 5 * 1024 * 1024,
          parts: vec![],
          complete: true,
        },
      })
    });
    let report = file_upload(super::super::super::FileUploadOptions {
      config: config(&root, &url),
      target: serde_json::to_string(&target).unwrap(),
      file: path,
      queue_dir: root.join("queue"),
    })
    .unwrap();
    assert_eq!(report["size"], 10);
    task.join().unwrap();
  }
}
