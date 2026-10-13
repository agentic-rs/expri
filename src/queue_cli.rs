use std::path::PathBuf;

use clap::{Args, Subcommand};
use expri_client::{ResourceQueueClient, resolve_client_config};
use expri_core::identifiers::validate_component;
use expri_core::resource_queue::{
  JobStatus, JobSubmission, QueueRequest, QueueResponse, ResourceRequest, SourceRef,
};
use expri_core::storage::StoragePaths;
use serde::Deserialize;
use serde_json::json;

use crate::context::CommandContext;
use crate::error::{ExpriError, Result};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceDefaults {
  pub cpus: Option<u32>,
  pub memory: Option<String>,
  pub gpus: Option<u32>,
  #[serde(default)]
  pub gpu_ids: Vec<String>,
  pub gpu_model: Option<String>,
  pub gpu_memory: Option<String>,
}

#[derive(Debug, Default, Args)]
pub(crate) struct RunResources {
  /// Logical CPU slots reserved while the queued run prepares and executes.
  #[arg(long, requires = "queue", value_parser = clap::value_parser!(u32).range(1..=1_000_000))]
  pub cpus: Option<u32>,
  /// Memory reservation, for example 16GiB. Required unless the task defines it.
  #[arg(long, requires = "queue")]
  pub memory: Option<String>,
  /// Number of whole GPUs reserved for this queued run.
  #[arg(long, requires = "queue", value_parser = clap::value_parser!(u32).range(0..=64))]
  pub gpus: Option<u32>,
  /// Exact worker GPU device ID; repeat for multiple GPUs.
  #[arg(long = "gpu-id", requires = "queue")]
  pub gpu_ids: Vec<String>,
  /// Match a GPU model, such as A100 or RTX 3090.
  #[arg(long, requires = "queue")]
  pub gpu_model: Option<String>,
  /// Minimum dedicated memory per GPU, for example 24GiB.
  #[arg(long, requires = "queue")]
  pub gpu_memory: Option<String>,
  /// Pin the queued job to a registered worker instead of automatic placement.
  #[arg(long, requires = "queue")]
  pub worker: Option<String>,
  /// Project ID for the shared queue; defaults to the selected service configuration.
  #[arg(long, requires = "queue")]
  pub project_id: Option<String>,
  /// Override the global server client configuration for submission.
  #[arg(long, requires = "queue")]
  pub server_config: Option<PathBuf>,
  /// Idempotency key for retrying an identical queued submission.
  #[arg(long, requires = "queue")]
  pub submission_id: Option<String>,
}

pub(crate) fn parse_bytes(value: &str) -> Result<u64> {
  let value = value.trim();
  let end = value.bytes().take_while(u8::is_ascii_digit).count();
  let number = value[..end].parse::<u64>().map_err(|_| {
    invalid("memory must be a positive integer followed by B, KiB, MiB, GiB, or TiB")
  })?;
  let multiplier = match value[end..].trim().to_ascii_lowercase().as_str() {
    "" | "b" => 1,
    "kib" => 1024,
    "mib" => 1024_u64.pow(2),
    "gib" => 1024_u64.pow(3),
    "tib" => 1024_u64.pow(4),
    "kb" => 1000,
    "mb" => 1000_u64.pow(2),
    "gb" => 1000_u64.pow(3),
    "tb" => 1000_u64.pow(4),
    _ => return Err(invalid("unknown memory unit; use B, KiB, MiB, GiB, or TiB")),
  };
  number
    .checked_mul(multiplier)
    .filter(|bytes| *bytes > 0 && *bytes <= i64::MAX as u64)
    .ok_or_else(|| invalid("memory reservation is zero or exceeds its limit"))
}

impl RunResources {
  fn resolve(&self, defaults: Option<&ResourceDefaults>) -> Result<ResourceRequest> {
    let empty = ResourceDefaults::default();
    let defaults = defaults.unwrap_or(&empty);
    let gpu_ids = if self.gpu_ids.is_empty() {
      defaults.gpu_ids.clone()
    } else {
      self.gpu_ids.clone()
    };
    let resources = ResourceRequest {
      cpus: self.cpus.or(defaults.cpus).unwrap_or(1),
      memory_bytes: parse_bytes(
        self
          .memory
          .as_deref()
          .or(defaults.memory.as_deref())
          .ok_or_else(|| {
            invalid("queued runs require --memory or tasks.<name>.resources.memory")
          })?,
      )?,
      gpus: self.gpus.or(defaults.gpus).unwrap_or(gpu_ids.len() as u32),
      gpu_ids,
      gpu_model: self
        .gpu_model
        .clone()
        .or_else(|| defaults.gpu_model.clone()),
      gpu_memory_bytes: self
        .gpu_memory
        .as_deref()
        .or(defaults.gpu_memory.as_deref())
        .map(parse_bytes)
        .transpose()?,
    };
    resources.validate()?;
    Ok(resources)
  }
}

