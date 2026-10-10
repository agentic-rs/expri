pub mod artifacts;
pub mod preview;
pub mod server;
pub mod table;
pub mod table_cache;
pub mod updates;

use serde::Serialize;
use serde_json::Value;

use crate::error::{ExpriError, Result};
use crate::metric_charts::ChartXAxis;
use crate::metrics::Reduction;

#[derive(Clone, Debug, Serialize)]
pub struct Source {
  pub source_id: String,
  pub label: String,
  pub kind: &'static str,
  pub target_name: Option<String>,
}

/// Shared dashboard views keep local and hosted routes on the same API contract.
pub trait DashboardView {
  fn catalog(&self) -> Result<Value>;
  fn projects(&self) -> Result<Option<Value>> {
    Ok(None)
  }
  fn storage(
    &self,
    _project_id: &str,
    _kind: &str,
    _search: &str,
    _limit: usize,
    _offset: usize,
  ) -> Result<Option<Value>> {
    Ok(None)
  }
  fn updates(&self, source: &str, run_ids: &[String]) -> Result<Value>;
  fn list_table(&self, source: &str, query: &table::ListQuery<'_>) -> Result<Value>;
  fn columns(&self, source: &str) -> Result<Value>;
  fn detail(&self, source: &str, run_id: &str) -> Result<Value>;
  fn artifacts(&self, source: &str, run_id: &str) -> Result<Value>;
  fn artifact_download(
    &self,
    source: &str,
    run_id: &str,
    path: &str,
  ) -> Result<artifacts::Download>;
  fn log(&self, source: &str, run_id: &str, stream: &str, tail: usize) -> Result<Value>;
  fn compare(
    &self,
    source: &str,
    run_ids: &[String],
    filters: &[String],
    reduction: Reduction,
  ) -> Result<Value>;
  fn chart(
    &self,
    source: &str,
    run_ids: &[String],
    filters: &[String],
    x_axis: ChartXAxis,
  ) -> Result<String>;
}

fn message(message: impl Into<String>) -> ExpriError {
  ExpriError::Message(message.into())
}
