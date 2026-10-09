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
    json!([{"key":"/a~1b/~0name","label":"a/b / ~name"},{"key":"/null","label":"null"}])
  );
  assert_eq!(
    columns.response()["metrics"],
    json!([{"key":"loss","label":"loss"}])
  );
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
