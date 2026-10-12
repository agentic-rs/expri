//! Check the human and JSON reports for local finalized output registrations.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn expri(run_dir: &Path, args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_expri"))
    .args(args)
    .arg("--run-dir")
    .arg(run_dir)
    .output()
    .unwrap()
}

fn text(output: Output) -> String {
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  String::from_utf8(output.stdout).unwrap()
}

#[test]
fn artifact_tables_and_json_share_registration_records_and_quiet_behavior() {
  let temporary = tempfile::tempdir().unwrap();
  let run_dir = temporary.path().canonicalize().unwrap();
  fs::create_dir(run_dir.join("outputs")).unwrap();
  fs::write(
    run_dir.join("run-state.json"),
    br#"{"run_id":"run-output"}"#,
  )
  .unwrap();
  fs::write(run_dir.join("outputs/checkpoint.pt"), b"weights").unwrap();

  let registered = text(expri(
    &run_dir,
    &[
      "artifact",
      "register",
      "outputs/checkpoint.pt",
      "--label",
      "best",
    ],
  ));
  assert!(registered.contains("outputs/checkpoint.pt"));
  assert!(registered.contains("registered"));
  assert!(serde_json::from_str::<serde_json::Value>(&registered).is_err());

  let listed = text(expri(&run_dir, &["artifact", "list"]));
  assert!(listed.contains("run-output"));
  assert!(listed.contains("outputs/checkpoint.pt"));
  assert!(listed.contains("best"));
  assert!(serde_json::from_str::<serde_json::Value>(&listed).is_err());
  assert!(text(expri(&run_dir, &["artifact", "list", "--quiet"])).is_empty());

  let json = text(expri(&run_dir, &["artifact", "--json", "list", "--quiet"]));
  let report: serde_json::Value = serde_json::from_str(&json).unwrap();
  assert_eq!(report["run_id"], "run-output");
  assert_eq!(report["files"][0]["path"], "outputs/checkpoint.pt");
  assert_eq!(report["files"][0]["size"], 7);
  assert_eq!(report["files"][0]["labels"], serde_json::json!(["best"]));

  let suffix_json = text(expri(&run_dir, &["artifact", "list", "--json"]));
  assert_eq!(
    serde_json::from_str::<serde_json::Value>(&suffix_json).unwrap(),
    report
  );
}
