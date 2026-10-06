use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;

use super::*;
use crate::service::storage::{CompletedPart, ObjectMetadata};
use crate::service::types::{Request, Response as ApiResponse};

#[derive(Clone)]
struct Object {
  bytes: Arc<Vec<u8>>,
  size: u64,
}

#[derive(Default)]
struct Objects {
  files: BTreeMap<String, Object>,
  requests: Vec<(String, Option<String>)>,
  next: usize,
  invalid_range: bool,
}

#[derive(Clone)]
struct Backend {
  url: String,
  objects: Arc<Mutex<Objects>>,
}

impl ObjectStorage for Backend {
  fn begin_upload(&self, key: &str, _: &str) -> Result<String> {
    Ok(key.into())
  }
  fn presign_part(&self, _: &str, _: &str, _: u32, _: u32) -> Result<String> {
    unreachable!()
  }
  fn complete_upload(&self, key: &str, _: &str, _: &[CompletedPart]) -> Result<ObjectMetadata> {
    self
      .head(key)?
      .ok_or_else(|| message("test object is missing"))
  }
  fn head(&self, key: &str) -> Result<Option<ObjectMetadata>> {
    Ok(
      self
        .objects
        .lock()
        .unwrap()
        .files
        .get(key)
        .map(|object| ObjectMetadata { size: object.size }),
    )
  }
  fn presign_get(&self, key: &str, _: u32) -> Result<String> {
    Ok(format!("{}/{key}?signature=private-test-value", self.url))
  }
}

struct Fixture {
  _directory: tempfile::TempDir,
  store: Store<Backend>,
  backend: Backend,
  stop: Arc<AtomicBool>,
  thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
  fn new() -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let backend = Backend {
      url: format!("http://{address}"),
      objects: Arc::default(),
    };
    let thread_backend = backend.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::spawn(move || {
      for connection in listener.incoming() {
        let mut connection = connection.unwrap();
        if thread_stop.load(Ordering::SeqCst) {
          break;
        }
        connection
          .set_read_timeout(Some(Duration::from_secs(2)))
          .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
          let mut byte = [0];
          if connection.read(&mut byte).unwrap_or(0) == 0 {
            break;
          }
          request.push(byte[0]);
        }
        let request = String::from_utf8_lossy(&request);
        let path = request
          .lines()
          .next()
          .unwrap()
          .split_whitespace()
          .nth(1)
          .unwrap()
          .split('?')
          .next()
          .unwrap()
          .trim_start_matches('/');
        let range = request
          .lines()
          .filter_map(|line| line.split_once(':'))
          .find(|(name, _)| name.eq_ignore_ascii_case("range"))
          .map(|(_, value)| value.trim().to_string());
        let (object, invalid_range) = {
          let mut objects = thread_backend.objects.lock().unwrap();
          objects.requests.push((path.into(), range.clone()));
          (
            objects.files.get(path).unwrap().clone(),
            objects.invalid_range,
          )
        };
        if let Some(range) = range {
          let (start, end) = range
            .strip_prefix("bytes=")
            .unwrap()
            .split_once('-')
            .unwrap();
          let start: usize = start.parse().unwrap();
          let end: usize = end.parse().unwrap();
          let bytes = &object.bytes[start..=end];
          let offset = usize::from(invalid_range);
          write!(connection, "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n", bytes.len(), start + offset, end, object.size).unwrap();
          connection.write_all(bytes).unwrap();
        } else {
          write!(
            connection,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            object.size
          )
          .unwrap();
          connection.write_all(&object.bytes).unwrap();
        }
      }
    });
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), backend.clone()).unwrap();
    Self {
      _directory: directory,
      store,
      backend,
      stop,
      thread: Some(thread),
    }
  }

  fn scope(run_id: &str) -> RunScope {
    RunScope {
      project_id: "project".into(),
      origin: "worker".into(),
      run_id: run_id.into(),
    }
  }

  fn publish(&self, target: FileTarget, bytes: Vec<u8>) {
    let size = bytes.len() as u64;
    self.publish_size(target, bytes, size);
  }

  fn publish_size(&self, target: FileTarget, bytes: Vec<u8>, size: u64) {
    let upload_id = {
      let mut objects = self.backend.objects.lock().unwrap();
      objects.next += 1;
      format!("upload-{}", objects.next)
    };
    let response = self
      .store
      .execute(Request::BeginUpload {
        upload_id: upload_id.clone(),
        target: target.clone(),
        size,
        sha256: hex_digest(&Sha256::digest(&bytes)),
      })
      .unwrap();
    let ApiResponse::Upload { upload } = response else {
      panic!("unexpected upload reply");
    };
    let key = match target {
      FileTarget::Run { scope, .. } => format!(
        "projects/{}/runs/{}/{}/objects/{upload_id}",
        scope.project_id, scope.origin, scope.run_id
      ),
      FileTarget::Input {
        project_id,
        input_id,
      } => format!("projects/{project_id}/inputs/{input_id}/objects/{upload_id}"),
    };
    self.backend.objects.lock().unwrap().files.insert(
      key,
      Object {
        bytes: Arc::new(bytes),
        size,
      },
    );
    for number in 1..=size.div_ceil(upload.part_size).max(1) as u32 {
      self
        .store
        .execute(Request::RecordPart {
          upload_id: upload_id.clone(),
          part: CompletedPart {
            part_number: number,
            etag: "opaque".into(),
          },
        })
        .unwrap();
    }
    self
      .store
      .execute(Request::CompleteUpload { upload_id })
      .unwrap();
  }

  fn run_file(&self, run_id: &str, path: &str, bytes: Vec<u8>) {
    self.publish(
      FileTarget::Run {
        scope: Self::scope(run_id),
        path: path.into(),
      },
      bytes,
    );
  }

  fn state(&self, run_id: &str, status: &str) {
    self.run_file(run_id, "run-state.json", serde_json::to_vec(&json!({"schema_version": 1, "run_id": run_id, "task": "train", "status": status, "started_at": "2026-10-03T01:00:00Z", "finished_at": "2026-10-03T02:00:00Z", "exit_code": 0})).unwrap());
  }

  fn append(&self, run_id: &str, path: &str, bytes: &[u8]) {
    for (index, data) in bytes.chunks(STREAM_BATCH).enumerate() {
      self
        .store
        .execute(Request::AppendStream {
          scope: Self::scope(run_id),
          path: path.into(),
          offset: (index * STREAM_BATCH) as u64,
          data_base64: base64::engine::general_purpose::STANDARD.encode(data),
        })
        .unwrap();
    }
  }
}

