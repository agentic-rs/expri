//! Journal the publication CAS before changing the worker's local epoch.

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const INTENT: &str = ".publishing-continuation.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
  request_id: String,
  expected_attempt: u64,
  next_attempt: u64,
  request: Request,
  complete: bool,
  #[serde(default)]
  abandoned: bool,
}

fn read_intent(path: &Path) -> Result<Option<Intent>> {
  if fs::inspect(path)?.is_none() {
    return Ok(None);
  }
  let intent: Intent = serde_json::from_slice(&fs::read_bounded(path, RECORD_LIMIT)?)?;
  let Request::BeginRunAttempt {
    request_id,
    attempt,
    expected_epoch,
    documents,
    streams,
    ..
  } = &intent.request
  else {
    return Err(message("invalid saved publication continuation"));
  };
  validate_component(&intent.request_id)?;
  if *request_id != intent.request_id
    || *attempt != intent.next_attempt
    || intent.expected_attempt == 0
    || intent.next_attempt <= intent.expected_attempt
    || intent.next_attempt > expri_core::protocol::MAX_TASK_ATTEMPTS as u64
    || *expected_epoch > i64::MAX as u64
    || (intent.abandoned && !intent.complete)
    || documents.len() > expri_core::service::TRACKING_DOCUMENT_LIMIT
    || streams.len() > STREAMS.len()
  {
    return Err(message(
      "saved publication continuation identity is inconsistent",
    ));
  }
  Ok(Some(intent))
}

/// A subsequent explicit continuation can retire an uncertain failed attempt.
/// The server serializes its tombstone with Begin, so a delayed old RPC cannot
/// later allocate an epoch after we have made a fresh publication snapshot.
pub(crate) fn reconcile_task_attempt(options: &PublishOptions, api: Api) -> Result<bool> {
  let path = options.run_dir.join(INTENT);
  let Some(mut intent) = read_intent(&path)? else {
    return Ok(false);
  };
  if intent.complete {
    return Ok(false);
  }
  let mut publisher = Publisher::new_with_api(options, api)?;
  let Request::BeginRunAttempt {
    scope,
    expected_epoch,
    ..
  } = &intent.request
  else {
    return Err(message("invalid saved publication continuation"));
  };
  if *scope != publisher.scope {
    return Err(message(
      "saved publication continuation belongs to another run",
    ));
  }
  let Response::ReconciledRunAttempt { committed_epoch } =
    publisher.api.request(&Request::ReconcileRunAttempt {
      scope: scope.clone(),
      request_id: intent.request_id.clone(),
      expected_epoch: *expected_epoch,
    })?
  else {
    return Err(message(
      "service did not reconcile task continuation publication",
    ));
  };
  match committed_epoch {
    Some(epoch) => finalize_committed(&mut publisher, &mut intent, epoch).map(|_| true),
    None => {
      if publisher.api.publication_epoch != *expected_epoch {
        return Err(message(
          "local publication epoch changed before abandoning task continuation",
        ));
      }
      intent.abandoned = true;
      intent.complete = true;
      fs::atomic_json(&path, &intent).map(|_| true)
    }
  }
}

pub(crate) fn check_task_attempt_support(api: &Api, scope: &RunScope) -> Result<()> {
  let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
    return Err(message(
      "service returned invalid task continuation capabilities",
    ));
  };
  if !features
    .iter()
    .any(|feature| feature == "run-task-continuation-v1")
  {
    return Err(message(
      "task continuation requires an upgraded expri service with run-task-continuation-v1",
    ));
  }
  let Response::RunAttempt { .. } = api.request(&Request::CheckRunContinuation {
    scope: scope.clone(),
  })?
  else {
    return Err(message(
      "service returned invalid task continuation preflight",
    ));
  };
  Ok(())
}

