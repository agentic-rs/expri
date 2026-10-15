use clap::Args;
use serde_json::Value;

use super::{ConnectionArgs, print_job_report};
use crate::context::CommandContext;
use crate::controller::jobs::{JobOptions, execute};
use crate::controller::sync::{SyncOptions, sync_target_with_diagnostic_receipt};
use crate::error::{ExpriError, Result};
use crate::protocol::{ContinueRunRequest, JobRequest, RunRequest};

#[derive(Debug, Args)]
pub(super) struct ContinueCommand {
  #[command(flatten)]
  connection: ConnectionArgs,
  run_id: String,
  /// Finalized checkpoint relative to the existing run, beneath outputs/.
  #[arg(long)]
  checkpoint: String,
  /// Record why another attempt is being started.
  #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
  reason: String,
  /// Reuse this ID when retrying an interrupted continuation.
  #[arg(long)]
  request_id: Option<String>,
  /// Refuse continuation if another attempt has started since inspection.
  #[arg(long)]
  expected_attempt: Option<u64>,
  /// Defaults to the run's recorded task, resolved from the current configuration.
  #[arg(long)]
  task: Option<String>,
  /// Use source already present on the worker instead of pushing the checkout.
  #[arg(long)]
  no_push: bool,
  #[arg(long)]
  json: bool,
  /// Arguments appended to the configured task command, following --.
  #[arg(last = true, value_name = "ARG")]
  args: Vec<String>,
}

pub(super) fn run(
  command: ContinueCommand,
  target: Option<&str>,
  verbosity: u8,
  quiet: bool,
) -> Result<()> {
  let context = CommandContext::load(command.connection.config, command.connection.repo)?;
  let (extras, sync_args) = crate::environment_selection(&context.config);
  let mut remote_managed = context
    .config
    .push
    .as_ref()
    .and_then(|push| push.remote_managed.clone())
    .unwrap_or_default();
  let request_id = command
    .request_id
    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
  if !quiet {
    eprintln!("Task continuation request: {request_id}");
  }
  let report = if target.is_some() {
    let context = context.into_target(target, command.connection.control_path)?;
    // Check worker support and identity before synchronizing a new checkout.
    crate::controller::jobs::check_continuation_target(
      &context.target,
      &context.control_path,
      &command.connection.control_persist,
      verbosity,
      quiet,
    )?;
    let status = execute(JobOptions {
      target: context.target.clone(),
      control_path: context.control_path.clone(),
      control_persist: command.connection.control_persist.clone(),
      verbosity,
      quiet,
      request: JobRequest::Status {
        run_id: command.run_id.clone(),
      },
    })?
    .ok_or_else(|| message("worker returned no run status"))?;
    let name = task_name(command.task, &status)?;
    let mut argv = context.config.task(&name)?.command;
    argv.extend(command.args);
    let environment = context
      .target
      .environment
      .clone()
      .ok_or_else(|| message("task continuation requires a configured target environment"))?;
    let expected_sync = if command.no_push {
      None
    } else {
      sync_target_with_diagnostic_receipt(SyncOptions {
        repo_root: context.repo_root.clone(),
        project_name: context.project_name,
        target_name: context.target_name,
        target: context.target.clone(),
        sync: context.config.push_rules()?,
        control_path: context.control_path.clone(),
        control_persist: command.connection.control_persist.clone(),
        dry_run: false,
        force: false,
        paths: Vec::new(),
        verbosity,
        quiet,
      })?
    };
    execute(JobOptions {
      target: context.target.clone(),
      control_path: context.control_path,
      control_persist: command.connection.control_persist,
      verbosity,
      quiet,
      request: JobRequest::Continue {
        run_id: command.run_id,
        continuation: Box::new(ContinueRunRequest {
          request_id,
          reason: command.reason,
          expected_attempt: command.expected_attempt,
          checkpoint: command.checkpoint,
          run: RunRequest {
            name,
            command: argv,
            environment,
            service: context.target.service,
            detach: true,
            smoke: status["state"]["smoke"].as_bool().unwrap_or(false),
            remote_managed,
            extras,
            sync_args,
            expected_sync,
          },
        }),
      },
    })?
    .ok_or_else(|| message("worker returned no continuation receipt"))?
  } else {
    let status = crate::jobs::status_at(&context.repo_root, &command.run_id)?;
    let name = task_name(command.task, &status)?;
    let mut argv = context.config.task(&name)?.command;
    argv.extend(command.args);
    let environment = context
      .config
      .local_environment()?
      .ok_or_else(|| message("task continuation requires a configured local environment"))?;
    if let Some(paths) = context
      .config
      .push
      .as_ref()
      .and_then(|push| push.include_ignored.as_ref())
    {
      remote_managed.extend(paths.iter().cloned());
    }
    crate::jobs::query_at(
      &JobRequest::Continue {
        run_id: command.run_id,
        continuation: Box::new(ContinueRunRequest {
          request_id,
          reason: command.reason,
          expected_attempt: command.expected_attempt,
          checkpoint: command.checkpoint,
          run: RunRequest {
            name,
            command: argv,
            environment,
            service: context.config.local_service()?,
            detach: true,
            smoke: status["state"]["smoke"].as_bool().unwrap_or(false),
            remote_managed,
            extras,
            sync_args,
            expected_sync: None,
          },
        }),
      },
      &context.repo_root,
    )?
  };
  if command.json {
    println!("{}", serde_json::to_string_pretty(&report)?);
  } else if !quiet {
    print_job_report(&report);
    if let Some(attempt) = report["state"]["task_attempts"]["current_attempt"].as_u64() {
      println!("Task attempt: {attempt}");
    }
  }
  if report["continuation"]["status"] == "failed" {
    return Err(message(
      report["continuation"]["error"]
        .as_str()
        .unwrap_or("task continuation failed; inspect runs status"),
    ));
  }
  Ok(())
}