impl Drop for Fixture {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::SeqCst);
    let address = self.backend.url.strip_prefix("http://").unwrap();
    let _ = TcpStream::connect(address);
    self.thread.take().unwrap().join().unwrap();
  }
}

#[test]
fn hosted_update_probes_skip_storage_reads_and_separate_changed_resources() {
  let fixture = Fixture::new();
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let empty = dashboard.updates("", &[]).unwrap();
  assert_eq!(
    empty,
    json!({"catalog_revision":"0","source_revision":null,"runs":[]})
  );
  fixture.publish(
    FileTarget::Input {
      project_id: "private-project".into(),
      input_id: "dataset".into(),
    },
    b"private input".to_vec(),
  );
  assert_eq!(dashboard.updates("", &[]).unwrap(), empty);

  // Both invalid state JSON and oversized scalar data remain cheap hints;
  // these files would fail a detail read and must never be parsed by a poll.
  fixture.run_file("run-one", "run-state.json", b"{invalid".to_vec());
  fixture.append("run-one", "outputs/metrics.jsonl", b"not metric JSON");
  fixture.append("run-one", "logs/stdout.log", b"first log\n");
  fixture.run_file("other-run", "outputs/model.pt", b"checkpoint".to_vec());
  fixture.publish_size(
    FileTarget::Run {
      scope: Fixture::scope("other-run"),
      path: "outputs/metrics.jsonl".into(),
    },
    b"invalid oversized metrics".to_vec(),
    METRICS_LIMIT + 1,
  );
  let ids = vec!["run-one".into(), "removed".into(), "other-run".into()];
  let first = dashboard.updates("hosted:project:worker", &ids).unwrap();
  assert_eq!(first["runs"][0]["missing"], false);
  assert_eq!(first["runs"][0]["metrics_revision"], "stream:15");
  assert_eq!(first["runs"][1]["missing"], true);
  assert!(first["runs"][2]["metrics_revision"].is_string());
  assert_eq!(
    dashboard.updates("hosted:project:worker", &ids).unwrap(),
    first
  );

  fixture.append(
    "run-one",
    "outputs/metrics.jsonl",
    b"not metric JSON plus new data",
  );
  let growth = dashboard.updates("hosted:project:worker", &ids).unwrap();
  assert_ne!(growth["source_revision"], first["source_revision"]);
  assert_ne!(growth["catalog_revision"], first["catalog_revision"]);
  assert_ne!(
    growth["runs"][0]["metrics_revision"],
    first["runs"][0]["metrics_revision"]
  );
  assert_eq!(
    growth["runs"][0]["metadata_revision"],
    first["runs"][0]["metadata_revision"]
  );
  assert_eq!(
    growth["runs"][0]["stdout_revision"],
    first["runs"][0]["stdout_revision"]
  );
  fixture.append(
    "run-one",
    "outputs/metrics.jsonl",
    b"not metric JSON plus new data",
  );
  assert_eq!(
    dashboard.updates("hosted:project:worker", &ids).unwrap(),
    growth
  );

  fixture.run_file("run-one", "run-state.json", b"{changed".to_vec());
  let state = dashboard.updates("hosted:project:worker", &ids).unwrap();
  assert_ne!(
    state["runs"][0]["metadata_revision"],
    growth["runs"][0]["metadata_revision"]
  );
  assert_eq!(
    state["runs"][0]["metrics_revision"],
    growth["runs"][0]["metrics_revision"]
  );
  fixture.run_file(
    "run-one",
    "outputs/metrics.jsonl",
    b"not metric JSON plus new data".to_vec(),
  );
  let finalized = dashboard.updates("hosted:project:worker", &ids).unwrap();
  assert!(
    finalized["runs"][0]["metrics_revision"]
      .as_str()
      .unwrap()
      .starts_with("object:")
  );
  assert_ne!(
    finalized["runs"][0]["metrics_revision"],
    state["runs"][0]["metrics_revision"]
  );
  assert!(fixture.backend.objects.lock().unwrap().requests.is_empty());
  assert!(serde_json::to_vec(&finalized).unwrap().len() < 4096);
  assert!(dashboard.updates("", &ids).is_err());
  assert!(
    dashboard
      .updates("hosted:project:worker", &["../outside".into()])
      .is_err()
  );

  fixture.run_file("-service-run", "snapshot.json", b"invalid JSON".to_vec());
  let identifier = dashboard
    .updates("hosted:project:worker", &["-service-run".into()])
    .unwrap();
  assert_eq!(identifier["runs"][0]["missing"], false);
  assert!(fixture.backend.objects.lock().unwrap().requests.is_empty());
}

