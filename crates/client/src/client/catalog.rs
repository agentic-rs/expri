//! Read-only published run and file catalogs over the bounded service transport.

use std::cmp::Ordering;
use std::path::Path;

use expri_core::service_catalog::{
  MAX_CATALOG_ROWS, RUN_SUMMARY_CAPABILITY, RunArchiveFilter, RunFilesPage, RunFilesQuery,
  RunListPage, RunListQuery, RunSelector, SERVICE_CATALOG_CAPABILITY, validate_run_files_page,
  validate_run_list_page,
};

use super::{Api, fs};
use crate::error::Result;
use crate::types::{Request, Response, RunScope};

pub struct ServiceCatalogClient {
  api: Api,
  summaries_supported: bool,
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
    let summaries_supported = features
      .iter()
      .any(|feature| feature == RUN_SUMMARY_CAPABILITY);
    Ok(Self {
      api,
      summaries_supported,
    })
  }

  /// Inspect published records without fetching run bytes or writing a local cache.
  pub fn runs(&self, query: &RunListQuery) -> Result<RunListPage> {
    query.validate()?;
    let request = if self.summaries_supported {
      Request::BrowseRunSummaries {
        query: query.clone(),
      }
    } else {
      Request::BrowseRuns {
        query: query.clone(),
      }
    };
    let Response::RunPage { page } = self.api.request(&request)? else {
      return Err(fs::message("service did not return a run catalog page"));
    };
    validate_run_list_page(query, &page)
      .map_err(|_| fs::message("service returned an inconsistent run catalog page"))?;
    Ok(page)
  }

  /// Find at most two accessible scopes for an exact run ID, including archived runs.
  /// One match is returned only after later pages cannot contain another match;
  /// two matches prove ambiguity. This uses the existing catalog-v1 wire contract.
  pub fn run_scopes(&self, selector: &RunSelector) -> Result<Vec<RunScope>> {
    selector.validate()?;
    let mut query = RunListQuery {
      project_id: selector.project_id.clone(),
      origin: selector.origin.clone(),
      archival: RunArchiveFilter::All,
      limit: MAX_CATALOG_ROWS,
      cursor: None,
    };
    let mut matches = Vec::new();
    loop {
      let page = self.runs(&query)?;
      for run in page.runs {
        match run.scope.run_id.cmp(&selector.run_id) {
          Ordering::Greater => {}
          Ordering::Equal => {
            matches.push(run.scope);
            if matches.len() == 2 {
              return Ok(matches);
            }
          }
          Ordering::Less => return Ok(matches),
        }
      }
      let Some(cursor) = page.next_cursor else {
        return Ok(matches);
      };
      query.cursor = Some(cursor);
    }
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
