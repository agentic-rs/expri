use std::collections::BTreeSet;
use std::path::PathBuf;

use clap::Args;
use serde_json::Value;

use crate::context::CommandContext;
use crate::controller;
use crate::error::{ExpriError, Result};
use crate::metrics::{self, Comparison, Reduction, RunMetrics};
use crate::runs_cli::ConnectionArgs;

#[derive(Debug, Args)]
pub struct MetricsCommand {
  #[command(flatten)]
  connection: ConnectionArgs,
  run_id: String,
  /// Select a named metric; repeat to select several.
  #[arg(long, value_name = "NAME")]
  metric: Vec<String>,
  /// Read previously pulled metric files without contacting the target.
  #[arg(long)]
  cached: bool,
  #[arg(long)]
  json: bool,
}

#[derive(Debug, Args)]
pub struct CompareCommand {
  #[command(flatten)]
  connection: ConnectionArgs,
  #[arg(value_name = "RUN_ID", num_args = 2..)]
  run_ids: Vec<String>,
  /// Select a named metric; repeat to select several.
  #[arg(long, value_name = "NAME")]
  metric: Vec<String>,
  #[arg(long, default_value = "last", value_parser = ["last", "min", "max"])]
  reduction: String,
  /// Write a standalone local HTML chart.
  #[arg(long, value_name = "PATH")]
  chart: Option<PathBuf>,
  #[arg(long)]
  cached: bool,
  #[arg(long)]
  json: bool,
}

pub fn run_metrics(
  command: MetricsCommand,
  target: Option<&str>,
  verbosity: u8,
  quiet: bool,
) -> Result<()> {
  let runs = load_metrics(
    command.connection,
    command.cached,
    target,
    vec![command.run_id],
    &command.metric,
    verbosity,
    quiet,
  )?;
  let run = runs.first().expect("one requested run");
  if command.json {
    println!("{}", serde_json::to_string_pretty(run)?);
  } else if !quiet {
    print_metrics(run);
    print_warnings(&run.warnings);
  }
  Ok(())
}

pub fn run_compare(
  command: CompareCommand,
  target: Option<&str>,
  verbosity: u8,
  quiet: bool,
) -> Result<()> {
  if command.run_ids.iter().collect::<BTreeSet<_>>().len() != command.run_ids.len() {
    return Err(ExpriError::Message(
      "comparison requires distinct run IDs".to_string(),
    ));
  }
  let runs = load_metrics(
    command.connection,
    command.cached,
    target,
    command.run_ids,
    &command.metric,
    verbosity,
    quiet,
  )?;
  let reduction = match command.reduction.as_str() {
    "last" => Reduction::Last,
    "min" => Reduction::Min,
    "max" => Reduction::Max,
    _ => unreachable!("clap validates reductions"),
  };
  let comparison = metrics::compare(&runs, &command.metric, reduction)?;
  if let Some(path) = command.chart {
    crate::metric_charts::write_chart(&path, &runs, &command.metric)?;
    if !quiet {
      eprintln!("Chart: {}", std::path::absolute(path)?.display());
    }
  }
  if command.json {
    println!("{}", serde_json::to_string_pretty(&comparison)?);
  } else if !quiet {
    print_comparison(&comparison);
    print_warnings(&comparison.warnings);
  }
  Ok(())
}

fn load_metrics(
  connection: ConnectionArgs,
  cached: bool,
  target: Option<&str>,
  run_ids: Vec<String>,
  filters: &[String],
  verbosity: u8,
  quiet: bool,
) -> Result<Vec<RunMetrics>> {
  if cached && target.is_none() {
    return Err(ExpriError::Message(
      "--cached requires --target (-T)".to_string(),
    ));
  }
  let context = CommandContext::load(connection.config, connection.repo)?;
  let Some(target) = target else {
    let runs_dir = context.repo_root.join(".expri/runs");
    return run_ids
      .iter()
      .map(|id| metrics::read(&runs_dir, id, filters))
      .collect();
  };
  controller::run_pull::validate_component(target, "target name")?;
  let results_dir = context.config.download_results_dir();
  if cached {
    let runs_dir = controller::run_pull::cached_runs_dir(&context.repo_root, &results_dir, target)?;
    return run_ids
      .iter()
      .map(|id| metrics::read(&runs_dir, id, filters))
      .collect();
  }
  let context = context.into_target(Some(target), connection.control_path)?;
  let fetched = controller::runs::fetch_metrics(controller::runs::MetricsFetchOptions {
    repo_root: context.repo_root,
    target_name: context.target_name,
    target: context.target,
    results_dir,
    control_path: context.control_path,
    control_persist: connection.control_persist,
    verbosity,
    quiet,
    run_ids,
  })?;
  fetched
    .selections
    .into_iter()
    .map(|selection| {
      let mut run = metrics::read_with_files(
        &fetched.runs_dir,
        &selection.run_id,
        filters,
        selection.files,
      )?;
      for warning in selection.warnings {
        if !run.warnings.contains(&warning) {
          run.warnings.push(warning);
        }
      }
      Ok(run)
    })
    .collect()
}

