//! Local resource holds owned by the same supervisor as the task process.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use expri_core::error::{ExpriError, Result};
use expri_core::resource_requirements::{
  DiskObservation, ResourcePhase, ResourceRequirements, ResourceStatus,
};
use expri_core::run_state::{MAX_STATE_BYTES, update_state};
use serde_json::{Value, json};

use crate::run_logs::groups;
use crate::supervisor::linux::transport::{cancel_requested, hex_digest, process_start, read_json};

const CHECK_INTERVAL: Duration = Duration::from_secs(5);
mod sampling;
use sampling::{Outcome, Sampler};

pub(crate) fn requirements(state: &Value) -> Result<ResourceRequirements> {
  let policy = state
    .get("requirements")
    .cloned()
    .map(serde_json::from_value)
    .transpose()?
    .unwrap_or_default();
  let policy: ResourceRequirements = policy;
  policy.validate()?;
  Ok(policy)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Filesystem {
  device: u64,
  mount: u64,
}

#[derive(Debug)]
struct Reading {
  available: u64,
  filesystem: Filesystem,
}

fn disk_reading(path: &Path) -> io::Result<Reading> {
  let name = CString::new(path.as_os_str().as_bytes())?;
  // Keep an open directory descriptor through the measurement so a mount or
  // rename racing the check cannot combine identities from two filesystems.
  let fd = unsafe {
    libc::open(
      name.as_ptr(),
      libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )
  };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }
  let directory = unsafe { File::from_raw_fd(fd) };
  let metadata = directory.metadata()?;
  let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
  if unsafe { libc::fstatvfs(directory.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
    return Err(io::Error::last_os_error());
  }
  let stats = unsafe { stats.assume_init() };
  let mount = fs::read_to_string(format!("/proc/self/fdinfo/{}", directory.as_raw_fd()))?
    .lines()
    .find_map(|line| {
      line
        .strip_prefix("mnt_id:")
        .and_then(|id| id.trim().parse().ok())
    })
    .ok_or_else(|| io::Error::other("filesystem mount identity is unavailable"))?;
  let current = fs::metadata(path)?;
  if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
    return Err(io::Error::other(
      "disk requirement directory changed during observation",
    ));
  }
  Ok(Reading {
    // f_bavail is available to this worker's UID; f_bfree includes root reserve.
    available: u64::try_from(u128::from(stats.f_bavail).saturating_mul(u128::from(stats.f_frsize)))
      .unwrap_or(u64::MAX),
    filesystem: Filesystem {
      device: metadata.dev(),
      mount,
    },
  })
}

#[derive(Default)]
pub(crate) struct Guard {
  policy: Option<ResourceRequirements>,
  filesystems: BTreeMap<String, Filesystem>,
  held: bool,
  recovering_since: Option<(Instant, String)>,
  next_check: Option<Instant>,
  launch_held: bool,
  sampler: Sampler,
}

impl Guard {
  pub(crate) fn force_check(&mut self) {
    self.next_check = None;
  }

  fn adopt_state(&mut self, state: &Value, policy: &ResourceRequirements) {
    if self.policy.is_none() {
      // A supervisor exec restarts an existing resource hold's dwell.
      self.held = state["resource_paused"] == true;
      for disk in &policy.disk {
        let identity = &state["resource_mounts"][hex_digest(disk.path.as_bytes())];
        if let (Some(device), Some(mount)) =
          (identity["device"].as_u64(), identity["mount"].as_u64())
        {
          self
            .filesystems
            .insert(disk.path.clone(), Filesystem { device, mount });
        }
      }
    }
  }