fn task_name(requested: Option<String>, status: &Value) -> Result<String> {
  requested
    .or_else(|| status["state"]["task"].as_str().map(str::to_owned))
    .ok_or_else(|| message("run has no recorded task; specify --task"))
}

fn message(detail: impl Into<String>) -> ExpriError {
  ExpriError::Message(detail.into())
}

#[cfg(test)]
mod tests {
  use super::*;
  use clap::Parser;
  use serde_json::json;

  #[test]
  fn continuation_cli_keeps_task_arguments_literal_and_requires_checkpoint_and_reason() {
    let parsed = crate::Cli::try_parse_from([
      "expri",
      "-T",
      "gpu",
      "runs",
      "continue",
      "run-old",
      "--checkpoint",
      "outputs/step-80.pt",
      "--reason",
      "Extend training",
      "--request-id",
      "continue-1",
      "--expected-attempt",
      "1",
      "--",
      "--max-steps",
      "200",
      "a value; $(literal)",
    ])
    .unwrap();
    let crate::Command::Runs(super::super::RunsCommand {
      command: super::super::RunsSubcommand::Continue(command),
    }) = parsed.command
    else {
      panic!("expected task continuation");
    };
    assert_eq!(command.args, ["--max-steps", "200", "a value; $(literal)"]);
    assert_eq!(command.expected_attempt, Some(1));
    for args in [
      vec!["expri", "runs", "continue", "run-old"],
      vec![
        "expri",
        "runs",
        "continue",
        "run-old",
        "--checkpoint",
        "outputs/a.pt",
      ],
      vec!["expri", "runs", "continue", "run-old", "--reason", "Extend"],
    ] {
      assert!(crate::Cli::try_parse_from(args).is_err());
    }
  }

  #[test]
  fn task_defaults_to_the_recorded_task_and_can_be_overridden() {
    let status = json!({"state":{"task":"train", "task_attempts":{"current_attempt":3}}});
    assert_eq!(task_name(None, &status).unwrap(), "train");
    assert_eq!(
      task_name(Some("retrain".into()), &status).unwrap(),
      "retrain"
    );
  }
}
