use crate::controller::transport::Remote;
use crate::error::{ExpriError, Result};
use crate::shell;

pub trait SyncProtocol {
  fn name(&self) -> &'static str;
  fn apply_sync(&self, remote: &Remote, request_path: &str) -> Result<()>;
}

pub trait SetupProtocol {
  fn apply_setup(&self, remote: &Remote, request_path: &str) -> Result<()>;
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
    remote.ssh_success(&format!(
      "command -v {} >/dev/null 2>&1",
      shell::quote(&self.node_bin)
    ))
  }
}

impl SyncProtocol for ExpriNodeProtocol {
  fn name(&self) -> &'static str {
    "expri-node"
  }

  fn apply_sync(&self, remote: &Remote, request_path: &str) -> Result<()> {
    remote.ssh(&format!(
      "cd {} && {} node sync-apply --request {}",
      remote.quoted_remote_dir(),
      shell::quote(&self.node_bin),
      shell::quote(request_path)
    ))
  }
}

impl SetupProtocol for ExpriNodeProtocol {
  fn apply_setup(&self, remote: &Remote, request_path: &str) -> Result<()> {
    remote.ssh(&format!(
      "cd {} && {} node setup --request {}",
      remote.quoted_remote_dir(),
      shell::quote(&self.node_bin),
      shell::quote(request_path)
    ))
  }
}

#[derive(Debug, Default)]
pub struct SshProtocol;

impl SyncProtocol for SshProtocol {
  fn name(&self) -> &'static str {
    "ssh"
  }

  fn apply_sync(&self, remote: &Remote, request_path: &str) -> Result<()> {
    let script = ssh_sync_apply_script(request_path);
    remote.ssh(&format!(
      "cd {} && python3 - <<'PY'\n{script}\nPY",
      remote.quoted_remote_dir()
    ))
  }
}