  fn pending(
    &mut self,
    run_dir: &Path,
    pid: Option<u32>,
    policy: &ResourceRequirements,
    snapshot: &Value,
    deadline: Instant,
  ) -> Result<bool> {
    if self.policy.as_ref() == Some(policy) {
      // A bounded in-flight probe is not an observed resource failure. Keep
      // its prior hold and recovery dwell, and never resume on old readings.
      self.launch_held =
        pid.is_none() || snapshot["resource_paused"] == true || manual_pause(snapshot);
      self.next_check = Some(deadline);
      return Ok(self.launch_held);
    }
    let held = update_state(&run_dir.join("run-state.json"), |state| {
      if requirements(state)? != *policy {
        return Ok(false);
      }
      self.adopt_state(state, policy);
      self.recovering_since = None;
      self
        .filesystems
        .retain(|path, _| policy.disk.iter().any(|disk| &disk.path == path));
      let status = ResourceStatus {
        phase: ResourcePhase::Unavailable,
        blocked: true,
        checked_at: chrono::Utc::now().to_rfc3339(),
        recovery_started_at: None,
        disk: policy
          .disk
          .iter()
          .map(|disk| DiskObservation {
            path: disk.path.clone(),
            available_bytes: None,
            min_free_bytes: disk.min_free_bytes,
            resume_free_bytes: disk.resume_free_bytes,
            phase: ResourcePhase::Unavailable,
            error: Some("Waiting for the first worker filesystem observation".into()),
          })
          .collect(),
      };
      status.validate(policy)?;
      state["manual_paused"] = json!(manual_pause(state));
      state["resource_status"] = serde_json::to_value(status)?;
      state["resource_paused"] = json!(true);
      hold_task(state, pid)?;
      Ok(true)
    })?;
    if held {
      // Waiting for a first sample must block launch, but does not latch an
      // actual shortage. A fresh healthy first result can start immediately.
      self.policy = Some(policy.clone());
      self.next_check = Some(deadline);
      self.launch_held = true;
    } else {
      self.force_check();
    }
    Ok(true)
  }

  fn evaluate(
    &mut self,
    policy: &ResourceRequirements,
    output_dir: &Path,
    now: Instant,
    timestamp: String,
    mut observe: impl FnMut(&str, &Path) -> io::Result<Reading>,
  ) -> ResourceStatus {
    if self.recovering_since.as_ref().is_some_and(|(_, began)| {
      chrono::DateTime::parse_from_rfc3339(began)
        .ok()
        .zip(chrono::DateTime::parse_from_rfc3339(&timestamp).ok())
        .is_some_and(|(began, checked)| began > checked)
    }) {
      // Wall-clock corrections must not publish a recovery start in the
      // future; monotonic time remains the dwell clock after this reset.
      self.recovering_since = None;
    }
    if self.policy.as_ref() != Some(policy) {
      self.policy = Some(policy.clone());
      self.recovering_since = None;
      self
        .filesystems
        .retain(|path, _| policy.disk.iter().any(|disk| &disk.path == path));
    }
    let mut unavailable = false;
    let mut below_minimum = false;
    let mut recovery_ready = true;
    let mut disk = Vec::with_capacity(policy.disk.len());
    for requirement in &policy.disk {
      let path = if requirement.path == "outputs" {
        output_dir
      } else {
        Path::new(&requirement.path)
      };
      let reading = observe(&requirement.path, path).and_then(|reading| {
        let previous = self
          .filesystems
          .insert(requirement.path.clone(), reading.filesystem);
        if previous.is_some_and(|identity| identity != reading.filesystem) {
          Err(io::Error::other(
            "filesystem mount changed; waiting for a stable observation",
          ))
        } else {
          Ok(reading)
        }
      });
      let (available_bytes, error) = match reading {
        Ok(reading) => (Some(reading.available), None),
        Err(error) => {
          let mut detail = String::new();
          for character in error
            .to_string()
            .chars()
            .filter(|character| !character.is_control())
          {
            if detail.len() + character.len_utf8() > 512 {
              break;
            }
            detail.push(character);
          }
          (None, Some(detail))
        }
      };
      unavailable |= available_bytes.is_none();
      below_minimum |= available_bytes.is_some_and(|bytes| bytes < requirement.min_free_bytes);
      recovery_ready &= available_bytes.is_some_and(|bytes| bytes >= requirement.resume_free_bytes);
      disk.push(DiskObservation {
        path: requirement.path.clone(),
        available_bytes,
        min_free_bytes: requirement.min_free_bytes,
        resume_free_bytes: requirement.resume_free_bytes,
        phase: if available_bytes.is_none() {
          ResourcePhase::Unavailable
        } else if available_bytes.is_some_and(|bytes| bytes < requirement.min_free_bytes) {
          ResourcePhase::Blocked
        } else {
          ResourcePhase::Ready
        },
        error,
      });
    }
    if policy.is_empty() {
      self.held = false;
      self.recovering_since = None;
    } else if unavailable || below_minimum {
      self.held = true;
      self.recovering_since = None;
    } else if self.held && recovery_ready {
      let (since, _) = self
        .recovering_since
        .get_or_insert_with(|| (now, timestamp.clone()));
      if now.saturating_duration_since(*since)
        >= Duration::from_secs(policy.recovery_seconds.into())
      {
        self.held = false;
        self.recovering_since = None;
      }
    } else {
      self.recovering_since = None;
    }
    let phase = if unavailable {
      ResourcePhase::Unavailable
    } else if self.held && self.recovering_since.is_some() {
      ResourcePhase::Recovering
    } else if self.held {
      ResourcePhase::Blocked
    } else {
      ResourcePhase::Ready
    };
    for observation in &mut disk {
      if observation.phase == ResourcePhase::Ready && self.held {
        observation.phase = if observation
          .available_bytes
          .is_some_and(|bytes| bytes >= observation.resume_free_bytes)
        {
          ResourcePhase::Recovering
        } else {
          ResourcePhase::Blocked
        };
      }
    }
    ResourceStatus {
      phase,
      blocked: self.held,
      checked_at: timestamp,
      recovery_started_at: self
        .recovering_since
        .as_ref()
        .map(|(_, timestamp)| timestamp.clone()),
      disk,
    }
  }

