"""Standard-library snapshot implementation for targets without the expri binary."""

import datetime as _snapshot_datetime
import fcntl as _snapshot_fcntl
import hashlib as _snapshot_hashlib
import json as _snapshot_json
import os as _snapshot_os
import pathlib as _snapshot_pathlib
import shutil as _snapshot_shutil
import stat as _snapshot_stat
import subprocess as _snapshot_subprocess
import tempfile as _snapshot_tempfile


_SNAPSHOT_EXCLUDED_DIRS = {
  ".expri", ".git", ".venv", "__pycache__", ".pytest_cache", ".mypy_cache",
  ".ruff_cache", ".pyre", ".hypothesis", "out", "target", "node_modules",
}


def create_snapshot(repo_root, remote_managed, expected_sync=None):
  """Return an isolated run directory after locking and verifying the checkout."""
  root = _snapshot_pathlib.Path(repo_root).resolve(strict=True)
  state_dir = root / ".expri"
  _snapshot_private_directory(state_dir)
  lock_path = state_dir / "worktree.lock"
  flags = _snapshot_os.O_CREAT | _snapshot_os.O_RDWR
  flags |= getattr(_snapshot_os, "O_NOFOLLOW", 0)
  flags |= getattr(_snapshot_os, "O_CLOEXEC", 0)
  with _snapshot_os.fdopen(_snapshot_os.open(lock_path, flags, 0o666), "r+b") as lock:
    if not _snapshot_stat.S_ISREG(_snapshot_os.fstat(lock.fileno()).st_mode):
      raise RuntimeError("checkout lock must be a regular file: " + str(lock_path))
    _snapshot_fcntl.flock(lock, _snapshot_fcntl.LOCK_EX)
    if expected_sync is not None:
      expected_identity = None
      if isinstance(expected_sync, dict) and all(
        isinstance(expected_sync.get(key), str) for key in ("head", "patch_sha256")
      ):
        expected_identity = {key: expected_sync[key] for key in ("head", "patch_sha256")}
      raw_state = _snapshot_read_optional_file(state_dir / "sync-state.json")
      actual_sync = None
      if raw_state is not None:
        try:
          state = _snapshot_json.loads(raw_state)
          if isinstance(state, dict):
            actual_sync = {"head": state.get("head"), "patch_sha256": state.get("patch_sha256")}
        except (ValueError, UnicodeDecodeError):
          pass
      if expected_identity is None or actual_sync != expected_identity:
        raise RuntimeError("target checkout changed after sync; retry run")
    source = _snapshot_select_source(root, remote_managed)
    runs_dir = state_dir / "runs"
    _snapshot_private_directory(runs_dir)
    run_dir = _snapshot_pathlib.Path(_snapshot_tempfile.mkdtemp(prefix="run-", dir=runs_dir))
    code_dir = run_dir / "code"
    try:
      code_dir.mkdir()
      (run_dir / "outputs").mkdir()
      files = []
      missing = []
      for name in source["paths"]:
        path = _snapshot_pathlib.Path(name)
        _snapshot_safe_parents(root, path)
        copied = _snapshot_copy_file(root, code_dir, path)
        if copied is not None:
          files.append(copied)
        elif source["kind"] == "git" or name in remote_managed:
          missing.append(path)
        else:
          raise _snapshot_changed(path)
      for copied in files:
        path = _snapshot_pathlib.Path(copied["path"])
        _snapshot_safe_parents(root, path)
        if _snapshot_inspect_file(root, path) != copied:
          raise _snapshot_changed(path)
      for path in missing:
        _snapshot_safe_parents(root, path)
        if _snapshot_inspect_file(root, path) is not None:
          raise _snapshot_changed(path)
      if _snapshot_select_source(root, remote_managed) != source:
        raise RuntimeError("checkout changed while preparing the run snapshot; retry after edits or sync finish")
      manifest = {
        "run_id": run_dir.name,
        "created_at": _snapshot_datetime.datetime.now(_snapshot_datetime.timezone.utc).isoformat(),
        "source": source,
        "files": files,
      }
      (run_dir / "snapshot.json").write_text(_snapshot_json.dumps(manifest, indent=2), encoding="utf-8")
      return {"run_id": run_dir.name, "run_dir": str(run_dir), "code_dir": str(code_dir)}
    except BaseException:
      _snapshot_shutil.rmtree(run_dir)
      raise


def _snapshot_select_source(root, remote_managed):
  raw = _snapshot_read_optional_file(root / ".expri/checkout.manifest")
  paths = set()
  if raw is not None:
    for name in raw.decode("utf-8").splitlines():
      if not name:
        continue
      path = _snapshot_validate_path(name)
      if _snapshot_should_include(path):
        paths.add(str(path))
    state = _snapshot_read_optional_file(root / ".expri/sync-state.json")
    git_head = None
    if state is not None:
      try:
        recorded = _snapshot_json.loads(state)
        if isinstance(recorded, dict) and isinstance(recorded.get("head"), str):
          git_head = recorded["head"]
      except (ValueError, UnicodeDecodeError):
        pass
    source = {
      "kind": "synced_checkout",
      "git_head": git_head,
      "checkout_manifest_sha256": _snapshot_digest(raw),
      "sync_state_sha256": _snapshot_digest(state) if state is not None else None,
    }
  else:
    try:
      result = _snapshot_subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=root, stdout=_snapshot_subprocess.PIPE, stderr=_snapshot_subprocess.PIPE,
      )
    except OSError as error:
      raise RuntimeError("cannot run git to select source files in " + str(root)) from error
    if result.returncode != 0:
      raise RuntimeError(
        "cannot select source files in " + str(root) +
        ": use a Git checkout or sync the target to create .expri/checkout.manifest"
      )
    for raw_name in result.stdout.split(b"\0"):
      if not raw_name:
        continue
      name = _snapshot_os.fsdecode(raw_name)
      path = _snapshot_pathlib.Path(name)
      if _snapshot_should_include(path):
        paths.add(str(_snapshot_validate_path(name)))
    head = _snapshot_subprocess.run(
      ["git", "rev-parse", "--verify", "HEAD"], cwd=root,
      stdout=_snapshot_subprocess.PIPE, stderr=_snapshot_subprocess.PIPE,
    )
    source = {
      "kind": "git",
      "git_head": head.stdout.decode("utf-8").strip() if head.returncode == 0 else None,
      "checkout_manifest_sha256": None,
      "sync_state_sha256": None,
    }
  for name in remote_managed:
    path = _snapshot_validate_path(name)
    if _snapshot_should_include(path):
      paths.add(str(path))
  source["paths"] = sorted(paths)
  return source


