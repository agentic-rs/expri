use super::*;

impl<S: ObjectStorage> Store<S> {
  /// Worker observations decorate dashboard reads independently of byte upload.
  /// The original tracking document and its versioned preview remain unchanged.
  pub(in crate::service) fn decorate_run_control(
    &self,
    run: &mut Value,
    scope: &RunScope,
    published_generation: u64,
  ) -> ApiResult<()> {
    let db = self.db()?;
    if resource_queue::is_queued_run(&db, scope)? {
      return Ok(());
    }
    let Some((facts, last_seen_at_ms)) = binding(&db, &encode(scope)?)? else {
      return Ok(());
    };
    let Some(lifecycle) = &facts.lifecycle else {
      return Ok(());
    };
    let Some(recorded_start) = run["started_at"].as_str().and_then(timestamp) else {
      return Ok(());
    };
    if timestamp(&lifecycle.started_at) != Some(recorded_start) {
      return Ok(());
    }
    let published_attempt = run["current_attempt"].as_u64().unwrap_or(1);
    if published_attempt > facts.attempt
      || published_attempt == facts.attempt && published_generation > facts.generation
    {
      return Ok(());
    }
    let recorded_status = run["status"].as_str().unwrap_or("unknown");
    let active = expri_core::run_state::is_active_status(&facts.status);
    if published_attempt == facts.attempt
      && expri_core::run_state::is_terminal_status(recorded_status)
      && active
    {
      return Ok(());
    }
    let terminal = expri_core::run_state::is_terminal_status(&facts.status);
    // A terminal command observation must carry the actual finish time. Lost
    // ownership has no known finish; display that uncertainty without inventing it.
    if terminal && facts.status != "lost" && lifecycle.finished_at.is_none() {
      return Ok(());
    }
    run["recorded_status"] = run["status"].clone();
    run["status"] = json!(facts.status);
    run["finished_at"] = if active {
      Value::Null
    } else {
      json!(lifecycle.finished_at)
    };
    run["exit_code"] = if active {
      Value::Null
    } else {
      json!(lifecycle.exit_code)
    };
    run["control"] = json!({"status":facts.status,"attempt":facts.attempt,"generation":facts.generation,
      "online":now_ms().saturating_sub(last_seen_at_ms)<=ONLINE_MS,"last_seen_at_ms":last_seen_at_ms});
    if let Some(requirements) = &facts.requirements {
      run["requirements"] = json!(requirements);
      run["resource_status"] = json!(facts.resource_status);
    }
    if let Some(paused) = facts.manual_paused {
      run["manual_paused"] = json!(paused);
    }
    Ok(())
  }
}

fn timestamp(value: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
  chrono::DateTime::parse_from_rfc3339(value).ok()
}