impl SetupProtocol for SshProtocol {
  fn apply_setup(&self, remote: &Remote, request_path: &str) -> Result<()> {
    let script = ssh_setup_script(request_path);
    remote.ssh(&format!(
      "cd {} && python3 - <<'PY'\n{script}\nPY",
      remote.quoted_remote_dir()
    ))
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolPreference {
  Auto,
  ExpriNode,
  Ssh,
}

impl ProtocolPreference {
  pub fn parse(value: Option<&str>) -> Result<Self> {
    match value.unwrap_or("auto") {
      "auto" => Ok(Self::Auto),
      "expri" | "expri-node" => Ok(Self::ExpriNode),
      "ssh" => Ok(Self::Ssh),
      value => Err(ExpriError::Message(format!(
        "unknown sync protocol {value:?}; expected auto, expri-node, or ssh"
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
  let expri = ExpriNodeProtocol::new(node_bin.to_string());
  let ssh = SshProtocol;
  match preference {
    ProtocolPreference::ExpriNode => expri.apply_sync(remote, request_path),
    ProtocolPreference::Ssh => ssh.apply_sync(remote, request_path),
    ProtocolPreference::Auto => {
      if expri.available(remote)? {
        if remote.verbosity > 0 && !remote.quiet {
          eprintln!("using sync protocol: {}", expri.name());
        }
        return expri.apply_sync(remote, request_path);
      }
      if remote.verbosity > 0 && !remote.quiet {
        eprintln!("using sync protocol: {}", ssh.name());
      }
      ssh.apply_sync(remote, request_path)
    }
  }
}

pub fn apply_setup_with_preference(
  remote: &Remote,
  request_path: &str,
  preference: ProtocolPreference,
  node_bin: &str,
) -> Result<()> {
  let expri = ExpriNodeProtocol::new(node_bin.to_string());
  let ssh = SshProtocol;
  match preference {
    ProtocolPreference::ExpriNode => expri.apply_setup(remote, request_path),
    ProtocolPreference::Ssh => ssh.apply_setup(remote, request_path),
    ProtocolPreference::Auto => {
      if expri.available(remote)? {
        if remote.verbosity > 0 && !remote.quiet {
          eprintln!("using setup protocol: {}", expri.name());
        }
        return expri.apply_setup(remote, request_path);
      }
      if remote.verbosity > 0 && !remote.quiet {
        eprintln!("using setup protocol: {}", ssh.name());
      }
      ssh.apply_setup(remote, request_path)
    }
  }
}

fn ssh_setup_script(request_path: &str) -> String {
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
    cmd = ["hf", "download", step["repo"]]
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

fn ssh_sync_apply_script(request_path: &str) -> String {
  let request_path =
    serde_json::to_string(request_path).expect("request path string is serializable");
  format!(
    r#"import hashlib, json, pathlib, shutil, subprocess, zipfile

def sha256(path):
  h = hashlib.sha256()
  with open(path, "rb") as f:
    for chunk in iter(lambda: f.read(1024 * 1024), b""):
      h.update(chunk)
  return h.hexdigest()

def check_path(path):
  p = pathlib.PurePosixPath(path)
  if p.is_absolute() or any(part in ("", ".", "..") for part in p.parts):
    raise SystemExit(f"unsafe patch path: {{path}}")
  return pathlib.Path(path)

request = json.loads(pathlib.Path({request_path}).read_text())
state_dir = pathlib.Path(request["state_dir"])
state_dir.mkdir(parents=True, exist_ok=True)

if request.get("source_bundle"):
  if sha256(request["source_bundle"]) != request["source_bundle_sha256"]:
    raise SystemExit("source bundle sha256 mismatch")
if sha256(request["patch"]) != request["patch_sha256"]:
  raise SystemExit("patch sha256 mismatch")

git_dir = state_dir / "git"
if not git_dir.is_dir():
  subprocess.run(["git", "init", "--bare", str(git_dir)], check=True)
if request.get("remote_url"):
  subprocess.run([
    "git", "--git-dir", str(git_dir), "fetch", request["remote_url"],
    "+refs/heads/*:refs/remotes/bootstrap/*", "+HEAD:refs/remotes/bootstrap/HEAD",
  ], check=False)
if subprocess.run(["git", "--git-dir", str(git_dir), "cat-file", "-e", request["head"] + "^{{commit}}"], check=False).returncode == 0:
  subprocess.run(["git", "--git-dir", str(git_dir), "update-ref", "refs/heads/synced", request["head"]], check=True)
else:
  if not request.get("source_bundle"):
    raise SystemExit(f"remote URL did not provide {{request['head']}}, and no source bundle was uploaded")
  subprocess.run(["git", "--git-dir", str(git_dir), "fetch", request["source_bundle"], "+HEAD:refs/heads/synced"], check=True)
subprocess.run(["git", "--git-dir", str(git_dir), "--work-tree", ".", "checkout", "-f", request["head"]], check=True)

manifest_path = state_dir / "patch.manifest"
if manifest_path.exists():
  for line in manifest_path.read_text().splitlines():
    if line:
      path = check_path(line)
      if path.exists() or path.is_symlink():
        path.unlink()

deleted = []
manifest = []
with zipfile.ZipFile(request["patch"]) as archive:
  if ".deleted" in archive.namelist():
    deleted = archive.read(".deleted").decode().splitlines()
  for line in deleted:
    if line:
      path = check_path(line)
      if path.exists() or path.is_symlink():
        path.unlink()
  for entry in archive.infolist():
    name = entry.filename
    if name == ".deleted" or entry.is_dir():
      continue
    dst = check_path(name)
    dst.parent.mkdir(parents=True, exist_ok=True)
    with archive.open(entry) as src, dst.open("wb") as out:
      shutil.copyfileobj(src, out)
    manifest.append(dst.as_posix())

manifest_path.write_text("".join(f"{{path}}\n" for path in sorted(manifest)))
(state_dir / "patch.sha256").write_text(request["patch_sha256"])
(state_dir / "sync-state.json").write_text(json.dumps({{
  "head": request["head"],
  "source_bundle_sha256": request["source_bundle_sha256"],
  "patch_sha256": request["patch_sha256"],
}}, indent=2, sort_keys=True))
"#
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ssh_sync_apply_script_quotes_request_path_as_python_string() {
    let script = ssh_sync_apply_script(".expri/inbox/sync-request.json");
    assert!(script.contains(r#"pathlib.Path(".expri/inbox/sync-request.json").read_text()"#));
    assert!(!script.contains("pathlib.Path(.expri/inbox"));
    assert!(script.contains(r#"request["head"] + "^{commit}""#));
    assert!(!script.contains(r#"f"{request['head']}^{commit}""#));
  }
}
