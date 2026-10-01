use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result};
use crate::shell;

trait RemoteProtocol {
  fn name(&self) -> &'static str;
  fn apply_sync(&self, remote: &Remote, request_path: &str) -> Result<()>;
  fn apply_setup(&self, remote: &Remote, request_path: &str) -> Result<()>;
  fn prepare_pull(&self, remote: &Remote) -> Result<()>;
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
      "command -v {} >/dev/null 2>&1",
      shell::quote(&self.node_bin)
    ))
  }
}

impl RemoteProtocol for ExpriNodeProtocol {
  fn name(&self) -> &'static str {
    "expri-node"
  }

  fn apply_sync(&self, remote: &Remote, request_path: &str) -> Result<()> {
    remote.execute(&format!(
      "cd {} && {} node sync-apply --request {}",
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

  fn prepare_pull(&self, remote: &Remote) -> Result<()> {
    remote.execute(&format!(
      "cd {} && {} node pull-prepare",
      remote.quoted_remote_dir(),
      shell::quote(&self.node_bin)
    ))
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

  fn prepare_pull(&self, remote: &Remote) -> Result<()> {
    remote.execute(&format!(
      "cd {} && python3 - <<'PY'\n{}\nPY",
      remote.quoted_remote_dir(),
      python_pull_prepare_script()
    ))
  }
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
) -> Result<()> {
  protocol_with_preference(remote, preference, node_bin, "sync")?.apply_sync(remote, request_path)
}

pub fn apply_setup_with_preference(
  remote: &Remote,
  request_path: &str,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  protocol_with_preference(remote, preference, node_bin, "setup")?.apply_setup(remote, request_path)
}

pub fn prepare_pull_with_preference(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  protocol_with_preference(remote, preference, node_bin, "pull")?.prepare_pull(remote)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectedProtocol {
  ExpriNode,
  Python,
}

fn select_protocol(
  preference: ProtocolPreference,
  node_available: impl FnOnce() -> Result<bool>,
) -> Result<SelectedProtocol> {
  match preference {
    ProtocolPreference::ExpriNode => Ok(SelectedProtocol::ExpriNode),
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

fn protocol_with_preference(
  remote: &Remote,
  preference: ProtocolPreference,
  node_bin: &str,
  operation: &str,
) -> Result<Box<dyn RemoteProtocol>> {
  let expri = ExpriNodeProtocol::new(node_bin.to_string());
  let protocol: Box<dyn RemoteProtocol> =
    match select_protocol(preference, || expri.available(remote))? {
      SelectedProtocol::ExpriNode => Box::new(expri),
      SelectedProtocol::Python => Box::new(PythonProtocol),
    };
  if preference == ProtocolPreference::Auto && remote.verbosity > 0 && !remote.quiet {
    eprintln!("using {operation} protocol: {}", protocol.name());
  }
  Ok(protocol)
}

fn python_setup_script(request_path: &str) -> String {
  let request_path =
    serde_json::to_string(request_path).expect("request path string is serializable");
  format!(
    r#"import json, pathlib, subprocess

def check_path(path):
  p = pathlib.PurePosixPath(path)
  if p.is_absolute() or any(part in ("", ".", "..") for part in p.parts):
    raise SystemExit(f"unsafe setup script path: {{path}}")
  return path

request = json.loads(pathlib.Path({request_path}).read_text())
pathlib.Path(request["state_dir"]).mkdir(parents=True, exist_ok=True)
for step in request["steps"]:
  kind = step["kind"]
  if kind == "uv":
    cmd = ["uv", "sync"]
    for extra in step.get("extras", []):
      cmd.extend(["--extra", extra])
    cmd.extend(step.get("args", []))
  elif kind == "hf":
    cmd = ["uv", "run", "hf", "download", step["repo"]]
    if step.get("revision"):
      cmd.extend(["--revision", step["revision"]])
    cmd.extend(step.get("args", []))
  elif kind == "script":
    cmd = ["bash", check_path(step["path"]), *step.get("args", [])]
  else:
    raise SystemExit(f"unknown setup step kind: {{kind}}")
  subprocess.run(cmd, check=True)
(pathlib.Path(request["state_dir"]) / "setup-state.json").write_text(json.dumps(request, indent=2, sort_keys=True))
"#
  )
}

fn python_sync_apply_script(request_path: &str) -> String {
  let request_path =
    serde_json::to_string(request_path).expect("request path string is serializable");
  format!(
    r#"import hashlib, json, pathlib, shutil, subprocess, tempfile, zipfile

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
  checkout_manifest.write_text("".join(f"{{path.as_posix()}}\n" for path in sorted(desired)))
  manifest_path = state_dir / "patch.manifest"
  if manifest_path.exists():
    manifest_path.unlink()
  return sha256(checkout_manifest)

request = json.loads(pathlib.Path({request_path}).read_text())
state_dir = pathlib.Path(request["state_dir"])
state_dir.mkdir(parents=True, exist_ok=True)

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
(state_dir / "sync-state.json").write_text(json.dumps({{
  "head": request["head"],
  "source_bundle_sha256": request["source_bundle_sha256"],
  "patch_sha256": request["patch_sha256"],
  "checkout_manifest_sha256": checkout_manifest_sha256,
}}, indent=2, sort_keys=True))
"#
  )
}

fn python_pull_prepare_script() -> String {
  r#"import hashlib, json, pathlib, subprocess, zipfile

def sha256(path):
  h = hashlib.sha256()
  with open(path, "rb") as f:
    for chunk in iter(lambda: f.read(1024 * 1024), b""):
      h.update(chunk)
  return h.hexdigest()

out = pathlib.Path(".expri/out")
out.mkdir(parents=True, exist_ok=True)
head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
bundle = out / "pull-source.bundle"
patch = out / "pull-patch.zip"
subprocess.run(["git", "bundle", "create", str(bundle), "HEAD"], check=True)
changed = subprocess.check_output(["git", "diff", "--name-only", "-z", "HEAD", "--"])
untracked = subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard", "-z"])
paths = sorted({p for p in (changed + untracked).decode().split("\0") if p})
with zipfile.ZipFile(patch, "w", compression=zipfile.ZIP_DEFLATED) as archive:
  deleted = []
  for path in paths:
    p = pathlib.Path(path)
    if p.is_file():
      archive.write(p, path)
    else:
      deleted.append(path)
  archive.writestr(".deleted", "".join(f"{path}\n" for path in deleted))
artifacts = {
  "head": head,
  "source_bundle": ".expri/out/pull-source.bundle",
  "source_bundle_sha256": sha256(bundle),
  "patch": ".expri/out/pull-patch.zip",
  "patch_sha256": sha256(patch),
  "state_dir": ".expri",
}
(out / "pull-artifacts.json").write_text(json.dumps(artifacts, indent=2, sort_keys=True))
"#
  .to_string()
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod sync_tests;

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
      select_protocol(ProtocolPreference::Auto, || Ok(false)).unwrap(),
      SelectedProtocol::Python
    );
    assert_eq!(
      select_protocol(ProtocolPreference::Auto, || Ok(true)).unwrap(),
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
        select_protocol(preference, || panic!("explicit protocol must not probe")).unwrap(),
        expected
      );
    }
  }

  #[test]
  fn auto_protocol_preserves_availability_probe_errors() {
    let result = select_protocol(ProtocolPreference::Auto, || {
      Err(ExpriError::Message("transport unavailable".to_string()))
    });
    assert!(
      matches!(result, Err(ExpriError::Message(message)) if message == "transport unavailable")
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