#[test]
fn hosted_catalog_filters_cached_overviews_and_refreshes_changed_states() {
  let fixture = Fixture::new();
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  assert_eq!(dashboard.catalog().unwrap()["initial_source"], "");
  fixture.state("run-one", "running");
  fixture.state("run-two", "completed");
  fixture.publish(
    FileTarget::Input {
      project_id: "private-project".into(),
      input_id: "dataset".into(),
    },
    b"private".to_vec(),
  );
  fixture.publish_size(
    FileTarget::Run {
      scope: Fixture::scope("run-one"),
      path: "outputs/checkpoint.pt".into(),
    },
    Vec::new(),
    METRICS_LIMIT + 1,
  );
  let catalog = dashboard.catalog().unwrap();
  assert_eq!(catalog["access_mode"], "hosted");
  assert_eq!(catalog["sources"].as_array().unwrap().len(), 1);
  assert_eq!(catalog["sources"][0]["kind"], "service");
  let rows = dashboard
    .list(
      "hosted:project:worker",
      None,
      Some("train"),
      Some("running"),
      20,
      0,
    )
    .unwrap();
  assert_eq!(rows["total_count"], 1);
  assert_eq!(rows["runs"][0]["run_id"], "run-one");
  assert_eq!(fixture.backend.objects.lock().unwrap().requests.len(), 2);
  dashboard
    .list("hosted:project:worker", None, None, None, 1, 1)
    .unwrap();
  assert_eq!(fixture.backend.objects.lock().unwrap().requests.len(), 2);
  fixture.state("run-one", "failed");
  let rows = dashboard
    .list("hosted:project:worker", None, None, Some("failed"), 20, 0)
    .unwrap();
  assert_eq!(rows["total_count"], 1);
  assert_eq!(fixture.backend.objects.lock().unwrap().requests.len(), 3);
  assert!(
    dashboard
      .list("hosted:unknown:worker", None, None, None, 20, 0)
      .is_err()
  );
  assert!(
    fixture
      .backend
      .objects
      .lock()
      .unwrap()
      .requests
      .iter()
      .all(|(path, _)| !path.contains("inputs"))
  );
}

