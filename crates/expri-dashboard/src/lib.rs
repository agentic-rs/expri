//! Shared experiment dashboard presentation and HTTP routes.

pub mod assets;
pub mod dashboard;
pub mod metric_charts;

pub use expri_core::error;
pub use expri_run_data::{metrics, run_artifacts, runs};
