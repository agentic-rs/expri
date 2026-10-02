use std::io::{Cursor, Read, Write};
use std::sync::Mutex;

use super::*;
use crate::error::command_exit_code;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
  fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    self.0.lock().expect("sink lock").extend_from_slice(bytes);
    Ok(bytes.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

struct BrokenTerminal;

impl Write for BrokenTerminal {
  fn write(&mut self, _: &[u8]) -> io::Result<usize> {
    Err(io::ErrorKind::BrokenPipe.into())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

fn python(script: &str) -> Command {
  let mut command = Command::new("python3");
  command.args(["-I", "-B", "-c", script]);
  command
}

fn saved(
  command: &mut Command,
  logs: &RunLogs,
  stdout: impl Write + Send + 'static,
  stderr: impl Write + Send + 'static,
) -> LoggedOutput {
  execute(
    command,
    Some(logs.stdout.try_clone().expect("stdout log")),
    logs.stderr.try_clone().expect("stderr log"),
    Some(Box::new(stdout)),
    Box::new(stderr),
  )
  .expect("run command")
}

#[test]
fn binary_large_streams_are_logged_and_teed_without_deadlock() {
  let root = tempfile::tempdir().expect("run directory");
  let logs = RunLogs::create(root.path()).expect("logs");
  let stdout = Sink::default();
  let stderr = Sink::default();
  let output = saved(
    &mut python(
      "import sys; sys.stdout.buffer.write(b'a\\x00\\xff' * 100000); sys.stderr.buffer.write(b'b\\x80\\x00' * 100000)",
    ),
    &logs,
    stdout.clone(),
    stderr.clone(),
  );
  assert!(output.status.success());
  assert!(
    output.stdout.is_empty(),
    "task output should not accumulate in memory"
  );
  assert!(output.log_error.is_none());
  let expected_stdout = b"a\x00\xff".repeat(100000);
  let expected_stderr = b"b\x80\x00".repeat(100000);
  assert_eq!(
    fs::read(root.path().join(STDOUT_LOG)).expect("stdout"),
    expected_stdout
  );
  assert_eq!(
    fs::read(root.path().join(STDERR_LOG)).expect("stderr"),
    expected_stderr
  );
  assert_eq!(*stdout.0.lock().expect("stdout sink"), expected_stdout);
  assert_eq!(*stderr.0.lock().expect("stderr sink"), expected_stderr);
}

#[test]
fn helper_stdout_is_private_and_preparation_stderr_is_saved() {
  let root = tempfile::tempdir().expect("run directory");
  let logs = RunLogs::create(root.path()).expect("logs");
  let stderr = Sink::default();
  let output = execute(
    &mut python("import os; os.write(1, b'{\"private\":true}'); os.write(2, b'prepare\\x00\\xff')"),
    None,
    logs.stderr.try_clone().expect("stderr log"),
    None,
    Box::new(stderr.clone()),
  )
  .expect("helper");
  assert!(output.status.success());
  assert_eq!(output.stdout, br#"{"private":true}"#);
  assert!(
    fs::read(root.path().join(STDOUT_LOG))
      .expect("stdout log")
      .is_empty()
  );
  assert_eq!(
    fs::read(root.path().join(STDERR_LOG)).expect("stderr log"),
    b"prepare\x00\xff"
  );
  assert_eq!(*stderr.0.lock().expect("stderr sink"), b"prepare\x00\xff");
}

struct GateSink {
  path: std::path::PathBuf,
  output: Sink,
}

impl Write for GateSink {
  fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    fs::write(&self.path, b"terminal received output")?;
    self.output.write(bytes)
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

#[test]
fn tee_delivers_output_while_the_child_is_still_running() {
  let root = tempfile::tempdir().expect("run directory");
  let logs = RunLogs::create(root.path()).expect("logs");
  let gate = root.path().join("gate");
  let terminal = Sink::default();
  let mut command = python(
    "import os, pathlib, sys, time; os.write(1, b'first\\x00'); gate = pathlib.Path(sys.argv[1]); deadline = time.monotonic() + 5\nwhile not gate.exists():\n  if time.monotonic() >= deadline: sys.exit(42)\n  time.sleep(.01)\nos.write(1, b'last')",
  );
  command.arg(&gate);
  let output = saved(
    &mut command,
    &logs,
    GateSink {
      path: gate,
      output: terminal.clone(),
    },
    Sink::default(),
  );
  assert!(
    output.status.success(),
    "child timed out waiting for live tee"
  );
  assert!(output.log_error.is_none());
  assert_eq!(
    fs::read(root.path().join(STDOUT_LOG)).expect("stdout log"),
    b"first\x00last"
  );
  assert_eq!(*terminal.0.lock().expect("terminal sink"), b"first\x00last");
}

#[test]
fn broken_terminal_does_not_stop_log_draining_or_change_exit_status() {
  let root = tempfile::tempdir().expect("run directory");
  let logs = RunLogs::create(root.path()).expect("logs");
  let output = saved(
    &mut python(
      "import sys; sys.stdout.buffer.write(b'x' * 200000); sys.stderr.buffer.write(b'y' * 200000); sys.exit(7)",
    ),
    &logs,
    BrokenTerminal,
    BrokenTerminal,
  );
  assert_eq!(command_exit_code(&output.status), Some(7));
  assert!(output.log_error.is_none());
  assert_eq!(
    fs::read(root.path().join(STDOUT_LOG)).expect("stdout log"),
    vec![b'x'; 200000]
  );
  assert_eq!(
    fs::read(root.path().join(STDERR_LOG)).expect("stderr log"),
    vec![b'y'; 200000]
  );
}

#[test]
fn log_write_failure_drains_both_pipes_and_preserves_child_status() {
  let root = tempfile::tempdir().expect("run directory");
  let logs = RunLogs::create(root.path()).expect("logs");
  let stdout = Sink::default();
  let output = execute(
    &mut python("import sys; sys.stdout.buffer.write(b'x' * 200000); sys.stderr.buffer.write(b'y' * 200000); sys.exit(17)"),
    Some(File::open(root.path().join(STDOUT_LOG)).expect("read-only log")),
    logs.stderr.try_clone().expect("stderr log"),
    Some(Box::new(stdout.clone())),
    Box::new(Sink::default()),
  ).expect("task is still reaped");
  assert_eq!(command_exit_code(&output.status), Some(17));
  assert!(
    output
      .log_error
      .expect("capture error")
      .to_string()
      .contains("write stdout log")
  );
  assert_eq!(stdout.0.lock().expect("terminal sink").len(), 200000);
  assert_eq!(
    fs::read(root.path().join(STDERR_LOG)).expect("stderr log"),
    vec![b'y'; 200000]
  );
}

#[test]
fn signal_status_and_configured_python_buffering_are_preserved() {
  let root = tempfile::tempdir().expect("run directory");
  let logs = RunLogs::create(root.path()).expect("logs");
  let mut command = python(
    "import os, signal; os.write(1, os.environ['PYTHONUNBUFFERED'].encode()); os.kill(os.getpid(), signal.SIGTERM)",
  );
  command.env("PYTHONUNBUFFERED", "configured");
  let output = saved(&mut command, &logs, Sink::default(), Sink::default());
  assert_eq!(command_exit_code(&output.status), Some(143));
  assert_eq!(
    fs::read(root.path().join(STDOUT_LOG)).expect("stdout log"),
    b"configured"
  );
  assert!(output.log_error.is_none());
}

#[test]
fn inherited_descendant_pipes_are_bounded_after_the_direct_child_exits() {
  let root = tempfile::tempdir().expect("run directory");
  let logs = RunLogs::create(root.path()).expect("logs");
  let pid_path = root.path().join("descendant.pid");
  let mut command = python(
    "import os, pathlib, subprocess, sys; child = subprocess.Popen([sys.executable, '-I', '-B', '-c', 'import time; time.sleep(30)']); pathlib.Path(sys.argv[1]).write_text(str(child.pid)); os.write(1, b'direct child'); sys.exit(9)",
  );
  command.arg(&pid_path);
  let started = Instant::now();
  let result = saved(&mut command, &logs, Sink::default(), Sink::default());
  let descendant: i32 = fs::read_to_string(pid_path)
    .expect("descendant pid")
    .parse()
    .expect("pid number");
  // The orphan is an intentional fixture; never leave it sleeping after the test.
  unsafe {
    libc::kill(descendant, libc::SIGTERM);
  }
  assert_eq!(command_exit_code(&result.status), Some(9));
  assert!(started.elapsed() < Duration::from_secs(5));
  assert!(
    result
      .log_error
      .expect("bounded capture warning")
      .to_string()
      .contains("remained open")
  );
  assert_eq!(
    fs::read(root.path().join(STDOUT_LOG)).expect("stdout log"),
    b"direct child"
  );
}

struct ClosedChunks(Cursor<Vec<u8>>);

impl Read for ClosedChunks {
  fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
    self.0.read(&mut bytes[..1])
  }
}

impl PipeRead for ClosedChunks {
  fn writers_closed(&self) -> io::Result<bool> {
    Ok(true)
  }
}

struct SlowTerminal;

impl Write for SlowTerminal {
  fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    thread::sleep(Duration::from_millis(250));
    Ok(bytes.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

#[test]
fn closed_pipes_drain_completely_even_after_a_slow_terminal_exceeds_grace() {
  let finished = AtomicBool::new(true);
  let output = drain(
    ClosedChunks(Cursor::new(b"normal".to_vec())),
    Some(Vec::new()),
    Some(SlowTerminal),
    true,
    &finished,
    "stdout",
  );
  assert_eq!(output.captured, b"normal");
  assert!(output.error.is_none());
}

#[test]
fn log_paths_are_private_and_preexisting_paths_are_not_overwritten() {
  use std::os::unix::fs::PermissionsExt;
  let root = tempfile::tempdir().expect("run directory");
  let _logs = RunLogs::create(root.path()).expect("logs");
  assert_eq!(
    fs::metadata(root.path().join("logs"))
      .expect("logs metadata")
      .permissions()
      .mode()
      & 0o777,
    0o700
  );
  assert_eq!(
    fs::metadata(root.path().join(STDOUT_LOG))
      .expect("log metadata")
      .permissions()
      .mode()
      & 0o777,
    0o600
  );
  assert!(RunLogs::create(root.path()).is_err());
}