pub(crate) fn submit(
  command: crate::RunCommand,
  context: CommandContext,
  target: Option<&str>,
  quiet: bool,
) -> Result<()> {
  let mut parts = command.task.iter();
  let name = parts.next().expect("clap requires a task");
  let task = context.config.task(name)?;
  let resources = command.resources.resolve(task.resources.as_ref())?;
  if target.is_some() && command.resources.worker.is_some() {
    return Err(invalid("choose either -T or --worker for queued placement"));
  }
  let selected = target.map(|name| context.config.target(name)).transpose()?;
  let environment = selected
    .as_ref()
    .and_then(|target| target.environment.clone())
    .or(context.config.local_environment()?)
    .ok_or_else(|| invalid("--queue requires a configured environment"))?;
  environment.validate()?;
  if environment.require_cuda && resources.gpus == 0 {
    return Err(invalid(
      "a CUDA environment requires at least one queued GPU reservation",
    ));
  }
  let service = selected
    .as_ref()
    .and_then(|target| target.service.as_ref())
    .or(context.config.service.as_ref());
  let project_id = command
    .resources
    .project_id
    .clone()
    .or_else(|| service.map(|service| service.project_id.clone()))
    .ok_or_else(|| invalid("queued runs require --project-id or service.project_id"))?;
  validate_component(&project_id)?;
  let worker_id = command
    .resources
    .worker
    .clone()
    .or_else(|| target.map(str::to_owned));
  if let Some(worker_id) = &worker_id {
    validate_component(worker_id)?;
  }
  let submission_id = command
    .resources
    .submission_id
    .clone()
    .unwrap_or_else(|| format!("submit-{}", uuid::Uuid::new_v4().simple()));
  validate_component(&submission_id)?;
  let mut argv = task.command;
  argv.extend(parts.cloned());
  let (extras, sync_args) = crate::environment_selection(&context.config);
  let mut remote_managed = context
    .config
    .push
    .as_ref()
    .and_then(|push| push.remote_managed.clone())
    .unwrap_or_default();
  if let Some(paths) = context
    .config
    .push
    .as_ref()
    .and_then(|push| push.include_ignored.as_ref())
  {
    remote_managed.extend(paths.iter().cloned());
  }
  remote_managed.sort();
  remote_managed.dedup();
  let publish = !command.no_publish && service.is_none_or(|service| service.publish);
  if command.dry_run {
    if command.json || !quiet {
      crate::output::print_report(
        &json!({"dry_run":true,"project_id":project_id,"task":name,"command":argv,
        "resources":resources,"worker_id":worker_id,"smoke":command.smoke,"publish":publish}),
        command.json,
      )?;
    }
    return Ok(());
  }
  let config = resolve_client_config(command.resources.server_config.as_deref())?;
  let client = ResourceQueueClient::new(&config)?;
  let bundle = expri_worker::queue::source::freeze_source(&context.repo_root, &remote_managed)?;
  let source = SourceRef {
    input_id: format!("queue-source-{}", bundle.sha256),
    size: bundle.size,
    sha256: bundle.sha256.clone(),
  };
  let submission = JobSubmission {
    submission_id: submission_id.clone(),
    project_id: project_id.clone(),
    task: name.clone(),
    command: argv,
    environment,
    extras,
    sync_args,
    remote_managed,
    smoke: command.smoke,
    publish,
    resources,
    worker_id,
    source: source.clone(),
  };
  submission.validate()?;
  let paths = StoragePaths::resolve(None)?;
  paths.ensure_private()?;
  let queue_root = std::fs::canonicalize(paths.root())?;
  expri_client::client::input_upload(expri_client::InputUploadOptions {
    config,
    project_id,
    input_id: source.input_id,
    file: bundle.path.clone(),
    queue_dir: queue_root
      .join("queue/source-uploads")
      .join(client.server_namespace()),
  })?;
  let response = client
    .request(&QueueRequest::Submit {
      submission: Box::new(submission),
    })
    .map_err(|error| {
      invalid(format!(
        "{error}; retry this identical queued submission with --submission-id {submission_id}"
      ))
    })?;
  if command.json || !quiet {
    print_response(response, command.json)?;
  }
  Ok(())
}

