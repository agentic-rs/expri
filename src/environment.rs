//! CLI composition preserves native/Python environment integration coverage.

pub use expri_worker::environment::*;

#[cfg(all(test, unix))]
#[path = "worker_integration_tests.rs"]
mod integration_tests;
