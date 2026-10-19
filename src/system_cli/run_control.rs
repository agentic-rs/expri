//! Bounded polling and execution inside the shared worker service.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
  Arc, RwLock,
  atomic::{AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use expri_client::RunControlClient;
use expri_client::publishing::PreparedJob;
use expri_client::worker_service::{ServiceContext, registered_runs};
use expri_core::error::Result;
use expri_core::run_control::{
  ControlCommand, ControlResult, MAX_CONTROL_POLLS, RunControlResponse,
};
use expri_core::storage::StoragePaths;
use serde_json::{Value, json};

const LANES: usize = 8;
const MAX_TRANSITIONS: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

struct TransitionPermit(Arc<AtomicUsize>);

impl TransitionPermit {
  fn acquire(active: &Arc<AtomicUsize>) -> Option<Self> {
    let mut count = active.load(Ordering::Acquire);
    loop {
      if count >= MAX_TRANSITIONS {
        return None;
      }
      match active.compare_exchange_weak(count, count + 1, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => return Some(Self(Arc::clone(active))),
        Err(current) => count = current,
      }
    }
  }
}

impl Drop for TransitionPermit {
  fn drop(&mut self) {
    self.0.fetch_sub(1, Ordering::AcqRel);
  }
}

struct Execution {
  request_id: String,
  thread: JoinHandle<ControlResult>,
}

struct Registration {
  job: PreparedJob,
  client: Option<RunControlClient>,
  retry_at: Instant,
  execution: Option<Execution>,
  result: Option<ControlResult>,
  facts: Option<Value>,
  transitions: Arc<AtomicUsize>,
  failures: u32,
}

struct Transport {
  client: Option<RunControlClient>,
  retry_at: Instant,
}

impl Registration {
  fn new(job: PreparedJob, transitions: Arc<AtomicUsize>) -> Self {
    Self {
      job,
      client: None,
      retry_at: Instant::now(),
      execution: None,
      result: None,
      facts: None,
      transitions,
      failures: 0,
    }
  }

  fn collect_result(&mut self) {
    if self
      .execution
      .as_ref()
      .is_some_and(|task| task.thread.is_finished())
    {
      let task = self.execution.take().unwrap();
      self.result = Some(task.thread.join().unwrap_or_else(|_| {
        failure(
          &task.request_id,
          "Worker lifecycle operation was interrupted",
        )
      }));
    }
  }

  fn prepare(
    &mut self,
    binding_id: &str,
    context: &ServiceContext,
    transports: &mut BTreeMap<String, Transport>,
  ) -> bool {
    self.collect_result();
    if Instant::now() < self.retry_at {
      return false;
    }
    // Never extend the server's online lease using an unreadable run's old facts.
    self.facts = expri_worker::run_control::inspect(self.job.run_dir()).ok();
    let Some(facts) = &self.facts else {
      return false;
    };
    if facts["protocol_version"] != 1 && self.result.is_none() && self.execution.is_none() {
      return false;
    }
    if self.client.is_none() {
      let Ok(destination) = self.job.run_control_destination() else {
        return false;
      };
      let transport = transports.entry(destination).or_insert(Transport {
        client: None,
        retry_at: Instant::now(),
      });
      if transport.client.is_none() && Instant::now() >= transport.retry_at {
        match self.job.run_control_client(context.stop_flag()) {
          Ok(Some(client)) => transport.client = Some(client),
          Ok(None) => transport.retry_at = Instant::now() + Duration::from_secs(60),
          Err(_) => transport.retry_at = Instant::now() + Duration::from_secs(5),
        }
      }
      self.client = transport
        .client
        .as_ref()
        .map(|client| client.for_run(self.job.scope().clone(), binding_id.into()));
    }
    if let Some(client) = &self.client
      && client.validate_facts(facts).is_err()
    {
      self.unavailable();
      return false;
    }
    self.client.is_some()
  }

  fn acknowledgement(&self) -> Option<ControlResult> {
    self
      .execution
      .as_ref()
      .map(|task| ControlResult {
        request_id: task.request_id.clone(),
        status: "executing".into(),
        error: None,
        new_run_id: None,
      })
      .or_else(|| self.result.clone())
  }

  fn unavailable(&mut self) {
    self.failures = self.failures.saturating_add(1);
    self.retry_at = Instant::now() + Duration::from_secs((1u64 << self.failures.min(5)).min(30));
  }

  fn accept(&mut self, binding_id: &str, response: RunControlResponse) {
    self.failures = 0;
    self.retry_at = Instant::now();
    // Keep a final acknowledgement until an HTTP success confirms its receipt.
    self.result = None;
    if let Some(command) = response.command
      && self.execution.is_none()
    {
      if command.binding_id != binding_id {
        self.result = Some(failure(
          &command.request_id,
          "Lifecycle destination binding changed",
        ));
        return;
      }
      self.start(command);
    }
  }

  fn start(&mut self, command: ControlCommand) {
    let Some(permit) = TransitionPermit::acquire(&self.transitions) else {
      // Leave the delivered command available for the next poll. Its delivery
      // deadline still applies until the worker actually accepts it.
      return;
    };
    let request_id = command.request_id.clone();
    let run_dir = self.job.run_dir().to_path_buf();
    let job = self.job.clone();
    match thread::Builder::new()
      .name("expri-task-transition".into())
      .spawn(move || {
        let _permit = permit;
        let mut result = expri_worker::run_control::execute(&run_dir, &command)
          .unwrap_or_else(|error| failure(&command.request_id, &job.redact(&error.to_string())));
        result.error = result.error.map(|error| job.redact(&error));
        result
      }) {
      Ok(thread) => self.execution = Some(Execution { request_id, thread }),
      Err(_) => {
        self.result = Some(failure(
          &request_id,
          "Worker could not start the lifecycle operation",
        ))
      }
    }
  }

  fn drain(self) {
    if let Some(execution) = self.execution {
      // A component restart never interrupts an already accepted transition.
      // Its durable worker journal can acknowledge the result after startup.
      let _ = execution.thread.join();
    }
  }
}

fn failure(request_id: &str, error: &str) -> ControlResult {
  ControlResult {
    request_id: request_id.into(),
    status: "failed".into(),
    error: Some(
      error
        .chars()
        .filter(|ch| !ch.is_control())
        .take(512)
        .collect(),
    ),
    new_run_id: None,
  }
}

fn lane(binding_id: &str) -> usize {
  // Stable assignment preserves HTTP pools and in-flight transitions on rescans.
  binding_id
    .bytes()
    .fold(0, |value, byte| (value * 17 + byte as usize) % LANES)
}

pub(super) fn run(paths: &StoragePaths, context: ServiceContext) -> Result<()> {
  let registrations = Arc::new(RwLock::new(Vec::<PreparedJob>::new()));
  let transitions = Arc::new(AtomicUsize::new(0));
  thread::scope(|scope| {
    for index in 0..LANES {
      let registrations = Arc::clone(&registrations);
      let context = context.clone();
      let transitions = Arc::clone(&transitions);
      scope.spawn(move || run_lane(index, registrations, transitions, context));
    }
    while !context.is_stopping() {
      match registered_runs(paths) {
        Ok(jobs) => {
          let count = jobs.len();
          *registrations
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = jobs;
          context.component_status(json!({"status":"running","ready":true,
            "pending_jobs":count,"active_jobs":transitions.load(Ordering::Acquire),"last_error":null}));
        }
        Err(_) => {
          registrations
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
          context.component_status(json!({"status":"running","ready":false,
            "last_error":"Run-control registrations could not be read"}));
        }
      }
      context.wait(Duration::from_secs(5));
    }
  });
  Ok(())
}

fn run_lane(
  index: usize,
  registrations: Arc<RwLock<Vec<PreparedJob>>>,
  transitions: Arc<AtomicUsize>,
  context: ServiceContext,
) {
  let mut sessions = BTreeMap::<String, Registration>::new();
  let mut transports = BTreeMap::<String, Transport>::new();
  while !context.is_stopping() {
    let jobs = registrations
      .read()
      .unwrap_or_else(|poison| poison.into_inner())
      .clone();
    let mut present = BTreeSet::new();
    for job in jobs {
      let Ok(identity) = job.identity() else {
        continue;
      };
      if lane(&identity) != index {
        continue;
      }
      present.insert(identity.clone());
      match sessions.entry(identity) {
        std::collections::btree_map::Entry::Occupied(mut entry) => {
          let registration = entry.get_mut();
          if registration.job != job {
            registration.job = job;
            registration.client = None;
            registration.retry_at = Instant::now();
          }
        }
        std::collections::btree_map::Entry::Vacant(entry) => {
          entry.insert(Registration::new(job, Arc::clone(&transitions)));
        }
      }
    }
    for registration in sessions.values_mut() {
      registration.collect_result();
    }
    let removed: Vec<_> = sessions
      .keys()
      .filter(|id| !present.contains(*id) && sessions[*id].execution.is_none())
      .cloned()
      .collect();
    for id in removed {
      sessions.remove(&id).unwrap().drain();
    }
    let mut groups = BTreeMap::<String, Vec<String>>::new();
    let mut destinations = BTreeSet::new();
    for (identity, registration) in &mut sessions {
      if context.is_stopping() {
        break;
      }
      // A disappearing registration cannot receive commands. Keep its accepted
      // transition in memory until it finishes, without blocking other runs.
      if !present.contains(identity) {
        continue;
      }
      if let Ok(destination) = registration.job.run_control_destination() {
        destinations.insert(destination.clone());
        if registration.prepare(identity, &context, &mut transports) {
          groups
            .entry(destination)
            .or_default()
            .push(identity.clone());
        }
      }
    }
    transports.retain(|destination, _| destinations.contains(destination));
    for identities in groups.into_values() {
      for batch in identities.chunks(MAX_CONTROL_POLLS) {
        if context.is_stopping() {
          break;
        }
        let responses = {
          let entries: Vec<_> = batch
            .iter()
            .map(|id| {
              let registration = &sessions[id];
              (
                registration.client.as_ref().unwrap(),
                registration.facts.as_ref().unwrap(),
                registration.acknowledgement(),
              )
            })
            .collect();
          entries[0].0.poll_batch(&entries)
        };
        match responses {
          Ok(responses) => {
            for (identity, response) in batch.iter().zip(responses) {
              let registration = sessions.get_mut(identity).unwrap();
              if let Some(response) = response.response {
                registration.accept(identity, response);
              } else {
                registration.unavailable();
              }
            }
          }
          Err(_) => {
            for identity in batch {
              sessions.get_mut(identity).unwrap().unavailable();
            }
          }
        }
      }
    }
    context.wait(POLL_INTERVAL);
  }
  for registration in sessions.into_values() {
    registration.drain();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn errors_never_expose_unbounded_control_characters() {
    let result = failure("request-1", &("a\n".repeat(600)));
    assert_eq!(result.request_id, "request-1");
    assert_eq!(result.error.unwrap(), "a".repeat(512));
  }
}
