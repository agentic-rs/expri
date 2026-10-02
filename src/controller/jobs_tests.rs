use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde_json::json;

use super::*;
use crate::config::TransportKind;

struct Fixture {
  _root: tempfile::TempDir,
  repo: PathBuf,
  home: PathBuf,
  remote: Remote,
}

impl Fixture {
  fn new() -> Self {
    let root = tempfile::Builder::new()
      .prefix("expri jobs with spaces ")
      .tempdir()
      .unwrap();
    let repo = root.path().join("repo 'quoted'");
    let home = root.path().join("login-home");
    fs::create_dir_all(repo.join(".expri/runs/run-old/logs")).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::write(
      home.join(".profile"),
      "echo login-banner\nexport EXPRI_PROFILE_LOADED=yes\n",
    )
    .unwrap();
    fs::write(
      repo.join(".expri/runs/run-old/run-state.json"),
      serde_json::to_vec(&json!({
        "schema_version": 1, "run_id": "run-old", "task": "train",
        "status": "completed", "exit_code": 0,
      }))
      .unwrap(),
    )
    .unwrap();
    fs::write(
      repo.join(".expri/runs/run-old/logs/stdout.log"),
      b"old line\nlast \xff\x00 line\n",
    )
    .unwrap();
    let ctl = root.path().join("fake-ctl");
    write_executable(
      &ctl,
      &format!(
        "#!/bin/sh\nHOME={}; export HOME\nfor argument do remote_command=$argument; done\nexec /bin/sh -c \"$remote_command\"\n",
        shell::quote(home.to_string_lossy())
      ),
    );
    let remote = Self::remote(&repo, &ctl);
    Self {
      _root: root,
      repo,
      home,
      remote,
    }
  }

  fn remote(repo: &std::path::Path, ctl: &std::path::Path) -> Remote {
    Remote::new(
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
    .unwrap()
  }

  fn node(&self, capability: &str) {
    write_executable(
      &self.repo.join("node-stub"),
      &format!(
        r#"#!/bin/sh
[ "$EXPRI_PROFILE_LOADED" = yes ] || exit 71
if [ "$2" = capabilities ]; then [ "$4" = {capability} ]; exit $?; fi
if [ "$2" = jobs ]; then
  [ "$3" = --request-stdin ] || exit 72
  python3 -c 'import json,sys; request=json.load(sys.stdin); sys.stdout.buffer.write(b"native\x00\xff\n") if request["operation"]=="logs" else print(json.dumps({{"native":True,"request":request}}))'
elif [ "$2" = run ]; then
  python3 -c 'import json,sys; request=json.load(open(sys.argv[1])); assert request["detach"]; print(json.dumps({{"run_id":"run-new","run_dir":"/remote/.expri/runs/run-new","status":"preparing","detached":True}}))' "$4"
else
  exit 73
fi
"#
      ),
    );
  }

  fn capture_logs(&self, preference: ProtocolPreference) -> Vec<u8> {
    let capture = self.repo.join("captured-log");
    let ctl = self.repo.join("capture-ctl");
    write_executable(
      &ctl,
      &format!(
        r#"#!/bin/sh
HOME={home}; export HOME
for argument do remote_command=$argument; done
case "$remote_command" in
  *'node capabilities'*) exec /bin/sh -c "$remote_command" ;;
  *) exec /bin/sh -c "$remote_command" >{capture} ;;
esac
"#,
        home = shell::quote(self.home.to_string_lossy()),
        capture = shell::quote(capture.to_string_lossy())
      ),
    );
    let remote = Self::remote(&self.repo, &ctl);
    let result = execute_with_preference(
      &remote,
      &JobRequest::Logs {
        run_id: "run-old".to_string(),
        stream: "stdout".to_string(),
        follow: false,
        tail: 1,
      },
      preference,
      "./node-stub",
    )
    .unwrap();
    assert!(result.is_none());
    fs::read(capture).unwrap()
  }
}

