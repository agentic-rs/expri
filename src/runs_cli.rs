use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::context::CommandContext;
use crate::error::{ExpriError, Result};
use crate::{controller, protocol, runs};

#[derive(Debug, Args)]
pub struct RunsCommand {
  #[command(subcommand)]
  command: RunsSubcommand,
}

#[derive(Debug, Subcommand)]
enum RunsSubcommand {
  /// List recorded experiments, newest first.
  List(RunsListCommand),
  /// Inspect a run's status, provenance, and environment record.
  Show(RunsShowCommand),
  /// Download remote metadata and logs, with optional artifacts.
  Pull(RunsPullCommand),
  /// Check recorded status and detached supervisor liveness.
  Status(RunsJobCommand),
  /// Read saved stdout or stderr, optionally following a running task.
  Logs(RunsLogsCommand),
  /// Request graceful cancellation of a detached run.
  Cancel(RunsJobCommand),
}

#[derive(Debug, Args)]
struct ConnectionArgs {
  #[arg(long)]
  config: Option<PathBuf>,
  #[arg(long)]
  repo: Option<PathBuf>,
  #[arg(long)]
  control_path: Option<String>,
  #[arg(long, default_value = "30m")]
  control_persist: String,
}

#[derive(Debug, Args)]
struct RunsArgs {
  #[command(flatten)]
  connection: ConnectionArgs,
  #[arg(long)]
  json: bool,
}

#[derive(Debug, Args)]
struct RunsJobCommand {
  #[command(flatten)]
  options: RunsArgs,
  run_id: String,
}

#[derive(Debug, Args)]
struct RunsLogsCommand {
  #[command(flatten)]
  connection: ConnectionArgs,
  run_id: String,
  #[arg(long, default_value = "stdout", value_parser = ["stdout", "stderr"])]
  stream: String,
  /// Number of trailing lines to show before following; zero starts at the end.
  #[arg(long, default_value_t = 100)]
  tail: usize,
  #[arg(long)]
  follow: bool,
}

#[derive(Debug, Args)]
struct RunsListCommand {
  #[command(flatten)]
  options: RunsArgs,
  /// Inspect previously pulled records without contacting the target.
  #[arg(long)]
  cached: bool,
  #[arg(long)]
  task: Option<String>,
  #[arg(long, value_parser = ["preparing", "running", "completed", "failed", "cancelled", "unknown"])]
  status: Option<String>,
  #[arg(long, default_value_t = 20)]
  limit: usize,
}

#[derive(Debug, Args)]
struct RunsShowCommand {
  #[command(flatten)]
  options: RunsArgs,
  #[arg(long)]
  cached: bool,
  run_id: String,
}

#[derive(Debug, Args)]
struct RunsPullCommand {
  #[command(flatten)]
  options: RunsArgs,
  run_id: String,
  /// Include a relative file or directory beneath outputs/ or code/.
  #[arg(long, value_name = "PATH")]
  artifact: Vec<String>,
  /// Include the entire outputs/ directory.
  #[arg(long)]
  outputs: bool,
  /// Inspect the remote selection and preview its download without writing locally.
  #[arg(long)]
  dry_run: bool,
}

pub fn run(command: RunsCommand, target: Option<&str>, verbosity: u8, quiet: bool) -> Result<()> {
  let (options, cached, request, dry_run) = match command.command {
    RunsSubcommand::Status(command) => {
      return run_job(
        command.options,
        protocol::JobRequest::Status {
          run_id: command.run_id,
        },
        target,
        verbosity,
        quiet,
      );
    }
    RunsSubcommand::Cancel(command) => {
      return run_job(
        command.options,
        protocol::JobRequest::Cancel {
          run_id: command.run_id,
        },
        target,
        verbosity,
        quiet,
      );
    }
    RunsSubcommand::Logs(command) => {
      return run_job(
        RunsArgs {
          connection: command.connection,
          json: false,
        },
        protocol::JobRequest::Logs {
          run_id: command.run_id,
          stream: command.stream,
          follow: command.follow,
          tail: command.tail,
        },
        target,
        verbosity,
        quiet,
      );
    }
    RunsSubcommand::List(command) => (
      command.options,
      command.cached,
      protocol::RunQueryRequest::List {
        task: command.task,
        status: command.status,
        limit: Some(command.limit),
      },
      false,
    ),
    RunsSubcommand::Show(command) => (
      command.options,
      command.cached,
      protocol::RunQueryRequest::Show {
        run_id: command.run_id,
      },
      false,
    ),
    RunsSubcommand::Pull(mut command) => {
      if target.is_none() {
        return Err(ExpriError::Message(
          "runs pull requires --target (-T)".to_string(),
        ));
      }
      if command.outputs {
        command.artifact.push("outputs".to_string());
      }
      (
        command.options,
        false,
        protocol::RunQueryRequest::Files {
          run_id: command.run_id,
          artifacts: command.artifact,
        },
        command.dry_run,
      )
    }
  };
  if cached && target.is_none() {
    return Err(ExpriError::Message(
      "--cached requires --target (-T)".to_string(),
    ));
  }
  let context = CommandContext::load(options.connection.config, options.connection.repo)?;
  let report = if let Some(target) = target {
    controller::run_pull::validate_component(target, "target name")?;
    let results_dir = context.config.download_results_dir();
    if cached {
      // Offline inspection needs only the label and cache path, not usable target credentials.
      let cache = controller::run_pull::cached_runs_dir(&context.repo_root, &results_dir, target)?;
      runs::query_directory(&cache, &request)?
    } else {
      let context = context.into_target(Some(target), options.connection.control_path)?;
      controller::runs::execute(controller::runs::RunOptions {
        repo_root: context.repo_root,
        target_name: context.target_name,
        target: context.target,
        results_dir,
        control_path: context.control_path,
        control_persist: options.connection.control_persist,
        verbosity,
        quiet,
        dry_run,
        request: request.clone(),
      })?
    }
  } else {
    runs::query(&context.repo_root, &request)?
  };
  if options.json {
    println!("{}", serde_json::to_string_pretty(&report)?);
  } else if !quiet {
    print_runs_report(&report, &request);
  }
  Ok(())
}

