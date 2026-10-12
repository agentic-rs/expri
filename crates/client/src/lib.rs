//! Durable service transfers, automatic publishing, and checkpoint registration.

use expri_core::service as types;
use expri_core::{config, error, lock};
use expri_runs::run_artifacts;

pub mod client;
mod configuration;
pub mod options;
pub mod publishing;
pub mod registrations;

pub use client::ResourceQueueClient;
pub use client::{ServiceCatalogClient, SystemProfileClient};
pub use client::{
  fetch_files, fetch_files_with_reporter, file_metadata, import_output, input_download_prepared,
};
pub use client::{login, logout, revoke_login, validate_login_url};
pub use configuration::{load_client_config, resolve_client_config};
pub use options::*;
pub use types::validate_component;
