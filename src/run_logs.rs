use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const STDOUT_LOG: &str = "logs/stdout.log";
pub const STDERR_LOG: &str = "logs/stderr.log";

pub struct RunLogs {
  stdout: File,
  stderr: File,
  cancel_path: Option<PathBuf>,
  mirror: bool,
  cancelled: Arc<AtomicBool>,
}

pub struct LoggedOutput {
  pub status: ExitStatus,
  pub stdout: Vec<u8>,
  pub log_error: Option<io::Error>,
}

struct Drained {
  captured: Vec<u8>,
  error: Option<io::Error>,
}

enum StopSignal {
  Terminate,
  Kill,
}

trait PipeRead: Read {
  fn writers_closed(&self) -> io::Result<bool>;
}

impl PipeRead for ChildStdout {
  fn writers_closed(&self) -> io::Result<bool> {
    writers_closed(self)
  }
}

impl PipeRead for ChildStderr {
  fn writers_closed(&self) -> io::Result<bool> {
    writers_closed(self)
  }
}
impl RunLogs {
  pub fn create(run_dir: &Path) -> io::Result<Self> {
    Self::create_mode(run_dir, false)
  }

  pub fn create_detached(run_dir: &Path) -> io::Result<Self> {
    Self::create_mode(run_dir, true)
  }

  fn create_mode(run_dir: &Path, detached: bool) -> io::Result<Self> {
    let mut directory = fs::DirBuilder::new();
    #[cfg(unix)]
    {
      use std::os::unix::fs::DirBuilderExt;
      directory.mode(0o700);
    }
    directory.create(run_dir.join("logs"))?;
    let open = |path| {
      let mut options = OpenOptions::new();
      options.append(true).create_new(true);
      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
      }
      options.open(path)
    };
    Ok(Self {
      stdout: open(run_dir.join(STDOUT_LOG))?,
      stderr: open(run_dir.join(STDERR_LOG))?,
      cancel_path: detached.then(|| run_dir.join(".cancel-request")),
      mirror: !detached,
      cancelled: Arc::new(AtomicBool::new(false)),
    })
  }

  pub fn task(&self, command: &mut Command) -> io::Result<LoggedOutput> {
    execute_controlled(
      command,
      Some(self.stdout.try_clone()?),
      self.stderr.try_clone()?,
      self
        .mirror
        .then(|| Box::new(io::stdout()) as Box<dyn Write + Send>),
      self.stderr_terminal(),
      self.cancel_path.as_deref(),
      Some(&self.cancelled),
    )
  }

  /// The environment helper's stdout is its private JSON response.
  pub fn helper(&self, command: &mut Command) -> io::Result<LoggedOutput> {
    execute_controlled(
      command,
      None,
      self.stderr.try_clone()?,
      None,
      self.stderr_terminal(),
      self.cancel_path.as_deref(),
      Some(&self.cancelled),
    )
  }

  pub fn cancel_requested(&self) -> io::Result<bool> {
    self
      .cancel_path
      .as_deref()
      .map(cancel_requested)
      .transpose()
      .map(|value| value.unwrap_or(false))
  }

  pub fn was_cancelled(&self) -> bool {
    self.cancelled.load(Ordering::Acquire)
  }

  fn stderr_terminal(&self) -> Box<dyn Write + Send> {
    if self.mirror {
      Box::new(io::stderr())
    } else {
      Box::new(io::sink())
    }
  }
}

fn unbuffered_if_absent(command: &mut Command) {
  let explicit = command
    .get_envs()
    .find(|(name, _)| *name == "PYTHONUNBUFFERED")
    .map(|(_, value)| value.is_some());
  if explicit == Some(false)
    || (explicit.is_none() && std::env::var_os("PYTHONUNBUFFERED").is_none())
  {
    command.env("PYTHONUNBUFFERED", "1");
  }
}

#[cfg(all(test, unix))]
fn execute(
  command: &mut Command,
  stdout_log: Option<File>,
  stderr_log: File,
  stdout_terminal: Option<Box<dyn Write + Send>>,
  stderr_terminal: Box<dyn Write + Send>,
) -> io::Result<LoggedOutput> {
  execute_controlled(
    command,
    stdout_log,
    stderr_log,
    stdout_terminal,
    stderr_terminal,
    None,
    None,
  )
}

