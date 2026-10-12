use std::path::PathBuf;

use clap::{Args, Subcommand};
use expri_client::{ServiceCatalogClient, resolve_client_config};
use expri_core::service::RunScope;
use expri_core::service_catalog::{
  RunArchiveFilter, RunFilesPage, RunFilesQuery, RunListPage, RunListQuery,
};

use crate::error::{ExpriError, Result};

#[derive(Debug, Args)]
pub struct QueryCommand {
  /// Print the complete structured page for scripts and agents.
  #[arg(long, global = true)]
  json: bool,
  #[command(subcommand)]
  command: QuerySubcommand,
}

#[derive(Debug, Subcommand)]
enum QuerySubcommand {
  /// List published runs visible to the configured server credentials.
  Runs(RunsOptions),
  /// List received files and reported outputs, including pending uploads.
  Files(FilesOptions),
}

pub fn run(command: QueryCommand, target: Option<&str>, quiet: bool) -> Result<()> {
  if target.is_some() {
    return Err(ExpriError::Message(
      "query commands run on the current machine; omit --target".into(),
    ));
  }
  match command.command {
    QuerySubcommand::Runs(options) => {
      let page = runs(options)?;
      if command.json {
        crate::output::print_report(&page, true)?;
      } else if !quiet {
        print_runs(&page);
      }
    }
    QuerySubcommand::Files(options) => {
      let page = files(options)?;
      if command.json {
        crate::output::print_report(&page, true)?;
      } else if !quiet {
        print_files(&page);
      }
    }
  }
  Ok(())
}

#[derive(Debug, Args)]
struct RunsOptions {
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  config: Option<PathBuf>,
  /// Filter by project; defaults to all projects accessible to the credentials.
  #[arg(long)]
  project_id: Option<String>,
  /// Filter by publishing worker; defaults to all accessible workers.
  #[arg(long)]
  origin: Option<String>,
  /// Select active runs, archived runs within their restore window, or both.
  #[arg(long, default_value = "active", value_parser = ["active", "archived", "all"])]
  archival: String,
  #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=100))]
  limit: u32,
  /// Continue using next_cursor from a prior response, keeping the same filters.
  #[arg(long)]
  cursor: Option<String>,
}

#[derive(Debug, Args)]
struct FilesOptions {
  /// Override the global [server] connection with another client configuration.
  #[arg(long)]
  config: Option<PathBuf>,
  #[arg(long)]
  project_id: String,
  #[arg(long)]
  origin: String,
  #[arg(long)]
  run_id: String,
  /// Literal path prefix, such as outputs/; % and _ are ordinary characters.
  #[arg(long)]
  prefix: Option<String>,
  #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=100))]
  limit: u32,
  /// Continue using next_cursor from a prior response, keeping the same prefix.
  #[arg(long)]
  cursor: Option<String>,
}

fn runs(options: RunsOptions) -> Result<RunListPage> {
  let archival = match options.archival.as_str() {
    "active" => RunArchiveFilter::Active,
    "archived" => RunArchiveFilter::Archived,
    "all" => RunArchiveFilter::All,
    _ => return Err(ExpriError::Message("invalid run archival filter".into())),
  };
  let query = RunListQuery {
    project_id: options.project_id,
    origin: options.origin,
    archival,
    limit: options.limit as usize,
    cursor: options.cursor,
  };
  query.validate()?;
  ServiceCatalogClient::new(&resolve_client_config(options.config.as_deref())?)?.runs(&query)
}

fn files(options: FilesOptions) -> Result<RunFilesPage> {
  let query = RunFilesQuery {
    scope: RunScope {
      project_id: options.project_id,
      origin: options.origin,
      run_id: options.run_id,
    },
    prefix: options.prefix,
    limit: options.limit as usize,
    cursor: options.cursor,
  };
  query.validate()?;
  ServiceCatalogClient::new(&resolve_client_config(options.config.as_deref())?)?.files(&query)
}

fn optional(value: Option<&str>) -> String {
  value.unwrap_or("-").into()
}

fn print_runs(page: &RunListPage) {
  let rows = page
    .runs
    .iter()
    .map(|run| {
      vec![
        run.scope.project_id.clone(),
        run.scope.origin.clone(),
        run.scope.run_id.clone(),
        optional(run.task.as_deref()),
        run.status.clone(),
        optional(run.started_at.as_deref()),
        run
          .exit_code
          .map(|code| code.to_string())
          .unwrap_or_else(|| "-".into()),
        if run.archived { "yes" } else { "no" }.into(),
      ]
    })
    .collect::<Vec<_>>();
  print!(
    "{}",
    crate::output::table(
      &[
        "PROJECT", "ORIGIN", "RUN", "TASK", "STATUS", "STARTED", "EXIT", "ARCHIVED"
      ],
      &rows,
      &[18, 18, 32, 24, 12, 25, 6, 8],
    )
  );
  if rows.is_empty() {
    println!("No published runs.");
  }
  for run in &page.runs {
    for warning in &run.warnings {
      eprintln!(
        "warning: {}/{}/{}: {}",
        run.scope.project_id,
        run.scope.origin,
        run.scope.run_id,
        crate::output::text(warning)
      );
    }
  }
  print_cursor(page.next_cursor.as_deref());
}

