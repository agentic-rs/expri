use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const STDOUT_LOG: &str = "logs/stdout.log";
pub const STDERR_LOG: &str = "logs/stderr.log";

pub struct RunLogs {
  stdout: File,
  stderr: File,
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
    })
  }

  pub fn task(&self, command: &mut Command) -> io::Result<LoggedOutput> {
    execute(
      command,
      Some(self.stdout.try_clone()?),
      self.stderr.try_clone()?,
      Some(Box::new(io::stdout())),
      Box::new(io::stderr()),
    )
  }

  /// The environment helper's stdout is its private JSON response.
  pub fn helper(&self, command: &mut Command) -> io::Result<LoggedOutput> {
    execute(
      command,
      None,
      self.stderr.try_clone()?,
      None,
      Box::new(io::stderr()),
    )
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

fn execute(
  command: &mut Command,
  stdout_log: Option<File>,
  stderr_log: File,
  stdout_terminal: Option<Box<dyn Write + Send>>,
  stderr_terminal: Box<dyn Write + Send>,
) -> io::Result<LoggedOutput> {
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
  let status = child.wait();
  if status.is_err() {
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
