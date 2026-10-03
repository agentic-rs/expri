#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::{ProtocolPreference, protocol_with_preference, query_runs_with_preference};
use crate::config::{TargetConfig, TransportKind};
use crate::controller::transport::Remote;
use crate::protocol::RunQueryRequest;
use crate::shell;

#[test]
fn run_queries_use_capabilities_keep_profile_banners_off_json_and_accept_legacy_records() {
  let fixture = tempfile::Builder::new()
    .prefix("expri query with spaces ")
    .tempdir()
    .unwrap();
  let home = fixture.path().join("login-home");
  let repo = fixture.path().join("repo 'quoted'");
  fs::create_dir_all(&home).unwrap();
  fs::create_dir_all(repo.join(".expri/runs/run-old")).unwrap();
  fs::write(
    home.join(".profile"),
    "echo login-banner\nexport EXPRI_PROFILE_LOADED=yes\n",
  )
  .unwrap();
  fs::write(repo.join(".expri/runs/run-old/run-state.json"), r#"{"run_id":"run-old","task":"train","status":"completed","started_at":"2026-10-02T00:00:00Z","exit_code":0}"#).unwrap();
  let ctl = fixture.path().join("fake-ctl");
  fs::write(&ctl, format!("#!/bin/sh\nHOME={}; export HOME\nfor argument do remote_command=$argument; done\nexec /bin/sh -c \"$remote_command\"\n", shell::quote(home.to_string_lossy()))).unwrap();
  fs::set_permissions(&ctl, fs::Permissions::from_mode(0o755)).unwrap();
  let node = repo.join("node-stub");
  fs::write(&node, r#"#!/bin/sh
[ "$EXPRI_PROFILE_LOADED" = yes ] || exit 71
if [ "$2" = capabilities ]; then [ "$4" = run-records-v1 ]; exit $?; fi
[ "$2" = runs ] && [ "$3" = --request-stdin ] || exit 72
python3 -c 'import json,sys; request=json.load(sys.stdin); print(json.dumps({"native":True,"request":request}))'
"#).unwrap();
  fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
  let remote = Remote::new(
    TargetConfig {
      host: "test-host".to_string(),
      remote_dir: repo.to_string_lossy().into_owned(),
      transport: TransportKind::Ctl,
      port: None,
      protocol: None,
      node_bin: None,
      ctl_bin: Some(ctl.to_string_lossy().into_owned()),
      ctl_method: None,
      environment: None,
    },
    "/tmp/unused-control".to_string(),
    "10m".to_string(),
    false,
    0,
    true,
  )
  .unwrap();
  let request = RunQueryRequest::List {
    task: None,
    status: None,
    limit: Some(20),
  };
  let native =
    query_runs_with_preference(&remote, &request, ProtocolPreference::Auto, "./node-stub").unwrap();
  assert_eq!(native["native"], true);
  assert_eq!(native["request"], serde_json::to_value(&request).unwrap());
  let python =
    query_runs_with_preference(&remote, &request, ProtocolPreference::Python, "./node-stub")
      .unwrap();
  assert_eq!(python["runs"][0]["run_id"], "run-old");
  assert_eq!(python["runs"][0]["schema_version"], 0);
  fs::create_dir_all(repo.join(".expri/runs/run-old/outputs")).unwrap();
  fs::write(
    repo.join(".expri/runs/run-old/outputs/metrics.jsonl"),
    b"{\"step\":0,\"metrics\":{\"loss\":1}}\n",
  )
  .unwrap();
  fs::write(repo.join(".expri/runs/run-old/outputs/params.json"), b"{}").unwrap();
  let metric_request = RunQueryRequest::Files {
    run_id: "run-old".to_string(),
    artifacts: Vec::new(),
    metrics: true,
  };
  // A node with run-records-v1 must not silently ignore the new file-selection flag.
  let metric_fallback = query_runs_with_preference(
    &remote,
    &metric_request,
    ProtocolPreference::Auto,
    "./node-stub",
  )
  .unwrap();
  assert_eq!(
    metric_fallback["files"],
    serde_json::json!([
      "outputs/metrics.jsonl",
      "outputs/params.json",
      "run-state.json"
    ])
  );
  let metric_error = query_runs_with_preference(
    &remote,
    &metric_request,
    ProtocolPreference::ExpriNode,
    "./node-stub",
  )
  .unwrap_err();
  assert!(metric_error.to_string().contains("run-metrics-v1"));
  fs::write(
    &node,
    "#!/bin/sh\n[ \"$2\" = capabilities ] && [ \"$4\" = env-maintenance-v1 ]\n",
  )
  .unwrap();
  let fallback =
    query_runs_with_preference(&remote, &request, ProtocolPreference::Auto, "./node-stub").unwrap();
  assert_eq!(fallback["runs"], python["runs"]);
  let error = query_runs_with_preference(
    &remote,
    &request,
    ProtocolPreference::ExpriNode,
    "./node-stub",
  )
  .unwrap_err();
  assert!(error.to_string().contains("run-records-v1"));
  assert_eq!(
    protocol_with_preference(
      &remote,
      ProtocolPreference::Auto,
      "./node-stub",
      "run",
      true
    )
    .unwrap()
    .name(),
    "python"
  );
  assert_eq!(
    protocol_with_preference(
      &remote,
      ProtocolPreference::Auto,
      "./node-stub",
      "setup",
      true
    )
    .unwrap()
    .name(),
    "expri-node"
  );
}
