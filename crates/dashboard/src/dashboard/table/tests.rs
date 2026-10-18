use super::*;

fn options(sort: &str, direction: &str) -> TableOptions {
  TableOptions::parse(
    vec!["/value".into()],
    vec!["loss".into()],
    "last",
    Some(sort),
    Some(direction),
  )
  .unwrap()
}

fn row(id: &str, value: Value) -> TableRow {
  TableRow::new(
    json!({"run_id":id,"status":"completed"}),
    ScalarData {
      params: Some(json!({"value":value})),
      metrics: BTreeMap::new(),
    },
  )
}

#[test]
fn numeric_sort_preserves_large_integers_and_puts_missing_last_in_both_directions() {
  let mut rows = vec![
    row("null", Value::Null),
    row("ten", json!(10)),
    row("two", json!(2)),
    row("large-a", json!(9_007_199_254_740_992_u64)),
    row("large-b", json!(9_007_199_254_740_993_u64)),
  ];
  options("param:/value", "asc").sort_rows(&mut rows);
  assert_eq!(
    rows
      .iter()
      .map(|row| row.run["run_id"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["two", "ten", "large-a", "large-b", "null"]
  );
  options("param:/value", "desc").sort_rows(&mut rows);
  assert_eq!(
    rows
      .iter()
      .map(|row| row.run["run_id"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["large-b", "large-a", "ten", "two", "null"]
  );
  assert_eq!(
    compare_number(
      &serde_json::from_str("-1.2e3").unwrap(),
      &serde_json::from_str("-1200").unwrap()
    ),
    Ordering::Equal
  );
  assert_eq!(
    compare_number(
      &serde_json::from_str("0.0").unwrap(),
      &serde_json::from_str("-0.0").unwrap()
    ),
    Ordering::Equal
  );
}

#[test]
fn text_boolean_types_and_ties_have_a_deterministic_order() {
  let mut rows = vec![
    row("z", json!("a")),
    row("a", json!("a")),
    row("string-ten", json!("10")),
    row("true", json!(true)),
    row("false", json!(false)),
    row("number", json!(1)),
  ];
  options("param:/value", "asc").sort_rows(&mut rows);
  assert_eq!(
    rows
      .iter()
      .map(|row| row.run["run_id"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["false", "true", "number", "string-ten", "a", "z"]
  );
}

#[test]
fn timestamps_sort_by_instant_and_unknown_dates_remain_last() {
  let mut rows = vec![
    row("missing", json!(1)),
    row("new", json!(1)),
    row("old", json!(1)),
  ];
  rows[1].run["started_at"] = json!("2026-10-09T01:00:00+01:00");
  rows[2].run["started_at"] = json!("2026-10-09T00:30:00+02:00");
  options("started_at", "desc").sort_rows(&mut rows);
  assert_eq!(
    rows
      .iter()
      .map(|row| row.run["run_id"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["new", "old", "missing"]
  );
}

#[test]
fn discovery_escapes_leaf_keys_omits_structured_values_and_reports_limits() {
  let data = ScalarData {
    params: Some(json!({"a/b":{"~name":0.01},"array":[1,2],"null":null})),
    metrics: BTreeMap::from([("loss".into(), [1., 0., 2.])]),
  };
  let mut columns = Columns::default();
  columns.add(&data);
  assert_eq!(
    columns.response()["params"],
    json!([{"key":"/a~1b/~0name","label":"a/b / ~name","run_count":1},{"key":"/null","label":"null","run_count":1}])
  );
  assert_eq!(
    columns.response()["metrics"],
    json!([{"key":"loss","label":"loss","run_count":1}])
  );
  assert_eq!(columns.response()["sampled_run_count"], 1);
  let values = (0..101)
    .map(|index| (format!("key-{index:03}"), json!(index)))
    .collect();
  columns.add(&ScalarData {
    params: Some(Value::Object(values)),
    metrics: BTreeMap::new(),
  });
  assert_eq!(columns.response()["params"].as_array().unwrap().len(), 100);
  assert!(columns.truncated);
}

#[test]
fn discovery_ranks_run_presence_before_alphabetical_names() {
  let mut columns = Columns::default();
  columns.add(&ScalarData {
    params: Some(json!({"a_rare":1,"z_common":null,"nested":{"shared":false}})),
    metrics: BTreeMap::from([
      ("a_rare".into(), [50., 0., 100.]),
      ("z_common".into(), [1., 1., 1.]),
    ]),
  });
  columns.add(&ScalarData {
    params: Some(json!({"z_common":2,"nested":{"shared":true}})),
    metrics: BTreeMap::from([("z_common".into(), [200., 0., 500.])]),
  });
  columns.add(&ScalarData::default());
  let response = columns.response();
  assert_eq!(response["sampled_run_count"], 3);
  assert_eq!(
    response["params"],
    json!([
      {"key":"/nested/shared","label":"nested / shared","run_count":2},
      {"key":"/z_common","label":"z_common","run_count":2},
      {"key":"/a_rare","label":"a_rare","run_count":1}
    ])
  );
  assert_eq!(
    response["metrics"],
    json!([
      {"key":"z_common","label":"z_common","run_count":2},
      {"key":"a_rare","label":"a_rare","run_count":1}
    ])
  );
  assert_eq!(response["truncated"], false);
}

#[test]
fn discovery_considers_later_common_columns_before_the_response_limit() {
  let mut columns = Columns::default();
  columns.add(&ScalarData {
    params: Some(Value::Object(
      (0..150)
        .map(|index| (format!("a_rare_{index:03}"), json!(index)))
        .collect(),
    )),
    metrics: (0..150)
      .map(|index| (format!("a_rare_{index:03}"), [1., 1., 1.]))
      .collect(),
  });
  let common = ScalarData {
    params: Some(json!({"z_common":1})),
    metrics: BTreeMap::from([("z_common".into(), [1., 1., 1.])]),
  };
  columns.add(&common);
  columns.add(&common);
  let response = columns.response();
  for (kind, key) in [("params", "/z_common"), ("metrics", "z_common")] {
    assert_eq!(response[kind].as_array().unwrap().len(), DISCOVERY_LIMIT);
    assert_eq!(response[kind][0]["key"], key);
    assert_eq!(response[kind][0]["run_count"], 2);
  }
  assert_eq!(response["sampled_run_count"], 3);
  assert_eq!(response["truncated"], true);
}

#[test]
fn discovery_bounds_candidates_without_overstating_retained_run_counts() {
  let mut columns = Columns::default();
  columns.add(&ScalarData {
    params: Some(Value::Object(
      (0..=DISCOVERY_CANDIDATE_LIMIT)
        .map(|index| (format!("key_{index:05}"), json!(index)))
        .collect(),
    )),
    metrics: (0..=DISCOVERY_CANDIDATE_LIMIT)
      .map(|index| (format!("key_{index:05}"), [1., 1., 1.]))
      .collect(),
  });
  columns.add(&ScalarData {
    params: Some(json!({"key_00000":2,"later_omitted":2})),
    metrics: BTreeMap::from([
      ("key_00000".into(), [2., 2., 2.]),
      ("later_omitted".into(), [2., 2., 2.]),
    ]),
  });
  assert_eq!(columns.params.len(), DISCOVERY_CANDIDATE_LIMIT);
  assert_eq!(columns.metrics.len(), DISCOVERY_CANDIDATE_LIMIT);
  let response = columns.response();
  for kind in ["params", "metrics"] {
    let values = response[kind].as_array().unwrap();
    assert_eq!(values.len(), DISCOVERY_LIMIT);
    assert_eq!(values[0]["run_count"], 2);
    assert!(values[1..].iter().all(|column| column["run_count"] == 1));
    assert!(
      values
        .iter()
        .all(|column| !column["key"].as_str().unwrap().contains("later_omitted"))
    );
  }
  assert_eq!(response["sampled_run_count"], 2);
  assert_eq!(response["truncated"], true);
}

#[test]
fn discovery_ties_sort_by_label_then_unambiguous_pointer() {
  let mut columns = Columns::default();
  columns.add(&ScalarData {
    params: Some(json!({"z":1,"a / b":2,"a":{"b":3}})),
    metrics: BTreeMap::new(),
  });
  let keys: Vec<_> = columns.response()["params"]
    .as_array()
    .unwrap()
    .iter()
    .map(|column| column["key"].as_str().unwrap().to_owned())
    .collect();
  assert_eq!(keys, ["/a ~1 b", "/a/b", "/z"]);
}

#[test]
fn validation_rejects_duplicates_unknown_sort_invalid_pointers_and_over_limit_selection() {
  for params in [
    vec!["/a".into(); 9],
    vec!["/a".into(); 2],
    vec!["a".into()],
    vec!["/a~2b".into()],
    vec![format!("/{}", "x".repeat(256))],
  ] {
    assert!(TableOptions::parse(params, vec![], "last", None, None).is_err());
  }
  for (reduction, sort, direction) in [
    ("mean", "run_id", "asc"),
    ("last", "param:/missing", "asc"),
    ("last", "task", "asc"),
    ("last", "run_id", "up"),
  ] {
    assert!(TableOptions::parse(vec![], vec![], reduction, Some(sort), Some(direction)).is_err());
  }
}

#[test]
fn long_scalar_previews_are_explicit_and_do_not_change_full_text_sort_order() {
  let prefix = "\n雪".repeat(300);
  let mut rows = vec![
    row("a", json!(format!("{prefix}z"))),
    row("z", json!(format!("{prefix}a"))),
  ];
  let options = options("param:/value", "asc");
  options.sort_rows(&mut rows);
  assert_eq!(rows[0].run["run_id"], "z");
  let row = options.project(rows.remove(0));
  assert_eq!(row["table_values_truncated"], true);
  assert!(
    row["table_values"]["params"]["/value"]
      .as_str()
      .unwrap()
      .ends_with('…')
  );
  assert!(
    serde_json::to_vec(&row["table_values"]["params"]["/value"])
      .unwrap()
      .len()
      <= STRING_LIMIT
  );
  assert_eq!(row["table_values"]["metrics"]["loss"], Value::Null);
}

#[test]
fn full_selected_column_page_obeys_the_json_response_budget() {
  let keys: Vec<_> = (0..8)
    .map(|index| format!("/{index}{}", "\"".repeat(125)))
    .collect();
  let options =
    TableOptions::parse(keys.clone(), vec![], "last", Some("run_id"), Some("asc")).unwrap();
  let params = keys
    .iter()
    .map(|key| (key[1..].to_string(), json!("\n雪".repeat(1000))))
    .collect();
  let data = ScalarData {
    params: Some(Value::Object(params)),
    metrics: BTreeMap::new(),
  };
  let rows: Vec<_> = (0..100).map(|index|options.project(TableRow::new(json!({"run_id":format!("run-{index}"),"task":"train","status":"completed","started_at":"2026-10-09T00:00:00Z"}),data.clone()))).collect();
  assert!(serde_json::to_vec(&json!({"runs":rows})).unwrap().len() < 512 * 1024);
  assert!(
    TableOptions::parse(
      vec![format!("/{}", "\"".repeat(130))],
      vec![],
      "last",
      None,
      None
    )
    .is_err()
  );
}

#[test]
fn queued_submission_sorting_keeps_native_start_times_and_unknown_records_honest() {
  let mut rows = vec![
    row("missing", json!(1)),
    row("native", json!(1)),
    row("pending", json!(1)),
    row("legacy", json!(1)),
  ];
  rows[1].run["started_at"] = json!("2026-10-14T01:00:00Z");
  rows[2].run["queue_only"] = json!(true);
  rows[2].run["started_at"] = Value::Null;
  rows[2].run["submitted_at"] = json!("2026-10-14T02:00:00Z");
  rows[3].run["submitted_at"] = json!("2026-10-14T03:00:00Z");
  options("started_at", "desc").sort_rows(&mut rows);
  assert_eq!(
    rows
      .iter()
      .map(|row| row.run["run_id"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["pending", "native", "legacy", "missing"]
  );
  assert!(rows[0].run["started_at"].is_null());
  options("started_at", "asc").sort_rows(&mut rows);
  assert_eq!(
    rows
      .iter()
      .map(|row| row.run["run_id"].as_str().unwrap())
      .collect::<Vec<_>>(),
    ["native", "pending", "legacy", "missing"]
  );
  assert!(
    rows[1].run["started_at"].is_null(),
    "submission sorting never invents a worker run start"
  );
}
