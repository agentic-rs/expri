use std::io::Read;
use std::path::Path;

use crate::error::Result;
use crate::protocol::JobRequest;

pub fn apply_request_stdin() -> Result<()> {
  let mut input = String::new();
  std::io::stdin().read_to_string(&mut input)?;
  let request: JobRequest = serde_json::from_str(&input)?;
  crate::jobs::execute_at(&request, &std::env::current_dir()?)
}

pub fn apply_request_file(path: &Path) -> Result<()> {
  let request: JobRequest = serde_json::from_slice(&std::fs::read(path)?)?;
  crate::jobs::execute_at(&request, &std::env::current_dir()?)
}
