use crate::config::TargetConfig;
use crate::controller::protocol::{ProtocolPreference, python_run_script, python_script_string};
use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result};
use crate::protocol::JobRequest;
use crate::shell;

pub struct JobOptions {
  pub target: TargetConfig,
  pub control_path: String,
  pub control_persist: String,
  pub verbosity: u8,
  pub quiet: bool,
  pub request: JobRequest,
}

pub fn execute(options: JobOptions) -> Result<Option<serde_json::Value>> {
  let preference = ProtocolPreference::parse(options.target.protocol.as_deref())?;
  let node_bin = options
    .target
    .node_bin
    .clone()
    .unwrap_or_else(|| "expri".to_string());
  let remote = Remote::new(
    options.target,
    options.control_path,
    options.control_persist,
    false,
    options.verbosity,
    options.quiet,
  )?;
  remote.connect()?;
  execute_with_preference(&remote, &options.request, preference, &node_bin)
}

pub fn start_with_preference(
  remote: &Remote,
  request_path: &str,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<Option<serde_json::Value>> {
  let native = select_native(remote, preference, node_bin)?;
  let command = if native {
    format!(
      "cd {} && {} node run --request {}",
      remote.quoted_remote_dir(),
      shell::quote(node_bin),
      shell::quote(request_path)
    )
  } else {
    python_command(remote, &python_run_script(request_path))
  };
  let bytes = remote.capture_bytes(&with_profile(&command))?;
  if remote.dry_run {
    return Ok(None);
  }
  Ok(Some(parse_report(&bytes)?))
}

fn execute_with_preference(
  remote: &Remote,
  request: &JobRequest,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<Option<serde_json::Value>> {
  let native = if matches!(request, JobRequest::RestartSupervisor { .. }) {
    require_supervisor_restart(remote, preference, node_bin)?;
    true
  } else {
    select_native(remote, preference, node_bin)?
  };
  let command = job_command(remote, request, node_bin, native)?;
  if matches!(request, JobRequest::Logs { .. }) {
    remote.execute_stream(&with_profile(&command))?;
    Ok(None)
  } else {
    let bytes = remote.capture_bytes(&with_profile(&command))?;
    Ok(Some(parse_report(&bytes)?))
  }
}

fn select_native(remote: &Remote, preference: ProtocolPreference, node_bin: &str) -> Result<bool> {
  if preference == ProtocolPreference::Python {
    return Ok(false);
  }
  let supported = remote.execute_success(&format!(
    "cd {} && {} node capabilities --has {}",
    remote.quoted_remote_dir(),
    shell::quote(node_bin),
    shell::quote(crate::node::cli::DURABLE_RUNS_CAPABILITY)
  ))?;
  if !supported && preference == ProtocolPreference::ExpriNode {
    return Err(ExpriError::Message(format!(
      "configured expri-node protocol lacks {}; upgrade expri on the target or choose protocol = \"python\"",
      crate::node::cli::DURABLE_RUNS_CAPABILITY
    )));
  }
  if preference == ProtocolPreference::Auto && remote.verbosity > 0 && !remote.quiet {
    eprintln!(
      "using durable runs protocol: {}",
      if supported { "expri-node" } else { "python" }
    );
  }
  Ok(supported)
}

fn require_supervisor_restart(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  if preference == ProtocolPreference::Python {
    return Err(ExpriError::Message(
      "supervisor restart requires a native Linux worker; Python fallback cannot transfer live ownership".into(),
    ));
  }
  let capability = crate::node::cli::SUPERVISOR_RESTART_CAPABILITY;
  if !remote.execute_success(&format!(
    "cd {} && {} node capabilities --has {}",
    remote.quoted_remote_dir(),
    shell::quote(node_bin),
    shell::quote(capability),
  ))? {
    return Err(ExpriError::Message(format!(
      "supervisor restart requires {capability} on a native Linux worker; upgrade expri on the target. Existing supervisors must also support live restart"
    )));
  }
  Ok(())
}

fn job_command(
  remote: &Remote,
  request: &JobRequest,
  node_bin: &str,
  native: bool,
) -> Result<String> {
  if native {
    let request = serde_json::to_string(request)?;
    Ok(format!(
      "cd {} && {} node jobs --request-stdin <<'EXPRI_JOB_REQUEST'\n{request}\nEXPRI_JOB_REQUEST",
      remote.quoted_remote_dir(),
      shell::quote(node_bin)
    ))
  } else {
    if matches!(request, JobRequest::RestartSupervisor { .. }) {
      return Err(ExpriError::Message(
        "supervisor restart cannot use the Python fallback".into(),
      ));
    }
    Ok(python_command(remote, &python_jobs_script(request)?))
  }
}

fn python_jobs_script(request: &JobRequest) -> Result<String> {
  Ok(format!(
    r#"import json, os, sys
namespace = {{"__name__": "expri_jobs"}}
exec({jobs}, namespace)
try:
  report = namespace["execute_job"](os.getcwd(), json.loads({request}))
  if report is not None:
    print(json.dumps(report))
except (OSError, ValueError, RuntimeError) as error:
  print(str(error), file=sys.stderr)
  sys.exit(1)
"#,
    jobs = python_script_string(expri_worker::JOBS_SCRIPT),
    request = serde_json::to_string(&serde_json::to_string(request)?)?,
  ))
}

fn python_command(remote: &Remote, script: &str) -> String {
  format!(
    "cd {} && python3 - <<'PY'\n{script}\nPY",
    remote.quoted_remote_dir()
  )
}

fn with_profile(command: &str) -> String {
  format!("if [ -f ~/.profile ]; then . ~/.profile >&2; fi\n{command}")
}

fn parse_report(bytes: &[u8]) -> Result<serde_json::Value> {
  serde_json::from_slice(bytes)
    .map_err(|error| ExpriError::Message(format!("invalid remote job report: {error}")))
}

#[cfg(all(test, unix))]
#[path = "jobs_tests.rs"]
mod tests;