def _snapshot_validate_path(name):
  path = _snapshot_pathlib.Path(name)
  if (
    not name or not path.parts or name.startswith("./") or path.is_absolute() or
    any(part in {"..", ".expri", ".venv", ".git"} for part in path.parts)
  ):
    raise RuntimeError("unsafe snapshot source path: " + str(path))
  return path


def _snapshot_should_include(path):
  return not any(part in _SNAPSHOT_EXCLUDED_DIRS for part in path.parts)


def _snapshot_safe_parents(root, relative_path):
  current = root
  for part in relative_path.parent.parts:
    current = current / part
    metadata = _snapshot_metadata(current)
    if metadata is None:
      break
    if not _snapshot_stat.S_ISDIR(metadata.st_mode):
      raise RuntimeError("snapshot source parent must be a directory without symlinks: " + str(current))


def _snapshot_private_directory(path):
  try:
    path.mkdir()
  except FileExistsError:
    metadata = path.lstat()
    if not _snapshot_stat.S_ISDIR(metadata.st_mode):
      raise RuntimeError("run state directory must not be a symlink or file: " + str(path))


def _snapshot_copy_file(root, code_dir, path):
  source = root / path
  metadata = _snapshot_metadata(source)
  if metadata is None:
    return None
  destination = code_dir / path
  _snapshot_safe_parents(code_dir, path)
  destination.parent.mkdir(parents=True, exist_ok=True)
  if _snapshot_stat.S_ISLNK(metadata.st_mode):
    target = _snapshot_os.readlink(source)
    destination.symlink_to(target)
    return _snapshot_file_record(path, metadata, symlink_target=target)
  if not _snapshot_stat.S_ISREG(metadata.st_mode):
    raise RuntimeError(
      "snapshot source must be a file or symlink: " + str(source) +
      " (Git submodules need their own synced source files)"
    )
  flags = _snapshot_os.O_RDONLY | getattr(_snapshot_os, "O_NOFOLLOW", 0)
  with _snapshot_os.fdopen(_snapshot_os.open(source, flags), "rb") as input_file:
    opened = _snapshot_os.fstat(input_file.fileno())
    if (metadata.st_dev, metadata.st_ino) != (opened.st_dev, opened.st_ino):
      raise _snapshot_changed(path)
    with destination.open("wb") as output:
      _snapshot_shutil.copyfileobj(input_file, output, length=64 * 1024)
      _snapshot_os.fchmod(output.fileno(), _snapshot_stat.S_IMODE(metadata.st_mode))
  digest, size = _snapshot_file_digest(destination)
  return _snapshot_file_record(path, metadata, digest, size)


def _snapshot_inspect_file(root, path):
  source = root / path
  metadata = _snapshot_metadata(source)
  if metadata is None:
    return None
  if _snapshot_stat.S_ISLNK(metadata.st_mode):
    return _snapshot_file_record(path, metadata, symlink_target=_snapshot_os.readlink(source))
  if not _snapshot_stat.S_ISREG(metadata.st_mode):
    raise _snapshot_changed(path)
  digest, size = _snapshot_file_digest(source)
  return _snapshot_file_record(path, metadata, digest, size)


def _snapshot_file_record(path, metadata, digest=None, size=None, symlink_target=None):
  return {
    "path": str(path), "sha256": digest, "size": size,
    "symlink_target": symlink_target, "mode": metadata.st_mode,
  }


def _snapshot_metadata(path):
  try:
    return path.lstat()
  except FileNotFoundError:
    return None


def _snapshot_read_optional_file(path):
  metadata = _snapshot_metadata(path)
  if metadata is None:
    return None
  if not _snapshot_stat.S_ISREG(metadata.st_mode):
    raise RuntimeError("snapshot metadata must be a regular file: " + str(path))
  return path.read_bytes()


def _snapshot_file_digest(path):
  hasher = _snapshot_hashlib.sha256()
  size = 0
  with path.open("rb") as source:
    while True:
      chunk = source.read(64 * 1024)
      if not chunk:
        break
      hasher.update(chunk)
      size += len(chunk)
  return hasher.hexdigest(), size


def _snapshot_digest(raw):
  return _snapshot_hashlib.sha256(raw).hexdigest()


def _snapshot_changed(path):
  return RuntimeError(
    "source changed while preparing the run snapshot: " + str(path) +
    "; retry after edits or sync finish"
  )
