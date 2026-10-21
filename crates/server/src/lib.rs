//! Hosted ingestion, tracking, retention, and object storage.

use expri_core::{error, lock};
use expri_dashboard::{dashboard, metric_charts};
use expri_runs::{metrics, run_artifacts, runs};

mod service;

pub use service::{ServerConfig, compact_system_captures, serve};