  /// The caller owns the run lease and, for a live task, this exact unreaped PID.
  pub(crate) fn tick(&mut self, run_dir: &Path, pid: Option<u32>) -> Result<bool> {
    let now = Instant::now();
    if self
      .policy
      .as_ref()
      .is_some_and(ResourceRequirements::is_empty)
      && self.next_check.is_some()
    {
      return Ok(self.launch_held);
    }
    if self.next_check.is_some_and(|deadline| now < deadline)
      && !self.sampler.has_completed(self.policy.as_ref())
    {
      return Ok(self.launch_held);
    }
    let snapshot = read_json(&run_dir.join("run-state.json"), MAX_STATE_BYTES)?;
    let sampled_policy = requirements(&snapshot)?;
    if sampled_policy.is_empty() {
      let manually_paused = manual_pause(&snapshot);
      let needs_update = snapshot["resource_paused"] == true
        || snapshot.get("resource_status").is_some()
        || snapshot.get("resource_mounts").is_some()
        || snapshot["status"] == "pending";
      let held = if needs_update {
        update_state(&run_dir.join("run-state.json"), |state| {
          if requirements(state)? != sampled_policy {
            return Ok(None);
          }
          let manually_paused = manual_pause(state);
          state["manual_paused"] = json!(manually_paused);
          for field in ["resource_status", "resource_paused", "resource_mounts"] {
            state.as_object_mut().unwrap().remove(field);
          }
          if !manually_paused {
            if state["status"] == "paused" {
              if let Some(pid) = pid {
                groups::signal(pid, libc::SIGCONT)?;
              }
              crate::run_control::linux::restore_phase(state);
            } else if state["status"] == "pending" {
              state["status"] = json!("preparing");
            }
          }
          Ok(Some(manually_paused))
        })?
      } else {
        Some(manually_paused)
      };
      let Some(held) = held else {
        self.force_check();
        return Ok(true);
      };
      self.policy = Some(sampled_policy);
      self.held = false;
      self.recovering_since = None;
      self.filesystems.clear();
      self.launch_held = held;
      self.next_check = Some(now);
      return Ok(held);
    }
    let mut readings = match self
      .sampler
      .sample(&sampled_policy, &run_dir.join("outputs"))
    {
      Outcome::Ready(readings) => readings,
      Outcome::Pending { deadline } => {
        return self.pending(run_dir, pid, &sampled_policy, &snapshot, deadline);
      }
    };
    let held = update_state(&run_dir.join("run-state.json"), |state| {
      let policy = requirements(state)?;
      if policy != sampled_policy {
        // A concurrent lifecycle edit must be observed before launch/resume.
        return Ok(None);
      }
      self.adopt_state(state, &policy);
      let status = self.evaluate(
        &policy,
        &run_dir.join("outputs"),
        now,
        chrono::Utc::now().to_rfc3339(),
        |configured, _| {
          readings
            .remove(configured)
            .unwrap_or_else(|| Err(io::Error::other("disk observation is unavailable")))
        },
      );
      let manually_paused = manual_pause(state);
      status.validate(&policy)?;
      state["manual_paused"] = json!(manually_paused);
      state["resource_status"] = serde_json::to_value(&status)?;
      state["resource_paused"] = json!(status.blocked);
      // Retain mount identity across a supervisor exec. Hashing keys avoids
      // duplicating every potentially long path in the bounded run metadata.
      state["resource_mounts"] = json!(
        self
          .filesystems
          .iter()
          .map(|(path, identity)| {
            (
              hex_digest(path.as_bytes()),
              json!({"device":identity.device,"mount":identity.mount}),
            )
          })
          .collect::<BTreeMap<_, _>>()
      );
      match pid {
        Some(pid) => {
          let paused = state["status"] == "paused";
          if status.blocked || manually_paused {
            hold_task(state, Some(pid))?;
          } else if paused {
            groups::signal(pid, libc::SIGCONT)?;
            crate::run_control::linux::restore_phase(state);
          }
        }
        None => {
          state["status"] = json!(if status.blocked || manually_paused {
            "pending"
          } else {
            "preparing"
          });
        }
      }
      Ok(Some(status.blocked || manually_paused))
    })?;
    let Some(held) = held else {
      self.force_check();
      return Ok(true);
    };
    self.next_check = Some(now + CHECK_INTERVAL);
    self.launch_held = held;
    Ok(held)
  }
}

