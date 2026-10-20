//! Bounded filesystem probes; a wedged former path cannot prevent policy repair.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use expri_core::resource_requirements::ResourceRequirements;

use super::{CHECK_INTERVAL, Reading, disk_reading};

const SAMPLE_WAIT: Duration = Duration::from_millis(100);
const MAX_GUARD_SAMPLERS: usize = 4;
const MAX_PROCESS_SAMPLERS: usize = 8;

type Readings = BTreeMap<String, io::Result<Reading>>;

struct Completed {
  started_at: Instant,
  completed_at: Instant,
  readings: Readings,
}

struct Sampling {
  paths: Vec<String>,
  started_at: Instant,
  receiver: Receiver<Completed>,
  completed: Option<Completed>,
}

struct Budget {
  active: AtomicUsize,
  limit: usize,
}

struct Permit(Arc<Budget>);

impl Permit {
  fn acquire(budget: &Arc<Budget>) -> Option<Self> {
    budget
      .active
      .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |active| {
        (active < budget.limit).then_some(active + 1)
      })
      .ok()
      .map(|_| Self(budget.clone()))
  }
}

impl Drop for Permit {
  fn drop(&mut self) {
    self.0.active.fetch_sub(1, Ordering::AcqRel);
  }
}

pub(super) struct Sampler {
  pending: Vec<Sampling>,
  budget: Arc<Budget>,
}

pub(super) enum Outcome {
  Ready(Readings),
  Pending { deadline: Instant },
}

impl Default for Sampler {
  fn default() -> Self {
    static BUDGET: OnceLock<Arc<Budget>> = OnceLock::new();
    Self {
      pending: Vec::new(),
      budget: BUDGET
        .get_or_init(|| {
          Arc::new(Budget {
            active: AtomicUsize::new(0),
            limit: MAX_PROCESS_SAMPLERS,
          })
        })
        .clone(),
    }
  }
}

fn paths(policy: &ResourceRequirements) -> Vec<String> {
  let mut paths: Vec<_> = policy.disk.iter().map(|disk| disk.path.clone()).collect();
  paths.sort();
  paths
}

impl Sampler {
  fn drain(&mut self, requested: &[String]) -> bool {
    let mut ready = false;
    self.pending.retain_mut(|sampling| {
      if sampling.completed.is_none() {
        match sampling.receiver.try_recv() {
          Ok(completed) => sampling.completed = Some(completed),
          Err(mpsc::TryRecvError::Disconnected) => {
            sampling.completed = Some(Completed {
              started_at: Instant::now(),
              completed_at: Instant::now(),
              readings: unavailable(
                &sampling.paths,
                "disk observation worker exited before returning its result",
              ),
            })
          }
          Err(mpsc::TryRecvError::Empty) => {}
        }
      }
      if sampling.completed.is_some() {
        if sampling.paths == requested {
          ready = true;
          true
        } else {
          false
        }
      } else {
        true
      }
    });
    ready
  }

  /// Called even between five-second probes, so a slow successful observation
  /// is consumed promptly rather than becoming stale behind the poll timer.
  pub(super) fn has_completed(&mut self, policy: Option<&ResourceRequirements>) -> bool {
    self.drain(&policy.map(paths).unwrap_or_default())
  }

  pub(super) fn sample(&mut self, policy: &ResourceRequirements, output_dir: &Path) -> Outcome {
    let paths = paths(policy);
    self.drain(&paths);
    if paths.is_empty() {
      return Outcome::Ready(BTreeMap::new());
    }
    if let Some(index) = self
      .pending
      .iter()
      .position(|sampling| sampling.paths == paths)
    {
      return self.receive(index, &paths);
    }
    match self.start(paths.clone(), output_dir, disk_reading) {
      Ok(index) => self.receive(index, &paths),
      Err(detail) => Outcome::Ready(unavailable(&paths, detail)),
    }
  }

  fn start(
    &mut self,
    paths: Vec<String>,
    output_dir: &Path,
    observe: impl Fn(&Path) -> io::Result<Reading> + Send + 'static,
  ) -> Result<usize, &'static str> {
    if self.pending.len() >= MAX_GUARD_SAMPLERS {
      return Err(
        "disk observation capacity is exhausted by stalled checks for previous paths; waiting for a check to finish",
      );
    }
    let Some(permit) = Permit::acquire(&self.budget) else {
      return Err(
        "disk observation capacity is exhausted by stalled checks in this supervisor; waiting for a check to finish",
      );
    };
    let (sender, receiver) = mpsc::channel();
    let requested = paths.clone();
    let output_dir = output_dir.to_owned();
    if thread::Builder::new()
      .name("expri-disk-check".into())
      .spawn(move || {
        // The process-wide permit stays with the thread even if its Guard is
        // replaced for another task attempt. Dropping a receiver cannot stop a
        // filesystem syscall, and must not make its capacity available again.
        let _permit = permit;
        let started_at = Instant::now();
        let readings = requested
          .into_iter()
          .map(|path| {
            let resolved = if path == "outputs" {
              output_dir.as_path()
            } else {
              Path::new(&path)
            };
            let reading = observe(resolved);
            (path, reading)
          })
          .collect();
        let _ = sender.send(Completed {
          started_at,
          completed_at: Instant::now(),
          readings,
        });
      })
      .is_err()
    {
      return Err("disk observation worker could not start");
    }
    self.pending.push(Sampling {
      paths: paths.clone(),
      started_at: Instant::now(),
      receiver,
      completed: None,
    });
    Ok(self.pending.len() - 1)
  }

  fn receive(&mut self, index: usize, paths: &[String]) -> Outcome {
    let sampling = &mut self.pending[index];
    if sampling.completed.is_none() {
      match sampling.receiver.recv_timeout(SAMPLE_WAIT) {
        Ok(completed) => sampling.completed = Some(completed),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
          self.pending.remove(index);
          return Outcome::Ready(unavailable(
            paths,
            "disk observation worker exited before returning its result",
          ));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
          let deadline = sampling.started_at + CHECK_INTERVAL;
          return if Instant::now() < deadline {
            Outcome::Pending { deadline }
          } else {
            Outcome::Ready(unavailable(
              paths,
              "disk observation timed out; waiting for the worker filesystem",
            ))
          };
        }
      }
    }
    let completed = self.pending.remove(index).completed.unwrap();
    if completed.completed_at.elapsed() > CHECK_INTERVAL
      || completed
        .completed_at
        .saturating_duration_since(completed.started_at)
        > CHECK_INTERVAL
    {
      Outcome::Ready(unavailable(
        paths,
        "disk observation became stale; waiting for a fresh filesystem check",
      ))
    } else {
      Outcome::Ready(completed.readings)
    }
  }
}

fn unavailable(paths: &[String], detail: &str) -> Readings {
  paths
    .iter()
    .map(|path| {
      (
        path.clone(),
        Err(io::Error::new(io::ErrorKind::TimedOut, detail.to_owned())),
      )
    })
    .collect()
}

#[cfg(test)]
mod tests;
