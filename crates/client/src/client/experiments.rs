//! Read-only owner access to the server's bounded experiment catalog.
use std::path::Path;

use expri_core::experiment_catalog::{
  EXPERIMENT_CATALOG_CAPABILITY, Experiment, ExperimentCatalog,
};

use super::{Api, fs::message};
use crate::error::Result;
use crate::types::{Request, Response, validate_component};

pub struct ExperimentCatalogClient {
  api: Api,
}

impl ExperimentCatalogClient {
  pub fn new(config: &Path) -> Result<Self> {
    Self::from_api(Api::new(config)?)
  }

  fn from_api(api: Api) -> Result<Self> {
    let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
      return Err(message(
        "service did not return its experiment capabilities",
      ));
    };
    if !features
      .iter()
      .any(|feature| feature == EXPERIMENT_CATALOG_CAPABILITY)
    {
      return Err(message(
        "experiment discovery is unavailable for this service or credential; upgrade the server and use owner credentials from `expri login`",
      ));
    }
    Ok(Self { api })
  }

  pub fn list(&self, project_id: &str) -> Result<ExperimentCatalog> {
    validate_component(project_id)?;
    let Response::Experiments { catalog } = self.api.request(&Request::ListExperiments {
      project_id: project_id.into(),
    })?
    else {
      return Err(message("service did not return an experiment catalog"));
    };
    catalog
      .validate(project_id)
      .map_err(|_| message("service returned an inconsistent experiment catalog"))?;
    Ok(catalog)
  }

  /// IDs are exact and project scoped. Names deliberately do not select groups.
  pub fn show(&self, project_id: &str, experiment_id: &str) -> Result<Experiment> {
    validate_component(experiment_id)?;
    self
      .list(project_id)?
      .experiments
      .into_iter()
      .find(|experiment| experiment.experiment_id == experiment_id)
      .ok_or_else(|| message("experiment ID was not found in the requested project"))
  }
}

#[cfg(test)]
mod tests;
