//! Read-only published run and file catalogs over the bounded service transport.

use std::path::Path;

use expri_core::service_catalog::{
  RunFilesPage, RunFilesQuery, RunListPage, RunListQuery, SERVICE_CATALOG_CAPABILITY,
  validate_run_files_page, validate_run_list_page,
};

use super::{Api, fs};
use crate::error::Result;
use crate::types::{Request, Response};

pub struct ServiceCatalogClient {
  api: Api,
}

impl ServiceCatalogClient {
  pub fn new(config: &Path) -> Result<Self> {
    Self::from_api(Api::new(config)?)
  }

  fn from_api(api: Api) -> Result<Self> {
    let Response::Capabilities { features } = api.request(&Request::Capabilities)? else {
      return Err(fs::message("service did not return its capabilities"));
    };
    if !features
      .iter()
      .any(|feature| feature == SERVICE_CATALOG_CAPABILITY)
    {
      return Err(fs::message(
        "service does not support service-catalog-v1; upgrade the server",
      ));
    }
    Ok(Self { api })
  }

  /// Inspect published records without fetching run bytes or writing a local cache.
  pub fn runs(&self, query: &RunListQuery) -> Result<RunListPage> {
    query.validate()?;
    let Response::RunPage { page } = self.api.request(&Request::BrowseRuns {
      query: query.clone(),
    })?
    else {
      return Err(fs::message("service did not return a run catalog page"));
    };
    validate_run_list_page(query, &page)
      .map_err(|_| fs::message("service returned an inconsistent run catalog page"))?;
    Ok(page)
  }

  /// Received records are authoritative; reported outputs may still await upload.
  pub fn files(&self, query: &RunFilesQuery) -> Result<RunFilesPage> {
    query.validate()?;
    let Response::RunFilesPage { page } = self.api.request(&Request::BrowseRunFiles {
      query: query.clone(),
    })?
    else {
      return Err(fs::message(
        "service did not return a run file catalog page",
      ));
    };
    validate_run_files_page(query, &page)
      .map_err(|_| fs::message("service returned an inconsistent run file catalog page"))?;
    Ok(page)
  }
}

#[cfg(test)]
mod tests;