#[derive(Debug, Args)]
pub(crate) struct QueueCommand {
  #[command(subcommand)]
  command: QueueSubcommand,
}

#[derive(Debug, Subcommand)]
enum QueueSubcommand {
  /// List queued and finished jobs using the configured server.
  List(ListOptions),
  /// Inspect one job and its current resource allocation.
  Show(JobOptions),
  /// Cancel a waiting job or request cancellation of its running attempt.
  Cancel(JobOptions),
  /// List worker status, reservations, and available execution resources.
  Workers(WorkerListOptions),
  /// Release an uncertain attempt after independently confirming it has stopped.
  Recover(RecoverOptions),
}

#[derive(Debug, Args)]
struct ReportOptions {
  #[arg(long)]
  json: bool,
  #[arg(long)]
  config: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ListOptions {
  #[command(flatten)]
  report: ReportOptions,
  #[arg(long)]
  project_id: Option<String>,
  #[arg(long, value_parser = parse_status)]
  status: Option<JobStatus>,
  #[arg(long)]
  cursor: Option<String>,
  #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=100))]
  limit: u16,
}

#[derive(Debug, Args)]
struct WorkerListOptions {
  #[command(flatten)]
  report: ReportOptions,
  #[arg(long)]
  cursor: Option<String>,
  #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=100))]
  limit: u16,
}

#[derive(Debug, Args)]
struct JobOptions {
  #[command(flatten)]
  report: ReportOptions,
  job_id: String,
}

#[derive(Debug, Args)]
struct RecoverOptions {
  #[command(flatten)]
  report: ReportOptions,
  attempt_id: String,
  #[arg(long, required = true)]
  confirmed_stopped: bool,
}

pub(crate) fn run(command: QueueCommand, target: Option<&str>, quiet: bool) -> Result<()> {
  if target.is_some() {
    return Err(invalid(
      "queue commands use the shared server or current worker; omit -T",
    ));
  }
  let (request, report) = match command.command {
    QueueSubcommand::List(options) => (
      QueueRequest::ListJobs {
        project_id: options.project_id,
        status: options.status,
        cursor: options.cursor,
        limit: options.limit as usize,
      },
      options.report,
    ),
    QueueSubcommand::Workers(options) => (
      QueueRequest::ListWorkers {
        cursor: options.cursor,
        limit: options.limit as usize,
      },
      options.report,
    ),
    QueueSubcommand::Show(options) => (
      QueueRequest::GetJob {
        job_id: options.job_id,
      },
      options.report,
    ),
    QueueSubcommand::Cancel(options) => (
      QueueRequest::Cancel {
        job_id: options.job_id,
      },
      options.report,
    ),
    QueueSubcommand::Recover(options) => (
      QueueRequest::Recover {
        attempt_id: options.attempt_id,
        confirmed_stopped: options.confirmed_stopped,
      },
      options.report,
    ),
  };
  // Reject bad selectors before reading credentials or contacting a server.
  request.validate()?;
  let client = ResourceQueueClient::new(&resolve_client_config(report.config.as_deref())?)?;
  let response = client.request(&request)?;
  if report.json || !quiet {
    print_response(response, report.json)?;
  }
  Ok(())
}

fn print_response(response: QueueResponse, json_output: bool) -> Result<()> {
  match response {
    QueueResponse::Job { job } => crate::output::print_report(&job, json_output),
    QueueResponse::Attempt { attempt } => crate::output::print_report(&attempt, json_output),
    QueueResponse::Jobs { jobs, next_cursor } => {
      if json_output {
        return crate::output::print_report(&json!({"jobs":jobs,"next_cursor":next_cursor}), true);
      }
      let rows = jobs
        .iter()
        .map(|job| {
          vec![
            job.job_id.clone(),
            job.project_id.clone(),
            job.task.clone(),
            job.status.as_str().into(),
            job.resources.cpus.to_string(),
            crate::output::bytes(job.resources.memory_bytes),
            job.resources.gpus.to_string(),
            job
              .assigned_worker_id
              .as_deref()
              .or_else(|| {
                job
                  .attempt
                  .as_ref()
                  .map(|attempt| attempt.worker_id.as_str())
                  .or(job.worker_id.as_deref())
              })
              .unwrap_or("-")
              .into(),
          ]
        })
        .collect::<Vec<_>>();
      print!(
        "{}",
        crate::output::table(
          &[
            "JOB", "PROJECT", "TASK", "STATUS", "CPU", "MEMORY", "GPUS", "WORKER"
          ],
          &rows,
          &[36, 24, 24, 18, 8, 12, 6, 24]
        )
      );
      if jobs.is_empty() {
        println!("No jobs.");
      }
      for job in &jobs {
        if let Some(reason) = &job.pending_reason {
          println!(
            "{}: {}",
            crate::output::text(&job.job_id),
            crate::output::text(reason)
          );
        }
      }
      if let Some(cursor) = next_cursor {
        println!("Next cursor: {}", crate::output::text(&cursor));
      }
      Ok(())
    }
    QueueResponse::Workers {
      workers,
      next_cursor,
    } => {
      if json_output {
        return crate::output::print_report(
          &json!({"workers":workers,"next_cursor":next_cursor}),
          true,
        );
      }
      let rows = workers
        .iter()
        .map(|worker| {
          vec![
            worker.worker_id.clone(),
            worker.project_ids.join(", "),
            format!("{}/{}", worker.available.cpus, worker.capacity.cpus),
            format!(
              "{}/{}",
              crate::output::bytes(worker.available.memory_bytes),
              crate::output::bytes(worker.capacity.memory_bytes)
            ),
            format!(
              "{}/{}",
              worker.available.gpus.len(),
              worker.capacity.gpus.len()
            ),
            worker.assigned_jobs.to_string(),
            worker.outstanding_attempts.to_string(),
            if worker.quarantined {
              "quarantined"
            } else if !worker.online {
              "offline"
            } else {
              "online"
            }
            .into(),
          ]
        })
        .collect::<Vec<_>>();
      print!(
        "{}",
        crate::output::table(
          &[
            "WORKER",
            "PROJECTS",
            "CPU FREE/TOTAL",
            "MEMORY FREE/TOTAL",
            "GPU FREE/TOTAL",
            "ASSIGNED",
            "ACTIVE",
            "STATUS"
          ],
          &rows,
          &[24, 32, 14, 26, 14, 8, 8, 16]
        )
      );
      if workers.is_empty() {
        println!("No workers.");
      }
      for worker in &workers {
        if !worker.available.gpus.is_empty() {
          println!(
            "{} free GPUs: {}",
            crate::output::text(&worker.worker_id),
            crate::output::text(
              &worker
                .available
                .gpus
                .iter()
                .map(|gpu| gpu.device_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
            )
          );
        }
      }
      if let Some(cursor) = next_cursor {
        println!("Next cursor: {}", crate::output::text(&cursor));
      }
      Ok(())
    }
    _ => Err(invalid("unexpected queue report")),
  }
}

fn parse_status(value: &str) -> std::result::Result<JobStatus, String> {
  value
    .parse::<JobStatus>()
    .map_err(|error| error.to_string())
}

fn invalid(message: impl Into<String>) -> ExpriError {
  ExpriError::Message(message.into())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn memory_units_are_explicit_checked_and_allow_decimal_or_binary_sizes() {
    assert_eq!(parse_bytes("16GiB").unwrap(), 16 * 1024_u64.pow(3));
    assert_eq!(parse_bytes("16 GB").unwrap(), 16_000_000_000);
    for value in [
      "",
      "0",
      "-1GiB",
      "1.5GiB",
      "1gig",
      "18446744073709551615TiB",
    ] {
      assert!(parse_bytes(value).is_err(), "{value}");
    }
  }

  #[test]
  fn task_defaults_merge_with_cli_overrides_without_losing_gpu_constraints() {
    let defaults: ResourceDefaults =
      toml::from_str("cpus=4\nmemory='16GiB'\ngpus=1\ngpu_model='A100'").unwrap();
    let request = RunResources {
      cpus: Some(8),
      ..RunResources::default()
    }
    .resolve(Some(&defaults))
    .unwrap();
    assert_eq!(request.cpus, 8);
    assert_eq!(request.gpu_model.as_deref(), Some("A100"));
    assert_eq!(request.gpus, 1);
    assert!(RunResources::default().resolve(None).is_err());
  }
}
