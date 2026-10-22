use std::fmt::Write;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use expri_client::{ExperimentCatalogClient, resolve_client_config};
use expri_core::experiment_catalog::{Experiment, ExperimentCatalog, LineageKind};
use expri_core::service::{RunScope, validate_component};

use crate::error::{ExpriError, Result};

#[derive(Debug, Args)]
pub struct ExperimentsCommand {
  /// Print complete structured records, including origin-qualified run identities.
  #[arg(long, global = true)]
  json: bool,
  #[command(subcommand)]
  command: ExperimentsSubcommand,
}

#[derive(Debug, Subcommand)]
enum ExperimentsSubcommand {
  /// List a project's experiments using the configured server connection.
  List(Connection),
  /// Show one exact experiment ID, its members, and lineage.
  Show(ShowCommand),
}

#[derive(Debug, Args)]
struct Connection {
  /// Override the global [server] connection with a standalone client file.
  #[arg(long)]
  config: Option<PathBuf>,
  /// Project to inspect. Experiment discovery requires owner credentials.
  #[arg(long)]
  project_id: String,
}

#[derive(Debug, Args)]
struct ShowCommand {
  #[command(flatten)]
  connection: Connection,
  /// Stable experiment ID from `expri experiments list`, not its display name.
  experiment_id: String,
}

pub fn run(command: ExperimentsCommand, target: Option<&str>, quiet: bool) -> Result<()> {
  if target.is_some() {
    return Err(ExpriError::Message(
      "experiments commands query the configured server; omit --target".into(),
    ));
  }
  let (connection, experiment_id) = match command.command {
    ExperimentsSubcommand::List(connection) => (connection, None),
    ExperimentsSubcommand::Show(command) => (command.connection, Some(command.experiment_id)),
  };
  // Reject selectors before resolving credentials or making a request.
  validate_component(&connection.project_id)?;
  if let Some(id) = &experiment_id {
    validate_component(id)?;
  }
  let config = resolve_client_config(connection.config.as_deref())?;
  let client = ExperimentCatalogClient::new(&config)?;
  if let Some(id) = experiment_id {
    let experiment = client.show(&connection.project_id, &id)?;
    if command.json {
      crate::output::print_report(
        &serde_json::json!({"project_id":connection.project_id,"experiment":experiment}),
        true,
      )?;
    } else if !quiet {
      print!("{}", show_text(&connection.project_id, &experiment));
    }
  } else {
    let catalog = client.list(&connection.project_id)?;
    if command.json {
      crate::output::print_report(&catalog, true)?;
    } else if !quiet {
      print!("{}", list_text(&catalog));
    }
  }
  Ok(())
}

fn list_text(catalog: &ExperimentCatalog) -> String {
  let mut output = format!("Project: {}\n", catalog.project_id);
  if catalog.experiments.is_empty() {
    output.push_str("No recorded experiments.\n");
    return output;
  }
  writeln!(
    output,
    "{:<28} {:>4} {:>11} {:<25} NAME",
    "EXPERIMENT", "RUNS", "UNAVAILABLE", "UPDATED"
  )
  .unwrap();
  let mut experiments: Vec<_> = catalog.experiments.iter().collect();
  experiments.sort_by(|a, b| {
    b.updated_at
      .cmp(&a.updated_at)
      .then_with(|| a.experiment_id.cmp(&b.experiment_id))
  });
  for experiment in experiments {
    writeln!(
      output,
      "{:<28} {:>4} {:>11} {:<25} {}",
      experiment.experiment_id,
      experiment.membership_count,
      experiment.unavailable_runs.len(),
      experiment.updated_at,
      experiment.name
    )
    .unwrap();
  }
  output
}

fn identity(scope: &RunScope) -> String {
  format!("{}:{}", scope.origin, scope.run_id)
}

