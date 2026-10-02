use super::*;

fn target(transport: TransportKind) -> TargetConfig {
  TargetConfig {
    host: "expri-test-host".to_string(),
    remote_dir: "/tmp/project".to_string(),
    transport,
    port: None,
    protocol: None,
    node_bin: None,
    ctl_bin: None,
    ctl_method: None,
    environment: None,
  }
}

fn remote(target: TargetConfig) -> Result<Remote> {
  Remote::new(
    target,
    "/tmp/expri-control".to_string(),
    "10m".to_string(),
    false,
    0,
    false,
  )
}

#[test]
fn ssh_remains_the_default_and_resolves_ports() {
  let target: TargetConfig = toml::from_str(
    r#"
host = "user@example.com:2222"
remote_dir = "~/project"
"#,
  )
  .expect("parse default transport");
  let default = remote(target.clone()).expect("create default remote");
  assert_eq!(default.command_program(), "ssh");
  assert_eq!(default.host, "user@example.com");
  assert_eq!(default.port, Some(2222));

  let mut target = target;
  target.port = Some(2200);
  let overridden = remote(target).expect("create remote with explicit port");
  assert_eq!(overridden.port, Some(2200));
  let args = overridden.command_args("true");
  assert!(args.windows(2).any(|pair| pair == ["-p", "2200"]));
  assert!(
    args
      .windows(2)
      .any(|pair| pair == ["-S", "/tmp/expri-control"])
  );
  assert!(args.iter().any(|argument| argument == "ControlMaster=auto"));
  assert!(args.iter().any(|argument| argument == "ControlPersist=10m"));
}

#[test]
fn ctl_preserves_host_selectors_and_uses_only_explicit_ports() {
  let mut target = target(TransportKind::Ctl);
  target.host = "saved host".to_string();
  let implicit = remote(target.clone()).expect("create ctl remote");
  assert_eq!(implicit.command_program(), "ctl");
  assert_eq!(implicit.host, "saved host");
  assert_eq!(implicit.port, None);
  assert_eq!(implicit.remote_shell_args(), ["ssh"]);

  target.port = Some(2200);
  let explicit = remote(target).expect("create ctl remote with port");
  assert_eq!(explicit.remote_shell_args(), ["ssh", "-p", "2200"]);
}

#[test]
fn ctl_accepts_plain_hosts_and_rejects_ambiguous_rsync_selectors() {
  for host in ["work", "user@work", "saved host"] {
    let mut target = target(TransportKind::Ctl);
    target.host = host.to_string();
    let remote = remote(target).expect("plain ctl selector is a valid host");
    assert_eq!(remote.host, host);
    assert_eq!(remote.port, None);
  }

  for host in [
    "saved host:2222",
    "saved/name",
    "user@host/path",
    "::1",
    "2001:db8::1",
    "user@2001:db8::1",
    "[::1]",
    "user@[2001:db8::1]",
  ] {
    let mut target = target(TransportKind::Ctl);
    target.host = host.to_string();
    assert!(
      remote(target).is_err(),
      "accepted ambiguous ctl selector {host:?}"
    );
  }
}

#[test]
fn ctl_settings_and_unsafe_host_selectors_are_rejected() {
  for host in ["", "-option"] {
    for transport in [TransportKind::Ssh, TransportKind::Ctl] {
      let mut target = target(transport);
      target.host = host.to_string();
      assert!(remote(target).is_err(), "accepted invalid host {host:?}");
    }
  }

  for (bin, method) in [(Some("ctl"), None), (None, Some("VPN"))] {
    let mut target = target(TransportKind::Ssh);
    target.ctl_bin = bin.map(str::to_string);
    target.ctl_method = method.map(str::to_string);
    assert!(remote(target).is_err(), "accepted ctl settings for ssh");
  }

  for (bin, method) in [(Some(""), None), (None, Some(""))] {
    let mut target = target(TransportKind::Ctl);
    target.ctl_bin = bin.map(str::to_string);
    target.ctl_method = method.map(str::to_string);
    assert!(remote(target).is_err(), "accepted empty ctl setting");
  }
}

