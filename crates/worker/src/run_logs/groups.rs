//! Queue-only process-group cleanup. The direct child stays unreaped throughout.

use std::io;
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

pub(super) fn stop(child: &mut Child) -> io::Result<()> {
  verify_parent()?;
  let deadline = Instant::now() + Duration::from_secs(5);
  let mut empty_observations = 0;
  loop {
    // ECHILD means ownership was lost; never signal that numeric ID afterward.
    super::child_exit_pending(child)?;
    // Its unreaped leader reserves this group ID until our last signal.
    super::signal_group(child, super::StopSignal::Kill)?;
    if super::child_exit_pending(child)? && !has_live_members(child.id(), child.id())? {
      empty_observations += 1;
      if empty_observations >= 2 {
        return Ok(());
      }
    } else {
      empty_observations = 0;
    }
    if Instant::now() >= deadline {
      return Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "queued process-group cleanup could not confirm all members stopped",
      ));
    }
    thread::sleep(Duration::from_millis(25));
  }
}

#[cfg(unix)]
pub(super) fn verify_parent() -> io::Result<()> {
  let mut disposition: libc::sigaction = unsafe { std::mem::zeroed() };
  // SAFETY: Reading the current action does not alter process signal handling.
  if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut disposition) } < 0 {
    return Err(io::Error::last_os_error());
  }
  if disposition.sa_sigaction != libc::SIG_DFL || disposition.sa_flags & libc::SA_NOCLDWAIT != 0 {
    return Err(io::Error::other(
      "queued cleanup requires default SIGCHLD handling so child IDs remain reserved",
    ));
  }
  Ok(())
}

#[cfg(not(unix))]
pub(super) fn verify_parent() -> io::Result<()> {
  Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "queued process-group verification requires Unix",
  ))
}

#[cfg(target_os = "linux")]
fn has_live_members(group_id: u32, leader_id: u32) -> io::Result<bool> {
  for entry in std::fs::read_dir("/proc")? {
    let entry = entry?;
    let Some(pid) = entry
      .file_name()
      .to_str()
      .and_then(|name| name.parse::<u32>().ok())
    else {
      continue;
    };
    if pid == leader_id {
      continue;
    }
    let stat = match std::fs::read(entry.path().join("stat")) {
      Ok(stat) => stat,
      Err(error)
        if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) =>
      {
        continue;
      }
      Err(error) => return Err(error),
    };
    let Some((state, group)) = linux_group_state(&stat) else {
      return Err(io::Error::other(
        "cannot inspect queued process-group membership",
      ));
    };
    if group == group_id && !matches!(state, "Z" | "X" | "x") {
      return Ok(true);
    }
  }
  Ok(false)
}

#[cfg(any(target_os = "linux", test))]
fn linux_group_state(stat: &[u8]) -> Option<(&str, u32)> {
  // comm is parenthesized and may itself contain spaces or ')'.
  let close = stat.iter().rposition(|byte| *byte == b')')?;
  let suffix = std::str::from_utf8(&stat[close + 1..]).ok()?;
  let mut fields = suffix.split_whitespace();
  let state = fields.next()?;
  fields.next()?;
  Some((state, fields.next()?.parse().ok()?))
}

#[cfg(target_os = "macos")]
fn has_live_members(group_id: u32, leader_id: u32) -> io::Result<bool> {
  const PROC_PGRP_ONLY: u32 = 2; // sys/proc_info.h
  let mut capacity = 128_usize;
  let pids = loop {
    let mut pids = vec![0_i32; capacity];
    let bytes = (pids.len() * std::mem::size_of::<i32>()) as i32;
    // SAFETY: libproc writes at most the supplied length into an aligned buffer.
    let actual = unsafe {
      *libc::__error() = 0;
      libc::proc_listpids(PROC_PGRP_ONLY, group_id, pids.as_mut_ptr().cast(), bytes)
    };
    if actual < 0 || actual == 0 && io::Error::last_os_error().raw_os_error() != Some(0) {
      return Err(io::Error::last_os_error());
    }
    if !(actual as usize).is_multiple_of(std::mem::size_of::<i32>()) || actual > bytes {
      return Err(io::Error::other("invalid queued process-group inventory"));
    }
    if actual < bytes {
      pids.truncate(actual as usize / std::mem::size_of::<i32>());
      break pids;
    }
    capacity *= 2;
    if capacity > 65_536 {
      return Err(io::Error::other(
        "queued process group exceeds its inspection limit",
      ));
    }
  };
  for pid in pids {
    if pid <= 0 || pid as u32 == leader_id {
      continue;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: info is a valid writable buffer with the ABI-declared size.
    let actual = unsafe {
      *libc::__error() = 0;
      libc::proc_pidinfo(
        pid,
        libc::PROC_PIDTBSDINFO,
        0,
        (&mut info as *mut libc::proc_bsdinfo).cast(),
        size,
      )
    };
    if actual != size {
      let error = io::Error::last_os_error();
      if error.raw_os_error() == Some(libc::ESRCH) {
        continue;
      }
      return Err(io::Error::other(
        "cannot inspect queued process-group member",
      ));
    }
    if info.pbi_pgid == group_id && info.pbi_status != libc::SZOMB {
      return Ok(true);
    }
  }
  Ok(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn has_live_members(_: u32, _: u32) -> io::Result<bool> {
  Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "queued process-group verification requires Linux or macOS",
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn proc_stat_group_parser_handles_parentheses_and_zombies() {
    assert_eq!(
      linux_group_state(b"123 (name ) with spaces) S 1 456 0"),
      Some(("S", 456))
    );
    assert_eq!(
      linux_group_state(b"123 (sleep) Z 1 456 0"),
      Some(("Z", 456))
    );
    assert_eq!(
      linux_group_state(b"123 (non-utf8\xff) S 1 456 0"),
      Some(("S", 456))
    );
    assert_eq!(linux_group_state(b"malformed"), None);
  }
}