fn text(value: &Value) -> String {
  match value {
    Value::String(value) => value.clone(),
    Value::Null => "-".to_string(),
    value => value.to_string(),
  }
}

fn print_metrics(run: &RunMetrics) {
  println!("Run: {}", run.run_id);
  println!("Task: {}", text(&run.run["task"]));
  println!("Status: {}", text(&run.run["status"]));
  if let Some(params) = &run.params {
    println!("Parameters: {params}");
  }
  println!(
    "{:<30} {:>8} {:>14} {:>14} {:>14} {:>10}",
    "METRIC", "POINTS", "LAST", "MIN", "MAX", "LAST STEP"
  );
  for (name, series) in &run.metrics {
    println!(
      "{:<30} {:>8} {:>14} {:>14} {:>14} {:>10}",
      name,
      series.summary.count,
      series.summary.last.value,
      series.summary.min.value,
      series.summary.max.value,
      series.summary.last.step,
    );
  }
  if run.metrics.is_empty() {
    println!("No matching metrics.");
  }
}

fn print_comparison(comparison: &Comparison) {
  let reduction = match comparison.reduction {
    Reduction::Last => "last",
    Reduction::Min => "min",
    Reduction::Max => "max",
  };
  println!("Reduction: {reduction}");
  let mut headers = vec!["RUN".to_string(), "TASK".to_string(), "STATUS".to_string()];
  headers.extend(comparison.metric_names.iter().cloned());
  println!("{}", headers.join("\t"));
  for run in &comparison.runs {
    let mut values = vec![
      run.run_id.clone(),
      text(&run.run["task"]),
      text(&run.run["status"]),
    ];
    for name in &comparison.metric_names {
      values.push(
        run
          .values
          .get(name)
          .and_then(Option::as_ref)
          .map(|point| point.value.to_string())
          .unwrap_or_else(|| "-".to_string()),
      );
    }
    println!("{}", values.join("\t"));
  }
}

fn print_warnings(warnings: &[Value]) {
  for warning in warnings {
    eprintln!(
      "warning: {}: {}",
      text(&warning["run_id"]),
      text(&warning["message"])
    );
  }
}

#[cfg(test)]
mod tests {
  use clap::Parser;

  use crate::Cli;

  #[test]
  fn metrics_comparison_parses_filters_reductions_and_offline_chart_options() {
    assert!(
      Cli::try_parse_from([
        "expri",
        "runs",
        "metrics",
        "run-one",
        "--metric",
        "train/loss",
        "--json"
      ])
      .is_ok()
    );
    assert!(
      Cli::try_parse_from([
        "expri",
        "-T",
        "gpu",
        "runs",
        "compare",
        "run-one",
        "run-two",
        "--cached",
        "--metric",
        "loss",
        "--metric",
        "accuracy",
        "--reduction",
        "min",
        "--chart",
        "comparison.html",
        "--json",
      ])
      .is_ok()
    );
    for args in [
      vec!["expri", "runs", "compare", "run-one"],
      vec![
        "expri",
        "runs",
        "compare",
        "run-one",
        "run-two",
        "--reduction",
        "mean",
      ],
      vec![
        "expri",
        "runs",
        "metrics",
        "run-one",
        "--chart",
        "chart.html",
      ],
    ] {
      assert!(Cli::try_parse_from(args).is_err());
    }
  }
}
