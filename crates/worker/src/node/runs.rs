use std::io::Read;
use std::path::Path;

use crate::error::Result;
use crate::protocol::RunQueryRequest;

pub fn apply_request_stdin() -> Result<()> {
  let mut input = String::new();
  std::io::stdin().read_to_string(&mut input)?;
  let request = serde_json::from_str(&input)?;
  apply_request_at(&request, &std::env::current_dir()?)
}

pub fn apply_request_file(path: &Path) -> Result<()> {
  let request = serde_json::from_slice(&std::fs::read(path)?)?;
  apply_request_at(&request, &std::env::current_dir()?)
}

pub fn apply_request_at(request: &RunQueryRequest, repo_root: &Path) -> Result<()> {
  let report = crate::runs::query(repo_root, request)?;
  println!("{}", serde_json::to_string_pretty(&report)?);
  Ok(())
}
