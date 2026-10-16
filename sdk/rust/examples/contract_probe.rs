//! Adapter used by sdk/contract/interop_tests.py, not a public CLI.

use std::io::{self, Read};

use expri_sdk::{MetricValue, Run, StepMode};
use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let mut request = String::new();
  io::stdin().read_to_string(&mut request)?;
  let request: Value = serde_json::from_str(&request)?;
  let output_dir = request["output_dir"]
    .as_str()
    .ok_or("output_dir must be a string")?;
  let run = Run::builder().output_dir(output_dir).init()?;
  if let Some(params) = request.get("params") {
    run.params(params)?;
  }
  if let Some(delay) = request.get("delay_before_log_ms") {
    std::thread::sleep(std::time::Duration::from_millis(
      delay.as_u64().ok_or("invalid delay")?,
    ));
  }
  for call in request["calls"]
    .as_array()
    .ok_or("calls must be an array")?
  {
    let step = call["step"].as_u64().ok_or("step must fit u64")?;
    let mode = match call
      .get("step_mode")
      .and_then(Value::as_str)
      .unwrap_or("relative")
    {
      "relative" => StepMode::Relative,
      "global" => StepMode::Global,
      _ => return Err("step_mode must be relative or global".into()),
    };
    let metrics = call["metrics"]
      .as_object()
      .ok_or("metrics must be an object")?
      .iter()
      .map(|(name, value)| Ok((name, MetricValue::try_from(value)?)))
      .collect::<expri_sdk::Result<Vec<_>>>()?;
    run.log_with_mode(step, metrics, mode)?;
  }
  println!("{}", json!({"initial_step": run.initial_step()}));
  Ok(())
}