#[test]
fn live_metrics_logs_compare_and_chart_use_bounded_saved_streams() {
  let fixture = Fixture::new();
  fixture.state("run-one", "running");
  fixture.state("run-two", "completed");
  fixture.run_file(
    "run-one",
    "outputs/params.json",
    br#"{"schema_version":1,"params":{"lr":0.001}}"#.to_vec(),
  );
  fixture.append(
    "run-one",
    "outputs/metrics.jsonl",
    b"{\"step\":0,\"metrics\":{\"loss\":2}}\n{\"step\":1,\"metrics\":{\"loss\":1}}\n",
  );
  fixture.run_file(
    "run-two",
    "outputs/metrics.jsonl",
    b"{\"step\":0,\"metrics\":{\"loss\":0.5}}\n".to_vec(),
  );
  let mut logs = b"discarded first line\n".repeat(5000);
  logs.extend_from_slice(b"last line\n");
  fixture.append("run-one", "logs/stdout.log", &logs);
  fixture.run_file("run-two", "logs/stderr.log", logs);
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let detail = dashboard
    .detail("hosted:project:worker", "run-one")
    .unwrap();
  assert_eq!(detail["params"]["lr"], 0.001);
  assert_eq!(detail["metrics"]["loss"]["count"], 2);
  assert_eq!(detail["metrics"]["loss"]["last"]["value"], 1.0);
  for (run_id, stream) in [("run-one", "stdout"), ("run-two", "stderr")] {
    let log = dashboard
      .log("hosted:project:worker", run_id, stream, 1)
      .unwrap();
    assert_eq!(log["content"], "last line\n");
    assert_eq!(log["truncated"], true);
  }
  assert!(
    fixture
      .backend
      .objects
      .lock()
      .unwrap()
      .requests
      .iter()
      .any(|(_, range)| range.as_deref().is_some_and(|range| {
        let Some((start, end)) = range
          .strip_prefix("bytes=")
          .and_then(|range| range.split_once('-'))
        else {
          return false;
        };
        let start: usize = start.parse().unwrap();
        let end: usize = end.parse().unwrap();
        start > 0 && end - start + 1 == STREAM_BATCH
      }))
  );
  let ids = ["run-one".into(), "run-two".into()];
  let comparison = dashboard
    .compare(
      "hosted:project:worker",
      &ids,
      &["loss".into()],
      Reduction::Last,
    )
    .unwrap();
  assert_eq!(
    comparison["comparison"]["runs"][0]["values"]["loss"]["value"],
    1.0
  );
  assert_eq!(
    comparison["comparison"]["runs"][1]["values"]["loss"]["value"],
    0.5
  );
  assert!(
    dashboard
      .chart(
        "hosted:project:worker",
        &ids,
        &["loss".into()],
        ChartXAxis::Step
      )
      .unwrap()
      .contains("loss")
  );
}

#[test]
fn oversized_metrics_and_invalid_ranges_do_not_expose_partial_or_signed_data() {
  let fixture = Fixture::new();
  fixture.state("run-one", "completed");
  fixture.publish_size(
    FileTarget::Run {
      scope: Fixture::scope("run-one"),
      path: "outputs/metrics.jsonl".into(),
    },
    Vec::new(),
    METRICS_LIMIT + 1,
  );
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let detail = dashboard
    .detail("hosted:project:worker", "run-one")
    .unwrap();
  assert!(detail["metrics_error"].as_str().unwrap().contains("16 MiB"));
  assert_eq!(fixture.backend.objects.lock().unwrap().requests.len(), 1);
  fixture.backend.objects.lock().unwrap().invalid_range = true;
  let detail = dashboard
    .detail("hosted:project:worker", "run-one")
    .unwrap();
  let text = detail.to_string();
  assert!(text.contains("invalid byte range"));
  assert!(!text.contains("private-test-value"));
  assert!(!text.contains(&fixture.backend.url));
  assert_eq!(detail["run"]["status"], "unknown");
}