fn show_text(project_id: &str, experiment: &Experiment) -> String {
  let mut output = format!(
    "Experiment: {}\nName: {}\nProject: {project_id}\nCreated: {}\nUpdated: {}\n",
    experiment.experiment_id, experiment.name, experiment.created_at, experiment.updated_at,
  );
  if !experiment.description.is_empty() {
    writeln!(output, "Description: {}", experiment.description).unwrap();
  }
  writeln!(output, "Runs: {}", experiment.membership_count).unwrap();
  for scope in &experiment.members {
    writeln!(
      output,
      "  {}{}",
      identity(scope),
      if experiment.unavailable_runs.contains(scope) {
        " (unavailable)"
      } else {
        ""
      }
    )
    .unwrap();
  }
  writeln!(output, "Lineage: {}", experiment.lineage.len()).unwrap();
  for edge in &experiment.lineage {
    let kind = match edge.kind {
      LineageKind::Continued => "continued",
      LineageKind::Restarted => "restarted",
      LineageKind::Split => "split",
    };
    write!(
      output,
      "  {} -> {} ({kind})",
      identity(&edge.parent),
      identity(&edge.child)
    )
    .unwrap();
    if let Some(checkpoint) = &edge.checkpoint {
      write!(output, "; checkpoint: {checkpoint}").unwrap();
    }
    if let Some(step) = &edge.step {
      write!(output, "; step: {step}").unwrap();
    }
    output.push('\n');
  }
  output
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{Cli, Command};
  use clap::{CommandFactory, Parser};
  use expri_core::experiment_catalog::{ExperimentLimits, Lineage};

  #[test]
  fn discovery_parses_server_options_and_requires_an_explicit_project() {
    let cli = Cli::try_parse_from([
      "expri",
      "experiments",
      "show",
      "exp-one",
      "--project-id",
      "project",
      "--config",
      "service.toml",
      "--json",
    ])
    .unwrap();
    let Command::Experiments(ExperimentsCommand {
      json,
      command: ExperimentsSubcommand::Show(command),
    }) = cli.command
    else {
      panic!("expected experiment show");
    };
    assert!(json);
    assert_eq!(command.experiment_id, "exp-one");
    assert_eq!(command.connection.project_id, "project");
    assert_eq!(command.connection.config, Some("service.toml".into()));
    assert!(Cli::try_parse_from(["expri", "experiments", "list"]).is_err());
    let mut cli = Cli::command();
    let help = cli
      .find_subcommand_mut("experiments")
      .unwrap()
      .find_subcommand_mut("show")
      .unwrap()
      .render_long_help()
      .to_string();
    assert!(help.contains("owner credentials") && help.contains("Stable experiment ID"));
  }

  #[test]
  fn remote_execution_and_invalid_selectors_fail_before_config_is_loaded() {
    for (target, project_id, expected) in [
      (Some("worker"), "project", "omit --target"),
      (None, "../project", "invalid"),
    ] {
      let command = ExperimentsCommand {
        json: false,
        command: ExperimentsSubcommand::List(Connection {
          config: Some("/does-not-exist".into()),
          project_id: project_id.into(),
        }),
      };
      assert!(
        run(command, target, false)
          .unwrap_err()
          .to_string()
          .contains(expected)
      );
    }
  }

  #[test]
  fn text_keeps_same_run_ids_distinct_and_exposes_lineage_and_unavailable_members() {
    let scope = |origin: &str| RunScope {
      project_id: "project".into(),
      origin: origin.into(),
      run_id: "run-one".into(),
    };
    let experiment = Experiment {
      experiment_id: "exp-one".into(),
      name: "Training".into(),
      description: "Goal".into(),
      revision: "1".into(),
      members: vec![scope("worker-a"), scope("worker-b")],
      unavailable_runs: vec![scope("worker-a")],
      membership_count: 2,
      lineage: vec![Lineage {
        parent: scope("worker-a"),
        child: scope("worker-b"),
        kind: LineageKind::Restarted,
        checkpoint: Some("outputs/best.pt".into()),
        step: Some(u64::MAX.to_string()),
      }],
      created_at: "2026-10-20T00:00:00Z".into(),
      updated_at: "2026-10-20T00:00:00Z".into(),
    };
    let output = show_text("project", &experiment);
    assert!(output.contains("worker-a:run-one (unavailable)"));
    assert!(output.contains("worker-a:run-one -> worker-b:run-one (restarted); checkpoint: outputs/best.pt; step: 18446744073709551615"));
    let mut catalog = ExperimentCatalog {
      project_id: "project".into(),
      revision: "1".into(),
      experiments: vec![experiment],
      limits: ExperimentLimits::default(),
    };
    assert!(list_text(&catalog).contains("exp-one"));
    catalog.experiments.clear();
    assert!(list_text(&catalog).contains("No recorded experiments."));
  }
}
