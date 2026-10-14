//! System inventory, bounded detail history, and durable hourly summaries.

#[cfg(feature = "collector")]
pub mod collector;
pub mod store;
#[cfg(feature = "collector")]
mod worker_capacity;

pub use expri_core::system_profile::*;
#[cfg(feature = "collector")]
pub use worker_capacity::detect_worker_capacity;

pub fn new_session_id() -> String {
  format!("session-{}", uuid::Uuid::new_v4())
}