#[test]
fn metadata_deadlines_and_full_metric_integrity_failures_are_reported_honestly() {
  let fixture = Fixture::new();
  fixture.state("run-one", "completed");
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let report = dashboard
    .overview(
      &Fixture::scope("run-one"),
      Instant::now() - Duration::from_secs(1),
    )
    .unwrap();
  assert_eq!(report["run"]["status"], "unknown");
  assert!(report.to_string().contains("time budget"));
  assert!(report.to_string().contains("Filters may omit"));
  assert!(fixture.backend.objects.lock().unwrap().requests.is_empty());
  fixture.run_file(
    "run-one",
    "outputs/metrics.jsonl",
    b"{\"step\":0,\"metrics\":{\"loss\":0.5}}\n".to_vec(),
  );
  {
    let mut objects = fixture.backend.objects.lock().unwrap();
    let object = objects
      .files
      .values_mut()
      .find(|object| object.bytes.starts_with(b"{\"step\""))
      .unwrap();
    object.bytes = Arc::new(b"{\"step\":0,\"metrics\":{\"loss\":0.6}}\n".to_vec());
  }
  let detail = dashboard
    .detail("hosted:project:worker", "run-one")
    .unwrap();
  assert!(
    detail["metrics_error"]
      .as_str()
      .unwrap()
      .contains("integrity")
  );
  assert_eq!(detail["metric_count"], 0);
  assert!(detail["metrics"].as_object().unwrap().is_empty());
}

#[test]
fn scalar_comparisons_skip_broken_parameters_and_charts_report_them_as_notes() {
  let fixture = Fixture::new();
  for run_id in ["run-one", "run-two"] {
    fixture.state(run_id, "completed");
    fixture.run_file(run_id, "outputs/params.json", b"broken params".to_vec());
    fixture.append(
      run_id,
      "outputs/metrics.jsonl",
      b"{\"step\":0,\"metrics\":{\"loss\":0.5}}\n",
    );
  }
  let parameter_keys: BTreeSet<_> = fixture
    .backend
    .objects
    .lock()
    .unwrap()
    .files
    .iter()
    .filter(|(_, object)| object.bytes.as_slice() == b"broken params")
    .map(|(key, _)| key.clone())
    .collect();
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let ids = ["run-one".into(), "run-two".into()];
  let comparison = dashboard
    .compare(
      "hosted:project:worker",
      &ids,
      &["loss".into()],
      Reduction::Last,
    )
    .unwrap();
  assert_eq!(
    comparison["comparison"]["runs"][0]["values"]["loss"]["value"],
    0.5
  );
  assert!(
    fixture
      .backend
      .objects
      .lock()
      .unwrap()
      .requests
      .iter()
      .all(|(key, _)| !parameter_keys.contains(key))
  );
  let chart = dashboard
    .chart("hosted:project:worker", &ids, &[], ChartXAxis::Step)
    .unwrap();
  assert!(chart.contains("contains invalid JSON"));
}

