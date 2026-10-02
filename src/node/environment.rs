use std::io::Read;
use std::path::Path;

use serde_json::Value;

use crate::environment;
use crate::error::{ExpriError, Result};
use crate::protocol::{EnvironmentAction, EnvironmentCommandRequest};

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

pub fn apply_request_at(request: &EnvironmentCommandRequest, repo_root: &Path) -> Result<()> {
  let (report, compatible) = match &request.action {
    EnvironmentAction::Doctor(spec) => {
      let mut helper_request = environment::setup_request(
        &spec.environment,
        repo_root,
        &repo_root.join(".expri"),
        &spec.extras,
        &spec.sync_args,
      )?;
      helper_request.operation = "doctor".to_string();
      let report = environment::doctor(&helper_request)?;
      let compatible = report
        .get("compatible")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
          ExpriError::Message(
            "environment doctor returned an invalid compatibility report".to_string(),
          )
        })?;
      (report, compatible)
    }
    EnvironmentAction::Prune(spec) => {
      let report = environment::maintenance::prune(repo_root, spec)?;
      (serde_json::to_value(report)?, true)
    }
  };
  print_report(&report, request.json)?;
  if !compatible {
    return Err(ExpriError::Message(
      "environment preflight failed; resolve the reported issues before setup or run".to_string(),
    ));
  }
  Ok(())
}

fn print_report(report: &Value, json: bool) -> Result<()> {
  if json {
    println!("{}", serde_json::to_string_pretty(report)?);
  } else if let Some(compatible) = report.get("compatible").and_then(Value::as_bool) {
    println!(
      "Base and lock preflight: {}",
      if compatible { "passed" } else { "failed" }
    );
    println!(
      "Base Python: {}",
      report
        .get("base_python")
        .and_then(Value::as_str)
        .unwrap_or("unavailable")
    );
    let reused = report
      .get("reused_packages")
      .and_then(Value::as_array)
      .map(|packages| {
        packages
          .iter()
          .filter_map(Value::as_str)
          .collect::<Vec<_>>()
          .join(", ")
      })
      .unwrap_or_default();
    println!(
      "Reused packages: {}",
      if reused.is_empty() { "none" } else { &reused }
    );
    if let Some(cache) = report.get("cache") {
      println!(
        "Cache: {} (link mode: {})",
        cache
          .get("directory")
          .and_then(Value::as_str)
          .unwrap_or("unavailable"),
        cache
          .get("link_mode")
          .and_then(Value::as_str)
          .unwrap_or("uv default")
      );
      if cache.get("disabled").and_then(Value::as_bool) == Some(true) {
        println!("Cache reuse: disabled");
      } else if cache.get("same_filesystem").and_then(Value::as_bool) == Some(false) {
        println!(
          "Cache is on another filesystem; package files may be copied into each environment."
        );
      }
    }
    if let Some(checks) = report.get("checks") {
      println!(
        "Prepared environment: {}",
        checks
          .get("combined_runtime")
          .and_then(Value::as_str)
          .unwrap_or("pending")
      );
      if let Some(fingerprint) = checks.get("prepared_fingerprint").and_then(Value::as_str) {
        println!("Prepared configuration: {fingerprint}");
      }
      if let Some(issues) = checks.get("combined_issues").and_then(Value::as_array) {
        for issue in issues {
          println!(
            "- Prepared environment: {}",
            issue
              .get("message")
              .and_then(Value::as_str)
              .unwrap_or("validation failed")
          );
        }
      }
    }
    if let Some(issues) = report.get("issues").and_then(Value::as_array) {
      for issue in issues {
        println!(
          "- {}",
          issue
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unspecified compatibility issue")
        );
      }
    }
    println!("Each run still validates its combined environment before launch.");
  } else {
    let apply = report
      .get("apply")
      .and_then(Value::as_bool)
      .unwrap_or(false);
    println!(
      "Run environment cleanup: {}",
      if apply { "applied" } else { "preview" }
    );
    if let Some(runs) = report.get("runs").and_then(Value::as_array) {
      for run in runs {
        println!(
          "{}: {} ({})",
          run
            .get("run_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown run"),
          run
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("skipped"),
          run.get("reason").and_then(Value::as_str).unwrap_or("")
        );
      }
    }
    println!(
      "Logical environment bytes: {} (physical disk savings depend on cache links and filesystem)",
      report
        .get("logical_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0)
    );
    if !apply {
      println!(
        "Use --apply to prune these environments; code, outputs, and manifests are retained."
      );
    }
  }
  Ok(())
}
