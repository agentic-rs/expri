use clap::Args;
use expri_core::experiment_assignment::{ExperimentSelection, RunExperimentIntent};

use crate::config::RunServiceConfig;
use crate::error::{ExpriError, Result};

#[derive(Debug, Default, Args)]
#[group(skip)]
pub(crate) struct RunExperiment {
  /// Add the run to an existing experiment in the publishing project.
  #[arg(long, value_name = "ID", conflicts_with_all = ["new_experiment", "join_run", "no_publish"])]
  experiment: Option<String>,
  /// Create an experiment for this run.
  #[arg(long, value_name = "NAME", conflicts_with_all = ["experiment", "join_run", "no_publish"])]
  new_experiment: Option<String>,
  /// Join a published run's experiment, creating one for both runs if needed.
  #[arg(long, value_name = "RUN", conflicts_with_all = ["experiment", "new_experiment", "no_publish"])]
  join_run: Option<String>,
  /// Disambiguate the joined run's machine/origin in the same project.
  #[arg(long, value_name = "ORIGIN", requires = "join_run")]
  join_origin: Option<String>,
}

impl RunExperiment {
  pub(crate) fn resolve(&self, request_id: Option<&str>) -> Result<Option<RunExperimentIntent>> {
    let selection = if let Some(experiment_id) = &self.experiment {
      ExperimentSelection::Existing {
        experiment_id: experiment_id.clone(),
      }
    } else if let Some(name) = &self.new_experiment {
      ExperimentSelection::New {
        name: name.trim().into(),
      }
    } else if let Some(run_id) = &self.join_run {
      ExperimentSelection::JoinRun {
        run_id: run_id.clone(),
        origin: self.join_origin.clone(),
      }
    } else {
      return Ok(None);
    };
    let intent = RunExperimentIntent {
      request_id: request_id
        .map(str::to_owned)
        .unwrap_or_else(|| format!("group-{}", uuid::Uuid::new_v4().simple())),
      selection,
    };
    intent.validate()?;
    Ok(Some(intent))
  }
}

pub(crate) fn validate_mode(
  experiment: Option<&RunExperimentIntent>,
  service: Option<&RunServiceConfig>,
  environment: bool,
) -> Result<()> {
  if experiment.is_some() && (!environment || !service.is_some_and(|service| service.publish)) {
    return Err(ExpriError::Message(
      "experiment assignment requires a configured environment and automatic service publishing"
        .into(),
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use clap::Parser;

  fn parse(arguments: &[&str]) -> crate::RunCommand {
    let crate::Command::Run(command) = crate::Cli::try_parse_from(arguments).unwrap().command
    else {
      panic!("expected run");
    };
    command
  }

  #[test]
  fn experiment_selection_is_explicit_and_task_arguments_stay_untouched() {
    let command = parse(&[
      "expri",
      "run",
      "--join-run",
      "run-parent",
      "--join-origin",
      "worker-b",
      "train",
      "--experiment",
      "task-value",
    ]);
    let intent = command
      .experiment
      .resolve(Some("submission-one"))
      .unwrap()
      .unwrap();
    assert_eq!(intent.request_id, "submission-one");
    assert_eq!(
      intent.selection,
      ExperimentSelection::JoinRun {
        run_id: "run-parent".into(),
        origin: Some("worker-b".into())
      }
    );
    assert_eq!(command.task, ["train", "--experiment", "task-value"]);
    let command = parse(&[
      "expri",
      "run",
      "--new-experiment",
      "  Scale study  ",
      "train",
    ]);
    assert_eq!(
      command.experiment.resolve(Some("same-submission")).unwrap(),
      command.experiment.resolve(Some("same-submission")).unwrap()
    );
    assert!(
      matches!(command.experiment.resolve(None).unwrap().unwrap().selection, ExperimentSelection::New { name } if name == "Scale study")
    );
    assert!(
      parse(&["expri", "run", "train"])
        .experiment
        .resolve(None)
        .unwrap()
        .is_none()
    );
  }

  #[test]
  fn invalid_or_conflicting_selection_is_rejected_before_launch() {
    for args in [
      vec![
        "expri",
        "run",
        "--experiment",
        "exp-one",
        "--join-run",
        "run-one",
        "train",
      ],
      vec![
        "expri",
        "run",
        "--new-experiment",
        "New",
        "--experiment",
        "exp-one",
        "train",
      ],
      vec!["expri", "run", "--join-origin", "worker", "train"],
      vec![
        "expri",
        "run",
        "--join-run",
        "run-one",
        "--no-publish",
        "train",
      ],
    ] {
      assert!(crate::Cli::try_parse_from(args).is_err());
    }
    for args in [
      vec!["expri", "run", "--experiment", "../invalid", "train"],
      vec!["expri", "run", "--new-experiment", "  ", "train"],
    ] {
      assert!(parse(&args).experiment.resolve(None).is_err());
    }
    let selected = parse(&["expri", "run", "--experiment", "exp-one", "train"])
      .experiment
      .resolve(None)
      .unwrap();
    assert!(validate_mode(selected.as_ref(), None, true).is_err());
    let mut service: RunServiceConfig =
      toml::from_str("client_config='/tmp/client.toml'\nproject_id='project'\norigin='worker'\n")
        .unwrap();
    assert!(validate_mode(selected.as_ref(), Some(&service), false).is_err());
    validate_mode(selected.as_ref(), Some(&service), true).unwrap();
    service.publish = false;
    assert!(validate_mode(selected.as_ref(), Some(&service), true).is_err());
  }
}
