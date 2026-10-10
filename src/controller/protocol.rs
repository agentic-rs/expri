use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result};
use crate::protocol::{EnvironmentCommandRequest, RunQueryRequest};
use crate::shell;

trait RemoteProtocol {
  fn name(&self) -> &'static str;
  fn apply_sync(&self, remote: &Remote, request_path: &str) -> Result<()>;
  fn apply_setup(&self, remote: &Remote, request_path: &str) -> Result<()>;
  fn apply_run(&self, remote: &Remote, request_path: &str) -> Result<()>;
  fn apply_environment(&self, remote: &Remote, request: &EnvironmentCommandRequest) -> Result<()>;
  fn query_runs(&self, remote: &Remote, request: &RunQueryRequest) -> Result<serde_json::Value>;
}

#[derive(Debug)]
pub struct ExpriNodeProtocol {
  node_bin: String,
}

impl ExpriNodeProtocol {
  pub fn new(node_bin: String) -> Self {
    Self { node_bin }
  }

  pub fn available(&self, remote: &Remote) -> Result<bool> {
    remote.execute_success(&format!(
      "cd {} && command -v {} >/dev/null 2>&1",
      remote.quoted_remote_dir(),
      shell::quote(&self.node_bin)
    ))
  }

  fn supports_capability(&self, remote: &Remote, capability: &str) -> Result<bool> {
    remote.execute_success(&format!(
      "cd {} && {} node capabilities --has {}",
      remote.quoted_remote_dir(),
      shell::quote(&self.node_bin),
      shell::quote(capability)
    ))
  }
}

impl RemoteProtocol for ExpriNodeProtocol {
  fn name(&self) -> &'static str {
    "expri-node"
  }

  fn apply_sync(&self, remote: &Remote, request_path: &str) -> Result<()> {
    remote.execute(&format!(
      "cd {} && {} node push-apply --request {}",
      remote.quoted_remote_dir(),
      shell::quote(&self.node_bin),
      shell::quote(request_path)
    ))
  }

  fn apply_setup(&self, remote: &Remote, request_path: &str) -> Result<()> {
    remote.execute(&format!(
      "cd {} && {} node setup --request {}",
      remote.quoted_remote_dir(),
      shell::quote(&self.node_bin),
      shell::quote(request_path)
    ))
  }

  fn apply_run(&self, remote: &Remote, request_path: &str) -> Result<()> {
    remote.execute(&format!(
      "cd {} && {} node run --request {}",
      remote.quoted_remote_dir(),
      shell::quote(&self.node_bin),
      shell::quote(request_path)
    ))
  }

  fn apply_environment(&self, remote: &Remote, request: &EnvironmentCommandRequest) -> Result<()> {
    let request = serde_json::to_string(request)?;
    remote.execute(&format!(
      "cd {} && {} node env --request-stdin <<'EXPRI_ENV_REQUEST'\n{request}\nEXPRI_ENV_REQUEST",
      remote.quoted_remote_dir(),
      shell::quote(&self.node_bin)
    ))
  }

  fn query_runs(&self, remote: &Remote, request: &RunQueryRequest) -> Result<serde_json::Value> {
    let request = serde_json::to_string(request)?;
    capture_run_report(
      remote,
      &format!(
        "cd {} && {} node runs --request-stdin <<'EXPRI_RUN_QUERY'\n{request}\nEXPRI_RUN_QUERY",
        remote.quoted_remote_dir(),
        shell::quote(&self.node_bin)
      ),
    )
  }
}

#[derive(Debug, Default)]
pub struct PythonProtocol;