fn print_files(page: &RunFilesPage) {
  println!(
    "Project: {}  Origin: {}  Run: {}",
    page.scope.project_id, page.scope.origin, page.scope.run_id
  );
  let rows = page
    .files
    .iter()
    .map(|file| {
      use expri_core::service_catalog::RunFileAvailability;
      let availability = match file.availability {
        RunFileAvailability::Received => "received",
        RunFileAvailability::ReportedOnly => "reported_only",
        RunFileAvailability::ReceivedDifferentVersion => "received_different_version",
      };
      let labels = file
        .reported
        .as_ref()
        .map(|reported| {
          reported
            .labels
            .iter()
            .map(|label| {
              if file.labels_pending {
                format!("{label} (pending)")
              } else {
                label.clone()
              }
            })
            .collect::<Vec<_>>()
            .join(", ")
        })
        .filter(|labels| !labels.is_empty())
        .unwrap_or_else(|| "-".into());
      vec![
        file.path.clone(),
        availability.into(),
        file
          .received
          .as_ref()
          .map(|received| crate::output::bytes(received.size))
          .unwrap_or_else(|| "-".into()),
        file
          .reported
          .as_ref()
          .map(|reported| crate::output::bytes(reported.size))
          .unwrap_or_else(|| "-".into()),
        labels,
        optional(
          file
            .reported
            .as_ref()
            .and_then(|reported| reported.sync_status.as_deref()),
        ),
      ]
    })
    .collect::<Vec<_>>();
  print!(
    "{}",
    crate::output::table(
      &[
        "PATH",
        "AVAILABILITY",
        "RECEIVED SIZE",
        "REPORTED SIZE",
        "LABELS",
        "SYNC"
      ],
      &rows,
      &[52, 26, 13, 13, 30, 18],
    )
  );
  if rows.is_empty() {
    println!("No files.");
  }
  if let Some(time) = &page.inventory_recorded_at {
    println!("Inventory recorded: {}", crate::output::text(time));
  }
  if page.reported_inventory_truncated {
    eprintln!("warning: reported file inventory was truncated; received records remain available");
  }
  for warning in &page.warnings {
    eprintln!("warning: {}", crate::output::text(warning));
  }
  for file in &page.files {
    if let Some(error) = file
      .reported
      .as_ref()
      .and_then(|reported| reported.sync_error.as_ref())
    {
      eprintln!(
        "warning: {}: {}",
        crate::output::text(&file.path),
        crate::output::text(error)
      );
    }
    if matches!(
      file.received.as_ref().map(|received| &received.storage),
      Some(expri_core::service::FileStorage::Tracking { sealed: false, .. })
    ) {
      println!(
        "{}: received tracking data is still growing.",
        crate::output::text(&file.path)
      );
    }
  }
  print_cursor(page.next_cursor.as_deref());
}

fn print_cursor(cursor: Option<&str>) {
  if let Some(cursor) = cursor {
    println!("\nNext cursor: {}", crate::output::text(cursor));
    println!("Continue with --cursor and the same scope and filters.");
  }
}

#[cfg(test)]
mod tests {
  use clap::{CommandFactory, Parser};

  use super::*;
  use crate::{Cli, Command};

  #[test]
  fn query_defaults_and_filters_need_no_local_project_or_worker() {
    for args in [
      vec!["expri", "query", "runs"],
      vec!["expri", "query", "runs", "--config", "client.toml"],
      vec![
        "expri",
        "query",
        "runs",
        "--origin",
        "gpu-1",
        "--archival",
        "all",
        "--cursor",
        "v1:project:gpu-1:run-one",
      ],
      vec![
        "expri",
        "query",
        "files",
        "--project-id",
        "project",
        "--origin",
        "worker",
        "--run-id",
        "run-one",
      ],
      vec![
        "expri",
        "query",
        "files",
        "--config",
        "client.toml",
        "--project-id",
        "project",
        "--origin",
        "worker",
        "--run-id",
        "run-one",
        "--prefix",
        "outputs/a_%/",
        "--limit",
        "1",
      ],
    ] {
      assert!(Cli::try_parse_from(args).is_ok());
    }
    for limit in ["0", "101"] {
      assert!(Cli::try_parse_from(["expri", "query", "runs", "--limit", limit]).is_err());
    }
    assert!(Cli::try_parse_from(["expri", "query", "files", "--run-id", "run-one"]).is_err());
  }

  #[test]
  fn query_rejects_ssh_target_before_opening_client_configuration() {
    let cli = Cli::try_parse_from([
      "expri",
      "-T",
      "worker",
      "query",
      "runs",
      "--config",
      "/missing/client.toml",
    ])
    .unwrap();
    let Command::Query(command) = cli.command else {
      unreachable!()
    };
    assert!(
      run(command, cli.target.as_deref(), false)
        .unwrap_err()
        .to_string()
        .contains("current machine")
    );
  }

  #[test]
  fn help_exposes_query_browsing_and_keeps_service_management_distinct() {
    let mut cli = Cli::command();
    let query = cli.find_subcommand_mut("query").unwrap();
    let help = query.render_long_help().to_string();
    assert!(help.contains("runs"));
    assert!(help.contains("files"));
    assert!(query.find_subcommand("list").is_none());
    let service = cli.find_subcommand_mut("service").unwrap();
    assert!(service.find_subcommand("list").is_some());
    let run = service.find_subcommand_mut("run").unwrap();
    assert!(run.find_subcommand("list").is_none());
    assert!(run.find_subcommand("files").is_none());
    for operation in ["list", "files"] {
      assert!(
        Cli::try_parse_from([
          "expri",
          "service",
          "run",
          operation,
          "--project-id",
          "project",
          "--origin",
          "worker",
          "--run-id",
          "run-one"
        ])
        .is_err()
      );
    }
    assert!(
      Cli::try_parse_from([
        "expri",
        "service",
        "list",
        "--project-id",
        "project",
        "--origin",
        "worker"
      ])
      .is_ok()
    );
  }
}
