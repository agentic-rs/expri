//! Lifecycle transport bound to the run's immutable publishing destination.

use std::sync::{Arc, atomic::AtomicBool};

use expri_core::resource_requirements::{
  RUN_REQUIREMENTS_CAPABILITY, ResourceRequirements, ResourceStatus,
};
use expri_core::run_control::{
  ControlAction, ControlLifecycle, ControlPollResult, ControlResult, MAX_CONTROL_POLLS,
  RunControlRequest, RunControlResponse,
};
use expri_core::service::{Request, Response, RunScope};
use serde::Deserialize;
use serde_json::Value;

use super::http::{Api, ResolvedClient};
use crate::error::Result;

/// Public facts and explicitly configured requirement paths, without credentials.
#[derive(Deserialize)]
struct Facts {
  #[serde(alias = "current_attempt")]
  attempt: u64,
  generation: u64,
  status: String,
  capabilities: Vec<ControlAction>,
  command: Vec<String>,
  checkpoints: Vec<String>,
  lifecycle: Option<ControlLifecycle>,
  #[serde(default)]
  requirements: Option<ResourceRequirements>,
  #[serde(default)]
  resource_status: Option<ResourceStatus>,
  #[serde(default)]
  manual_paused: Option<bool>,
}

/// Reuses one HTTP pool without exposing its saved bearer credential.
#[derive(Clone)]
pub struct RunControlClient {
  api: Api,
  scope: RunScope,
  binding_id: String,
  batch: bool,
  requirements: bool,
}