fn execute_controlled(
  command: &mut Command,
  stdout_log: Option<File>,
  stderr_log: File,
  stdout_terminal: Option<Box<dyn Write + Send>>,
  stderr_terminal: Box<dyn Write + Send>,
  cancel_path: Option<&Path>,
  cancelled: Option<&AtomicBool>,
) -> io::Result<LoggedOutput> {
  if let Some(path) = cancel_path {
    if cancel_requested(path)? {
      if let Some(cancelled) = cancelled {
        cancelled.store(true, Ordering::Release);
      }
      return Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "run cancellation requested",
      ));
    }
    #[cfg(unix)]
    {
      use std::os::unix::process::CommandExt;
      command.process_group(0);
    }
  }
  unbuffered_if_absent(command);
  let capture_stdout = stdout_log.is_none();
  let mut child = command
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;
  let stdout = child.stdout.take().expect("piped stdout");
  let stderr = child.stderr.take().expect("piped stderr");
  if let Err(error) = nonblocking(&stdout).and_then(|()| nonblocking(&stderr)) {
    let _ = child.kill();
    let _ = child.wait();
    return Err(error);
  }
  let finished = Arc::new(AtomicBool::new(false));
  let stdout_reader = spawn_reader(
    "stdout",
    stdout,
    stdout_log,
    stdout_terminal,
    capture_stdout,
    finished.clone(),
  );
  let stdout_reader = match stdout_reader {
    Ok(reader) => reader,
    Err(error) => {
      let _ = child.kill();
      let _ = child.wait();
      return Err(error);
    }
  };
  let stderr_reader = spawn_reader(
    "stderr",
    stderr,
    Some(stderr_log),
    Some(stderr_terminal),
    false,
    finished.clone(),
  );
  let stderr_reader = match stderr_reader {
    Ok(reader) => reader,
    Err(error) => {
      let _ = child.kill();
      let _ = child.wait();
      finished.store(true, Ordering::Release);
      stdout_reader.thread().unpark();
      let _ = stdout_reader.join();
      return Err(error);
    }
  };
  let status = wait_child(
    &mut child,
    cancel_path,
    cancelled,
    &finished,
    &stdout_reader,
    &stderr_reader,
  );
  if status.is_err() {
    if cancel_path.is_some() {
      let _ = signal_group(&mut child, StopSignal::Kill);
    }
    let _ = child.kill();
    let _ = child.wait();
  }
  finished.store(true, Ordering::Release);
  stdout_reader.thread().unpark();
  stderr_reader.thread().unpark();
  let stdout = joined(stdout_reader, "stdout");
  let stderr = joined(stderr_reader, "stderr");
  Ok(LoggedOutput {
    status: status?,
    stdout: stdout.captured,
    log_error: stdout.error.or(stderr.error),
  })
}

/// Cancellation is a request to this supervisor, never a PID read from a record.
fn cancel_requested(path: &Path) -> io::Result<bool> {
  match fs::symlink_metadata(path) {
    Ok(metadata)
      if metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == 0 =>
    {
      Ok(true)
    }
    Ok(_) => Err(io::Error::other(
      "cancellation request must be an empty regular file",
    )),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(error),
  }
}

fn wait_child(
  child: &mut Child,
  cancel_path: Option<&Path>,
  cancelled: Option<&AtomicBool>,
  finished: &AtomicBool,
  stdout: &JoinHandle<Drained>,
  stderr: &JoinHandle<Drained>,
) -> io::Result<ExitStatus> {
  let Some(cancel_path) = cancel_path else {
    return child.wait();
  };
  let mut deadline = None;
  let mut cancellation_started = false;
  loop {
    if !cancellation_started && cancel_requested(cancel_path)? {
      cancellation_started = true;
      if let Some(cancelled) = cancelled {
        cancelled.store(true, Ordering::Release);
      }
      signal_group(child, StopSignal::Terminate)?;
      deadline = Some(Instant::now() + Duration::from_secs(2));
      finished.store(false, Ordering::Release);
    }
    if let Some(expires_at) = deadline
      && Instant::now() >= expires_at
    {
      // Keep the direct child unreaped during the grace period. Its reserved
      // PID prevents this process group ID from being reused before escalation.
      signal_group(child, StopSignal::Kill)?;
      deadline = None;
    }
    if deadline.is_none() && child_exit_pending(child)? {
      finished.store(true, Ordering::Release);
      stdout.thread().unpark();
      stderr.thread().unpark();
      if stdout.is_finished() && stderr.is_finished() {
        // Keep PID ownership through log draining, including descendants that
        // hold a pipe open. Only reap after the last possible group signal.
        return child.wait();
      }
    }
    thread::sleep(Duration::from_millis(25));
  }
}

#[cfg(unix)]
fn child_exit_pending(child: &mut Child) -> io::Result<bool> {
  let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
  let result = unsafe {
    libc::waitid(
      libc::P_PID,
      child.id(),
      &mut info,
      libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
    )
  };
  if result < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(unsafe { info.si_pid() } != 0)
}

#[cfg(not(unix))]
fn child_exit_pending(child: &mut Child) -> io::Result<bool> {
  Ok(child.try_wait()?.is_some())
}