impl RemoteProtocol for PythonProtocol {
  fn name(&self) -> &'static str {
    "python"
  }

  fn apply_sync(&self, remote: &Remote, request_path: &str) -> Result<()> {
    let script = python_sync_apply_script(request_path);
    remote.execute(&format!(
      "cd {} && python3 - <<'PY'\n{script}\nPY",
      remote.quoted_remote_dir()
    ))
  }

  fn apply_setup(&self, remote: &Remote, request_path: &str) -> Result<()> {
    let script = python_setup_script(request_path);
    remote.execute(&format!(
      "cd {} && python3 - <<'PY'\n{script}\nPY",
      remote.quoted_remote_dir()
    ))
  }

  fn apply_run(&self, remote: &Remote, request_path: &str) -> Result<()> {
    remote.execute(&format!(
      "cd {} && python3 - <<'PY'\n{}\nPY",
      remote.quoted_remote_dir(),
      python_run_script(request_path)
    ))
  }

  fn apply_environment(&self, remote: &Remote, request: &EnvironmentCommandRequest) -> Result<()> {
    remote.execute(&format!(
      "cd {} && python3 - <<'PY'\n{}\nPY",
      remote.quoted_remote_dir(),
      python_environment_script(request)
    ))
  }

  fn query_runs(&self, remote: &Remote, request: &RunQueryRequest) -> Result<serde_json::Value> {
    capture_run_report(
      remote,
      &format!(
        "cd {} && python3 - <<'PY'\n{}\nPY",
        remote.quoted_remote_dir(),
        python_runs_script(request)
      ),
    )
  }
}

fn capture_run_report(remote: &Remote, command: &str) -> Result<serde_json::Value> {
  // A login profile may set the node/Python path and print a banner. Keep the banner off JSON stdout.
  let bytes = remote.capture_bytes(&format!(
    "if [ -f ~/.profile ]; then . ~/.profile >&2; fi\n{command}"
  ))?;
  serde_json::from_slice(&bytes)
    .map_err(|error| ExpriError::Message(format!("invalid remote run report: {error}")))
}