impl RunControlClient {
  pub(crate) fn new(
    client: &ResolvedClient,
    scope: RunScope,
    binding_id: String,
    stop: Arc<AtomicBool>,
  ) -> Result<Option<Self>> {
    let api = Api::new_with_resolved(client, Some(stop))?.with_short_control_timeout()?;
    let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
      return Err(super::fs::message(
        "service did not report its lifecycle capabilities",
      ));
    };
    if !features.iter().any(|feature| feature == "run-control-v1") {
      return Ok(None);
    }
    let batch = features
      .iter()
      .any(|feature| feature == "run-control-batch-v1");
    let requirements = features
      .iter()
      .any(|feature| feature == RUN_REQUIREMENTS_CAPABILITY);
    Ok(Some(Self {
      api,
      scope,
      binding_id,
      batch,
      requirements,
    }))
  }

  /// Bind another validated registration to the same credential's HTTP pool.
  pub fn for_run(&self, scope: RunScope, binding_id: String) -> Self {
    Self {
      api: self.api.clone(),
      scope,
      binding_id,
      batch: self.batch,
      requirements: self.requirements,
    }
  }

  fn request(&self, facts: &Value, result: Option<ControlResult>) -> Result<RunControlRequest> {
    let mut facts: Facts = serde_json::from_value(facts.clone())?;
    if let Some(requirements) = &facts.requirements {
      requirements.validate()?;
    }
    if let Some(status) = &facts.resource_status {
      let requirements = facts
        .requirements
        .as_ref()
        .ok_or_else(|| super::fs::message("resource observations require a requirements policy"))?;
      status.validate(requirements)?;
    }
    if !self.requirements {
      facts
        .capabilities
        .retain(|action| *action != ControlAction::SetRequirements);
      facts.requirements = None;
      facts.resource_status = None;
      facts.manual_paused = None;
    }
    Ok(RunControlRequest::Poll {
      scope: self.scope.clone(),
      binding_id: self.binding_id.clone(),
      attempt: facts.attempt,
      generation: facts.generation,
      status: facts.status,
      capabilities: facts.capabilities,
      command: facts.command,
      checkpoints: facts.checkpoints,
      lifecycle: facts.lifecycle,
      requirements: facts.requirements,
      resource_status: facts.resource_status.map(Box::new),
      manual_paused: facts.manual_paused,
      result,
    })
  }

  /// Reject a malformed local record before grouping it with healthy runs.
  pub fn validate_facts(&self, facts: &Value) -> Result<()> {
    self.request(facts, None).map(|_| ())
  }

  pub fn poll(&self, facts: &Value, result: Option<ControlResult>) -> Result<RunControlResponse> {
    let response = self.api.request(&Request::RunControl {
      request: self.request(facts, result)?,
    })?;
    let Response::RunControl { response } = response else {
      return Err(super::fs::message(
        "service returned an invalid lifecycle response",
      ));
    };
    Ok(response)
  }

  pub fn poll_batch(
    &self,
    entries: &[(&Self, &Value, Option<ControlResult>)],
  ) -> Result<Vec<ControlPollResult>> {
    if entries.is_empty()
      || entries.len() > MAX_CONTROL_POLLS
      || entries
        .iter()
        .any(|(client, _, _)| client.api.binding_id() != self.api.binding_id())
    {
      return Err(super::fs::message("invalid lifecycle polling batch"));
    }
    if !self.batch {
      return entries
        .iter()
        .map(|(client, facts, result)| {
          client
            .poll(facts, result.clone())
            .map(|response| ControlPollResult {
              response: Some(response),
              error: None,
            })
        })
        .collect();
    }
    let requests = entries
      .iter()
      .map(|(client, facts, result)| client.request(facts, result.clone()))
      .collect::<Result<Vec<_>>>()?;
    let mut responses = Vec::with_capacity(requests.len());
    let mut pending = Vec::new();
    let mut bytes = 0;
    for request in requests {
      let size = serde_json::to_vec(&request)?.len() + 1;
      if size > super::super::types::MAX_REQUEST - 4096 {
        return Err(super::fs::message(
          "run-control facts exceed the transport limit",
        ));
      }
      if bytes + size > super::super::types::MAX_REQUEST - 4096 {
        responses.extend(self.send_batch(std::mem::take(&mut pending))?);
        bytes = 0;
      }
      bytes += size;
      pending.push(request);
    }
    if !pending.is_empty() {
      responses.extend(self.send_batch(pending)?);
    }
    Ok(responses)
  }

  fn send_batch(&self, requests: Vec<RunControlRequest>) -> Result<Vec<ControlPollResult>> {
    let expected = requests.len();
    let Response::RunControlBatch { responses } =
      self.api.request(&Request::RunControlBatch { requests })?
    else {
      return Err(super::fs::message(
        "service returned an invalid lifecycle batch response",
      ));
    };
    if responses.len() != expected
      || responses
        .iter()
        .any(|item| item.response.is_some() == item.error.is_some())
    {
      return Err(super::fs::message(
        "service returned an incomplete lifecycle batch response",
      ));
    }
    Ok(responses)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::client::tests::mock;
  use expri_core::service::ClientConfig;
  use serde_json::json;

  #[test]
  fn polls_use_saved_scope_and_send_only_public_facts() {
    let scope = RunScope {
      project_id: "project".into(),
      origin: "worker".into(),
      run_id: "run-one".into(),
    };
    let expected = scope.clone();
    let (url, server) = mock(2, move |request, _| {
      assert!(request.headers.contains("Bearer bound-worker-token"));
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::Capabilities => Response::Capabilities {
          features: vec!["run-control-v1".into()],
        },
        Request::RunControl {
          request:
            RunControlRequest::Poll {
              scope,
              binding_id,
              result,
              ..
            },
        } => {
          assert_eq!(scope, expected);
          assert_eq!(binding_id, "binding-one");
          assert_eq!(result.unwrap().status, "applied");
          let body = String::from_utf8(request.body).unwrap();
          assert!(!body.contains("/private/"));
          assert!(!body.contains("bound-worker-token"));
          assert!(!body.contains("set_requirements"));
          assert!(!body.contains("requirements"));
          assert!(!body.contains("manual_paused"));
          Response::RunControl {
            response: RunControlResponse { command: None },
          }
        }
        _ => panic!("unexpected lifecycle request"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    });
    let client = ResolvedClient::from_config(ClientConfig {
      url,
      token: Some("bound-worker-token".into()),
      token_env: None,
      project_id: None,
    })
    .unwrap();
    let transport = RunControlClient::new(
      &client,
      scope,
      "binding-one".into(),
      Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
    .unwrap();
    let facts = json!({"current_attempt":2,"generation":1,"status":"paused","capabilities":["resume","stop","set_requirements"],"requirements":{"disk":[],"recovery_seconds":30},"manual_paused":true,"command":["python","train.py"],"checkpoints":["outputs/best.pt"],"scope":{"run_id":"untrusted"},"run_dir":"/private/run","source":"/private/source"});
    let response = transport
      .poll(
        &facts,
        Some(ControlResult {
          request_id: "pause-one".into(),
          status: "applied".into(),
          error: None,
          new_run_id: None,
        }),
      )
      .unwrap();
    assert!(response.command.is_none());
    let mut damaged = facts;
    damaged["command"] = json!([42]);
    assert!(transport.validate_facts(&damaged).is_err());
    server.join().unwrap();
  }

  #[test]
  fn resource_facts_are_sent_only_after_capability_negotiation() {
    let (url, server) = mock(2, move |request, _| {
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::Capabilities => Response::Capabilities {
          features: vec!["run-control-v1".into(), RUN_REQUIREMENTS_CAPABILITY.into()],
        },
        Request::RunControl {
          request:
            RunControlRequest::Poll {
              status,
              capabilities,
              requirements,
              resource_status,
              manual_paused,
              ..
            },
        } => {
          assert_eq!(status, "pending");
          assert_eq!(
            capabilities,
            vec![ControlAction::Stop, ControlAction::SetRequirements]
          );
          assert_eq!(requirements.unwrap().disk[0].path, "/mnt/checkpoints");
          assert!(resource_status.unwrap().blocked);
          assert_eq!(manual_paused, Some(false));
          Response::RunControl {
            response: RunControlResponse { command: None },
          }
        }
        _ => panic!("unexpected resource lifecycle request"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    });
    let client = ResolvedClient::from_config(ClientConfig {
      url,
      token: Some("bound-worker-token".into()),
      token_env: None,
      project_id: None,
    })
    .unwrap();
    let transport = RunControlClient::new(
      &client,
      RunScope {
        project_id: "project".into(),
        origin: "worker".into(),
        run_id: "run-one".into(),
      },
      "binding-one".into(),
      Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
    .unwrap();
    let facts = json!({"attempt":1,"generation":1,"status":"pending",
      "capabilities":["stop","set_requirements"],"command":["train"],"checkpoints":[],
      "requirements":{"disk":[{"path":"/mnt/checkpoints","min_free_bytes":100,"resume_free_bytes":120}],"recovery_seconds":30},
      "resource_status":{"phase":"blocked","blocked":true,"checked_at":"2026-10-20T00:00:00Z","recovery_started_at":null,
        "disk":[{"path":"/mnt/checkpoints","available_bytes":90,"min_free_bytes":100,"resume_free_bytes":120,"phase":"blocked","error":null}]},
      "manual_paused":false});
    assert!(transport.poll(&facts, None).unwrap().command.is_none());
    let mut mismatched = facts;
    mismatched["resource_status"]["disk"][0]["path"] = json!("/unconfigured");
    assert!(transport.validate_facts(&mismatched).is_err());
    server.join().unwrap();
  }

  #[test]
  fn old_servers_do_not_receive_lifecycle_polls() {
    let (url, server) = mock(1, |_, _| {
      (
        200,
        Vec::new(),
        serde_json::to_vec(&Response::Capabilities {
          features: Vec::new(),
        })
        .unwrap(),
      )
    });
    let client = ResolvedClient::from_config(ClientConfig {
      url,
      token: Some("token".into()),
      token_env: None,
      project_id: None,
    })
    .unwrap();
    let scope = RunScope {
      project_id: "project".into(),
      origin: "worker".into(),
      run_id: "run-one".into(),
    };
    assert!(
      RunControlClient::new(
        &client,
        scope,
        "binding-one".into(),
        Arc::new(AtomicBool::new(false))
      )
      .unwrap()
      .is_none()
    );
    server.join().unwrap();
  }

  #[test]
  fn batched_polls_share_credentials_and_split_at_the_wire_byte_limit() {
    let (url, server) = mock(3, |request, _| {
      assert!(request.body.len() <= super::super::super::types::MAX_REQUEST);
      let response = match serde_json::from_slice::<Request>(&request.body).unwrap() {
        Request::Capabilities => Response::Capabilities {
          features: vec!["run-control-v1".into(), "run-control-batch-v1".into()],
        },
        Request::RunControlBatch { requests } => {
          assert!(requests.len() <= MAX_CONTROL_POLLS);
          Response::RunControlBatch {
            responses: requests
              .into_iter()
              .map(|_| ControlPollResult {
                response: Some(RunControlResponse { command: None }),
                error: None,
              })
              .collect(),
          }
        }
        _ => panic!("expected a lifecycle batch"),
      };
      (200, Vec::new(), serde_json::to_vec(&response).unwrap())
    });
    let client = ResolvedClient::from_config(ClientConfig {
      url,
      token: Some("token".into()),
      token_env: None,
      project_id: None,
    })
    .unwrap();
    let scope = RunScope {
      project_id: "project".into(),
      origin: "worker".into(),
      run_id: "run-one".into(),
    };
    let transport = RunControlClient::new(
      &client,
      scope.clone(),
      "one".into(),
      Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
    .unwrap();
    let clients: Vec<_> = (0..64)
      .map(|index| {
        transport.for_run(
          RunScope {
            run_id: format!("run-{index}"),
            ..scope.clone()
          },
          format!("binding-{index}"),
        )
      })
      .collect();
    let facts = json!({"attempt":1,"generation":1,"status":"running","capabilities":["pause"],"command":["python","x".repeat(8192),"y".repeat(8192),"z".repeat(8192)],"checkpoints":[]});
    let entries: Vec<_> = clients
      .iter()
      .map(|client| (client, &facts, None))
      .collect();
    let responses = transport.poll_batch(&entries).unwrap();
    assert_eq!(responses.len(), 64);
    assert!(responses.iter().all(|response| response.response.is_some()));
    server.join().unwrap();
  }
}