fn manual_pause(state: &Value) -> bool {
  state["manual_paused"]
    .as_bool()
    .unwrap_or(state["status"] == "paused" && state["resource_paused"] != true)
}

fn hold_task(state: &mut Value, pid: Option<u32>) -> Result<()> {
  match pid {
    Some(pid) if state["status"] != "paused" => {
      // The caller holds the state lease before freezing its owned child.
      groups::signal(pid, libc::SIGSTOP)?;
      state["paused_previous_status"] = state["status"].clone();
      state["status"] = json!("paused");
      crate::run_control::linux::set_attempt_phase(state, "paused");
    }
    None => state["status"] = json!("pending"),
    _ => {}
  }
  Ok(())
}

/// Prelaunch uses the existing run lease, not a second daemon or hosted loop.
/// Lifecycle requests remain responsive while no child process exists.
pub(crate) fn wait_before_start(run_dir: &Path) -> Result<bool> {
  update_state(&run_dir.join("run-state.json"), |state| {
    if state["supervisor"]["owner_pid"] != std::process::id() {
      let generation = state["supervisor"]["generation"]
        .as_u64()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| ExpriError::Message("supervisor generation exhausted".into()))?;
      state["supervisor"]["generation"] = json!(generation);
    }
    state["supervisor"]["worker_pid"] = Value::Null;
    state["supervisor"]["owner_pid"] = json!(std::process::id());
    state["supervisor"]["owner_start"] = json!(process_start(std::process::id())?);
    state["supervisor"]["protocol_version"] = json!(1);
    state["supervisor"]["mode"] = json!("live");
    if !state["supervisor"]["events"].is_array() {
      state["supervisor"]["events"] = json!([]);
    }
    state["run_control_protocol_version"] = json!(1);
    state["run_requirements_protocol_version"] = json!(1);
    state["task_process_group_clean"] = json!(true);
    Ok(())
  })?;
  let mut guard = Guard::default();
  let mut observed_lifecycle = None;
  loop {
    if cancel_requested(run_dir)? {
      return Ok(false);
    }
    if let Ok(metadata) = fs::symlink_metadata(run_dir.join(crate::run_control::linux::MAILBOX)) {
      let identity = (metadata.dev(), metadata.ino());
      if observed_lifecycle != Some(identity) {
        observed_lifecycle = Some(identity);
        if crate::run_control::linux::process_pending(run_dir)?.is_some() {
          return Ok(false);
        }
        guard.force_check();
      }
    }
    if !guard.tick(run_dir, None)? {
      return Ok(!cancel_requested(run_dir)?);
    }
    thread::sleep(Duration::from_millis(25));
  }
}

#[cfg(test)]
mod tests;