#[cfg(unix)]
fn signal_group(child: &mut Child, signal: StopSignal) -> io::Result<()> {
  let signal = match signal {
    StopSignal::Terminate => libc::SIGTERM,
    StopSignal::Kill => libc::SIGKILL,
  };
  // This ID comes from our own live child, not from persisted user-editable state.
  let result = unsafe { libc::kill(-(child.id() as i32), signal) };
  if result == 0 {
    return Ok(());
  }
  let error = io::Error::last_os_error();
  #[cfg(target_os = "macos")]
  if error.raw_os_error() == Some(libc::EPERM) {
    // Darwin excludes zombies from killpg and returns EPERM for a group with
    // no live members. The unreaped child still reserves this PID; signaling
    // it directly succeeds for that case while retaining real permission errors.
    if unsafe { libc::kill(child.id() as i32, signal) } == 0 {
      return Ok(());
    }
    return Err(io::Error::last_os_error());
  }
  if error.raw_os_error() == Some(libc::ESRCH) {
    Ok(())
  } else {
    Err(error)
  }
}

#[cfg(not(unix))]
fn signal_group(child: &mut Child, _: StopSignal) -> io::Result<()> {
  child.kill()
}

#[cfg(unix)]
fn nonblocking(pipe: &impl std::os::fd::AsRawFd) -> io::Result<()> {
  let fd = pipe.as_raw_fd();
  // Only this reader owns the pipe; nonblocking reads allow it to stop after
  // the direct child exits even if an unrelated descendant keeps a copy open.
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
  if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(not(unix))]
fn nonblocking<T>(_: &T) -> io::Result<()> {
  Ok(())
}

#[cfg(unix)]
fn writers_closed(pipe: &impl std::os::fd::AsRawFd) -> io::Result<bool> {
  let mut descriptor = libc::pollfd {
    fd: pipe.as_raw_fd(),
    events: libc::POLLIN,
    revents: 0,
  };
  if unsafe { libc::poll(&mut descriptor, 1, 0) } < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(descriptor.revents & libc::POLLHUP != 0)
}

#[cfg(not(unix))]
fn writers_closed<T>(_: &T) -> io::Result<bool> {
  Ok(true)
}

fn spawn_reader<R: PipeRead + Send + 'static>(
  name: &'static str,
  source: R,
  log: Option<File>,
  terminal: Option<Box<dyn Write + Send>>,
  capture: bool,
  finished: Arc<AtomicBool>,
) -> io::Result<JoinHandle<Drained>> {
  thread::Builder::new()
    .name(format!("expri-{name}"))
    .spawn(move || drain(source, log, terminal, capture, &finished, name))
}

fn joined(reader: JoinHandle<Drained>, name: &str) -> Drained {
  reader.join().unwrap_or_else(|_| Drained {
    captured: Vec::new(),
    error: Some(io::Error::other(format!("{name} log reader panicked"))),
  })
}

fn drain<R: PipeRead, L: Write, T: Write>(
  mut source: R,
  mut log: Option<L>,
  mut terminal: Option<T>,
  capture: bool,
  finished: &AtomicBool,
  name: &str,
) -> Drained {
  let mut captured = Vec::new();
  let mut error = None;
  let mut tail_deadline = None;
  let mut buffer = [0_u8; 64 * 1024];
  loop {
    if finished.load(Ordering::Acquire) {
      match source.writers_closed() {
        // Closed pipes may still contain the direct child's bytes. Always
        // drain those to EOF, including when terminal output is slow.
        Ok(true) => tail_deadline = None,
        Ok(false) => {
          let deadline =
            tail_deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(1));
          if Instant::now() >= *deadline {
            error.get_or_insert_with(|| {
              io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                  "{name} remained open after the task exited; descendant output capture stopped"
                ),
              )
            });
            break;
          }
        }
        Err(poll_error) => {
          error.get_or_insert(poll_error);
          break;
        }
      }
    }
    let size = match source.read(&mut buffer) {
      Ok(0) => break,
      Ok(size) => size,
      Err(read_error) if read_error.kind() == io::ErrorKind::Interrupted => continue,
      Err(read_error) if read_error.kind() == io::ErrorKind::WouldBlock => {
        thread::park_timeout(Duration::from_millis(25));
        continue;
      }
      Err(read_error) => {
        error.get_or_insert(read_error);
        break;
      }
    };
    let bytes = &buffer[..size];
    if capture {
      captured.extend_from_slice(bytes);
    }
    if let Some(writer) = log.as_mut()
      && let Err(write_error) = writer.write_all(bytes)
    {
      error.get_or_insert_with(|| io::Error::other(format!("write {name} log: {write_error}")));
      log = None;
    }
    if let Some(writer) = terminal.as_mut()
      && writer
        .write_all(bytes)
        .and_then(|()| writer.flush())
        .is_err()
    {
      // Terminal failure must not change the task status or stop file logging.
      terminal = None;
    }
  }
  if let Some(writer) = log.as_mut()
    && let Err(flush_error) = writer.flush()
  {
    error.get_or_insert_with(|| io::Error::other(format!("flush {name} log: {flush_error}")));
  }
  Drained { captured, error }
}

#[cfg(all(test, unix))]
mod tests;
