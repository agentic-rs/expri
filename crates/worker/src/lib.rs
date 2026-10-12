//! Training-machine execution, environment preparation, and source snapshots.

use expri_assets as assets;
use expri_client as service;
use expri_core::{config, error, filter, lock, protocol};

pub mod archive;
pub mod environment;
pub mod git;
pub mod jobs;
pub mod node;
pub mod run_logs;
pub mod runs;
pub mod shell;

pub const JOBS_SCRIPT: &str = include_str!("jobs.py");
pub const RUN_LOGS_SCRIPT: &str = include_str!("run_logs.py");
pub const RUN_LIFECYCLE_SCRIPT: &str = include_str!("run_lifecycle.py");
pub fn run_lifecycle_script() -> String {
  let capture = include_str!("system_capture.py").replacen(
    "__EXPRI_SYSTEM_CAPTURE_SCHEMA__",
    expri_system::store::run_capture::SCHEMA,
    1,
  );
  format!("{capture}\n{RUN_LIFECYCLE_SCRIPT}")
}
pub const SNAPSHOT_SCRIPT: &str = include_str!("environment/snapshot.py");
pub const MAINTENANCE_SCRIPT: &str = include_str!("environment/maintenance.py");
pub const METRICS_PRODUCER_SCRIPT: &str = include_str!("../python/expri_metrics.py");
