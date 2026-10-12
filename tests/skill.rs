use std::process::{Command, Output};

fn skill(directory: &std::path::Path, args: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_expri"))
    .current_dir(directory)
    .arg("skill")
    .args(args)
    .output()
    .unwrap()
}

#[test]
fn agent_can_discover_and_read_topics_without_project_configuration() {
  let directory = tempfile::tempdir().unwrap();
  let output = skill(directory.path(), &[]);
  assert!(output.status.success(), "{:?}", output);
  assert!(output.stderr.is_empty());
  let overview = String::from_utf8(output.stdout).unwrap();
  let topics: Vec<_> = overview
    .lines()
    .filter_map(|line| line.strip_prefix("| `expri skill "))
    .map(|line| line.split('`').next().unwrap())
    .collect();
  assert!(
    !topics.is_empty(),
    "the entry guide must expose topic commands"
  );
  for topic in topics {
    let output = skill(directory.path(), &[topic]);
    assert!(output.status.success(), "{topic}: {:?}", output);
    assert!(output.stderr.is_empty(), "{topic}: {:?}", output);
    let detail = String::from_utf8(output.stdout).unwrap();
    assert!(!detail.trim().is_empty(), "{topic}");
    assert_ne!(detail, overview, "{topic} must expose its own guide");
  }
  assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn unknown_topics_fail_without_printing_a_guide() {
  let directory = tempfile::tempdir().unwrap();
  let output = skill(directory.path(), &["missing-topic"]);
  assert_eq!(output.status.code(), Some(2));
  assert!(output.stdout.is_empty());
  let error = String::from_utf8(output.stderr).unwrap();
  assert!(error.contains("missing-topic"));
  assert!(error.contains("--help"));
  assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}
