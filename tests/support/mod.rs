use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// Detached services must relinquish their leases before fixture storage goes away.
pub fn stop_worker_service(root: &Path) {
  let Ok(paths) = expri_core::storage::StoragePaths::resolve(Some(root)) else {
    return;
  };
  let _ = expri_client::worker_service::stop(&paths);
  let deadline = Instant::now() + Duration::from_secs(5);
  while expri_client::worker_service::status(&paths).is_ok_and(|status| status["running"] == true) {
    if Instant::now() >= deadline {
      if !thread::panicking() {
        panic!("fixture worker service did not stop: {}", root.display());
      }
      return;
    }
    thread::sleep(Duration::from_millis(25));
  }
}
