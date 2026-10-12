//! System inventory, bounded detail history, and durable hourly summaries.

#[cfg(feature = "collector")]
pub mod collector;
pub mod store;

pub use expri_core::system_profile::*;

pub fn new_session_id() -> String {
  format!("session-{}", uuid::Uuid::new_v4())
}