pub(crate) fn begin_task_attempt(
  options: &PublishOptions,
  api: Api,
  request_id: &str,
  expected_attempt: u64,
  next_attempt: u64,
) -> Result<()> {
  validate_component(request_id)?;
  if next_attempt <= expected_attempt
    || next_attempt > expri_core::protocol::MAX_TASK_ATTEMPTS as u64
  {
    return Err(message("task continuation attempt changed"));
  }
  let path = options.run_dir.join(INTENT);
  let mut saved = read_intent(&path)?;
  if saved
    .as_ref()
    .is_some_and(|saved| !saved.complete && saved.request_id != request_id)
  {
    reconcile_task_attempt(options, api.clone())?;
    saved = read_intent(&path)?;
  }
  let mut publisher = Publisher::new_with_api(options, api)?;
  check_task_attempt_support(&publisher.api, &publisher.scope)?;
  let mut intent = match saved {
    Some(saved) if saved.request_id == request_id => {
      if saved.abandoned {
        return Err(message(
          "this task continuation publication was abandoned; use a new request ID",
        ));
      }
      if saved.expected_attempt != expected_attempt || saved.next_attempt != next_attempt {
        return Err(message(
          "continuation request ID was already used with another attempt",
        ));
      }
      saved
    }
    Some(saved) if !saved.complete => {
      return Err(message(format!(
        "publication continuation {} is pending; retry that request first",
        saved.request_id
      )));
    }
    _ => {
      // Publish the stopped attempt's final prefix before allocating a new epoch.
      // Its task is already reaped and the caller retains the run lifetime lease.
      let sealed = sealed_snapshot(&publisher.api, &publisher.scope)?;
      if sealed.is_none() {
        publisher.flush_for_continuation()?;
      }
      if publisher.queue.state.protocol != Some(queue::Protocol::TrackingV1) {
        return Err(message(
          "task continuation requires tracking-v1 publication",
        ));
      }
      if sealed.is_none()
        && publisher
          .queue
          .state
          .documents
          .values()
          .any(|document| !document.complete)
      {
        return Err(message(
          "previous task attempt metadata is not fully acknowledged",
        ));
      }
      let (documents, streams) = sealed.unwrap_or_else(|| {
        (
          publisher
            .queue
            .state
            .documents
            .iter()
            .map(|(path, document)| (path.clone(), document.revision))
            .collect(),
          publisher.queue.state.streams.clone(),
        )
      });
      let intent = Intent {
        request_id: request_id.into(),
        expected_attempt,
        next_attempt,
        complete: false,
        abandoned: false,
        request: Request::BeginRunAttempt {
          scope: publisher.scope.clone(),
          request_id: request_id.into(),
          expected_epoch: publisher.api.publication_epoch,
          attempt: next_attempt,
          documents,
          streams,
        },
      };
      fs::atomic_json(&path, &intent)?;
      intent
    }
  };
  let Response::RunAttempt { epoch } = publisher.api.request(&intent.request)? else {
    return Err(message("service did not acknowledge task continuation"));
  };
  finalize_committed(&mut publisher, &mut intent, epoch)
}

fn finalize_committed(publisher: &mut Publisher, intent: &mut Intent, epoch: u64) -> Result<()> {
  let Request::BeginRunAttempt { expected_epoch, .. } = &intent.request else {
    return Err(message("invalid saved publication continuation"));
  };
  if expected_epoch.checked_add(1) != Some(epoch) {
    return Err(message(
      "service returned an unexpected task continuation epoch",
    ));
  }
  expri_core::run_state::update_state(&publisher.run_dir.join("run-state.json"), |state| {
    let current = state["publication_epoch"].as_u64().unwrap_or(0);
    if current != *expected_epoch && current != epoch {
      return Err(message(
        "local publication epoch changed during continuation",
      ));
    }
    state["publication_epoch"] = json!(epoch);
    Ok(())
  })?;
  if !intent.complete {
    if let Some(archive) = publisher.queue.state.archive.take() {
      publisher.queue.state.archive_history.push(archive);
    }
    publisher.queue.save()?;
    intent.complete = true;
    fs::atomic_json(&publisher.run_dir.join(INTENT), &intent)?;
  }
  Ok(())
}

type Snapshot = (BTreeMap<String, u64>, BTreeMap<String, u64>);

fn sealed_snapshot(api: &Api, scope: &RunScope) -> Result<Option<Snapshot>> {
  let Response::Files { files } = api.request(&Request::ListFiles {
    scope: scope.clone(),
  })?
  else {
    return Err(message("service did not return its tracking catalog"));
  };
  if files.len() > 1000 {
    return Err(message("service file catalog exceeds its size limit"));
  }
  let mut documents = BTreeMap::new();
  let mut streams = BTreeMap::new();
  let mut sealed = false;
  for file in files {
    download::validate_record(&file)?;
    let FileTarget::Run {
      scope: returned,
      path,
    } = file.target
    else {
      return Err(message("tracking catalog contains private inputs"));
    };
    if returned != *scope {
      return Err(message("tracking catalog belongs to another run"));
    }
    if let FileStorage::Tracking {
      revision,
      sealed: closed,
    } = file.storage
    {
      if document_path(&path) {
        documents.insert(path, revision);
      } else if stream_path(&path) {
        sealed |= closed;
        streams.insert(path, file.size);
      }
    }
  }
  Ok(sealed.then_some((documents, streams)))
}