fn python_runs_script(request: &RunQueryRequest) -> String {
  format!(
    r#"import json, os, sys
namespace = {{"__name__": "expri_runs_catalog"}}
exec({catalog}, namespace)
try:
  report = namespace["query_runs"](os.getcwd(), json.loads({request}))
  print(json.dumps(report))
except (OSError, ValueError) as error:
  print(str(error), file=sys.stderr)
  sys.exit(1)
"#,
    catalog = serde_json::to_string(crate::runs::CATALOG_SCRIPT).expect("catalog script"),
    request = serde_json::to_string(&serde_json::to_string(request).expect("run query"))
      .expect("query string"),
  )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolPreference {
  Auto,
  ExpriNode,
  Python,
}

impl ProtocolPreference {
  pub fn parse(value: Option<&str>) -> Result<Self> {
    match value.unwrap_or("auto") {
      "auto" => Ok(Self::Auto),
      "expri" | "expri-node" => Ok(Self::ExpriNode),
      "python" | "ssh" => Ok(Self::Python),
      value => Err(ExpriError::Message(format!(
        "unknown protocol {value:?}; expected auto, expri-node, or python (ssh is an alias)"
      ))),
    }
  }
}

pub fn apply_sync_with_preference(
  remote: &Remote,
  request_path: &str,
  preference: ProtocolPreference,
  node_bin: &str,
  requires_environment: bool,
) -> Result<()> {
  protocol_with_preference(remote, preference, node_bin, "push", requires_environment)?
    .apply_sync(remote, request_path)
}

pub fn apply_setup_with_preference(
  remote: &Remote,
  request_path: &str,
  preference: ProtocolPreference,
  node_bin: &str,
  requires_environment: bool,
) -> Result<()> {
  protocol_with_preference(remote, preference, node_bin, "setup", requires_environment)?
    .apply_setup(remote, request_path)
}

pub fn apply_run_with_preference(
  remote: &Remote,
  request_path: &str,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  protocol_with_preference(remote, preference, node_bin, "run", true)?
    .apply_run(remote, request_path)
}

pub fn apply_environment_with_preference(
  remote: &Remote,
  request: &EnvironmentCommandRequest,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  protocol_with_capability(
    remote,
    preference,
    node_bin,
    "environment",
    Some(crate::node::cli::ENVIRONMENT_MAINTENANCE_CAPABILITY),
  )?
  .apply_environment(remote, request)
}

pub fn query_runs_with_preference(
  remote: &Remote,
  request: &RunQueryRequest,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<serde_json::Value> {
  let capability = if matches!(request, RunQueryRequest::Files { metrics: true, .. }) {
    crate::node::cli::RUN_METRICS_CAPABILITY
  } else {
    crate::node::cli::RUN_RECORDS_CAPABILITY
  };
  protocol_with_capability(remote, preference, node_bin, "runs", Some(capability))?
    .query_runs(remote, request)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectedProtocol {
  ExpriNode,
  Python,
}

#[cfg(test)]
fn select_protocol(
  preference: ProtocolPreference,
  requires_environment: bool,
  node_available: impl FnOnce() -> Result<bool>,
) -> Result<SelectedProtocol> {
  select_capability_protocol(
    preference,
    requires_environment.then_some(crate::node::cli::UV_ENVIRONMENT_CAPABILITY),
    node_available,
  )
}

fn select_capability_protocol(
  preference: ProtocolPreference,
  capability: Option<&str>,
  node_available: impl FnOnce() -> Result<bool>,
) -> Result<SelectedProtocol> {
  match preference {
    ProtocolPreference::ExpriNode => {
      if let Some(capability) = capability
        && !node_available()?
      {
        return Err(ExpriError::Message(format!(
          "configured expri-node protocol lacks {}; upgrade expri on the target or choose protocol = \"python\"",
          capability
        )));
      }
      Ok(SelectedProtocol::ExpriNode)
    }
    ProtocolPreference::Python => Ok(SelectedProtocol::Python),
    ProtocolPreference::Auto => {
      if node_available()? {
        Ok(SelectedProtocol::ExpriNode)
      } else {
        Ok(SelectedProtocol::Python)
      }
    }
  }
}

pub(crate) fn require_run_publishing(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  require_run_capability(
    remote,
    preference,
    node_bin,
    crate::node::cli::RUN_PUBLISHING_CAPABILITY,
    "automatic publishing",
    "or use --no-publish",
  )
}

pub(crate) fn require_run_inputs(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  require_run_capability(
    remote,
    preference,
    node_bin,
    crate::node::cli::RUN_INPUTS_CAPABILITY,
    "private input preparation",
    "or download inputs manually and remove service.inputs",
  )
}

fn require_run_capability(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
  capability: &str,
  feature: &str,
  alternative: &str,
) -> Result<()> {
  if preference == ProtocolPreference::Python {
    return Err(ExpriError::Message(format!(
      "{feature} requires a native expri worker; upgrade the worker and use protocol = \"auto\" or \"expri-node\", {alternative}"
    )));
  }
  // A first push has not created the checkout yet. Installed PATH/absolute
  // nodes can still be checked, while existing relative nodes resolve there.
  let directory = remote.quoted_remote_dir();
  let supported = remote.execute_success(&format!(
    "if [ -e {directory} ] || [ -L {directory} ]; then cd {directory} || exit 1; fi; {} node capabilities --has {}",
    shell::quote(node_bin),
    shell::quote(capability)
  ))?;
  if !remote.dry_run && !supported {
    return Err(ExpriError::Message(format!(
      "{feature} requires {capability}; upgrade expri on the target {alternative}"
    )));
  }
  Ok(())
}

fn protocol_with_preference(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
  operation: &str,
  requires_environment: bool,
) -> Result<Box<dyn RemoteProtocol>> {
  let capability = requires_environment.then_some(match operation {
    "run" => crate::node::cli::RUN_RECORDS_CAPABILITY,
    "setup" => crate::node::cli::ENVIRONMENT_MAINTENANCE_CAPABILITY,
    _ => crate::node::cli::UV_ENVIRONMENT_CAPABILITY,
  });
  protocol_with_capability(remote, preference, node_bin, operation, capability)
}

fn protocol_with_capability(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
  operation: &str,
  capability: Option<&str>,
) -> Result<Box<dyn RemoteProtocol>> {
  let expri = ExpriNodeProtocol::new(node_bin.to_string());
  let protocol: Box<dyn RemoteProtocol> =
    match select_capability_protocol(preference, capability, || {
      if let Some(capability) = capability {
        expri.supports_capability(remote, capability)
      } else {
        expri.available(remote)
      }
    })? {
      SelectedProtocol::ExpriNode => Box::new(expri),
      SelectedProtocol::Python => Box::new(PythonProtocol),
    };
  if preference == ProtocolPreference::Auto && remote.verbosity > 0 && !remote.quiet {
    eprintln!("using {operation} protocol: {}", protocol.name());
  }
  Ok(protocol)
}

pub(crate) fn python_setup_script(request_path: &str) -> String {
  let request_path =
    serde_json::to_string(request_path).expect("request path string is serializable");
  python_preserve_command_exit_code(format!(
    r#"import json, os, pathlib, subprocess
{preamble}

def check_path(path):
  p = pathlib.PurePosixPath(path)
  if p.is_absolute() or any(part in ("", ".", "..") for part in p.parts):
    raise SystemExit(f"unsafe setup script path: {{path}}")
  return path

request = json.loads(pathlib.Path({request_path}).read_text())
pathlib.Path(request["state_dir"]).mkdir(parents=True, exist_ok=True)
environment = request.get("environment")
prepared = None
for step in request["steps"]:
  kind = step["kind"]
  if kind == "uv":
    if environment is not None:
      prepared = prepare_environment(environment, os.getcwd(), request["state_dir"], step.get("extras", []), step.get("args", []), False)
      continue
    cmd = ["uv", "sync"]
    for extra in step.get("extras", []):
      cmd.extend(["--extra", extra])
    cmd.extend(step.get("args", []))
  elif kind == "hf":
    if environment is not None and prepared is None:
      prepared = prepare_environment(environment, os.getcwd(), request["state_dir"], [], [], False)
    cmd = ["uv", "run"]
    if prepared is not None:
      cmd.extend(["--no-sync", "--no-env-file"])
    cmd.extend(["hf", "download", step["repo"]])
    if step.get("revision"):
      cmd.extend(["--revision", step["revision"]])
    cmd.extend(step.get("args", []))
  elif kind == "script":
    cmd = ["bash", check_path(step["path"]), *step.get("args", [])]
    if prepared is not None:
      cmd = ["uv", "run", "--no-sync", "--no-env-file", "--", *cmd]
  else:
    raise SystemExit(f"unknown setup step kind: {{kind}}")
  child_env = os.environ.copy()
  if prepared is not None:
    for key in [*ENV_REMOVE, *prepared.get("env_remove", [])]:
      child_env.pop(key, None)
  if environment is not None:
    child_env.update(environment.get("env", {{}}))
  if prepared is not None:
    child_env.update(prepared.get("run_env", {{}}))
    child_env["UV_PROJECT_ENVIRONMENT"] = prepared["environment_path"]
    child_env["PYTHONNOUSERSITE"] = "1"
  subprocess.run(cmd, check=True, env=child_env)
if environment is not None and prepared is None:
  prepare_environment(environment, os.getcwd(), request["state_dir"], [], [], False)
(pathlib.Path(request["state_dir"]) / "setup-state.json").write_text(json.dumps(request, indent=2, sort_keys=True))
"#,
    preamble = python_environment_preamble(),
  ))
}

fn python_preserve_command_exit_code(script: String) -> String {
  let indented = script
    .lines()
    .map(|line| format!("  {line}\n"))
    .collect::<String>();
  format!(
    r#"import subprocess, sys
try:
{indented}except subprocess.CalledProcessError as error:
  exit_code = error.returncode if error.returncode >= 0 else 128 - error.returncode
  print("error: command exited with status " + str(exit_code), file=sys.stderr)
  raise SystemExit(exit_code)
"#
  )
}

fn python_environment_preamble() -> String {
  let runtime =
    serde_json::to_string(crate::environment::RUNTIME_SCRIPT).expect("runtime script string");
  let env_remove =
    serde_json::to_string(crate::environment::ENV_REMOVE).expect("environment variable names");
  format!(
    r#"
RUNTIME_SCRIPT = {runtime}
ENV_REMOVE = {env_remove}

def environment_cache_dir(repo_root, sync_args):
  selected = os.environ.get("UV_CACHE_DIR")
  for index, argument in enumerate(sync_args):
    if argument == "--cache-dir":
      if index + 1 >= len(sync_args) or sync_args[index + 1].startswith("-"):
        raise ValueError("--cache-dir requires a path")
      selected = sync_args[index + 1]
    elif argument.startswith("--cache-dir="):
      selected = argument.split("=", 1)[1]
  if selected is None:
    selected = ".expri/cache/uv"
  if not selected:
    raise ValueError("cache directory must not be empty")
  path = pathlib.Path(selected)
  if not path.is_absolute():
    path = pathlib.Path(repo_root) / path
  return str(path.resolve())

def prepare_environment(environment, repo_root, state_dir, extras, sync_args, install_project, cache_dir=None, operation=None, process_runner=None):
  cache_dir = cache_dir or environment_cache_dir(repo_root, sync_args)
  spec = {{"environment": environment, "repo_root": repo_root, "state_dir": state_dir,
          "operation": operation or ("run" if install_project else "setup"), "extras": extras,
          "sync_args": sync_args, "install_project": install_project, "cache_dir": cache_dir}}
  cmd = ["uv", "run", "--isolated", "--no-project", "--no-config", "--no-env-file", "--with", "packaging==25.0",
         "--with", "tomli==2.2.1", "python", "-I", "-c", RUNTIME_SCRIPT, json.dumps(spec)]
  child_env = os.environ.copy()
  for key in ENV_REMOVE:
    child_env.pop(key, None)
  child_env.update(environment.get("env", {{}}))
  child_env["UV_CACHE_DIR"] = cache_dir
  if "--no-cache" in sync_args:
    child_env["UV_NO_CACHE"] = "true"
  if process_runner is None:
    output = subprocess.run(cmd, check=True, stdout=subprocess.PIPE, text=True, env=child_env)
  else:
    output = process_runner(cmd, env=child_env, capture_stdout=True)
    if output.returncode != 0:
      raise subprocess.CalledProcessError(output.returncode, cmd, output=output.stdout)
    if output.log_error:
      raise OSError(output.log_error)
  return json.loads(output.stdout)
"#
  )
}

pub(crate) fn python_run_script(request_path: &str) -> String {
  let request_path = serde_json::to_string(request_path).expect("request path string");
  let snapshot = serde_json::to_string(include_str!("../environment/snapshot.py"))
    .expect("snapshot script string");
  let maintenance = serde_json::to_string(include_str!("../environment/maintenance.py"))
    .expect("maintenance script string");
  let run_logs =
    serde_json::to_string(include_str!("../run_logs.py")).expect("run logging script string");
  let lifecycle = serde_json::to_string(include_str!("../run_lifecycle.py"))
    .expect("run lifecycle script string");
  let common = format!(
    r#"import json, os, pathlib, subprocess, sys
{preamble}
snapshot_module = {{"__name__": "expri_snapshot"}}
exec({snapshot}, snapshot_module)
maintenance_module = {{"__name__": "expri_environment_maintenance"}}
exec({maintenance}, maintenance_module)
logging_module = {{"__name__": "expri_run_logs"}}
exec({run_logs}, logging_module)
lifecycle_module = {{"__name__": "expri_run_lifecycle"}}
exec({lifecycle}, lifecycle_module)
context = {{"create_snapshot": snapshot_module["create_snapshot"],
           "acquire_run_lock": maintenance_module["acquire_run_lock"],
           "RunLogs": logging_module["RunLogs"], "ENV_REMOVE": ENV_REMOVE,
           "environment_cache_dir": environment_cache_dir, "prepare_environment": prepare_environment}}
"#,
    preamble = python_environment_preamble()
  );
  let worker = serde_json::to_string(
    r#"payload = json.loads(pathlib.Path(sys.argv[1]).read_text())
raise SystemExit(lifecycle_module["execute_worker"](payload, context))
"#,
  )
  .expect("worker script string");
  let common = serde_json::to_string(&common).expect("run script string");
  format!(
    r#"RUN_SOURCE = {common}
exec(RUN_SOURCE)
try:
  request = json.loads(pathlib.Path({request_path}).read_text())
  report = lifecycle_module["start_run"](os.getcwd(), request, context, RUN_SOURCE + {worker})
  if isinstance(report, dict):
    print(json.dumps(report), flush=True)
  else:
    raise SystemExit(report)
except Exception as error:
  print("error: " + str(error), file=sys.stderr)
  raise SystemExit(1)
"#
  )
}

pub(crate) fn python_environment_script(request: &EnvironmentCommandRequest) -> String {
  let request =
    serde_json::to_string(&serde_json::to_string(request).expect("environment request"))
      .expect("request string");
  let maintenance = serde_json::to_string(include_str!("../environment/maintenance.py"))
    .expect("maintenance script string");
  python_preserve_command_exit_code(format!(
    r#"import json, os, pathlib, subprocess, sys
{preamble}
request = json.loads({request})
if request["operation"] == "doctor":
  report = prepare_environment(request["environment"], os.getcwd(), str(pathlib.Path.cwd() / ".expri"), request.get("extras", []), request.get("sync_args", []), False, operation="doctor")
elif request["operation"] == "prune":
  maintenance_module = {{"__name__": "expri_environment_maintenance"}}
  exec({maintenance}, maintenance_module)
  report = maintenance_module["prune_environments"](os.getcwd(), request["apply"], request["keep_last"])
else:
  raise SystemExit("unsupported environment operation")
if request.get("json"):
  print(json.dumps(report, indent=2, sort_keys=True))
elif "compatible" in report:
  print("Base and lock preflight: " + ("passed" if report["compatible"] else "failed"))
  print("Base Python: " + (report.get("base_python") or "unavailable"))
  print("Reused packages: " + (", ".join(report.get("reused_packages", [])) or "none"))
  cache = report.get("cache", {{}})
  print("Cache: " + str(cache.get("directory") or "unavailable") + " (link mode: " + str(cache.get("link_mode") or "uv default") + ")")
  if cache.get("disabled") is True:
    print("Cache reuse: disabled")
  elif cache.get("same_filesystem") is False:
    print("Cache is on another filesystem; package files may be copied into each environment.")
  for issue in report.get("issues", []):
    print("- " + issue["message"])
  checks = report.get("checks", {{}})
  print("Prepared environment: " + checks.get("combined_runtime", "pending"))
  if "prepared_fingerprint" in checks:
    print("Prepared configuration: " + checks["prepared_fingerprint"])
  for issue in checks.get("combined_issues", []):
    print("- Prepared environment: " + issue["message"])
  print("Each run still validates its combined environment before launch.")
else:
  print("Run environment cleanup: " + ("applied" if report["apply"] else "preview"))
  for run in report["runs"]:
    print(run["run_id"] + ": " + run["action"] + " (" + run["reason"] + ")")
  print("Logical environment bytes: " + str(report["logical_bytes"]) + " (physical disk savings depend on cache links and filesystem)")
  if not report["apply"]:
    print("Use --apply to prune these environments; code, outputs, and manifests are retained.")
if report.get("compatible") is False:
  print("error: environment preflight failed; resolve the reported issues before setup or run", file=sys.stderr)
  raise SystemExit(1)
"#,
    preamble = python_environment_preamble()
  ))
}

fn python_sync_apply_script(request_path: &str) -> String {
  let request_path =
    serde_json::to_string(request_path).expect("request path string is serializable");
  format!(
    r#"import atexit, fcntl, hashlib, json, pathlib, shutil, subprocess, tempfile, zipfile

def sha256(path):
  h = hashlib.sha256()
  with open(path, "rb") as f:
    for chunk in iter(lambda: f.read(1024 * 1024), b""):
      h.update(chunk)
  return h.hexdigest()

def check_path(path):
  p = pathlib.PurePosixPath(path)
  if not p.parts or p.is_absolute() or any(part in ("", ".", "..") for part in p.parts):
    raise SystemExit(f"unsafe patch path: {{path}}")
  return pathlib.Path(path)

def remove_worktree_file(root, path):
  path = root / check_path(path)
  if path.is_file() or path.is_symlink():
    path.unlink()

def read_manifest(path):
  if not path.is_file():
    return set()
  return {{check_path(line) for line in path.read_text().splitlines() if line}}

def atomic_write(path, raw):
  temporary_path = None
  try:
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as temporary:
      temporary_path = pathlib.Path(temporary.name)
      temporary.write(raw)
    temporary_path.replace(path)
  finally:
    if temporary_path is not None:
      temporary_path.unlink(missing_ok=True)

def previous_installed_files(state_dir, git_dir):
  checkout_manifest = state_dir / "checkout.manifest"
  previous = read_manifest(checkout_manifest) | read_manifest(state_dir / "patch.manifest")
  state_path = state_dir / "sync-state.json"
  state = {{}}
  if state_path.is_file():
    try:
      state = json.loads(state_path.read_text())
    except ValueError:
      pass
  # Legacy SSH may leave an older native manifest behind. Recover its HEAD too.
  if (not checkout_manifest.is_file() or not state.get("checkout_manifest_sha256")) and state.get("head"):
    tree = subprocess.run(
      ["git", "--git-dir", str(git_dir), "ls-tree", "-r", "-z", "--name-only", state["head"]],
      check=False, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    )
    if tree.returncode == 0:
      previous.update(check_path(path) for path in tree.stdout.decode().split("\0") if path)
  return previous

def apply_patch(stage_dir, patch, remote_managed):
  with zipfile.ZipFile(patch) as archive:
    if ".deleted" in archive.namelist():
      for line in archive.read(".deleted").decode().splitlines():
        if line:
          path = check_path(line)
          if path not in remote_managed:
            remove_worktree_file(stage_dir, path)
    for entry in archive.infolist():
      if entry.filename == ".deleted" or entry.is_dir():
        continue
      path = check_path(entry.filename)
      if path in remote_managed:
        continue
      dst = stage_dir / path
      dst.parent.mkdir(parents=True, exist_ok=True)
      with archive.open(entry) as src, dst.open("wb") as out:
        shutil.copyfileobj(src, out)

def install_staged_checkout(state_dir, stage_dir, previous, remote_managed):
  desired = {{
    path.relative_to(stage_dir) for path in stage_dir.rglob("*")
    if path.is_file() or path.is_symlink()
  }} - remote_managed
  # Persist ownership before invalidating the old success receipt, including
  # paths this attempt may add before an interrupted or failed installation.
  atomic_write(state_dir / "patch.manifest", "".join(
    f"{{path.as_posix()}}\n" for path in sorted(previous | desired)
  ))
  (state_dir / "sync-state.json").unlink(missing_ok=True)
  # A previous overlay may now be in HEAD; only remove paths absent from the
  # complete desired checkout. Clear these first so files can become directories.
  for path in sorted(previous - desired - remote_managed):
    remove_worktree_file(pathlib.Path("."), path)
  for path in sorted(desired):
    for parent in reversed(path.parents):
      if parent.is_symlink():
        raise SystemExit(f"cannot install {{path}} through symlink parent {{parent}}")
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_dir() and not path.is_symlink():
      shutil.rmtree(path)
    else:
      remove_worktree_file(pathlib.Path("."), path)
    shutil.copy2(stage_dir / path, path, follow_symlinks=False)
  checkout_manifest = state_dir / "checkout.manifest"
  atomic_write(checkout_manifest, "".join(f"{{path.as_posix()}}\n" for path in sorted(desired)))
  manifest_path = state_dir / "patch.manifest"
  if manifest_path.exists():
    manifest_path.unlink()
  return sha256(checkout_manifest)

request = json.loads(pathlib.Path({request_path}).read_text())
state_dir = pathlib.Path(request["state_dir"])
if state_dir.is_symlink():
  raise SystemExit("push state directory must not be a symlink")
state_dir.mkdir(parents=True, exist_ok=True)
if (state_dir / "worktree.lock").is_symlink():
  raise SystemExit("checkout lock must not be a symlink")
checkout_lock = (state_dir / "worktree.lock").open("a+b")
fcntl.flock(checkout_lock, fcntl.LOCK_EX)
atexit.register(fcntl.flock, checkout_lock, fcntl.LOCK_UN)

if request.get("source_bundle"):
  if sha256(request["source_bundle"]) != request["source_bundle_sha256"]:
    raise SystemExit("source bundle sha256 mismatch")
if sha256(request["patch"]) != request["patch_sha256"]:
  raise SystemExit("patch sha256 mismatch")

remote_managed = {{check_path(path) for path in request.get("remote_managed", [])}}
git_dir = state_dir / "git"
if not git_dir.is_dir():
  subprocess.run(["git", "init", "--bare", str(git_dir)], check=True)
if request.get("remote_url"):
  subprocess.run([
    "git", "--git-dir", str(git_dir), "fetch", request["remote_url"],
    "+refs/heads/*:refs/remotes/bootstrap/*", "+HEAD:refs/remotes/bootstrap/HEAD",
  ], check=False)
if subprocess.run(
  ["git", "--git-dir", str(git_dir), "cat-file", "-e", request["head"] + "^{{commit}}"],
  check=False,
  stdout=subprocess.DEVNULL,
  stderr=subprocess.DEVNULL,
).returncode == 0:
  subprocess.run(["git", "--git-dir", str(git_dir), "update-ref", "refs/heads/synced", request["head"]], check=True)
else:
  if not request.get("source_bundle"):
    raise SystemExit(f"remote URL did not provide {{request['head']}}, and no source bundle was uploaded")
  subprocess.run(["git", "--git-dir", str(git_dir), "fetch", request["source_bundle"], "+HEAD:refs/heads/synced"], check=True)

previous = previous_installed_files(state_dir, git_dir)
tmp_dir = state_dir / "tmp"
tmp_dir.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix="sync-", dir=tmp_dir) as stage:
  stage_dir = pathlib.Path(stage)
  subprocess.run([
    "git", "--git-dir", str(git_dir), "--work-tree", str(stage_dir),
    "checkout", "-f", request["head"],
  ], check=True)
  apply_patch(stage_dir, request["patch"], remote_managed)
  checkout_manifest_sha256 = install_staged_checkout(state_dir, stage_dir, previous, remote_managed)

(state_dir / "patch.sha256").write_text(request["patch_sha256"])
atomic_write(state_dir / "sync-state.json", json.dumps({{
  "head": request["head"],
  "source_bundle_sha256": request["source_bundle_sha256"],
  "patch_sha256": request["patch_sha256"],
  "checkout_manifest_sha256": checkout_manifest_sha256,
}}, indent=2, sort_keys=True))
"#
  )
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod sync_tests;

#[cfg(test)]
#[path = "run_protocol_tests.rs"]
mod run_tests;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn protocol_preference_accepts_python_and_legacy_ssh_alias() {
    for value in ["python", "ssh"] {
      assert_eq!(
        ProtocolPreference::parse(Some(value)).unwrap(),
        ProtocolPreference::Python
      );
    }
    assert_eq!(
      ProtocolPreference::parse(None).unwrap(),
      ProtocolPreference::Auto
    );
    assert!(ProtocolPreference::parse(Some("ctl")).is_err());
  }

  #[test]
  fn auto_protocol_uses_python_when_node_is_unavailable() {
    assert_eq!(
      select_protocol(ProtocolPreference::Auto, false, || Ok(false)).unwrap(),
      SelectedProtocol::Python
    );
    assert_eq!(
      select_protocol(ProtocolPreference::Auto, false, || Ok(true)).unwrap(),
      SelectedProtocol::ExpriNode
    );
  }

  #[test]
  fn explicit_protocol_does_not_probe_node_availability() {
    for (preference, expected) in [
      (ProtocolPreference::ExpriNode, SelectedProtocol::ExpriNode),
      (ProtocolPreference::Python, SelectedProtocol::Python),
    ] {
      assert_eq!(
        select_protocol(preference, false, || panic!(
          "explicit protocol must not probe"
        ))
        .unwrap(),
        expected
      );
    }
  }

  #[test]
  fn auto_protocol_preserves_availability_probe_errors() {
    let result = select_protocol(ProtocolPreference::Auto, false, || {
      Err(ExpriError::Message("transport unavailable".to_string()))
    });
    assert!(
      matches!(result, Err(ExpriError::Message(message)) if message == "transport unavailable")
    );
  }

  #[test]
  fn configured_environment_falls_back_for_older_node_capabilities() {
    assert_eq!(
      select_protocol(ProtocolPreference::Auto, true, || Ok(false)).expect("Python fallback"),
      SelectedProtocol::Python
    );
    assert_eq!(
      select_protocol(ProtocolPreference::Auto, true, || Ok(true)).expect("capable node"),
      SelectedProtocol::ExpriNode
    );
  }

  #[test]
  fn explicit_environment_node_requires_capability_but_python_does_not_probe() {
    let error = select_protocol(ProtocolPreference::ExpriNode, true, || Ok(false))
      .expect_err("old node should not receive environment request");
    assert!(error.to_string().contains("uv-environment-v1"));
    assert!(error.to_string().contains("upgrade expri on the target"));
    assert_eq!(
      select_protocol(ProtocolPreference::Python, true, || panic!(
        "Python must not probe"
      ))
      .expect("explicit Python"),
      SelectedProtocol::Python
    );
  }

  #[test]
  fn python_sync_apply_script_quotes_request_path_as_python_string() {
    let script = python_sync_apply_script(".expri/inbox/sync-request.json");
    assert!(script.contains(r#"pathlib.Path(".expri/inbox/sync-request.json").read_text()"#));
    assert!(!script.contains("pathlib.Path(.expri/inbox"));
    assert!(script.contains(r#"request["head"] + "^{commit}""#));
    assert!(!script.contains(r#"f"{request['head']}^{commit}""#));
  }
}