#[test]
fn hosted_time_charts_keep_filtered_origin_and_exact_timestamp_coordinates() {
  let fixture = Fixture::new();
  let events = [
    json!({"step":20,"timestamp":"2026-10-03T01:00:00Z","metrics":{"setup":1}}),
    json!({"step":2,"timestamp":"2026-10-03T01:00:02.000000001Z","metrics":{"loss":0.5}}),
    json!({"step":2,"metrics":{"loss":0.25}}),
    json!({"step":0,"timestamp":"2026-10-03T01:00:05.000000002Z","metrics":{"loss":0.125}}),
  ]
  .map(|row| row.to_string())
  .join("\n")
    + "\n";
  for run_id in ["run-object", "run-stream"] {
    fixture.state(run_id, "completed");
    if run_id == "run-object" {
      fixture.run_file(run_id, "outputs/metrics.jsonl", events.as_bytes().to_vec());
    } else {
      fixture.append(run_id, "outputs/metrics.jsonl", events.as_bytes());
    }
  }
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  for run_id in ["run-object", "run-stream"] {
    let ids = [run_id.into()];
    let step = dashboard
      .chart(
        "hosted:project:worker",
        &ids,
        &["loss".into()],
        ChartXAxis::Step,
      )
      .unwrap();
    assert_eq!(step.matches("<circle class=point ").count(), 3);
    let elapsed = dashboard
      .chart(
        "hosted:project:worker",
        &ids,
        &["loss".into()],
        ChartXAxis::Elapsed,
      )
      .unwrap();
    assert!(elapsed.contains("data-x-value=\"2000000001\""));
    assert!(elapsed.contains("data-x-value=\"5000000002\""));
    assert!(elapsed.contains("1 of 3 samples omitted"));
    assert_eq!(elapsed.matches("<circle class=point ").count(), 2);
    let wall_clock = dashboard
      .chart(
        "hosted:project:worker",
        &ids,
        &["loss".into()],
        ChartXAxis::WallClock,
      )
      .unwrap();
    assert!(wall_clock.contains("data-x-axis=\"wall_clock\""));
    assert!(wall_clock.contains("data-timestamp=\"2026-10-03T01:00:02.000000001Z\""));
  }
}

#[test]
fn hosted_time_preview_thins_points_without_losing_timestamp_omission_totals() {
  let fixture = Fixture::new();
  fixture.state("run-one", "completed");
  let mut events =
    json!({"step":0,"timestamp":"2026-10-03T01:00:00Z","metrics":{"setup":1}}).to_string() + "\n";
  for index in 0..1300 {
    let mut row = json!({"step":index,"metrics":{"loss":1.0 / (index + 1) as f64}});
    if index % 2 == 0 {
      row["timestamp"] = json!(format!("2026-10-03T01:00:02.{index:09}Z"));
    }
    events.push_str(&row.to_string());
    events.push('\n');
  }
  fixture.run_file("run-one", "outputs/metrics.jsonl", events.into_bytes());
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let html = dashboard
    .chart(
      "hosted:project:worker",
      &["run-one".into()],
      &["loss".into()],
      ChartXAxis::Elapsed,
    )
    .unwrap();
  assert!(html.contains("650 of 1300 samples omitted"));
  assert!(html.contains("data-x-min=\"2000000000\""));
  assert!(html.matches("<circle class=point ").count() <= 600);
  assert!(html.contains("data-timestamp=\"2026-10-03T01:00:02.000001298Z\""));
}

#[test]
fn exact_task_filters_use_full_bounded_names_and_explicitly_report_longer_names() {
  let fixture = Fixture::new();
  let prefix = "x".repeat(600);
  let left = format!("{prefix}-left");
  let right = format!("{prefix}-right");
  for (run_id, task) in [
    ("run-one", left.as_str()),
    ("run-two", right.as_str()),
    ("run-three", &"x".repeat(5000)),
  ] {
    fixture.run_file(run_id, "run-state.json", serde_json::to_vec(&json!({"schema_version":1,"run_id":run_id,"task":task,"status":"running","started_at":"2026-10-03T01:00:00Z"})).unwrap());
  }
  let dashboard = HostedDashboard::new(&fixture.store).unwrap();
  let rows = dashboard
    .list("hosted:project:worker", None, Some(&left), None, 20, 0)
    .unwrap();
  assert_eq!(rows["total_count"], 1);
  assert_eq!(rows["runs"][0]["run_id"], "run-one");
  assert_eq!(rows["metadata_truncated"], true);
  assert!(
    rows["warnings"]
      .to_string()
      .contains("4 KiB hosted filter limit")
  );
  let rows = dashboard
    .list("hosted:project:worker", Some("-right"), None, None, 20, 0)
    .unwrap();
  assert_eq!(rows["total_count"], 1);
  assert_eq!(rows["runs"][0]["run_id"], "run-two");
  let rows = dashboard
    .list(
      "hosted:project:worker",
      None,
      Some(&"x".repeat(5000)),
      None,
      20,
      0,
    )
    .unwrap();
  assert_eq!(rows["total_count"], 0);
  assert!(
    rows["warnings"]
      .to_string()
      .contains("task filters and searches omit it")
  );
}
