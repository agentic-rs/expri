use crate::FetchCommand;
use crate::context::CommandContext;
use crate::error::{ExpriError, Result};
use crate::service::{FetchOptions, fetch_files};

pub fn run(command: FetchCommand, target: Option<&str>, quiet: bool) -> Result<()> {
  if target.is_some() {
    return Err(ExpriError::Message(
      "fetch reads from the expri service; configure worker origins in [fetch].origins and omit -T. Use expri -T <worker> push to update a worker checkout."
        .into(),
    ));
  }
  let context = CommandContext::load(command.config.clone(), command.repo.clone())?;
  let options = fetch_options(context, command, quiet)?;
  let report = fetch_files(options)?;
  if !quiet {
    println!("{}", serde_json::to_string(&report)?);
  }
  Ok(())
}

fn fetch_options(
  context: CommandContext,
  command: FetchCommand,
  quiet: bool,
) -> Result<FetchOptions> {
  let fetch = context.config.fetch.as_ref().ok_or_else(|| {
    ExpriError::Message(
      "fetch requires a [fetch] section with client_config, project_id, and origins in expri.toml"
        .into(),
    )
  })?;
  Ok(FetchOptions {
    config: if fetch.client_config.is_absolute() {
      fetch.client_config.clone()
    } else {
      context.repo_root.join(&fetch.client_config)
    },
    project_id: fetch.project_id.clone(),
    origins: fetch.origins.clone(),
    repo: context.repo_root.clone(),
    results_dir: context.config.download_results_dir().into(),
    artifacts: fetch.artifacts.clone(),
    labels: fetch.labels.clone(),
    watch: command.watch,
    dry_run: command.dry_run,
    quiet,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn command() -> FetchCommand {
    FetchCommand {
      config: None,
      repo: None,
      dry_run: true,
      watch: true,
    }
  }

  #[test]
  fn fetch_rejects_ssh_target_before_loading_files() {
    let error = run(command(), Some("gpu-1"), true).unwrap_err();
    assert!(error.to_string().contains("[fetch].origins"));
    assert!(error.to_string().contains("omit -T"));
  }

  #[test]
  fn fetch_requires_its_own_configuration() {
    let context = CommandContext {
      config: toml::from_str("[target.gpu]\nhost='gpu.example'\nremote_dir='/srv/project'\n")
        .unwrap(),
      repo_root: "/repo".into(),
      project_name: None,
    };
    let error = fetch_options(context, command(), true).unwrap_err();
    assert!(error.to_string().contains("requires a [fetch] section"));
  }

  #[test]
  fn fetch_uses_explicit_origins_without_an_ssh_target() {
    let context = CommandContext {
      config: toml::from_str(
        "[fetch]\nclient_config='owner.toml'\nproject_id='vision'\norigins=['gpu-1','gpu-2']\nlabels=['best']\n[download]\nresults_dir='results'\n",
      )
      .unwrap(),
      repo_root: "/repo".into(),
      project_name: None,
    };
    let options = fetch_options(context, command(), true).unwrap();
    assert_eq!(options.config, std::path::PathBuf::from("/repo/owner.toml"));
    assert_eq!(options.repo, std::path::PathBuf::from("/repo"));
    assert_eq!(options.origins, ["gpu-1", "gpu-2"]);
    assert_eq!(options.labels, ["best"]);
    assert!(options.watch);
    assert!(options.dry_run);
    assert!(options.quiet);
  }
}
