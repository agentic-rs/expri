mod browser;
mod browser_assets;
mod browser_auth;
mod dashboard_data;
mod notifications;
mod server;
mod storage;
mod store;
mod types;

pub use server::serve;
pub use store::compact_system_captures;
pub use types::ServerConfig;