fn run_job(
  options: RunsArgs,
  request: protocol::JobRequest,
  target: Option<&str>,
  verbosity: u8,
  quiet: bool,
) -> Result<()> {
  let context = CommandContext::load(options.connection.config, options.connection.repo)?;
  let report = if target.is_some() {
    let context = context.into_target(target, options.connection.control_path)?;
    controller::jobs::execute(controller::jobs::JobOptions {
      target: context.target,
      control_path: context.control_path,
      control_persist: options.connection.control_persist,
      verbosity,
      quiet,
      request,
    })?
  } else if matches!(request, protocol::JobRequest::Logs { .. }) {
    crate::jobs::execute_at(&request, &context.repo_root)?;
    None
  } else {
    Some(crate::jobs::query_at(&request, &context.repo_root)?)
  };
  if let Some(report) = report {
    if options.json {
      println!("{}", serde_json::to_string_pretty(&report)?);
    } else if !quiet {
      print_job_report(&report);
    }
  }
  Ok(())
}

fn print_job_report(report: &serde_json::Value) {
  for (label, value) in [
    ("Run", &report["run_id"]),
    ("Status", &report["status"]),
    ("Detached", &report["detached"]),
    ("Supervisor alive", &report["alive"]),
    ("Cancellation requested", &report["cancel_requested"]),
    ("Exit code", &report["state"]["exit_code"]),
  ] {
    let value = match value {
      serde_json::Value::String(value) => value.clone(),
      serde_json::Value::Null => "-".to_string(),
      value => value.to_string(),
    };
    println!("{label}: {value}");
  }
  if report["already_finished"] == true {
    println!("The run already finished; no cancellation was needed.");
  }
}

