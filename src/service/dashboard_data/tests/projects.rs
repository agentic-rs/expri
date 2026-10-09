use super::*;

fn put(fixture: &Fixture, scope: &RunScope, path: &str, value: Value) {
  let bytes = value.to_string().into_bytes();
  fixture
    .store
    .execute(Request::PutDocument {
      scope: scope.clone(),
      path: path.into(),
      revision: 1,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

fn append(fixture: &Fixture, scope: &RunScope, path: &str, bytes: &[u8], offset: u64) {
  fixture
    .store
    .execute(Request::AppendTracking {
      scope: scope.clone(),
      path: path.into(),
      offset,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
}

fn run(fixture: &Fixture, project: &str, origin: &str, id: &str, value: f64) -> RunScope {
  let scope = RunScope {
    project_id: project.into(),
    origin: origin.into(),
    run_id: id.into(),
  };
  put(
    fixture,
    &scope,
    "run-state.json",
    json!({"schema_version":1,"run_id":id,"task":"train","status":"completed","started_at":"2026-10-09T00:00:00Z","finished_at":"2026-10-09T00:01:00Z","exit_code":0}),
  );
  put(
    fixture,
    &scope,
    "snapshot.json",
    json!({"source":{"git_head":origin}}),
  );
  put(
    fixture,
    &scope,
    "outputs/params.json",
    json!({"rate":value}),
  );
  append(
    fixture,
    &scope,
    "outputs/metrics.jsonl",
    format!("{}\n", json!({"step":1,"metrics":{"loss":value}})).as_bytes(),
    0,
  );
  append(
    fixture,
    &scope,
    "logs/stdout.log",
    format!("{project}/{origin}/{id}\n").as_bytes(),
    0,
  );
  fixture.publish(
    FileTarget::Run {
      scope: scope.clone(),
      path: "outputs/checkpoint.pt".into(),
    },
    format!("{project}/{origin}").into_bytes(),
  );
  scope
}

fn list<'a>(
  origin: Option<&'a str>,
  options: Option<&'a table::TableOptions>,
  limit: usize,
  offset: usize,
) -> ListQuery<'a> {
  ListQuery {
    origin,
    search: None,
    task: None,
    status: None,
    limit,
    offset,
    table: options,
  }
}

#[test]
fn projects_aggregate_published_machines_and_keep_the_legacy_catalog_unchanged() {
  let fixture = Fixture::new();
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  assert_eq!(dashboard.projects().unwrap()["sources"], json!([]));
  run(&fixture, "project", "worker-b", "shared", 4.);
  run(&fixture, "project", "worker-a", "shared", 2.);
  run(&fixture, "other", "worker-a", "shared", 8.);
  fixture.publish(
    FileTarget::Input {
      project_id: "private-only".into(),
      input_id: "dataset".into(),
    },
    b"private".to_vec(),
  );
  let projects = dashboard.projects().unwrap();
  assert_eq!(
    projects["sources"],
    json!([
      {"source_id":"hosted-project:other","label":"other","kind":"hosted_project","target_name":null,"project_id":"other","origin":null,"machines":["worker-a"]},
      {"source_id":"hosted-project:private-only","label":"private-only","kind":"hosted_project","target_name":null,"project_id":"private-only","origin":null,"machines":[]},
      {"source_id":"hosted-project:project","label":"project","kind":"hosted_project","target_name":null,"project_id":"project","origin":null,"machines":["worker-a","worker-b"]}
    ])
  );
  assert_eq!(projects["initial_source"], "hosted-project:other");
  let legacy = dashboard.catalog().unwrap();
  assert_eq!(legacy["sources"].as_array().unwrap().len(), 3);
  assert!(
    legacy["sources"]
      .as_array()
      .unwrap()
      .iter()
      .all(|source| source["kind"] == "service"
        && source.get("machines").is_none()
        && source["origin"].is_string())
  );
  assert!(fixture.backend.objects.lock().unwrap().requests.is_empty());
}

#[test]
fn storage_lists_completed_project_inputs_and_outputs_without_reading_object_bytes() {
  let fixture = Fixture::new();
  fixture.publish(
    FileTarget::Input {
      project_id: "project".into(),
      input_id: "dataset-v1".into(),
    },
    b"private input".to_vec(),
  );
  fixture.publish(
    FileTarget::Input {
      project_id: "project".into(),
      input_id: "weights-v1".into(),
    },
    b"weights".to_vec(),
  );
  fixture.publish(
    FileTarget::Input {
      project_id: "other".into(),
      input_id: "hidden".into(),
    },
    b"another project".to_vec(),
  );
  let pending = FileTarget::Input {
    project_id: "project".into(),
    input_id: "pending".into(),
  };
  fixture
    .store
    .execute(Request::BeginUpload {
      upload_id: "pending-storage-input".into(),
      target: pending,
      size: 10,
      sha256: "a".repeat(64),
    })
    .unwrap();
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let input_page = dashboard.storage("project", "input", "", 1, 0).unwrap();
  assert_eq!(input_page["total_count"], 2);
  assert_eq!(input_page["next_offset"], 1);
  assert_eq!(
    input_page["items"][0],
    json!({
      "input_id": "weights-v1",
      "size": 7,
      "download_url": "/api/input?project_id=project&input_id=weights-v1",
    })
  );
  let next = dashboard.storage("project", "input", "", 1, 1).unwrap();
  assert_eq!(next["items"][0]["input_id"], "dataset-v1");
  assert!(next["next_offset"].is_null());
  let searched = dashboard
    .storage("project", "input", "DATASET", 100, 0)
    .unwrap();
  assert_eq!(searched["total_count"], 1);
  assert_eq!(searched["items"][0]["input_id"], "dataset-v1");

  for (origin, run_id, path) in [
    ("worker-a", "shared", "outputs/checkpoint.pt"),
    ("worker-b", "shared", "outputs/model.pt"),
  ] {
    fixture.publish(
      FileTarget::Run {
        scope: RunScope {
          project_id: "project".into(),
          origin: origin.into(),
          run_id: run_id.into(),
        },
        path: path.into(),
      },
      b"checkpoint".to_vec(),
    );
  }
  fixture.publish(
    FileTarget::Run {
      scope: RunScope {
        project_id: "other".into(),
        origin: "worker-a".into(),
        run_id: "shared".into(),
      },
      path: "outputs/foreign.pt".into(),
    },
    b"foreign".to_vec(),
  );
  fixture.publish(
    FileTarget::Run {
      scope: RunScope {
        project_id: "project".into(),
        origin: "worker-a".into(),
        run_id: "shared".into(),
      },
      path: crate::run_artifacts::INVENTORY_PATH.into(),
    },
    b"{}".to_vec(),
  );
  fixture.publish(
    FileTarget::Run {
      scope: RunScope {
        project_id: "project".into(),
        origin: "worker-a".into(),
        run_id: "shared".into(),
      },
      path: "outputs/.hidden".into(),
    },
    b"excluded".to_vec(),
  );
  for path in ["outputs/control\n.pt", "outputs/control\u{0085}.pt"] {
    fixture.publish(
      FileTarget::Run {
        scope: RunScope {
          project_id: "project".into(),
          origin: "worker-a".into(),
          run_id: "shared".into(),
        },
        path: path.into(),
      },
      b"excluded".to_vec(),
    );
  }
  let output_page = dashboard.storage("project", "output", "", 100, 0).unwrap();
  assert_eq!(output_page["total_count"], 2);
  assert_eq!(
    output_page["items"][0],
    json!({
      "origin": "worker-b",
      "run_id": "shared",
      "path": "outputs/model.pt",
      "size": 10,
      "download_url": "/api/artifact?source=hosted-project%3Aproject&run_id=worker-b%3Ashared&path=outputs%2Fmodel.pt",
    })
  );
  let searched = dashboard
    .storage("project", "output", "WORKER-A", 100, 0)
    .unwrap();
  assert_eq!(searched["total_count"], 1);
  assert_eq!(searched["items"][0]["path"], "outputs/checkpoint.pt");
  let serialized = output_page.to_string() + &input_page.to_string();
  assert!(!serialized.contains("projects/") && !serialized.contains("X-Amz-"));
  assert!(fixture.backend.objects.lock().unwrap().requests.is_empty());
  assert!(dashboard.storage("project", "input", "", 101, 0).is_err());
  assert!(
    dashboard
      .storage("project", "input", "", 1, usize::MAX)
      .is_err()
  );
  assert!(
    dashboard
      .storage("project", "input", &"x".repeat(257), 1, 0)
      .is_err()
  );
  assert!(dashboard.storage("project", "unknown", "", 1, 0).is_err());
}

#[test]
fn input_uploads_change_project_storage_revision_without_changing_run_revisions() {
  let fixture = Fixture::new();
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let before = dashboard.updates("hosted-project:input-only", &[]).unwrap();
  assert_eq!(before["storage_revision"], "0");
  fixture.publish(
    FileTarget::Input {
      project_id: "input-only".into(),
      input_id: "dataset".into(),
    },
    b"data".to_vec(),
  );
  let after = dashboard.updates("hosted-project:input-only", &[]).unwrap();
  assert_ne!(after["storage_revision"], before["storage_revision"]);
  assert_eq!(after["source_revision"], before["source_revision"]);
  assert_eq!(after["catalog_revision"], before["catalog_revision"]);
  assert_eq!(
    dashboard.projects().unwrap()["sources"][0]["source_id"],
    "hosted-project:input-only"
  );
  let empty_runs = dashboard
    .list_table("hosted-project:input-only", &list(None, None, 100, 0))
    .unwrap();
  assert_eq!(empty_runs["total_count"], 0);
  assert!(empty_runs["runs"].as_array().unwrap().is_empty());
  assert!(
    dashboard.columns("hosted-project:input-only").unwrap()["available_columns"]["metrics"]
      .as_array()
      .unwrap()
      .is_empty()
  );
  assert!(
    dashboard.catalog().unwrap()["sources"]
      .as_array()
      .unwrap()
      .is_empty()
  );
  let input = dashboard.input_download("input-only", "dataset").unwrap();
  let Download::Cloud {
    url,
    size,
    filename,
  } = input
  else {
    panic!("cloud expected")
  };
  assert_eq!(size, 4);
  assert_eq!(filename, "dataset");
  assert!(url.contains("projects/input-only/inputs/dataset/"));
  assert!(dashboard.input_download("other", "dataset").is_err());
}

#[test]
fn project_run_identity_survives_global_sort_paging_machine_filters_and_collisions() {
  let fixture = Fixture::new();
  run(&fixture, "project", "worker-a", "shared", 10.);
  run(&fixture, "project", "worker-b", "shared", 2.);
  run(&fixture, "project", "worker-b", "third", 5.);
  run(&fixture, "other", "worker-a", "shared", 99.);
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let options = table::TableOptions::parse(
    vec!["/rate".into()],
    vec!["loss".into()],
    "last",
    Some("metric:loss"),
    Some("asc"),
  )
  .unwrap();
  let page = dashboard
    .list_table("hosted-project:project", &list(None, Some(&options), 1, 1))
    .unwrap();
  assert_eq!(page["total_count"], 3);
  assert_eq!(page["runs"][0]["run_id"], "third");
  assert_eq!(page["runs"][0]["run_key"], "worker-b:third");
  assert_eq!(page["runs"][0]["origin"], "worker-b");
  assert_eq!(page["next_offset"], 2);
  let page = dashboard
    .list_table(
      "hosted-project:project",
      &list(Some("worker-b"), Some(&options), 100, 0),
    )
    .unwrap();
  assert_eq!(page["total_count"], 2);
  assert_eq!(page["runs"][0]["run_key"], "worker-b:shared");
  let machine =
    table::TableOptions::parse(vec![], vec![], "last", Some("origin"), Some("asc")).unwrap();
  let page = dashboard
    .list_table(
      "hosted-project:project",
      &list(None, Some(&machine), 100, 0),
    )
    .unwrap();
  assert_eq!(page["runs"][0]["origin"], "worker-a");
  let legacy = dashboard
    .list("hosted:project:worker-a", None, None, None, 100, 0)
    .unwrap();
  assert_eq!(legacy["runs"][0]["run_id"], "shared");
  assert!(legacy["runs"][0].get("run_key").is_none());
  assert!(
    dashboard
      .list_table(
        "hosted:project:worker-a",
        &list(Some("worker-b"), None, 100, 0)
      )
      .is_err()
  );
  assert!(
    dashboard
      .list_table(
        "hosted-project:project",
        &list(Some("../worker"), None, 100, 0)
      )
      .is_err()
  );
  assert_eq!(
    dashboard.columns("hosted-project:project").unwrap()["available_columns"]["params"],
    json!([{"key":"/rate","label":"rate"}])
  );
}

#[test]
fn project_detail_logs_artifacts_comparisons_and_charts_resolve_full_scopes() {
  let fixture = Fixture::new();
  run(&fixture, "project", "worker-a", "shared", 10.);
  run(&fixture, "project", "worker-b", "shared", 2.);
  run(&fixture, "other", "worker-a", "shared", 99.);
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  for (origin, value) in [("worker-a", 10.), ("worker-b", 2.)] {
    let key = format!("{origin}:shared");
    let detail = dashboard.detail("hosted-project:project", &key).unwrap();
    assert_eq!(detail["run"]["run_id"], "shared");
    assert_eq!(detail["run"]["run_key"], key);
    assert_eq!(detail["run"]["origin"], origin);
    assert_eq!(detail["params"]["rate"], value);
    assert_eq!(detail["metrics"]["loss"]["last"]["value"], value);
    assert_eq!(detail["snapshot"]["source"]["git_head"], origin);
    assert_eq!(
      dashboard
        .log("hosted-project:project", &key, "stdout", 100)
        .unwrap()["content"],
      format!("project/{origin}/shared\n")
    );
    let files = dashboard.artifacts("hosted-project:project", &key).unwrap();
    assert_eq!(files["run_id"], "shared");
    assert_eq!(files["run_key"], key);
    assert_eq!(
      files["pull_scope"],
      json!({"project_id":"project","origin":origin,"run_id":"shared"})
    );
    assert!(
      files["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "outputs/checkpoint.pt")
        .unwrap()["download_url"]
        .as_str()
        .unwrap()
        .contains(&format!("run_id={origin}%3Ashared"))
    );
    let Download::Cloud { url, .. } = dashboard
      .artifact_download("hosted-project:project", &key, "outputs/checkpoint.pt")
      .unwrap()
    else {
      panic!("cloud expected")
    };
    assert!(url.contains(&format!("projects/project/runs/{origin}/shared/")));
  }
  let keys = vec!["worker-a:shared".into(), "worker-b:shared".into()];
  let comparison = dashboard
    .compare(
      "hosted-project:project",
      &keys,
      &["loss".into()],
      Reduction::Last,
    )
    .unwrap();
  assert_eq!(
    comparison["comparison"]["runs"][0]["run_id"],
    "worker-a:shared"
  );
  assert_eq!(
    comparison["comparison"]["runs"][1]["run_id"],
    "worker-b:shared"
  );
  assert_eq!(
    comparison["comparison"]["runs"][1]["run"]["run_id"],
    "shared"
  );
  let chart = dashboard
    .chart(
      "hosted-project:project",
      &keys,
      &["loss".into()],
      ChartXAxis::Step,
    )
    .unwrap();
  assert!(chart.contains("worker-a:shared") && chart.contains("worker-b:shared"));
  assert_eq!(
    dashboard
      .detail("hosted:project:worker-a", "shared")
      .unwrap()["params"]["rate"],
    10.
  );
  for key in [
    "shared",
    "worker-a:",
    ":shared",
    "worker-a:other:shared",
    "project:worker-a:shared",
    "../worker:shared",
    "worker-a:../shared",
  ] {
    assert!(
      dashboard.detail("hosted-project:project", key).is_err(),
      "{key}"
    );
    assert!(
      dashboard
        .log("hosted-project:project", key, "stdout", 1)
        .is_err(),
      "{key}"
    );
    assert!(
      dashboard.artifacts("hosted-project:project", key).is_err(),
      "{key}"
    );
    assert!(
      dashboard
        .artifact_download("hosted-project:project", key, "outputs/checkpoint.pt")
        .is_err(),
      "{key}"
    );
    assert!(
      dashboard
        .archive_download("hosted-project:project", key)
        .is_err(),
      "{key}"
    );
    assert!(
      dashboard
        .updates("hosted-project:project", &[key.into()])
        .is_err(),
      "{key}"
    );
  }
  assert!(
    dashboard
      .detail("hosted-project:unknown", "worker-a:shared")
      .is_err()
  );
  assert!(
    dashboard
      .detail("hosted:project:worker-a", "worker-b:shared")
      .is_err()
  );
}

#[test]
fn project_updates_track_either_machine_and_keep_requested_keys_and_global_catalog_revision() {
  let fixture = Fixture::new();
  let a = run(&fixture, "project", "worker-a", "shared", 10.);
  let b = run(&fixture, "project", "worker-b", "shared", 2.);
  let other = run(&fixture, "other", "worker-a", "shared", 99.);
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let keys = vec![
    "worker-a:shared".into(),
    "worker-b:shared".into(),
    "missing:shared".into(),
  ];
  let first = dashboard.updates("hosted-project:project", &keys).unwrap();
  assert_eq!(first["runs"][0]["run_id"], "worker-a:shared");
  assert_eq!(first["runs"][1]["run_id"], "worker-b:shared");
  assert_eq!(first["runs"][2]["missing"], true);
  for scope in [&a, &b] {
    let size = fixture
      .store
      .dashboard_artifact(scope, "outputs/metrics.jsonl")
      .unwrap()
      .unwrap()
      .size;
    append(
      &fixture,
      scope,
      "outputs/metrics.jsonl",
      b"{\"step\":2,\"metrics\":{\"loss\":1}}\n",
      size,
    );
    let next = dashboard.updates("hosted-project:project", &keys).unwrap();
    assert_ne!(next["source_revision"], first["source_revision"]);
    let legacy = dashboard
      .updates(
        &format!("hosted:project:{}", scope.origin),
        &["shared".into()],
      )
      .unwrap();
    assert_eq!(legacy["runs"][0]["run_id"], "shared");
  }
  let before_other = dashboard.updates("hosted-project:project", &keys).unwrap();
  let size = fixture
    .store
    .dashboard_artifact(&other, "logs/stdout.log")
    .unwrap()
    .unwrap()
    .size;
  append(&fixture, &other, "logs/stdout.log", b"new\n", size);
  let after_other = dashboard.updates("hosted-project:project", &keys).unwrap();
  assert_eq!(
    after_other["source_revision"],
    before_other["source_revision"]
  );
  assert_ne!(
    after_other["catalog_revision"],
    before_other["catalog_revision"]
  );
}

#[test]
fn project_catalog_bounds_maximum_identifier_sizes_without_storage_reads() {
  let fixture = Fixture::new();
  let mut db =
    rusqlite::Connection::open(fixture._directory.path().join("metadata.sqlite3")).unwrap();
  let tx = db.transaction().unwrap();
  for index in 0..1001 {
    let scope = RunScope {
      project_id: format!("project-{index:04}-{}", "x".repeat(83)),
      origin: "m".repeat(96),
      run_id: "shared".into(),
    };
    assert_eq!(scope.project_id.len(), 96);
    let target = serde_json::to_string(&FileTarget::Run {
      scope,
      path: "logs/stdout.log".into(),
    })
    .unwrap();
    tx.execute("INSERT INTO streams(target,size) VALUES(?1,0)", [target])
      .unwrap();
  }
  tx.commit().unwrap();
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let projects = dashboard.projects().unwrap();
  assert!(projects["sources"].as_array().unwrap().len() <= 1000);
  assert!(!projects["warnings"].as_array().unwrap().is_empty());
  assert!(serde_json::to_vec(&projects).unwrap().len() < 512 * 1024);
  assert!(fixture.backend.objects.lock().unwrap().requests.is_empty());
}

fn archived(fixture: &Fixture, scope: &RunScope) {
  fixture
    .store
    .execute(Request::SealRun {
      scope: scope.clone(),
      documents: BTreeMap::from([
        ("run-state.json".into(), 1),
        ("snapshot.json".into(), 1),
        ("outputs/params.json".into(), 1),
      ]),
      streams: BTreeMap::new(),
      incomplete: true,
    })
    .unwrap();
  let db = rusqlite::Connection::open(fixture._directory.path().join("metadata.sqlite3")).unwrap();
  let job: i64 = db
    .query_row(
      "SELECT MAX(id) FROM result_archives WHERE scope=?1",
      [serde_json::to_string(scope).unwrap()],
      |row| row.get(0),
    )
    .unwrap();
  let upload_id = format!("result-archive-{job}");
  let bytes = format!("archive:{}/{}", scope.project_id, scope.origin).into_bytes();
  fixture
    .store
    .execute(Request::BeginUpload {
      upload_id: upload_id.clone(),
      target: FileTarget::Run {
        scope: scope.clone(),
        path: "result.zip".into(),
      },
      size: bytes.len() as u64,
      sha256: hex_digest(&Sha256::digest(&bytes)),
    })
    .unwrap();
  fixture.backend.objects.lock().unwrap().files.insert(
    format!(
      "projects/{}/runs/{}/{}/objects/{upload_id}",
      scope.project_id, scope.origin, scope.run_id
    ),
    Object {
      size: bytes.len() as u64,
      bytes: Arc::new(bytes),
    },
  );
  fixture
    .store
    .execute(Request::RecordPart {
      upload_id: upload_id.clone(),
      part: CompletedPart {
        part_number: 1,
        etag: "archive-part".into(),
      },
    })
    .unwrap();
  fixture
    .store
    .execute(Request::CompleteUpload { upload_id })
    .unwrap();
  // Simulate the archive worker's completion marker; upload and scope
  // publication above exercise the real managed result.zip upload contract.
  db.execute(
    "UPDATE result_archives SET status='archived' WHERE id=?1",
    [job],
  )
  .unwrap();
}

#[test]
fn project_archive_links_and_downloads_resolve_the_selected_machine_scope() {
  let fixture = Fixture::new();
  let a = run(&fixture, "project", "worker-a", "shared", 10.);
  let b = run(&fixture, "project", "worker-b", "shared", 2.);
  let outside = run(&fixture, "other", "worker-a", "shared", 99.);
  for scope in [&a, &b, &outside] {
    archived(&fixture, scope);
  }
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  for origin in ["worker-a", "worker-b"] {
    let key = format!("{origin}:shared");
    let detail = dashboard.detail("hosted-project:project", &key).unwrap();
    assert_eq!(detail["archive"]["status"], "archived");
    assert_eq!(
      detail["archive"]["file"]["target"]["scope"]["origin"],
      origin
    );
    assert!(
      detail["archive"]["download_url"]
        .as_str()
        .unwrap()
        .contains(&format!("run_id={origin}%3Ashared"))
    );
    let Download::Cloud { url, .. } = dashboard
      .archive_download("hosted-project:project", &key)
      .unwrap()
    else {
      panic!("cloud archive expected")
    };
    assert!(url.contains(&format!("projects/project/runs/{origin}/shared/")));
    assert!(!url.contains("projects/other/"));
  }
  assert!(
    dashboard
      .archive_download("hosted-project:unknown", "worker-a:shared")
      .is_err()
  );
  let Download::Cloud { url, .. } = dashboard
    .archive_download("hosted:project:worker-a", "shared")
    .unwrap()
  else {
    panic!("legacy archive expected")
  };
  assert!(url.contains("projects/project/runs/worker-a/shared/"));
}

#[test]
fn project_chart_parameter_errors_qualify_each_warning_once_and_keep_metric_curves() {
  let fixture = Fixture::new();
  for (origin, oversized) in [("worker-a", false), ("worker-b", true)] {
    let scope = RunScope {
      project_id: "project".into(),
      origin: origin.into(),
      run_id: "shared".into(),
    };
    fixture.publish(FileTarget::Run {scope:scope.clone(),path:"run-state.json".into()},json!({"schema_version":1,"run_id":"shared","task":"train","status":"completed","started_at":"2026-10-09T00:00:00Z"}).to_string().into_bytes());
    let target = FileTarget::Run {
      scope: scope.clone(),
      path: "outputs/params.json".into(),
    };
    if oversized {
      fixture.publish_size(target, vec![], JSON_LIMIT + 1);
    } else {
      fixture.publish(target, b"broken params".to_vec());
    }
    fixture
      .store
      .execute(Request::AppendStream {
        scope,
        path: "outputs/metrics.jsonl".into(),
        offset: 0,
        data_base64: base64::engine::general_purpose::STANDARD
          .encode(b"{\"step\":1,\"metrics\":{\"loss\":0.5}}\n"),
      })
      .unwrap();
  }
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let keys = vec!["worker-a:shared".into(), "worker-b:shared".into()];
  let runs = dashboard
    .read_metrics(
      "hosted-project:project",
      &keys,
      &["loss".into()],
      MetricReadMode {
        minimum: 1,
        retain_points: true,
        include_params: true,
      },
      Instant::now() + REQUEST_TIMEOUT,
    )
    .unwrap();
  for (run, key) in runs.iter().zip(&keys) {
    assert_eq!(&run.run_id, key);
    assert_eq!(run.metrics["loss"].summary.last.value, 0.5);
    let warning = run
      .warnings
      .iter()
      .find(|warning| {
        warning["message"]
          .as_str()
          .is_some_and(|message| message.contains("params.json"))
      })
      .unwrap();
    assert_eq!(warning["run_id"].as_str(), Some(key.as_str()));
    let detail = dashboard.detail("hosted-project:project", key).unwrap();
    assert_eq!(
      detail["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["message"]
          .as_str()
          .is_some_and(|message| message.contains("params.json")))
        .unwrap()["run_id"]
        .as_str(),
      Some(key.as_str())
    );
  }
  let chart = dashboard
    .chart(
      "hosted-project:project",
      &keys,
      &["loss".into()],
      ChartXAxis::Step,
    )
    .unwrap();
  assert!(chart.contains("contains invalid JSON"));
  assert!(chart.contains("exceeds the 1 MiB size limit"));
  assert!(chart.contains("worker-a:shared") && chart.contains("worker-b:shared"));
  assert!(!chart.contains("worker-a:worker-a:") && !chart.contains("worker-b:worker-b:"));
}