fn write_executable(path: &std::path::Path, contents: &str) {
  fs::write(path, contents).unwrap();
  fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn job_queries_select_capability_and_keep_profile_banners_off_json() {
  let fixture = Fixture::new();
  fixture.node("durable-runs-v1");
  let request = JobRequest::Status {
    run_id: "run-old".to_string(),
  };
  let report = execute_with_preference(
    &fixture.remote,
    &request,
    ProtocolPreference::Auto,
    "./node-stub",
  )
  .unwrap()
  .unwrap();
  assert_eq!(report["native"], true);
  assert_eq!(report["request"], serde_json::to_value(&request).unwrap());
  let python = execute_with_preference(
    &fixture.remote,
    &request,
    ProtocolPreference::Python,
    "./node-stub",
  )
  .unwrap()
  .unwrap();
  assert_eq!(python["run_id"], "run-old");
  assert_eq!(python["status"], "completed");
  fixture.node("run-records-v1");
  let fallback = execute_with_preference(
    &fixture.remote,
    &request,
    ProtocolPreference::Auto,
    "./node-stub",
  )
  .unwrap()
  .unwrap();
  assert_eq!(fallback, python);
  let error = execute_with_preference(
    &fixture.remote,
    &request,
    ProtocolPreference::ExpriNode,
    "./node-stub",
  )
  .unwrap_err();
  assert!(error.to_string().contains("durable-runs-v1"));
  let terminal = execute_with_preference(
    &fixture.remote,
    &JobRequest::Cancel {
      run_id: "run-old".to_string(),
    },
    ProtocolPreference::Python,
    "./node-stub",
  )
  .unwrap()
  .unwrap();
  assert_eq!(terminal["already_finished"], true);
  assert!(
    !fixture
      .repo
      .join(".expri/runs/run-old/.cancel-request")
      .exists()
  );
}

#[test]
fn streamed_logs_preserve_binary_bytes_without_profile_banners() {
  let fixture = Fixture::new();
  fixture.node("durable-runs-v1");
  assert_eq!(
    fixture.capture_logs(ProtocolPreference::Auto),
    b"native\x00\xff\n"
  );
  assert_eq!(
    fixture.capture_logs(ProtocolPreference::Python),
    b"last \xff\x00 line\n"
  );
}

#[test]
fn detached_start_uses_durable_capability_and_accepts_quoted_request_paths() {
  let fixture = Fixture::new();
  fixture.node("durable-runs-v1");
  let request = fixture.repo.join("request 'with spaces'.json");
  fs::write(&request, br#"{"detach":true}"#).unwrap();
  start_with_preference(
    &fixture.remote,
    request.to_str().unwrap(),
    ProtocolPreference::Auto,
    "./node-stub",
  )
  .unwrap();
  fixture.node("run-records-v1");
  let error = start_with_preference(
    &fixture.remote,
    request.to_str().unwrap(),
    ProtocolPreference::ExpriNode,
    "./node-stub",
  )
  .unwrap_err();
  assert!(error.to_string().contains("durable-runs-v1"));
}

#[test]
fn python_durable_commands_fit_linux_per_argument_limit_after_transport_formatting() {
  let remote = Remote::new(
    TargetConfig {
      host: "rental.example".to_string(),
      remote_dir: "/srv/experiment project 'quoted'".to_string(),
      transport: TransportKind::Ssh,
      port: Some(2200),
      protocol: Some("python".to_string()),
      node_bin: None,
      ctl_bin: None,
      ctl_method: None,
      environment: None,
    },
    "/tmp/expri-control-%r@%h:%p".to_string(),
    "30m".to_string(),
    false,
    2,
    false,
  )
  .unwrap();
  let startup = with_profile(&python_command(
    &remote,
    &python_run_script(".expri/inbox/run 'quoted path'/run-request.json"),
  ));
  let requests = [
    JobRequest::Status {
      run_id: "run-test".to_string(),
    },
    JobRequest::Cancel {
      run_id: "run-test".to_string(),
    },
    JobRequest::Logs {
      run_id: "run-test".to_string(),
      stream: "stderr".to_string(),
      follow: true,
      tail: 100,
    },
  ];
  let commands = std::iter::once(startup).chain(
    requests
      .iter()
      .map(|request| with_profile(&job_command(&remote, request, "unused-node", false).unwrap())),
  );
  for (command_index, command) in commands.enumerate() {
    let arguments = remote.command_args(&command);
    assert_eq!(arguments.last(), Some(&command));
    for (argument_index, argument) in arguments.iter().enumerate() {
      // Linux counts the terminating NUL in MAX_ARG_STRLEN (32 * 4 KiB).
      // Keep a margin for command growth instead of relying on macOS's limit.
      assert!(
        argument.len() + 1 < 120 * 1024,
        "command {command_index}, argument {argument_index} is {} bytes",
        argument.len() + 1,
      );
    }
  }
}
