use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::Child;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::{
  Handoff, REQUEST_FILE, RESULT_FILE, acknowledge_handoff, cancel_requested, process_request,
  process_start, read_json, reconcile_failure,
};
use crate::run_logs::{LoggedOutput, groups};
use expri_core::lock::FileLock;
use expri_core::run_state::{MAX_STATE_BYTES, update_state};
use serde_json::json;

pub(crate) struct TaskOutput {
  pub output: LoggedOutput,
  pub cancelled: bool,
  pub cleanup_uncertain: bool,
}

pub(super) struct LogPipe {
  pub(super) source: File,
  pub(super) log: File,
  closed: bool,
  tail_deadline: Option<Instant>,
  pub(super) error: Option<String>,
}

impl LogPipe {
  pub(super) fn new(source: File, log: File) -> io::Result<Self> {
    let flags = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
      || unsafe { libc::fcntl(source.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
      return Err(io::Error::last_os_error());
    }
    Ok(Self {
      source,
      log,
      closed: false,
      tail_deadline: None,
      error: None,
    })
  }

  fn drain(&mut self, finished: bool) {
    if self.closed {
      return;
    }
    if finished {
      let mut poll = libc::pollfd {
        fd: self.source.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
      };
      if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
        self.error = Some(io::Error::last_os_error().to_string());
        self.closed = true;
        return;
      }
      if poll.revents & libc::POLLHUP == 0 {
        let deadline = self
          .tail_deadline
          .get_or_insert_with(|| Instant::now() + Duration::from_secs(1));
        if Instant::now() >= *deadline {
          self.error = Some(
            "log pipe remained open after task exit; descendant output capture stopped".into(),
          );
          self.closed = true;
          return;
        }
      } else {
        self.tail_deadline = None;
      }
    }
    let mut buffer = [0_u8; 64 * 1024];
    // Bound each stream so sustained output cannot starve control/cancellation.
    for _ in 0..16 {
      match self.source.read(&mut buffer) {
        Ok(0) => {
          self.closed = true;
          break;
        }
        Ok(size) => {
          if self.error.is_none()
            && let Err(error) = self.log.write_all(&buffer[..size])
          {
            self.error = Some(format!("write task log: {error}"));
          }
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
        Err(error) => {
          self.error = Some(error.to_string());
          self.closed = true;
          break;
        }
      }
    }
  }

  pub(super) fn inherited(source: File, log: File) -> Self {
    Self {
      source,
      log,
      closed: false,
      tail_deadline: None,
      error: None,
    }
  }
}

pub(crate) fn task(
  command: &mut Command,
  run_dir: &Path,
  lease: &FileLock,
  stdout: File,
  stderr: File,
  on_started: impl FnOnce() -> io::Result<()>,
) -> io::Result<TaskOutput> {
  groups::verify_parent()?;
  let cleanup_groups =
    read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?["queue_cleanup"] == true;
  command
    .process_group(0)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  crate::run_logs::unbuffered_if_absent(command);
  let mut child = command.spawn()?;
  let pid = child.id();
  let stdout_pipe =
    unsafe { File::from_raw_fd(child.stdout.take().expect("piped stdout").into_raw_fd()) };
  let stderr_pipe =
    unsafe { File::from_raw_fd(child.stderr.take().expect("piped stderr").into_raw_fd()) };
  let initialized = (|| {
    update_state(&run_dir.join("run-state.json"), |state| {
      state["supervisor"] = json!({
        "protocol_version": 1, "generation": 1, "mode": "live", "events": [],
        "owner_pid": std::process::id(), "owner_start": process_start(std::process::id())?,
        "worker_pid": pid,
      });
      state["task_continuation_protocol_version"] = json!(1);
      state["run_control_protocol_version"] = json!(1);
      state["task_process_group_clean"] = json!(false);
      expri_core::task_attempts::ensure_initial(state)?;
      Ok(())
    })
    .map_err(io::Error::other)?;
    on_started()
  })();
  if let Err(error) = initialized {
    drop(child);
    return Ok(failed_task(pid, error));
  }
  let pipes = LogPipe::new(stdout_pipe, stdout)
    .and_then(|stdout| LogPipe::new(stderr_pipe, stderr).map(|stderr| (stdout, stderr)));
  let (stdout, stderr) = match pipes {
    Ok(pipes) => pipes,
    Err(error) => {
      drop(child);
      return Ok(failed_task(pid, error));
    }
  };
  drop(child); // Child::drop does not wait or signal. We retain its exact PID.
  pump(run_dir, lease, pid, stdout, stderr, cleanup_groups, None)
}

pub(super) fn pump(
  run_dir: &Path,
  lease: &impl AsRawFd,
  pid: u32,
  stdout: LogPipe,
  stderr: LogPipe,
  cleanup_groups: bool,
  acknowledgment: Option<&Handoff>,
) -> io::Result<TaskOutput> {
  let mut pid = pid;
  match pump_inner(
    run_dir,
    lease,
    &mut pid,
    stdout,
    stderr,
    cleanup_groups,
    acknowledgment,
  ) {
    Ok(output) => Ok(output),
    Err(error) => Ok(failed_task(pid, error)),
  }
}

pub(crate) fn resume_task(
  run_dir: &Path,
  lease: &impl AsRawFd,
  mut child: Child,
  stdout: File,
  stderr: File,
) -> io::Result<TaskOutput> {
  let pid = child.id();
  let source_out =
    unsafe { File::from_raw_fd(child.stdout.take().expect("piped stdout").into_raw_fd()) };
  let source_err =
    unsafe { File::from_raw_fd(child.stderr.take().expect("piped stderr").into_raw_fd()) };
  let pipes = LogPipe::new(source_out, stdout)
    .and_then(|out| LogPipe::new(source_err, stderr).map(|err| (out, err)));
  drop(child);
  match pipes {
    Ok((out, err)) => pump(run_dir, lease, pid, out, err, false, None),
    Err(error) => Ok(failed_task(pid, error)),
  }
}

fn failed_task(pid: u32, error: io::Error) -> TaskOutput {
  // A process-group inventory failure can leave descendants uncertain. It must
  // never release the supervisor's lease while its owned direct child is live.
  let mut cleanup_uncertain = groups::stop_pid(pid).is_err();
  let mut raw_status = 1 << 8;
  loop {
    match groups::exit_pending(pid) {
      Ok(true) => {
        let result = unsafe { libc::waitpid(pid as i32, &mut raw_status, 0) };
        if result == pid as i32 {
          break;
        }
        if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
          continue;
        }
        cleanup_uncertain = true;
        break;
      }
      Ok(false) => {
        // The waitid check above reserves this exact PID. Never signal a value
        // recovered from user-editable run metadata.
        unsafe {
          libc::kill(pid as i32, libc::SIGKILL);
        }
        thread::sleep(Duration::from_millis(25));
      }
      Err(ownership_error) if ownership_error.raw_os_error() == Some(libc::ECHILD) => {
        cleanup_uncertain = true;
        break;
      }
      Err(_) => {
        // Retain the lease and retry instead of publishing terminal state for a
        // worker whose stop cannot yet be verified.
        cleanup_uncertain = true;
        thread::sleep(Duration::from_millis(25));
      }
    }
  }
  TaskOutput {
    output: LoggedOutput {
      status: std::process::ExitStatus::from_raw(raw_status),
      stdout: Vec::new(),
      log_error: Some(error),
    },
    cancelled: false,
    cleanup_uncertain,
  }
}

