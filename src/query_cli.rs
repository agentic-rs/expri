use std::path::PathBuf;

use clap::{Args, Subcommand};
use expri_client::{ServiceCatalogClient, resolve_client_config};
use expri_core::service::RunScope;
use expri_core::service_catalog::{RunArchiveFilter, RunFilesQuery, RunListQuery};
use serde_json::Value;

use crate::error::{ExpriError, Result};

#[derive(Debug, Args)]
pub struct QueryCommand {
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

pub fn run(command: QueryCommand, target: Option<&str>) -> Result<()> {
  if target.is_some() {
    return Err(ExpriError::Message(
      "query commands run on the current machine; omit --target".into(),
    ));
  }
  let report = match command.command {
    QuerySubcommand::Runs(options) => runs(options)?,
    QuerySubcommand::Files(options) => files(options)?,
  };
  println!("{}", serde_json::to_string(&report)?);
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

fn runs(options: RunsOptions) -> Result<Value> {
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
  let page =
    ServiceCatalogClient::new(&resolve_client_config(options.config.as_deref())?)?.runs(&query)?;
  serde_json::to_value(page).map_err(Into::into)
}

fn files(options: FilesOptions) -> Result<Value> {
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
  let page =
    ServiceCatalogClient::new(&resolve_client_config(options.config.as_deref())?)?.files(&query)?;
  serde_json::to_value(page).map_err(Into::into)
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
      run(command, cli.target.as_deref())
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
