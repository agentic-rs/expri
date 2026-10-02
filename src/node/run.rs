use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use serde_json::json;

use crate::environment::{self, EnvironmentRequest};
use crate::error::{ExpriError, Result, command_exit_code};
use crate::protocol::RunRequest;
use crate::run_logs::{RunLogs, STDERR_LOG, STDOUT_LOG};

pub fn apply_request_file(path: &Path) -> Result<()> {
  let request: RunRequest = serde_json::from_slice(&fs::read(path)?)?;
  apply_request(&request)
}

pub fn apply_request(request: &RunRequest) -> Result<()> {
  apply_request_at(request, &std::env::current_dir()?)
}

pub fn apply_request_at(request: &RunRequest, repo_root: &Path) -> Result<()> {
  request.environment.validate()?;
  environment::task_argv(&request.command)?;
  let snapshot = environment::snapshot::create_expected(
    repo_root,
    &request.remote_managed,
    request.expected_sync.as_ref(),
  )?;
  let _run_lock = crate::lock::run_lock(&snapshot.run_dir)?;
  let state_path = snapshot.run_dir.join("run-state.json");
  let mut state = json!({
    "schema_version": 1,
    "run_id": snapshot.run_id,
    "task": request.name,
    "command": request.command,
    "code_dir": snapshot.code_dir,
    "output_dir": snapshot.run_dir.join("outputs"),
    "status": "preparing",
    "started_at": chrono::Utc::now().to_rfc3339(),
    "logs": {"stdout": STDOUT_LOG, "stderr": STDERR_LOG},
  });
  write_state(&state_path, &state)?;
  let _ = writeln!(std::io::stderr(), "run: {}", snapshot.run_id);
  let _ = writeln!(
    std::io::stderr(),
    "run directory: {}",
    snapshot.run_dir.display()
  );
  let result = (|| {
    let logs = RunLogs::create(&snapshot.run_dir)?;
    let prepared = environment::prepare_logged(
      &EnvironmentRequest {
        environment: request.environment.clone(),
        repo_root: snapshot.code_dir.to_string_lossy().into_owned(),
        state_dir: snapshot.run_dir.to_string_lossy().into_owned(),
        operation: "run".to_string(),
        extras: request.extras.clone(),
        sync_args: request.sync_args.clone(),
        install_project: true,
        cache_dir: Some(
          environment::cache_dir(repo_root, &request.sync_args)?
            .to_string_lossy()
            .into_owned(),
        ),
      },
      &logs,
    )?;
    let argv = environment::task_argv(&request.command)?;
    state["status"] = json!("running");
    state["environment_manifest"] = json!(prepared.manifest_path);
    state["python"] = json!(prepared.python);
    write_state(&state_path, &state)?;
    let mut command = Command::new(&argv[0]);
    environment::configure_command(&mut command, &request.environment, &prepared);
    command
      .args(&argv[1..])
      .current_dir(&snapshot.code_dir)
      .env("EXPRI_RUN_ID", &snapshot.run_id)
      .env("EXPRI_RUN_DIR", &snapshot.run_dir)
      .env("EXPRI_OUTPUT_DIR", snapshot.run_dir.join("outputs"));
    let output = logs.task(&mut command)?;
    state["task_exit_code"] = json!(command_exit_code(&output.status));
    if let Some(error) = &output.log_error {
      state["logging_error"] = json!(error.to_string());
    }
    if !output.status.success() {
      return Err(ExpriError::CommandFailed {
        program: argv[0].clone(),
        code: command_exit_code(&output.status),
      });
    }
    if let Some(error) = output.log_error {
      return Err(error.into());
    }
    Ok(())
  })();
  state["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
  state["status"] = json!(if result.is_ok() {
    "completed"
  } else {
    "failed"
  });
  if let Err(error) = &result {
    state["error"] = json!(error.to_string());
    state["exit_code"] = json!(error.exit_code());
  } else {
    state["exit_code"] = json!(0);
  }
  write_state(&state_path, &state)?;
  result
}

fn write_state(path: &Path, state: &serde_json::Value) -> Result<()> {
  let temporary = path.with_extension("json.tmp");
  fs::write(&temporary, serde_json::to_vec_pretty(state)?)?;
  fs::rename(temporary, path)?;
  Ok(())
}