fn pump_inner(
  run_dir: &Path,
  lease: &impl AsRawFd,
  pid: &mut u32,
  mut stdout: LogPipe,
  mut stderr: LogPipe,
  cleanup_groups: bool,
  mut acknowledgment: Option<&Handoff>,
) -> io::Result<TaskOutput> {
  let mut cancelled = false;
  let mut cleanup_uncertain = false;
  let mut cancellation_deadline = None;
  let mut group_cleaned = false;
  let mut observed_request = None;
  let mut observed_result = None;
  let mut observed_continuation = None;
  let mut observed_lifecycle = None;
  let mut continuing: Option<crate::continuation::PreparedContinuation> = None;
  update_state(&run_dir.join("run-state.json"), |state| {
    state["run_control_protocol_version"] = json!(1);
    Ok(())
  })
  .map_err(io::Error::other)?;
  loop {
    if !cancelled && cancel_requested(run_dir)? {
      cancelled = true;
      update_state(&run_dir.join("run-state.json"), |state| {
        crate::run_control::linux::restore_phase(state);
        Ok(())
      })
      .map_err(io::Error::other)?;
      groups::signal(*pid, libc::SIGTERM)?;
      groups::signal(*pid, libc::SIGCONT)?;
      cancellation_deadline = Some(Instant::now() + Duration::from_secs(2));
    }
    if cancellation_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
      groups::signal(*pid, libc::SIGKILL)?;
      cancellation_deadline = None;
    }
    let finished = groups::exit_pending(*pid)?
      && (cancellation_deadline.is_none() || !groups::has_live_members(*pid, *pid)?);
    if finished && !group_cleaned {
      if let Err(error) = groups::stop_pid(*pid) {
        cleanup_uncertain = true;
        stdout.error.get_or_insert_with(|| error.to_string());
      }
      update_state(&run_dir.join("run-state.json"), |state| {
        state["task_process_group_clean"] = json!(!cleanup_uncertain);
        Ok(())
      })
      .map_err(io::Error::other)?;
      group_cleaned = true;
    }
    stdout.drain(finished);
    stderr.drain(finished);
    if let Some(payload) = acknowledgment
      && matches!(acknowledge_handoff(payload, lease), Ok(true))
    {
      acknowledgment = None;
    }
    if finished && stdout.closed && stderr.closed {
      // Cleanup and log writes can outlast the loop's first cancellation
      // check. Consume requests accepted while the supervisor still owned the
      // run before publishing the completed child's terminal result.
      if !cancelled {
        cancelled = cancel_requested(run_dir)?;
      }
      let mut raw_status = 0;
      loop {
        if unsafe { libc::waitpid(*pid as i32, &mut raw_status, 0) } == *pid as i32 {
          break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
          return Err(error);
        }
      }
      if let Some(mut prepared) = continuing.take() {
        // This deadline belongs to the reaped predecessor. Replacement setup
        // can outlast its grace period and must not kill the newly owned child.
        cancellation_deadline = None;
        let replacement = (|| -> expri_core::error::Result<std::process::Child> {
          if cancelled || cancel_requested(run_dir)? {
            return Err(expri_core::error::ExpriError::Message(
              "run cancellation requested during continuation".into(),
            ));
          }
          if cleanup_uncertain || stdout.error.is_some() || stderr.error.is_some() {
            return Err(expri_core::error::ExpriError::Message("old task cleanup or log capture could not be verified; no replacement worker was launched".into()));
          }
          let exit =
            expri_core::error::command_exit_code(&std::process::ExitStatus::from_raw(raw_status));
          crate::continuation::complete_old_attempt(run_dir, &mut prepared, exit)?;
          crate::continuation::launch_replacement(run_dir, &mut prepared)
        })();
        match replacement {
          Ok(mut child) => {
            *pid = child.id();
            let out = unsafe {
              File::from_raw_fd(child.stdout.take().expect("piped stdout").into_raw_fd())
            };
            let err = unsafe {
              File::from_raw_fd(child.stderr.take().expect("piped stderr").into_raw_fd())
            };
            let files = stdout
              .log
              .try_clone()
              .and_then(|log| LogPipe::new(out, log))
              .and_then(|out| {
                stderr
                  .log
                  .try_clone()
                  .and_then(|log| LogPipe::new(err, log))
                  .map(|err| (out, err))
              });
            drop(child);
            match files {
              Ok((out, err)) => {
                stdout = out;
                stderr = err;
                group_cleaned = false;
                cleanup_uncertain = false;
                continue;
              }
              Err(error) => return Ok(failed_task(*pid, error)),
            }
          }
          Err(error) => {
            let _ = crate::continuation::fail_after_stop(run_dir, &prepared, &error);
            return Ok(TaskOutput {
              output: LoggedOutput {
                status: std::process::ExitStatus::from_raw(1 << 8),
                stdout: Vec::new(),
                log_error: Some(io::Error::other(error)),
              },
              cancelled: cancelled || cancel_requested(run_dir).unwrap_or(false),
              cleanup_uncertain,
            });
          }
        }
      }
      return Ok(TaskOutput {
        output: LoggedOutput {
          status: std::process::ExitStatus::from_raw(raw_status),
          stdout: Vec::new(),
          log_error: stdout.error.or(stderr.error).map(io::Error::other),
        },
        cancelled,
        cleanup_uncertain,
      });
    }
    if !finished && !cancelled && acknowledgment.is_none() && continuing.is_none() {
      if let Ok(metadata) = fs::symlink_metadata(run_dir.join(crate::run_control::linux::MAILBOX)) {
        let identity = (metadata.dev(), metadata.ino());
        if observed_lifecycle != Some(identity) {
          observed_lifecycle = Some(identity);
          if let Some(grace) =
            crate::run_control::linux::process(run_dir, *pid).map_err(io::Error::other)?
          {
            cancelled = true;
            cancellation_deadline = Some(Instant::now() + Duration::from_secs(u64::from(grace)));
          }
        }
      }
      if cancelled {
        continue;
      }
      if let Ok(metadata) = fs::symlink_metadata(run_dir.join(".task-continuation.json")) {
        let identity = (metadata.dev(), metadata.ino());
        if observed_continuation != Some(identity) {
          observed_continuation = Some(identity);
          if let Ok(Some(mut prepared)) = crate::continuation::pending(run_dir) {
            if stdout.error.is_some() || stderr.error.is_some() {
              let _ = crate::continuation::reject_prepared(
                run_dir,
                &prepared,
                "task log capture has failed; continuation was not started",
              );
            } else if crate::continuation::mark_stopping(run_dir, &mut prepared).is_ok() {
              groups::signal(*pid, libc::SIGTERM)?;
              groups::signal(*pid, libc::SIGCONT)?;
              cancellation_deadline =
                Some(Instant::now() + Duration::from_secs(u64::from(prepared.grace_seconds)));
              continuing = Some(prepared);
            }
          }
        }
      }
      if continuing.is_none() {
        if let Ok(metadata) = fs::symlink_metadata(run_dir.join(RESULT_FILE)) {
          let identity = (metadata.dev(), metadata.ino());
          if observed_result != Some(identity) && matches!(reconcile_failure(run_dir), Ok(true)) {
            observed_result = Some(identity);
          }
        }
        if let Ok(metadata) = fs::symlink_metadata(run_dir.join(REQUEST_FILE)) {
          let identity = (metadata.dev(), metadata.ino());
          if observed_request != Some(identity) {
            observed_request = Some(identity);
            let _ = process_request(
              run_dir,
              lease,
              *pid,
              &mut stdout,
              &mut stderr,
              cleanup_groups,
            );
          }
        }
      }
    }
    thread::sleep(Duration::from_millis(25));
  }
}
