//! Public server job history, including the gap before worker metadata arrives.
use expri_core::resource_queue::{JobStatus, QueueJobContext};

use super::*;

pub(super) fn queue_run(source: &Source, context: &QueueJobContext) -> Value {
  let status = match context.status {
    JobStatus::Pending => "pending",
    JobStatus::Starting | JobStatus::Running | JobStatus::CancelRequested => "preparing",
    JobStatus::Completed => "completed",
    JobStatus::Failed => "failed",
    JobStatus::Cancelled => "cancelled",
    JobStatus::Unknown => "unknown",
    JobStatus::Lost => "lost",
  };
  let submitted_at =
    chrono::DateTime::from_timestamp_millis(context.submitted_at_ms).map(|time| time.to_rfc3339());
  let message = if context.status == JobStatus::Pending {
    None
  } else if matches!(
    context.status,
    JobStatus::Starting | JobStatus::Running | JobStatus::CancelRequested
  ) {
    Some("Awaiting worker run metadata")
  } else {
    Some("No published worker run metadata is available")
  };
  json!({
    "run_id":context.job_id,"run_key":format!("queue::{}",context.job_id),
    "task":context.task,"status":status,"started_at":null,"finished_at":null,
    "exit_code":null,"submitted_at":submitted_at,"origin":context.worker_id,
    "queue":context,"queue_only":true,"queue_message":message,
    "archival":{"status":"active"},"warnings":[],"project_id":source.project_id,
  })
}

fn queue_digest(context: &QueueJobContext) -> Result<String> {
  Ok(hex_digest(&Sha256::digest(serde_json::to_vec(context)?)))
}

impl<S: ObjectStorage> HostedDashboard<'_, S> {
  pub(super) fn queue_selection(
    &self,
    source_id: &str,
    run_key: &str,
  ) -> Result<Option<(Source, QueueJobContext)>> {
    let Some(job_id) = run_key.strip_prefix("queue::") else {
      return Ok(None);
    };
    let source = parse_source(source_id)?;
    let context = self
      .store
      .dashboard_queue_job(&source.project_id, source.origin.as_deref(), job_id)
      .map_err(api_error)?;
    Ok(Some((source, context)))
  }

  pub(super) fn queue_native_scope(&self, context: &QueueJobContext) -> Result<Option<RunScope>> {
    let (Some(origin), Some(run_id)) = (&context.worker_id, &context.run_id) else {
      return Ok(None);
    };
    let scope = RunScope {
      project_id: context.project_id.clone(),
      origin: origin.clone(),
      run_id: run_id.clone(),
    };
    Ok(
      self
        .store
        .dashboard_artifact(&scope, "run-state.json")
        .map_err(api_error)?
        .is_some()
        .then_some(scope),
    )
  }

  fn native_reference(source: &Source, scope: &RunScope) -> String {
    if source.origin.is_none() {
      format!("{}:{}", scope.origin, scope.run_id)
    } else {
      scope.run_id.clone()
    }
  }

  pub(super) fn queue_detail(&self, source: &Source, context: &QueueJobContext) -> Result<Value> {
    if let Some(scope) = self.queue_native_scope(context)? {
      let run_id = Self::native_reference(source, &scope);
      let mut result = self.detail(&source.source_id, &run_id)?;
      result["run"]["queue"] = serde_json::to_value(context)?;
      result["redirect"] = json!({"source_id":source.source_id,"run_id":run_id});
      return Ok(result);
    }
    Ok(
      json!({"source":source,"run":queue_run(source,context),"state":null,"snapshot":null,
      "environment":null,"metadata_truncated":false,"params":null,"params_truncated":false,
      "metrics":{},"metric_count":0,"metrics_truncated":false,"metrics_error":null,
      "result_upload":null,"warnings":[],"cache":null,"redirect":null}),
    )
  }

  pub(super) fn queue_updates(&self, source_id: &str, run_keys: &[String]) -> Result<Value> {
    crate::dashboard::updates::validate_selection(source_id, run_keys)?;
    let source = (!source_id.is_empty())
      .then(|| parse_source(source_id))
      .transpose()?;
    let mut selections = Vec::new();
    let mut native_keys = Vec::new();
    for key in run_keys {
      if let Some((_, context)) = self.queue_selection(source_id, key)? {
        let native = self
          .queue_native_scope(&context)?
          .map(|scope| Self::native_reference(source.as_ref().unwrap(), &scope));
        if let Some(native) = &native
          && !native_keys.contains(native)
        {
          native_keys.push(native.clone());
        }
        selections.push((key.clone(), Some(context), native));
      } else {
        if !native_keys.contains(key) {
          native_keys.push(key.clone());
        }
        selections.push((key.clone(), None, Some(key.clone())));
      }
    }
    let mut result = self.artifact_updates(source_id, &native_keys)?;
    let queue_revision = self
      .store
      .dashboard_queue_revision(
        source.as_ref().map(|source| source.project_id.as_str()),
        source.as_ref().and_then(|source| source.origin.as_deref()),
      )
      .map_err(api_error)?;
    let artifact_runs = result["runs"].as_array().cloned().unwrap_or_default();
    let mut runs = Vec::new();
    for (key, context, native) in selections {
      let mut update = native.as_ref().and_then(|native|artifact_runs.iter().find(|update|update["run_id"] == *native)).cloned()
        .unwrap_or_else(||json!({"run_id":key,"metadata_revision":null,"metrics_revision":null,"system_revision":null,"stdout_revision":null,"stderr_revision":null,"missing":true}));
      update["run_id"] = json!(key);
      if let Some(context) = context {
        let metadata = queue_digest(&context)?;
        update["metadata_revision"] = json!(format!(
          "{}:queue:{metadata}",
          update["metadata_revision"].as_str().unwrap_or_default()
        ));
        update["missing"] = json!(false);
        update["queue"] = serde_json::to_value(&context)?;
        update["redirect"] = native
          .map(|run_id| json!({"source_id":source_id,"run_id":run_id}))
          .unwrap_or(Value::Null);
      } else if let Some(source) = &source {
        let scope = source.resolve(&key)?;
        if let Some(context) = self
          .store
          .dashboard_run_queue_context(&scope)
          .map_err(api_error)?
        {
          let metadata = queue_digest(&context)?;
          update["metadata_revision"] = json!(format!(
            "{}:queue:{metadata}",
            update["metadata_revision"].as_str().unwrap_or_default()
          ));
          update["queue"] = serde_json::to_value(context)?;
        }
      }
      runs.push(update);
    }
    result["runs"] = json!(runs);
    if let Some(queue_revision) = queue_revision {
      for field in ["catalog_revision", "source_revision"] {
        if let Some(revision) = result[field].as_str() {
          result[field] = json!(format!("{revision}:queue:{queue_revision}"));
        }
      }
      result["queue_revision"] = json!(queue_revision);
    }
    Ok(result)
  }
}
