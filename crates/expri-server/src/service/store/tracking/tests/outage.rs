use std::sync::{
  Arc,
  atomic::{AtomicUsize, Ordering},
};

use super::*;
use crate::metric_charts::ChartXAxis;
use crate::service::dashboard_data::HostedDashboard;
use crate::service::storage::ObjectMetadata;

#[derive(Clone, Default)]
struct UnavailableStorage(Arc<AtomicUsize>);

impl UnavailableStorage {
  fn unavailable<T>(&self) -> crate::error::Result<T> {
    self.0.fetch_add(1, Ordering::SeqCst);
    Err(crate::error::ExpriError::Message("S3 unavailable".into()))
  }
}

impl ObjectStorage for UnavailableStorage {
  fn begin_upload(&self, _: &str, _: &str) -> crate::error::Result<String> {
    self.unavailable()
  }
  fn presign_part(&self, _: &str, _: &str, _: u32, _: u32) -> crate::error::Result<String> {
    self.unavailable()
  }
  fn complete_upload(
    &self,
    _: &str,
    _: &str,
    _: &[CompletedPart],
  ) -> crate::error::Result<ObjectMetadata> {
    self.unavailable()
  }
  fn head(&self, _: &str) -> crate::error::Result<Option<ObjectMetadata>> {
    self.unavailable()
  }
  fn presign_get(&self, _: &str, _: u32) -> crate::error::Result<String> {
    self.unavailable()
  }
}

#[test]
fn tracking_and_hosted_review_survive_s3_outage_and_restart() {
  let dir = tempfile::tempdir().unwrap();
  let storage = UnavailableStorage::default();
  let store = Store::open(dir.path(), storage.clone()).unwrap();
  put(
    &store,
    "run-state.json",
    1,
    br#"{"schema_version":1,"run_id":"run-1","task":"train","status":"completed","started_at":"2026-10-09T00:00:00Z","finished_at":"2026-10-09T00:01:00Z","exit_code":0}"#,
  );
  put(
    &store,
    "snapshot.json",
    1,
    br#"{"schema_version":1,"run_id":"run-1","source":{"kind":"git","git_head":"original"},"files":[]}"#,
  );
  put(&store, "outputs/params.json", 1, br#"{"seed":42}"#);
  let raw_metrics = concat!(
    "{\"step\":9,\"metrics\":{\"loss\":4}}\n",
    "{\"step\":10,\"metrics\":{\"loss\":2}}\n"
  );
  append(&store, "outputs/metrics.jsonl", 0, raw_metrics.as_bytes()).unwrap();
  append(&store, "logs/stdout.log", 0, b"first\nlast\n").unwrap();

  let assert_available = |store: &Store<UnavailableStorage>| {
    let dashboard = HostedDashboard::new(store).unwrap();
    assert_eq!(
      dashboard.catalog().unwrap()["sources"]
        .as_array()
        .unwrap()
        .len(),
      1
    );
    let detail = dashboard.detail("hosted:project:worker", "run-1").unwrap();
    assert_eq!(detail["run"]["status"], "completed");
    assert_eq!(detail["snapshot"]["source"]["git_head"], "original");
    assert_eq!(detail["params"]["seed"], 42);
    assert_eq!(detail["metrics"]["loss"]["count"], 2);
    assert_eq!(detail["metrics"]["loss"]["last"]["value"], 2.0);
    assert!(detail["metrics_error"].is_null());
    let chart = dashboard
      .chart(
        "hosted:project:worker",
        &["run-1".into()],
        &["loss".into()],
        ChartXAxis::Step,
      )
      .unwrap();
    assert!(chart.contains("<svg") && chart.contains("loss"));
    assert_eq!(
      dashboard
        .log("hosted:project:worker", "run-1", "stdout", 1)
        .unwrap()["content"],
      "last\n"
    );
    let Response::Stream {
      total_size,
      data_base64,
      ..
    } = store
      .execute(Request::ReadStream {
        scope: scope(),
        path: "logs/stdout.log".into(),
        offset: 0,
        limit: 64,
      })
      .unwrap()
    else {
      panic!("tracking log response");
    };
    assert_eq!(total_size, 11);
    assert_eq!(
      base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .unwrap(),
      b"first\nlast\n"
    );
    let raw = store.file(&run_target("outputs/metrics.jsonl")).unwrap();
    assert_eq!(
      store.tracking_range(&raw, 0, raw_metrics.len()).unwrap(),
      raw_metrics.as_bytes()
    );
    assert_eq!(metrics(store, true).metrics["loss"].points.len(), 2);
    let Response::Files { files } = store.list_files(&scope()).unwrap() else {
      panic!("tracking catalog response");
    };
    assert_eq!(files.len(), 5);
    assert!(
      files
        .iter()
        .all(|file| matches!(file.storage, FileStorage::Tracking { .. }))
    );
  };

  assert_available(&store);
  assert_eq!(storage.0.load(Ordering::SeqCst), 0);
  let Response::Archive { archive } = seal(&store, false).unwrap() else {
    panic!("archive queued response");
  };
  assert_eq!(archive.status, "pending");
  assert_eq!(
    storage.0.load(Ordering::SeqCst),
    0,
    "sealing performs no S3 I/O"
  );
  assert!(store.archive_cycle().is_err());
  let calls_after_failure = storage.0.load(Ordering::SeqCst);
  assert!(calls_after_failure > 0);
  let Response::Archive { archive } = store
    .execute(Request::ArchiveStatus { scope: scope() })
    .unwrap()
  else {
    panic!("archive failure response");
  };
  assert_eq!(archive.status, "failed");
  assert!(archive.file.is_none());
  assert!(
    archive
      .last_error
      .unwrap()
      .contains("stored tracking data is retained")
  );
  assert_available(&store);
  assert_eq!(storage.0.load(Ordering::SeqCst), calls_after_failure);
  drop(store);

  let store = Store::open(dir.path(), storage.clone()).unwrap();
  assert_available(&store);
  let Response::Archive { archive } = store
    .execute(Request::ArchiveStatus { scope: scope() })
    .unwrap()
  else {
    panic!("recovered archive response");
  };
  assert_eq!(archive.status, "failed");
  assert_eq!(storage.0.load(Ordering::SeqCst), calls_after_failure);
}