fn print_runs_report(report: &serde_json::Value, request: &protocol::RunQueryRequest) {
  fn field(value: &serde_json::Value, key: &str) -> String {
    match &value[key] {
      serde_json::Value::String(value) => value.clone(),
      serde_json::Value::Null => "-".to_string(),
      value => value.to_string(),
    }
  }
  match request {
    protocol::RunQueryRequest::List { .. } => {
      println!(
        "{:<26} {:<18} {:<12} {:<25} EXIT",
        "RUN", "TASK", "STATUS", "STARTED"
      );
      if let Some(rows) = report["runs"].as_array() {
        for row in rows {
          println!(
            "{:<26} {:<18} {:<12} {:<25} {}",
            field(row, "run_id"),
            field(row, "task"),
            field(row, "status"),
            field(row, "started_at"),
            field(row, "exit_code")
          );
        }
        if rows.is_empty() {
          println!("No recorded runs.");
        }
      }
    }
    protocol::RunQueryRequest::Show { .. } => {
      let summary = &report["run"];
      for (label, key) in [
        ("Run", "run_id"),
        ("Task", "task"),
        ("Status", "status"),
        ("Started", "started_at"),
        ("Finished", "finished_at"),
        ("Exit code", "exit_code"),
      ] {
        println!("{label}: {}", field(summary, key));
      }
      let state = &report["state"];
      if !state.is_null() {
        println!("Command: {}", field(state, "command"));
        println!("Code: {}", field(state, "code_dir"));
        println!("Outputs: {}", field(state, "output_dir"));
        if let Some(logs) = state["logs"].as_object() {
          for name in ["stdout", "stderr"] {
            if let Some(path) = logs.get(name).and_then(serde_json::Value::as_str) {
              println!("{name} log: {path}");
            }
          }
        }
        if let Some(error) = state["error"].as_str() {
          println!("Error: {error}");
        }
      }
      let snapshot = &report["snapshot"];
      if !snapshot.is_null() {
        println!("Source: {}", field(&snapshot["source"], "kind"));
        println!("Git commit: {}", field(&snapshot["source"], "git_head"));
        if let Some(files) = snapshot["files"].as_array() {
          println!("Snapshot files: {}", files.len());
        }
      }
      let environment = &report["environment"];
      if !environment.is_null() {
        println!("Base Python: {}", field(environment, "base_python"));
        if let Some(packages) = environment["reuse_packages"].as_array() {
          println!(
            "Reused packages: {}",
            packages
              .iter()
              .filter_map(serde_json::Value::as_str)
              .collect::<Vec<_>>()
              .join(", ")
          );
        }
        if !environment["combined_manifest"]["torch"].is_null() {
          println!("Torch: {}", environment["combined_manifest"]["torch"]);
        }
      }
    }
    protocol::RunQueryRequest::Files { .. } => {
      println!(
        "{} {} to {}",
        if report["dry_run"] == true {
          "Would pull"
        } else {
          "Pulled"
        },
        field(report, "run_id"),
        field(report, "destination")
      );
      if let Some(files) = report["files"].as_array() {
        for file in files {
          if let Some(file) = file.as_str() {
            println!("  {file}");
          }
        }
      }
    }
  }
  if let Some(warnings) = report["warnings"].as_array() {
    for warning in warnings {
      if warning.is_object() {
        eprintln!(
          "warning: {}: {}",
          field(warning, "run_id"),
          field(warning, "message")
        );
      } else {
        eprintln!(
          "warning: {}",
          warning.as_str().unwrap_or("invalid run record")
        );
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use clap::Parser;

  use super::*;
  use crate::{Cli, Command};

  #[test]
  fn run_queries_keep_filters_cache_and_artifacts_separate() {
    let cli = Cli::try_parse_from([
      "expri", "-T", "gpu", "runs", "list", "--cached", "--task", "train", "--status", "failed",
      "--limit", "3", "--json",
    ])
    .unwrap();
    assert_eq!(cli.target.as_deref(), Some("gpu"));
    let Command::Runs(RunsCommand {
      command: RunsSubcommand::List(command),
    }) = cli.command
    else {
      panic!("expected runs list");
    };
    assert!(command.cached && command.options.json);
    assert_eq!(command.task.as_deref(), Some("train"));
    assert_eq!(command.status.as_deref(), Some("failed"));
    assert_eq!(command.limit, 3);
    let cli = Cli::try_parse_from([
      "expri",
      "-T",
      "gpu",
      "runs",
      "pull",
      "run-123",
      "--artifact",
      "outputs/model one.pt",
      "--artifact",
      "code/out/metrics.json",
      "--outputs",
      "--dry-run",
    ])
    .unwrap();
    let Command::Runs(RunsCommand {
      command: RunsSubcommand::Pull(command),
    }) = cli.command
    else {
      panic!("expected runs pull");
    };
    assert_eq!(
      command.artifact,
      ["outputs/model one.pt", "code/out/metrics.json"]
    );
    assert!(command.outputs && command.dry_run);
    assert!(Cli::try_parse_from(["expri", "runs", "list", "--status", "success"]).is_err());
    assert!(Cli::try_parse_from(["expri", "runs", "pull", "run-123", "--cached"]).is_err());
  }

  #[test]
  fn cached_queries_and_pulls_require_an_explicit_target() {
    for args in [
      vec!["expri", "runs", "list", "--cached"],
      vec!["expri", "runs", "show", "run-123", "--cached"],
      vec!["expri", "runs", "pull", "run-123"],
    ] {
      let cli = Cli::try_parse_from(args).unwrap();
      let Command::Runs(command) = cli.command else {
        panic!("expected runs command");
      };
      let error = run(command, None, 0, true).unwrap_err();
      assert!(error.to_string().contains("requires --target"));
    }
  }

  #[test]
  fn job_commands_parse_log_defaults_and_explicit_controls() {
    let cli = Cli::try_parse_from(["expri", "runs", "logs", "run-123"]).unwrap();
    let Command::Runs(RunsCommand {
      command: RunsSubcommand::Logs(command),
    }) = cli.command
    else {
      panic!("expected runs logs");
    };
    assert_eq!(command.stream, "stdout");
    assert_eq!(command.tail, 100);
    assert!(!command.follow);
    let cli = Cli::try_parse_from([
      "expri", "-T", "gpu", "runs", "logs", "run-123", "--stream", "stderr", "--tail", "0",
      "--follow",
    ])
    .unwrap();
    let Command::Runs(RunsCommand {
      command: RunsSubcommand::Logs(command),
    }) = cli.command
    else {
      panic!("expected runs logs");
    };
    assert_eq!(command.stream, "stderr");
    assert_eq!(command.tail, 0);
    assert!(command.follow);
    for name in ["status", "cancel"] {
      assert!(Cli::try_parse_from(["expri", "runs", name, "run-123", "--json"]).is_ok());
    }
    for args in [
      vec!["expri", "runs", "logs", "run-123", "--stream", "combined"],
      vec!["expri", "runs", "logs", "run-123", "--tail", "-1"],
      vec!["expri", "runs", "logs", "run-123", "--json"],
      vec!["expri", "runs", "status", "run-123", "--cached"],
    ] {
      assert!(Cli::try_parse_from(args).is_err());
    }
  }
}
