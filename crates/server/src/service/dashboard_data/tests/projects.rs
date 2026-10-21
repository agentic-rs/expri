use super::*;

#[test]
fn continued_runs_show_current_attempt_parameters_without_falling_back_to_originals() {
  let fixture = Fixture::new();
  let id = "continued-parameters";
  fixture.run_file(id, "outputs/params.json", br#"{"max_steps":100}"#.to_vec());
  fixture.run_file(
    id,
    "outputs/attempts/2/params.json",
    br#"{"max_steps":300}"#.to_vec(),
  );
  let mut state = json!({"schema_version":1,"run_id":id,"task":"train","status":"running","started_at":"2026-10-15T01:00:00Z",
    "task_attempts":{"protocol_version":1,"current_attempt":2,"total_count":2,"truncated":false,"attempts":[{"attempt":1},{"attempt":2}]}});
  fixture.run_file(id, "run-state.json", serde_json::to_vec(&state).unwrap());
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let scope = Fixture::scope(id);
  let deadline = Instant::now() + REQUEST_TIMEOUT;
  assert_eq!(
    dashboard
      .parameters(&scope, None, deadline)
      .unwrap()
      .unwrap()["max_steps"],
    300
  );
  assert_eq!(
    dashboard
      .table_parameters(&scope, deadline)
      .unwrap()
      .unwrap()["max_steps"],
    300
  );
  assert_eq!(
    dashboard.detail("hosted:project:worker", id).unwrap()["params"]["max_steps"],
    300
  );
  state["task_attempts"]["current_attempt"] = json!(3);
  state["task_attempts"]["total_count"] = json!(3);
  state["task_attempts"]["attempts"]
    .as_array_mut()
    .unwrap()
    .push(json!({"attempt":3}));
  fixture.run_file(id, "run-state.json", serde_json::to_vec(&state).unwrap());
  assert!(
    dashboard
      .parameters(&scope, None, deadline)
      .unwrap()
      .is_none()
  );
  assert!(
    dashboard
      .table_parameters(&scope, deadline)
      .unwrap()
      .is_none()
  );
}

#[test]
fn project_run_rows_retain_errors_after_aggregate_truncation_and_separate_origins() {
  let fixture = Fixture::new();
  let put_state = |origin: &str, id: &str, task: Value, started_at: Value| {
    let scope = RunScope {
      project_id: "project".into(),
      origin: origin.into(),
      run_id: id.into(),
    };
    put(
      &fixture,
      &scope,
      "run-state.json",
      json!({"schema_version":1,"run_id":id,"task":task,"status":"completed","started_at":started_at,"finished_at":"2026-10-09T00:01:00Z","exit_code":0}),
    );
    scope
  };
  let broken = put_state("worker-a", "shared", json!("train"), Value::Null);
  fixture.publish(
    FileTarget::Run {
      scope: broken,
      path: "outputs/params.json".into(),
    },
    b"{".to_vec(),
  );
  put_state(
    "worker-b",
    "shared",
    json!("train"),
    json!("2026-10-09T00:00:00Z"),
  );
  for index in 0..101 {
    put_state(
      "worker-noise",
      &format!("noise-{index:03}"),
      Value::Null,
      json!("2026-10-09T00:00:00Z"),
    );
  }
  let options =
    table::TableOptions::parse(vec!["/rate".into()], Vec::new(), "last", None, None).unwrap();
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  for table in [None, Some(&options)] {
    let report = dashboard
      .list_table("hosted-project:project", &list(None, table, 1, 102))
      .unwrap();
    let row = &report["runs"][0];
    assert_eq!(row["run_key"], "worker-a:shared");
    assert_eq!(row["status"], "completed");
    let warnings = row["warnings"].as_array().unwrap();
    assert!(
      warnings
        .iter()
        .any(|warning| warning["message"] == "run-state.json has invalid or missing started_at")
    );
    assert!(
      warnings
        .iter()
        .all(|warning| warning["run_id"] == "worker-a:shared")
    );
    if table.is_some() {
      assert!(warnings.iter().any(|warning| {
        warning["message"]
          .as_str()
          .is_some_and(|message| message.contains("params.json"))
      }));
    }
    assert!(
      report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .all(|warning| warning["run_id"] != "worker-a:shared")
    );
    let healthy = dashboard
      .list_table(
        "hosted-project:project",
        &list(Some("worker-b"), table, 1, 0),
      )
      .unwrap();
    assert_eq!(healthy["runs"][0]["run_key"], "worker-b:shared");
    assert_eq!(healthy["runs"][0]["warnings"], json!([]));
  }
}

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
    group_experiments: false,
    experiment_id: None,
    archived: false,
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
fn annotations_follow_the_scope_and_refresh_cached_list_detail_and_comparison() {
  let fixture = Fixture::new();
  let selected = run(&fixture, "project", "worker-a", "shared", 2.);
  let other_worker = run(&fixture, "project", "worker-b", "shared", 4.);
  let other_project = run(&fixture, "other", "worker-a", "shared", 8.);
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let source = "hosted-project:project";
  let query = list(None, None, 100, 0);
  let first = dashboard.list_table(source, &query).unwrap();
  assert!(
    first["runs"]
      .as_array()
      .unwrap()
      .iter()
      .all(|run| run["nickname"].is_null() && run["tags"] == json!([]))
  );
  let before = dashboard
    .updates(source, &["worker-a:shared".into()])
    .unwrap();
  let annotations = dashboard
    .set_run_annotations(
      &selected,
      Some("Baseline α".into()),
      vec!["Fast-path".into(), "gold".into()],
    )
    .unwrap();
  let expected = json!(annotations);
  let after = dashboard
    .updates(source, &["worker-a:shared".into()])
    .unwrap();
  assert_ne!(before["source_revision"], after["source_revision"]);
  let cached = dashboard.list_table(source, &query).unwrap();
  let selected_row = cached["runs"]
    .as_array()
    .unwrap()
    .iter()
    .find(|run| run["origin"] == "worker-a")
    .unwrap();
  assert_eq!(selected_row["nickname"], "Baseline α");
  assert_eq!(selected_row["tags"], json!(["Fast-path", "gold"]));
  assert_eq!(selected_row["annotations"], expected);
  for search in ["BASELINE", "α", "FAST-PATH", "GOLD"] {
    let mut search_query = list(None, None, 100, 0);
    search_query.search = Some(search);
    let page = dashboard.list_table(source, &search_query).unwrap();
    assert_eq!(page["total_count"], 1);
    assert_eq!(page["runs"][0]["origin"], "worker-a");
  }
  let detail = dashboard.detail(source, "worker-a:shared").unwrap();
  assert_eq!(detail["run"]["annotations"], expected);
  assert_eq!(
    dashboard
      .detail("hosted:project:worker-a", "shared")
      .unwrap()["run"]["annotations"],
    expected
  );
  let comparison = dashboard
    .compare(
      source,
      &["worker-a:shared".into(), "worker-b:shared".into()],
      &["loss".into()],
      Reduction::Last,
    )
    .unwrap();
  let comparison_run = comparison["comparison"]["runs"]
    .as_array()
    .unwrap()
    .iter()
    .find(|run| run["run"]["origin"] == "worker-a")
    .unwrap();
  assert_eq!(comparison_run["run"]["annotations"], expected);
  for scope in [other_worker, other_project] {
    assert_eq!(
      fixture.store.run_annotations(&scope).unwrap().nickname,
      None
    );
    assert!(
      fixture
        .store
        .run_annotations(&scope)
        .unwrap()
        .tags
        .is_empty()
    );
  }

  // Producer publication changes training metadata without replacing annotations.
  let bytes = json!({"schema_version":1,"run_id":"shared","task":"train","status":"failed","nickname":"producer name","tags":["producer tag"]}).to_string().into_bytes();
  fixture
    .store
    .execute(Request::PutDocument {
      scope: selected.clone(),
      path: "run-state.json".into(),
      revision: 2,
      offset: 0,
      total_size: bytes.len() as u64,
      data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .unwrap();
  let republished = dashboard.list_table(source, &query).unwrap();
  let row = republished["runs"]
    .as_array()
    .unwrap()
    .iter()
    .find(|run| run["origin"] == "worker-a")
    .unwrap();
  assert_eq!(row["status"], "failed");
  assert_eq!(row["annotations"], expected);
  dashboard
    .set_run_annotations(&selected, None, Vec::new())
    .unwrap();
  let mut old_search = list(None, None, 100, 0);
  old_search.search = Some("baseline");
  assert_eq!(
    dashboard.list_table(source, &old_search).unwrap()["total_count"],
    0
  );
  assert_eq!(
    dashboard.detail(source, "worker-a:shared").unwrap()["run"]["annotations"],
    json!({"scope":selected,"nickname":null,"tags":[]})
  );
}

#[test]
fn archival_filters_keep_machine_scope_training_state_and_cached_views_current() {
  let fixture = Fixture::new();
  let archived_scope = run(&fixture, "project", "worker-a", "shared", 2.);
  run(&fixture, "project", "worker-b", "shared", 4.);
  run(&fixture, "other", "worker-a", "shared", 8.);
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let source = "hosted-project:project";
  let query = list(None, None, 100, 0);
  let first = dashboard.list_table(source, &query).unwrap();
  assert_eq!(first["total_count"], 2);
  let before = dashboard
    .updates(
      source,
      &["worker-a:shared".into(), "worker-b:shared".into()],
    )
    .unwrap();

  let archival = dashboard.archive_run(&archived_scope).unwrap();
  assert_eq!(archival.status, "archived");
  let active = dashboard.list_table(source, &query).unwrap();
  assert_eq!(active["total_count"], 1);
  assert_eq!(active["runs"][0]["origin"], "worker-b");
  let mut archived_query = list(None, None, 100, 0);
  archived_query.archived = true;
  let archived = dashboard.list_table(source, &archived_query).unwrap();
  assert_eq!(archived["total_count"], 1);
  assert_eq!(archived["runs"][0]["origin"], "worker-a");
  assert_eq!(archived["runs"][0]["status"], "completed");
  assert_eq!(
    archived["runs"][0]["archival"]["delete_after"],
    json!(archival.delete_after)
  );
  let after = dashboard
    .updates(
      source,
      &["worker-a:shared".into(), "worker-b:shared".into()],
    )
    .unwrap();
  assert_ne!(before["source_revision"], after["source_revision"]);
  let detail = dashboard.detail(source, "worker-a:shared").unwrap();
  assert_eq!(detail["run"]["status"], "completed");
  assert_eq!(detail["run"]["archival"]["status"], "archived");
  let comparison = dashboard
    .compare(
      source,
      &["worker-a:shared".into(), "worker-b:shared".into()],
      &["loss".into()],
      Reduction::Last,
    )
    .unwrap();
  assert!(
    comparison["comparison"]["runs"]
      .as_array()
      .unwrap()
      .iter()
      .any(|run| {
        run["run"]["origin"] == "worker-a" && run["run"]["archival"]["status"] == "archived"
      })
  );

  dashboard.restore_run(&archived_scope).unwrap();
  let restored = dashboard.list_table(source, &query).unwrap();
  assert_eq!(restored["total_count"], 2);
  assert!(
    restored["runs"]
      .as_array()
      .unwrap()
      .iter()
      .all(|run| run["archival"]["status"] == "active")
  );
  assert_eq!(
    dashboard.list_table(source, &archived_query).unwrap()["total_count"],
    0
  );
  assert_eq!(
    dashboard
      .list_table("hosted-project:other", &query)
      .unwrap()["total_count"],
    1
  );
}

#[test]
fn maximum_annotations_and_table_values_paginate_within_the_json_response_limit() {
  let fixture = Fixture::new();
  let nickname = "😀".repeat(128);
  let tags: Vec<_> = (0..16)
    .map(|index| {
      format!(
        "{}{}",
        "😀".repeat(47),
        char::from_u32(0x1f600 + index).unwrap()
      )
    })
    .collect();
  // Scalar table previews permit 256 serialized bytes, including the quotes.
  let scalar = format!("{}xy", "界".repeat(84));
  let params: serde_json::Map<_, _> = (0..8)
    .map(|index| (format!("column_{index}"), json!(scalar)))
    .collect();
  let expected_keys: BTreeSet<_> = (0..100)
    .map(|index| format!("worker:run-{index:03}"))
    .collect();
  for index in 0..100 {
    let scope = RunScope {
      project_id: "project".into(),
      origin: "worker".into(),
      run_id: format!("run-{index:03}"),
    };
    put(
      &fixture,
      &scope,
      "run-state.json",
      json!({"schema_version":1,"run_id":scope.run_id,"task":"train","status":"completed"}),
    );
    put(
      &fixture,
      &scope,
      "outputs/params.json",
      Value::Object(params.clone()),
    );
    fixture
      .store
      .set_run_annotations(&scope, Some(nickname.clone()), tags.clone())
      .unwrap();
  }
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let options = table::TableOptions::parse(
    (0..8).map(|index| format!("/column_{index}")).collect(),
    vec![],
    "last",
    None,
    None,
  )
  .unwrap();
  let mut offset = 0;
  let mut received = BTreeSet::new();
  let mut pages = 0;
  loop {
    let query = list(None, Some(&options), 100, offset);
    let page = dashboard
      .list_table("hosted-project:project", &query)
      .unwrap();
    assert!(serde_json::to_vec(&page).unwrap().len() <= LIST_RESPONSE_LIMIT);
    assert_eq!(page["total_count"], 100);
    let runs = page["runs"].as_array().unwrap();
    assert!(!runs.is_empty());
    if offset == 0 {
      assert!(
        page["next_offset"]
          .as_u64()
          .is_some_and(|next| next > 0 && next < 100)
      );
    }
    for run in runs {
      assert!(
        received.insert(run["run_key"].as_str().unwrap().to_owned()),
        "duplicate run: {run}"
      );
      assert_eq!(run["nickname"], nickname);
      assert_eq!(run["tags"], json!(tags));
      assert_eq!(run["annotations"]["nickname"], nickname);
      assert_eq!(run["annotations"]["tags"], json!(tags));
      assert_eq!(run["table_values_truncated"], false);
      for index in 0..8 {
        assert_eq!(
          run["table_values"]["params"][format!("/column_{index}")],
          scalar
        );
      }
    }
    pages += 1;
    match page["next_offset"].as_u64() {
      Some(next) => {
        assert_eq!(next as usize, offset + runs.len());
        offset = next as usize;
      }
      None => break,
    }
  }
  assert!(pages > 1);
  assert_eq!(received, expected_keys);
}

#[test]
fn expired_annotations_keep_direct_cleanup_review_readable_without_editing_capability() {
  let fixture = Fixture::new();
  let selected = run(&fixture, "project", "worker-a", "shared", 2.);
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  dashboard
    .set_run_annotations(&selected, Some("Baseline".into()), vec!["gold".into()])
    .unwrap();
  dashboard.archive_run(&selected).unwrap();
  let connection =
    rusqlite::Connection::open(fixture._directory.path().join("metadata.sqlite3")).unwrap();
  connection
    .execute("UPDATE run_retention SET delete_after=0", [])
    .unwrap();
  let mut query = list(None, None, 100, 0);
  query.archived = true;
  let list = dashboard
    .list_table("hosted-project:project", &query)
    .unwrap();
  // The existing retention policy hides expired rows from normal browsing.
  assert_eq!(list["total_count"], 0);
  let detail = dashboard
    .detail("hosted-project:project", "worker-a:shared")
    .unwrap();
  assert_eq!(detail["run"]["archival"]["status"], "deleting");
  assert_eq!(detail["run"]["nickname"], "Baseline");
  assert_eq!(detail["run"]["tags"], json!(["gold"]));
  assert!(detail["run"]["annotations"].is_null());
  assert!(detail["result_upload"].is_null());
  for field in ["state", "snapshot", "environment", "params"] {
    assert!(detail[field].is_null(), "{field}: {}", detail[field]);
  }
  assert_eq!(detail["metrics"], json!({}));
  assert!(fixture.backend.objects.lock().unwrap().requests.is_empty());
  assert_eq!(
    fixture.store.run_annotations(&selected).unwrap_err().status,
    410
  );
  assert_eq!(
    dashboard
      .set_run_annotations(&selected, Some("Changed".into()), vec![])
      .unwrap_err()
      .status,
    410
  );
  assert!(
    dashboard
      .result_zip_download("hosted-project:project", "worker-a:shared")
      .is_err()
  );
  assert!(
    dashboard
      .artifact_download(
        "hosted-project:project",
        "worker-a:shared",
        "outputs/checkpoint.pt"
      )
      .is_err()
  );
}

#[test]
fn archived_only_sources_keep_empty_active_views_and_column_discovery_available() {
  let fixture = Fixture::new();
  let scope = run(&fixture, "project", "worker-a", "only-run", 2.);
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  dashboard.archive_run(&scope).unwrap();
  for source in ["hosted-project:project", "hosted:project:worker-a"] {
    let query = list(None, None, 100, 0);
    assert_eq!(
      dashboard.list_table(source, &query).unwrap()["total_count"],
      0
    );
    let columns = dashboard.columns(source).unwrap();
    assert!(
      columns["available_columns"]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|column| column["key"] == "loss")
    );
    let mut archived = list(None, None, 100, 0);
    archived.archived = true;
    assert_eq!(
      dashboard.list_table(source, &archived).unwrap()["total_count"],
      1
    );
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
      "producer":null,"used_by":[],"used_by_count":0,
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
      "assets":[],"assets_count":0,
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
  let columns = dashboard.columns("hosted-project:project").unwrap();
  assert_eq!(
    columns["available_columns"]["params"],
    json!([{"key":"/rate","label":"rate","run_count":3}])
  );
  assert_eq!(columns["available_columns"]["sampled_run_count"], 3);
  let columns = dashboard.columns("hosted:project:worker-b").unwrap();
  assert_eq!(
    columns["available_columns"]["params"],
    json!([{"key":"/rate","label":"rate","run_count":2}])
  );
  assert_eq!(columns["available_columns"]["sampled_run_count"], 2);
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
        .result_zip_download("hosted-project:project", key)
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
fn project_result_upload_links_and_downloads_resolve_the_selected_machine_scope() {
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
    assert!(detail.get("archive").is_none());
    assert_eq!(detail["result_upload"]["status"], "uploaded");
    assert_eq!(detail["result_upload"]["incomplete"], true);
    assert_eq!(
      detail["result_upload"]["file"]["target"]["scope"]["origin"],
      origin
    );
    let result_url = detail["result_upload"]["download_url"].as_str().unwrap();
    assert!(result_url.starts_with("/api/result-zip?"));
    assert!(result_url.contains(&format!("run_id={origin}%3Ashared")));
    let Download::Cloud { url, .. } = dashboard
      .result_zip_download("hosted-project:project", &key)
      .unwrap()
    else {
      panic!("cloud archive expected")
    };
    assert!(url.contains(&format!("projects/project/runs/{origin}/shared/")));
    assert!(!url.contains("projects/other/"));
  }
  assert!(
    dashboard
      .result_zip_download("hosted-project:unknown", "worker-a:shared")
      .is_err()
  );
  let Download::Cloud { url, .. } = dashboard
    .result_zip_download("hosted:project:worker-a", "shared")
    .unwrap()
  else {
    panic!("machine-source result ZIP expected")
  };
  assert!(url.contains("projects/project/runs/worker-a/shared/"));
}

#[test]
fn result_upload_detail_exposes_canonical_progress_without_changing_training_status() {
  let fixture = Fixture::new();
  let scope = run(&fixture, "project", "worker", "run", 1.);
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let detail = dashboard
    .detail("hosted-project:project", "worker:run")
    .unwrap();
  assert_eq!(detail["result_upload"]["status"], "none");
  assert_eq!(detail["result_upload"]["download_url"], Value::Null);
  archived(&fixture, &scope);
  let db = rusqlite::Connection::open(fixture._directory.path().join("metadata.sqlite3")).unwrap();
  for status in ["pending", "uploading", "failed"] {
    db.execute(
      "UPDATE result_archives SET status=?1,last_error=?2 WHERE scope=?3",
      rusqlite::params![
        status,
        "Upload was interrupted",
        serde_json::to_string(&scope).unwrap()
      ],
    )
    .unwrap();
    let detail = dashboard
      .detail("hosted-project:project", "worker:run")
      .unwrap();
    assert_eq!(detail["result_upload"]["status"], status);
    assert!(detail.get("archive").is_none());
    assert_eq!(
      detail["result_upload"]["last_error"],
      "Upload was interrupted"
    );
    assert_eq!(detail["result_upload"]["download_url"], Value::Null);
    assert_eq!(detail["run"]["status"], "completed");
  }
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