#[test]
fn ctl_dry_run_does_not_require_an_installed_executable() {
  let temp = tempfile::tempdir().expect("create temp directory");
  let mut target = target(TransportKind::Ctl);
  target.ctl_bin = Some(temp.path().join("missing-ctl").display().to_string());
  let mut remote = remote(target).expect("create dry-run remote");
  remote.dry_run = true;
  remote.quiet = true;

  remote.connect().expect("dry-run connect");
  remote.execute("exit 37").expect("dry-run execute");
  assert!(remote.execute_success("exit 37").expect("dry-run probe"));
  assert_eq!(
    remote
      .capture_bytes("printf data")
      .expect("dry-run capture"),
    b""
  );
  remote
    .upload_file(temp.path(), "/tmp/file")
    .expect("dry-run upload");
  remote
    .download_file("/tmp/file", temp.path())
    .expect("dry-run download");
}

#[cfg(unix)]
mod subprocess {
  use super::*;
  use std::fs;
  use std::os::unix::fs::{PermissionsExt as _, symlink};
  use std::path::PathBuf;

  const METHOD: &str = "VPN with 'single' and \"double\" quotes\\route";

  struct FakeCtl {
    root: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    log: PathBuf,
  }

  impl FakeCtl {
    fn new(body: &str) -> Self {
      let root = tempfile::Builder::new()
        .prefix("expri-transport-")
        .tempdir()
        .expect("create transport fixture");
      let bin = root.path().join("ctl bin's \"quotes\"");
      let log = root.path().join("arguments");
      let home = root.path().join("home");
      fs::create_dir(&home).expect("create isolated remote home");
      fs::write(
        &bin,
        format!(
          "#!/bin/sh\nHOME={}; export HOME\nprintf '%s\\0' \"$@\" > {}\n{body}\n",
          shell::quote(home.to_string_lossy()),
          shell::quote(log.to_string_lossy()),
        ),
      )
      .expect("write fake ctl");
      fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))
        .expect("make fake ctl executable");
      Self {
        root,
        home,
        bin,
        log,
      }
    }

    fn remote(&self) -> Remote {
      let mut target = target(TransportKind::Ctl);
      target.ctl_bin = Some(self.bin.display().to_string());
      target.ctl_method = Some(METHOD.to_string());
      remote(target).expect("create fake ctl remote")
    }

    fn arguments(&self) -> Vec<String> {
      fs::read(&self.log)
        .expect("read ctl arguments")
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8(argument.to_vec()).expect("UTF-8 argument"))
        .collect()
    }
  }

  #[test]
  fn ctl_capture_preserves_binary_stdout_and_argument_order() {
    let fixture = FakeCtl::new("printf 'alpha\\000beta\\377'");
    let mut remote = fixture.remote();
    remote.port = Some(2222);
    remote.verbosity = 3;
    remote.quiet = false;

    assert_eq!(
      remote
        .capture_bytes("remote command")
        .expect("capture output"),
      b"alpha\0beta\xff"
    );
    assert_eq!(
      fixture.arguments(),
      [
        "--method",
        METHOD,
        "ssh",
        "-vv",
        "-p",
        "2222",
        "--",
        "expri-test-host",
        "remote command"
      ],
    );

    remote.quiet = true;
    remote
      .capture_bytes("quiet command")
      .expect("capture quiet output");
    assert_eq!(
      fixture.arguments(),
      [
        "--method",
        METHOD,
        "ssh",
        "-q",
        "-p",
        "2222",
        "--",
        "expri-test-host",
        "quiet command"
      ],
    );
  }

  #[test]
  fn ctl_reports_the_command_exit_code_and_portable_profile_prefix() {
    let fixture = FakeCtl::new("exit 37");
    let remote = fixture.remote();

    for result in [
      remote.execute("false"),
      remote.capture_bytes("false").map(|_| ()),
    ] {
      match result.expect_err("failed ctl command must be reported") {
        ExpriError::CommandFailed { program, code } => {
          assert_eq!(program, fixture.bin.display().to_string());
          assert_eq!(code, Some(37));
        }
        error => panic!("unexpected command failure: {error}"),
      }
    }

    remote.execute("false").expect_err("execute fails");
    assert_eq!(
      fixture.arguments().last().expect("remote command"),
      "[ -f ~/.profile ] && . ~/.profile; false",
    );
  }

  #[test]
  fn ctl_probes_return_success_and_failure_without_command_output() {
    let fixture = FakeCtl::new("while [ \"$#\" -gt 1 ]; do shift; done\nexec /bin/sh -c \"$1\"");
    let remote = fixture.remote();
    assert!(remote.execute_success("exit 0").expect("successful probe"));
    assert!(
      remote
        .execute_success("printf 'unexpected\\000bytes'; printf diagnostics >&2; exit 0")
        .expect("probe ignores command output")
    );
    assert!(!remote.execute_success("exit 37").expect("failed probe"));
    let arguments = fixture.arguments();
    let command = arguments.last().expect("probe command");
    assert!(command.contains("exit 37"));
    assert!(command.contains(".profile"));
  }

  #[test]
  fn ctl_probes_use_profile_path_and_suppress_noisy_profile_output() {
    let fixture = FakeCtl::new("while [ \"$#\" -gt 1 ]; do shift; done\nexec /bin/sh -c \"$1\"");
    let bin = fixture.home.join("bin");
    fs::create_dir(&bin).expect("create profile bin directory");
    let probe = bin.join("expri-profile-probe");
    fs::write(
      &probe,
      "#!/bin/sh\n[ \"$EXPRI_PROFILE_MARKER\" = profile-loaded ]\n",
    )
    .expect("write profile probe");
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o700))
      .expect("make profile probe executable");
    fs::write(
      fixture.home.join(".profile"),
      format!(
        "printf noisy-profile-output\nprintf noisy-profile-error >&2\n\
EXPRI_PROFILE_MARKER=profile-loaded; export EXPRI_PROFILE_MARKER\n\
PATH={}:$PATH; export PATH\n",
        shell::quote(bin.to_string_lossy()),
      ),
    )
    .expect("write noisy remote profile");

    assert!(
      fixture
        .remote()
        .execute_success("expri-profile-probe")
        .expect("probe finds program exposed by remote profile")
    );
  }

  #[test]
  fn ctl_probe_transport_failures_are_errors_instead_of_false_results() {
    let fixture = FakeCtl::new("exit 1");
    let remote = fixture.remote();
    match remote
      .execute_success("true")
      .expect_err("transport failure must be reported")
    {
      ExpriError::CommandFailed { program, code } => {
        assert_eq!(program, fixture.bin.display().to_string());
        assert_eq!(code, Some(1));
      }
      error => panic!("unexpected predicate failure: {error}"),
    }
  }

  #[test]
  fn ctl_connection_reuse_never_launches_an_expri_owned_master() {
    let fixture = FakeCtl::new("exit 91");
    let remote = fixture.remote();
    remote.connect().expect("ctl owns connection startup");
    assert!(!fixture.log.exists(), "connect unexpectedly launched ctl");
    let args = remote.remote_shell_args();
    assert_eq!(args, ["--method", METHOD, "ssh"]);
    assert!(
      !args
        .iter()
        .any(|argument| argument.contains("Control") || argument == "-S" || argument == "-M")
    );
  }

  #[test]
  fn missing_ctl_executable_reports_its_path() {
    let fixture = FakeCtl::new("exit 0");
    let mut target = target(TransportKind::Ctl);
    let missing = fixture
      .root
      .path()
      .join("missing-ctl")
      .display()
      .to_string();
    target.ctl_bin = Some(missing.clone());
    let remote = remote(target).expect("create missing ctl remote");
    match remote
      .capture_bytes("true")
      .expect_err("missing ctl must fail")
    {
      ExpriError::IoContext {
        action,
        path,
        source,
      } => {
        assert_eq!(action, "launch");
        assert_eq!(path, missing);
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
      }
      error => panic!("unexpected launch failure: {error}"),
    }
  }

  #[test]
  fn rsync_transfers_preserve_files_selections_exclusions_and_symlinks_through_ctl() {
    match Command::new("rsync")
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
    {
      Ok(status) => assert!(status.success(), "rsync --version failed"),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
        eprintln!("skipping local transport transfer test: rsync is unavailable");
        return;
      }
      Err(error) => panic!("could not inspect rsync: {error}"),
    }

    let fixture = FakeCtl::new(&format!(
      "[ \"$1\" = --method ] && [ \"$2\" = {} ] || exit 91\n\
shift 2\n\
[ \"$1\" = ssh ] || exit 92\n\
shift\n\
[ \"$1\" = expri-test-host ] || exit 93\n\
shift\n\
exec /bin/sh -c \"$*\"",
      shell::quote(METHOD),
    ));
    let remote = fixture.remote();
    let source = fixture.root.path().join("source");
    let server = fixture.root.path().join("server");
    fs::create_dir_all(source.join("nested")).expect("create source directory");
    fs::create_dir_all(source.join("cache")).expect("create excluded directory");
    fs::create_dir(&server).expect("create local server directory");
    fs::write(source.join("run.sh"), b"#!/bin/sh\nprintf success\n").expect("write executable");
    fs::set_permissions(source.join("run.sh"), fs::Permissions::from_mode(0o751))
      .expect("set executable mode");
    symlink("run.sh", source.join("link")).expect("create symlink");
    let selected_name = "nested/space file\nnewline.bin";
    let selected_bytes = b"selected\0bytes\xff";
    fs::write(source.join(selected_name), selected_bytes).expect("write selected binary file");
    fs::write(source.join("cache/skip"), b"excluded directory")
      .expect("write excluded directory file");
    fs::write(source.join("notes.tmp"), b"excluded pattern").expect("write excluded file");

    let uploaded_file = server.join("single.sh");
    remote
      .upload_file(&source.join("run.sh"), &uploaded_file.to_string_lossy())
      .expect("upload file through ctl");
    let downloaded_file = fixture.root.path().join("downloaded.sh");
    remote
      .download_file(&uploaded_file.to_string_lossy(), &downloaded_file)
      .expect("download file through ctl");
    assert_eq!(
      fs::read(&downloaded_file).expect("read downloaded file"),
      fs::read(source.join("run.sh")).expect("read original file")
    );
    assert_eq!(
      fs::metadata(&downloaded_file)
        .expect("downloaded metadata")
        .permissions()
        .mode()
        & 0o777,
      0o751
    );

    let full = server.join("full");
    remote
      .upload_dir(&source, &full.to_string_lossy())
      .expect("upload directory through ctl");
    assert_eq!(
      fs::read_link(full.join("link")).expect("uploaded symlink"),
      Path::new("run.sh")
    );
    assert_eq!(
      fs::read(full.join(selected_name)).expect("uploaded binary"),
      selected_bytes
    );

    let files_from = fixture.root.path().join("files-from");
    fs::write(&files_from, format!("run.sh\0link\0{selected_name}\0"))
      .expect("write NUL-delimited file selection");
    let selected = server.join("selected");
    remote
      .upload_files_from(&source, &selected.to_string_lossy(), &files_from)
      .expect("upload selected files through ctl");
    assert_eq!(
      fs::read(selected.join(selected_name)).expect("selected upload"),
      selected_bytes
    );
    assert!(!selected.join("notes.tmp").exists());
    assert!(!selected.join("cache").exists());

    let scoped = fixture.root.path().join("scoped-download");
    remote
      .download_files_from(&full.to_string_lossy(), &scoped, &files_from)
      .expect("download selected files through ctl");
    assert_eq!(
      fs::read(scoped.join(selected_name)).expect("selected download"),
      selected_bytes
    );
    assert_eq!(
      fs::read_link(scoped.join("link")).expect("selected symlink"),
      Path::new("run.sh")
    );
    assert!(!scoped.join("notes.tmp").exists());

    let filtered = fixture.root.path().join("filtered-download");
    remote
      .download_dir_with_excludes(
        &full.to_string_lossy(),
        &filtered,
        &["cache".to_string(), "*.tmp".to_string()],
      )
      .expect("download directory with exclusions through ctl");
    assert_eq!(
      fs::read(filtered.join(selected_name)).expect("filtered download"),
      selected_bytes
    );
    assert_eq!(
      fs::read_link(filtered.join("link")).expect("filtered symlink"),
      Path::new("run.sh")
    );
    assert!(!filtered.join("cache").exists());
    assert!(!filtered.join("notes.tmp").exists());
    assert_eq!(
      &fixture.arguments()[..4],
      ["--method", METHOD, "ssh", "expri-test-host"]
    );
  }
}
