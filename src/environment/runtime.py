"""Prepare uv environments without modifying an explicitly reused Python stack."""

import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import sys
import tempfile

try:
  import tomllib
except ImportError:
  import tomli as tomllib

from packaging.requirements import InvalidRequirement, Requirement
from packaging.specifiers import SpecifierSet
from packaging.utils import canonicalize_name
from packaging.version import InvalidVersion, Version


class RuntimeErrorDetail(Exception):
  pass


def add_issue(issues, kind, message, package=None):
  if issues is None:
    raise RuntimeErrorDetail(message)
  issue = {"kind": kind, "message": message}
  if package is not None:
    issue["package"] = canonicalize_name(package)
  if issue not in issues:
    issues.append(issue)


def checked_requirement(value, context, issues, package=None):
  try:
    return parse_requirement(value, context)
  except RuntimeErrorDetail as error:
    add_issue(issues, "metadata", str(error), package)
    return None


def check_python_requirement(package, marker_env, issues):
  value = package.get("requires_python")
  if not value:
    return
  try:
    valid = SpecifierSet(value).contains(marker_env["python_full_version"], prereleases=True)
  except ValueError as error:
    add_issue(issues, "python_requirement", f"invalid Requires-Python for {package['name']}: {value!r}: {error}", package["name"])
    return
  if not valid:
    add_issue(issues, "python_requirement", f"installed {package['name']} requires Python {value}", package["name"])


# This probe deliberately has no third-party dependencies: it runs in the base
# interpreter, not the helper's uv-managed packaging/tomli environment.
INVENTORY_SCRIPT = r'''
import csv
import importlib.metadata
import importlib.util
import json
import os
import platform
from pathlib import Path
import shutil
import subprocess
import sys

def real_path(value):
  return str(Path(value).resolve()) if value else value

version = sys.implementation.version
implementation_version = f"{version.major}.{version.minor}.{version.micro}"
if version.releaselevel != "final":
  implementation_version += version.releaselevel[0] + str(version.serial)
packages = []
for distribution in importlib.metadata.distributions() if sys.argv[4] == "1" else []:
  name = distribution.metadata.get("Name")
  if not name:
    continue
  top_level = distribution.read_text("top_level.txt")
  modules = set()
  for declared in (top_level or "").split():
    # Native wheels may declare nested namespaces as paths rather than imports.
    module = declared.replace("\\", ".").replace("/", ".")
    if all(component.isidentifier() for component in module.split(".")):
      modules.add(module)
  if not modules:
    for file in distribution.files or []:
      first = str(file).replace("\\", "/").split("/")[0]
      if first.endswith(".py"):
        first = first[:-3]
      elif first.endswith((".so", ".pyd")):
        first = first.split(".")[0]
      if first.isidentifier() and first != "__pycache__":
        modules.add(first)
  packages.append({
    "name": name,
    "version": distribution.version,
    "requires": distribution.requires or [],
    "requires_python": distribution.metadata.get("Requires-Python"),
    "extras": distribution.metadata.get_all("Provides-Extra") or [],
    "metadata_path": real_path(str(distribution._path)),
    "modules": sorted(modules),
    "entry_points": [
      {"name": entry.name, "value": entry.value}
      for entry in distribution.entry_points if entry.group == "console_scripts"
    ],
  })
origins = {}
for module in json.loads(sys.argv[1]):
  try:
    spec = importlib.util.find_spec(module)
    origins[module] = None if spec is None else {
      "origin": real_path(spec.origin) if spec.origin not in ("built-in", "frozen") else spec.origin,
      "search_locations": [real_path(path) for path in spec.submodule_search_locations or []],
    }
  except (ImportError, ValueError, AttributeError) as error:
    origins[module] = {"error": str(error)}
torch = None
gpu_driver = None
if sys.argv[2] == "1":
  import torch as torch_module
  torch = {
    "version": str(torch_module.__version__),
    "origin": real_path(torch_module.__file__),
    "cuda_version": torch_module.version.cuda,
    "cuda_available": bool(torch_module.cuda.is_available()),
  }
  if sys.argv[3] == "1":
    if not torch["cuda_available"]:
      raise RuntimeError("CUDA is required but torch.cuda.is_available() is false")
    value = torch_module.ones(1, device="cuda") + 1
    torch_module.cuda.synchronize()
    if value.item() != 2:
      raise RuntimeError("PyTorch CUDA smoke test returned an unexpected result")
    torch["device_name"] = torch_module.cuda.get_device_name(0)
  if shutil.which("nvidia-smi"):
    try:
      result = subprocess.run(
        ["nvidia-smi", "--query-gpu=name,driver_version", "--format=csv,noheader"],
        text=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=5,
      )
      if result.returncode == 0:
        gpu_driver = [
          {"name": row[0].strip(), "driver_version": row[1].strip()}
          for row in csv.reader(result.stdout.splitlines()) if len(row) == 2
        ]
    except (OSError, subprocess.TimeoutExpired):
      pass
print(json.dumps({
  "python": real_path(sys.executable),
  "python_prefix": real_path(sys.prefix),
  "base_prefix": real_path(sys.base_prefix),
  "marker_env": {
    "implementation_name": sys.implementation.name,
    "implementation_version": implementation_version,
    "os_name": os.name,
    "platform_machine": platform.machine(),
    "platform_release": platform.release(),
    "platform_system": platform.system(),
    "platform_version": platform.version(),
    "python_full_version": platform.python_version(),
    "platform_python_implementation": platform.python_implementation(),
    "python_version": ".".join(platform.python_version_tuple()[:2]),
    "sys_platform": sys.platform,
  },
  "packages": packages,
  "origins": origins,
  "torch": torch,
  "gpu_driver": gpu_driver,
}, sort_keys=True))
'''


def digest(value):
  return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def file_digest(path):
  return hashlib.sha256(path.read_bytes()).hexdigest()


def invoke(argv, *, cwd, env, capture=False):
  try:
    result = subprocess.run(
      argv,
      cwd=cwd,
      env=env,
      text=True,
      stdout=subprocess.PIPE if capture else sys.stderr,
      stderr=subprocess.PIPE if capture else None,
    )
  except OSError as error:
    raise RuntimeErrorDetail(f"could not launch {argv[0]}: {error}") from error
  if result.returncode:
    if capture and result.stderr:
      sys.stderr.write(result.stderr)
    detail = f": {result.stderr.strip()}" if capture and result.stderr else ""
    raise RuntimeErrorDetail(f"{argv[0]} failed with exit status {result.returncode}{detail}")
  return result.stdout if capture else None


def inspect_python(python, repo_root, env, *, isolated=True, modules=(), torch=False, require_cuda=False, include_packages=True):
  argv = [str(python), "-B"]
  if isolated:
    argv.append("-I")
  argv.extend([
    "-c", INVENTORY_SCRIPT, json.dumps(sorted(modules)),
    "1" if torch else "0", "1" if require_cuda else "0", "1" if include_packages else "0",
  ])
  output = invoke(argv, cwd=repo_root, env=env, capture=True)
  try:
    inventory = json.loads(output)
  except json.JSONDecodeError as error:
    raise RuntimeErrorDetail("Python environment inspection did not produce valid JSON") from error
  packages = {}
  shadowed = {}
  for package in inventory["packages"]:
    name = canonicalize_name(package["name"])
    if name in packages:
      previous = packages[name]
      if Path(previous["metadata_path"]).parent == Path(package["metadata_path"]).parent:
        previous["ambiguous"] = True
      shadowed.setdefault(name, []).append(package)
      continue
    packages[name] = package
  inventory["packages"] = packages
  inventory["shadowed_packages"] = shadowed
  return inventory


def parse_requirement(value, context):
  try:
    return Requirement(value)
  except InvalidRequirement as error:
    raise RuntimeErrorDetail(f"unsupported requirement in {context}: {value!r}") from error


def marker_applies(requirement, marker_env, extras=()):
  if requirement.marker is None:
    return True
  try:
    return any(
      requirement.marker.evaluate({**marker_env, "extra": extra})
      for extra in {"", *extras}
    )
  except (KeyError, ValueError) as error:
    raise RuntimeErrorDetail(f"cannot evaluate marker for {requirement}") from error


def require_installed(requirement, packages, context):
  name = canonicalize_name(requirement.name)
  package = packages.get(name)
  if package is None:
    raise RuntimeErrorDetail(f"{context} requires {requirement}, but {name} is not installed")
  if package.get("ambiguous"):
    raise RuntimeErrorDetail(f"ambiguous installed metadata for required package {name}; remove duplicate installations")
  if requirement.url:
    raise RuntimeErrorDetail(f"cannot verify reused URL dependency {requirement} in {context}")
  try:
    valid = requirement.specifier.contains(package["version"], prereleases=True)
  except (InvalidVersion, TypeError) as error:
    raise RuntimeErrorDetail(f"required package {name} has invalid installed version {package['version']!r}") from error
  if not valid:
    raise RuntimeErrorDetail(
      f"{context} requires {requirement}, but the environment provides {name}=={package['version']}"
    )
  return name


def inherited_closure(inventory, roots, *, issues=None):
  packages = inventory["packages"]
  selected = {}
  pending = []
  for root in roots:
    requirement = checked_requirement(root, "reuse_packages", issues)
    if requirement:
      pending.append((requirement, False))
  while pending:
    requirement, already_selected = pending.pop()
    if not already_selected and not marker_applies(requirement, inventory["marker_env"]):
      continue
    name = canonicalize_name(requirement.name)
    try:
      require_installed(requirement, packages, "reused environment")
    except RuntimeErrorDetail as error:
      add_issue(issues, "inherited_dependency", str(error), name)
      if name not in packages:
        continue
    requested_extras = {canonicalize_name(extra) for extra in requirement.extras}
    package_extras = {canonicalize_name(extra) for extra in packages[name]["extras"]}
    if not requested_extras.issubset(package_extras):
      unknown = ", ".join(sorted(requested_extras - package_extras))
      add_issue(issues, "inherited_extra", f"{name} does not declare requested extras: {unknown}", name)
      requested_extras.intersection_update(package_extras)
    if name in selected and requested_extras.issubset(selected[name]):
      continue
    selected.setdefault(name, set()).update(requested_extras)
    check_python_requirement(packages[name], inventory["marker_env"], issues)
    for raw in packages[name]["requires"]:
      dependency = checked_requirement(raw, f"{name} metadata", issues, name)
      if dependency and marker_applies(dependency, inventory["marker_env"], selected[name]):
        pending.append((dependency, True))
  return selected


SELECTION_FLAGS = {"--all-extras", "--no-dev", "--only-dev", "--no-default-groups", "--all-groups"}
SELECTION_VALUES = {"--extra", "--no-extra", "--group", "--no-group", "--only-group"}
COMMON_FLAGS = {"--offline", "--no-index", "--no-progress", "--native-tls", "--no-cache"}
COMMON_VALUES = {"--index", "--default-index", "--index-url", "--extra-index-url", "--find-links", "--cache-dir", "--link-mode"}


def normalize_sync_args(args):
  normalized = []
  index = 0
  while index < len(args):
    argument = args[index]
    option, separator, value = argument.partition("=")
    if option in SELECTION_FLAGS | COMMON_FLAGS:
      if separator:
        raise RuntimeErrorDetail(f"{option} does not accept a value")
      normalized.append(option)
    elif option in SELECTION_VALUES | COMMON_VALUES:
      if not separator:
        index += 1
        if index >= len(args) or args[index].startswith("-"):
          raise RuntimeErrorDetail(f"{option} requires a value")
        value = args[index]
      if not value:
        raise RuntimeErrorDetail(f"{option} requires a value")
      normalized.extend([option, value])
    elif option != "--locked" or separator:
      raise RuntimeErrorDetail(
        f"unsupported environment sync argument {argument!r}; environment paths, lock policy, "
        "build policy, and package exclusions are controlled by expri"
      )
    index += 1
  return normalized


def exported_requirements(output, marker_env, *, strict=True, issues=None):
  requirements = {}
  for line in output.splitlines():
    line = line.strip()
    if not line or line.startswith("#"):
      continue
    if line.startswith("-") or line.endswith("\\"):
      if not strict:
        continue
      add_issue(issues, "locked_source", "reused environments currently require indexed/wheel dependencies, not editable/path lock entries")
      continue
    try:
      requirement = parse_requirement(line, "uv export")
    except RuntimeErrorDetail as error:
      if not strict:
        continue
      add_issue(issues, "locked_source", str(error))
      continue
    if not marker_applies(requirement, marker_env):
      continue
    name = canonicalize_name(requirement.name)
    if name in requirements:
      previous = requirements[name]
      if previous.specifier != requirement.specifier or previous.url != requirement.url:
        add_issue(issues, "locked_selection", f"uv export selected conflicting requirements for {name}", name)
        continue
      requirement.extras.update(previous.extras)
    requirements[name] = requirement
  return requirements


def validate_locked_reuse(inventory, inherited, requirements, *, issues=None):
  if "torch" in inherited:
    missing_cuda_wheels = sorted(
      name for name in requirements if name.startswith("nvidia-") and name not in inherited
    )
    for name in missing_cuda_wheels:
      add_issue(issues, "cuda_wheel",
        "uv.lock requires CUDA wheel distributions outside the reused PyTorch dependency closure: "
        + name
        + "; conda native CUDA libraries do not establish equivalence to these Python wheel "
        "distributions. Select a compatible lock/base environment profile or explicitly reuse "
        "matching installed Python distributions; expri will not silently download a second CUDA stack",
        name,
      )
  for name in sorted(inherited):
    requirement = requirements.get(name)
    if requirement is None:
      continue
    pins = list(requirement.specifier)
    if requirement.url or len(pins) != 1 or pins[0].operator != "==" or "*" in pins[0].version:
      add_issue(issues, "locked_version", f"cannot verify reused {name} against a single exact uv.lock version", name)
      continue
    actual = inventory["packages"][name]["version"]
    try:
      matches = Version(actual) == Version(pins[0].version)
    except (InvalidVersion, TypeError) as error:
      add_issue(issues, "locked_version", f"cannot compare reused {name} with uv.lock: {error}", name)
      continue
    if not matches:
      add_issue(issues, "locked_version",
        f"uv.lock selects {name}=={pins[0].version}, but the reused environment provides "
        f"{name}=={actual}; use a compatible lockfile or an environment without reuse",
        name,
      )


def validate_combined(inventory, requirements, inherited, base, *, project_name=None, project_extras=(), build_requirements=(), issues=None):
  packages = inventory["packages"]
  activated_extras = {name: set(extras) for name, extras in inherited.items()}
  pending = list(requirements.values())
  for name, requirement in requirements.items():
    package = packages.get(name)
    if package is None:
      add_issue(issues, "combined_dependency", f"prepared environment is missing locked dependency {name}", name)
      continue
    if requirement.url:
      # uv performed the locked installation; URL identity cannot be inferred
      # from a reused conda distribution, which was rejected before syncing.
      pass
    else:
      pins = list(requirement.specifier)
      if len(pins) != 1 or pins[0].operator != "==" or "*" in pins[0].version:
        add_issue(issues, "combined_version", f"locked dependency {name} is not exactly pinned", name)
      else:
        try:
          matches = Version(package["version"]) == Version(pins[0].version)
        except (InvalidVersion, TypeError) as error:
          add_issue(issues, "combined_version", f"cannot compare installed {name} with uv.lock: {error}", name)
          matches = True
        if not matches:
          add_issue(issues, "combined_version", f"prepared {name}=={package['version']} does not match uv.lock {pins[0].version}", name)
    activated_extras.setdefault(name, set()).update(requirement.extras)
  active_names = set(requirements) | set(inherited)
  if project_name:
    project_name = canonicalize_name(project_name)
    active_names.add(project_name)
    activated_extras.setdefault(project_name, set()).update(project_extras)
    pending.append(parse_requirement(project_name, "installed project"))
  for raw in build_requirements:
    requirement = checked_requirement(raw, "build-system.requires", issues)
    if requirement and marker_applies(requirement, inventory["marker_env"]):
      active_names.add(canonicalize_name(requirement.name))
      pending.append(requirement)
  for name in sorted(active_names):
    if name not in packages:
      add_issue(issues, "combined_dependency", f"prepared environment is missing declared package {name}", name)
      continue
    try:
      require_installed(parse_requirement(name, "declared package"), packages, "declared package")
    except RuntimeErrorDetail as error:
      add_issue(issues, "combined_dependency", str(error), name)
    package = packages[name]
    check_python_requirement(package, inventory["marker_env"], issues)
    for raw in package["requires"]:
      dependency = checked_requirement(raw, f"{name} metadata", issues, name)
      if dependency and marker_applies(dependency, inventory["marker_env"], activated_extras.get(name, ())):
        pending.append(dependency)
  checked = set()
  while pending:
    requirement = pending.pop()
    key = str(requirement)
    if key in checked:
      continue
    checked.add(key)
    name = canonicalize_name(requirement.name)
    if requirement.url and name not in inherited:
      if name not in packages:
        add_issue(issues, "combined_dependency", f"missing URL dependency {name}", name)
        continue
    else:
      try:
        require_installed(requirement, packages, "combined environment")
      except RuntimeErrorDetail as error:
        add_issue(issues, "combined_dependency", str(error), name)
        if name not in packages:
          continue
    package = packages[name]
    check_python_requirement(package, inventory["marker_env"], issues)
    before = set(activated_extras.get(name, ()))
    first_visit = name not in active_names
    active_names.add(name)
    activated_extras.setdefault(name, set()).update(requirement.extras)
    if first_visit or activated_extras[name] != before:
      for raw in packages[name]["requires"]:
        dependency = checked_requirement(raw, f"{name} metadata", issues, name)
        if dependency and marker_applies(dependency, inventory["marker_env"], activated_extras[name]):
          pending.append(dependency)
  for name in inherited:
    actual = packages.get(name)
    expected = base["packages"][name]
    if actual is None or any(actual[key] != expected[key] for key in ("version", "metadata_path")):
      add_issue(issues, "combined_shadow", f"overlay shadows reused package {name}; refusing to publish environment", name)
  for module, origin in base["origins"].items():
    if inventory["origins"].get(module) != origin:
      add_issue(issues, "combined_shadow", f"project or overlay shadows reused module {module}", module)
  if base["torch"] and inventory["torch"] != base["torch"]:
    add_issue(issues, "combined_torch", "prepared environment does not import the original PyTorch/CUDA stack", "torch")
  if base.get("gpu_driver") is not None and inventory.get("gpu_driver") != base["gpu_driver"]:
    add_issue(issues, "combined_driver", "GPU driver inventory changed while preparing the reused stack")


def atomic_json(path, value):
  descriptor, temporary = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
  try:
    with os.fdopen(descriptor, "w") as handle:
      json.dump(value, handle, indent=2, sort_keys=True)
      handle.write("\n")
    os.replace(temporary, path)
  finally:
    if os.path.exists(temporary):
      os.unlink(temporary)


def inherited_entry_points(environment_path, inherited, base, operation):
  # An inherited torchrun/pip executable otherwise retains its base-Python
  # shebang and bypasses the run's overlay packages. Only the wrapper is new;
  # its entry point and implementation still come from the protected base.
  commands = {}
  for name in sorted(inherited):
    for entry in base["packages"][name].get("entry_points", []):
      command = entry["name"]
      if not command or command in {".", ".."} or Path(command).name != command or "\x00" in command:
        raise RuntimeErrorDetail(f"unsafe inherited console script name for {name}")
      if command in commands:
        raise RuntimeErrorDetail(f"inherited packages declare conflicting console script {command}")
      commands[command] = name
  for command, name in commands.items():
    path = environment_path / "bin" / command
    code = (
      "import sys; from importlib.metadata import distribution; sys.argv.pop(0); "
      f"entry = next(entry for entry in distribution({name!r}).entry_points "
      f"if entry.group == 'console_scripts' and entry.name == {command!r}); "
      "sys.exit(entry.load()())"
    )
    script = (
      "#!/bin/sh\n# expri inherited console entry point\n"
      f"exec {shlex.quote(str(environment_path / 'bin' / 'python'))} -c {shlex.quote(code)} \"$0\" \"$@\"\n"
    )
    if path.is_symlink():
      raise RuntimeErrorDetail(f"inherited console script {command} conflicts with an overlay symlink")
    if path.exists():
      if path.read_text() != script:
        raise RuntimeErrorDetail(f"inherited console script {command} conflicts with an overlay executable")
    elif operation == "validate":
      raise RuntimeErrorDetail(f"prepared environment is missing inherited console script {command}")
    else:
      descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o755)
      with os.fdopen(descriptor, "w") as handle:
        handle.write(script)


ENV_REMOVE = {
  "VIRTUAL_ENV", "PYTHONHOME", "PYTHONPATH", "UV_PROJECT", "UV_PROJECT_ENVIRONMENT", "UV_PYTHON",
  "UV_NO_SYNC", "UV_FROZEN", "UV_TARGET", "UV_PREFIX", "UV_SYSTEM_PYTHON",
  "UV_BREAK_SYSTEM_PACKAGES", "UV_ACTIVE", "UV_DIRECTORY", "UV_WORKING_DIRECTORY",
  "UV_LOCKED", "UV_NO_BUILD", "UV_NO_BUILD_ISOLATION", "UV_NO_BUILD_ISOLATION_PACKAGE",
  "UV_NO_INSTALL_PACKAGE", "UV_NO_INSTALL_PROJECT", "UV_NO_INSTALL_WORKSPACE", "UV_NO_INSTALL_LOCAL",
  "UV_NO_BINARY", "UV_NO_BINARY_PACKAGE", "UV_ONLY_BINARY", "UV_NO_EDITABLE",
  "UV_NO_CONFIG", "UV_CONFIG_FILE", "UV_ENV_FILE", "UV_NO_ENV_FILE", "UV_PYTHON_PLATFORM",
}


def runtime_env(environment):
  env = os.environ.copy()
  # The helper itself commonly runs under uv run --isolated. Its environment
  # selection must never leak into the project's uv operations or base probe.
  for key in ENV_REMOVE:
    env.pop(key, None)
  overrides = environment.get("env", {})
  if not isinstance(overrides, dict) or any(not isinstance(key, str) or not isinstance(value, str) for key, value in overrides.items()):
    raise RuntimeErrorDetail("environment.env must map string names to string values")
  if (ENV_REMOVE - {"PYTHONPATH"}).intersection(overrides) or any(key.startswith("UV_") for key in overrides):
    raise RuntimeErrorDetail("environment.env cannot override expri's Python/environment selection")
  env.update(overrides)
  if sys.prefix != sys.base_prefix:
    helper_bin = (Path(sys.prefix) / "bin").resolve()
    env["PATH"] = os.pathsep.join(
      entry for entry in env.get("PATH", "").split(os.pathsep)
      if entry and Path(entry).resolve() != helper_bin
    )
  env["UV_PYTHON_DOWNLOADS"] = "never"
  env["PYTHONNOUSERSITE"] = "1"
  env["PYTHONDONTWRITEBYTECODE"] = "1"
  return env


def base_python(environment, repo_root, env):
  requested = environment.get("base_python")
  if requested:
    if not isinstance(requested, str):
      raise RuntimeErrorDetail("environment.base_python must be a string")
    return requested
  if environment.get("reuse_packages"):
    raise RuntimeErrorDetail("base_python is required when reuse_packages is configured")
  return invoke(["uv", "python", "find", "--system"], cwd=repo_root, env=env, capture=True).strip()


def ensure_build_requirements(project, inventory):
  build_system = project.get("build-system", {})
  for raw in build_system.get("requires", []):
    requirement = parse_requirement(raw, "build-system.requires")
    if marker_applies(requirement, inventory["marker_env"]):
      try:
        require_installed(requirement, inventory["packages"], "editable project build")
      except RuntimeErrorDetail as error:
        raise RuntimeErrorDetail(
          f"{error}; add the build dependency to the locked project environment or the base "
          "environment explicitly; expri will not download isolated build dependencies in reuse mode"
        ) from error


def is_packaged(project):
  configured = project.get("tool", {}).get("uv", {}).get("package")
  return configured if configured is not None else "build-system" in project


def option_value(args, option):
  values = [args[index + 1] for index, value in enumerate(args[:-1]) if value == option]
  return values[-1] if values else None


def existing_device(path):
  while not path.exists() and path != path.parent:
    path = path.parent
  try:
    return path.stat().st_dev
  except OSError:
    return None


def configure_cache(request, repo_root, state_dir, env, sync_args):
  directory = request.get("cache_dir")
  if directory is None:
    directory = option_value(sync_args, "--cache-dir") or env.get("UV_CACHE_DIR")
  if directory is None:
    directory = invoke(["uv", "cache", "dir"], cwd=repo_root, env=env, capture=True).strip()
  if not isinstance(directory, str) or not directory:
    raise RuntimeErrorDetail("cache_dir must be a nonempty path string")
  path = Path(directory).expanduser()
  if not path.is_absolute():
    path = repo_root / path
  path = path.resolve()
  for index, value in enumerate(sync_args[:-1]):
    if value == "--cache-dir":
      sync_args[index + 1] = str(path)
  env["UV_CACHE_DIR"] = str(path)
  link_mode = option_value(sync_args, "--link-mode") or env.get("UV_LINK_MODE") or "default"
  disabled = "--no-cache" in sync_args or env.get("UV_NO_CACHE", "").lower() in {"1", "true", "yes"}
  cache_device = existing_device(path)
  environment_device = existing_device(state_dir)
  return {
    "directory": str(path),
    "link_mode": link_mode,
    "same_filesystem": cache_device == environment_device if cache_device is not None and environment_device is not None else None,
    "disabled": disabled,
  }


def installation_args(sync_args):
  # Extras/groups select the locked project graph and are not uv pip options.
  args = []
  index = 0
  while index < len(sync_args):
    option = sync_args[index]
    if option in COMMON_FLAGS:
      args.append(option)
    elif option in COMMON_VALUES:
      args.extend(sync_args[index:index + 2])
    index += 2 if option in SELECTION_VALUES | COMMON_VALUES else 1
  return args


def request_options(request):
  operation = request.get("operation", "setup")
  if operation not in {"setup", "run", "validate", "doctor"}:
    raise RuntimeErrorDetail(f"unknown environment operation: {operation}")
  repo_root = Path(request["repo_root"]).resolve()
  state_dir = Path(request.get("state_dir", ".expri"))
  if not state_dir.is_absolute():
    state_dir = repo_root / state_dir
  state_dir = state_dir.resolve()
  environment = request.get("environment", {})
  if not isinstance(environment, dict):
    raise RuntimeErrorDetail("environment must be an object")
  roots = environment.get("reuse_packages", [])
  extras = request.get("extras", [])
  if not isinstance(roots, list) or not isinstance(extras, list) or not isinstance(request.get("sync_args", []), list):
    raise RuntimeErrorDetail("reuse_packages, extras, and sync_args must be lists")
  if any(not isinstance(item, str) for item in [*roots, *extras, *request.get("sync_args", [])]):
    raise RuntimeErrorDetail("reuse_packages, extras, and sync_args must contain strings")
  sync_args = normalize_sync_args(request.get("sync_args", []))
  install_project = request.get("install_project", operation == "run")
  require_cuda = environment.get("require_cuda", False)
  if not isinstance(install_project, bool) or not isinstance(require_cuda, bool):
    raise RuntimeErrorDetail("install_project and require_cuda must be booleans")
  return {
    "operation": operation, "repo_root": repo_root, "state_dir": state_dir,
    "environment": environment, "roots": roots, "extras": extras, "sync_args": sync_args,
    "install_project": install_project, "require_cuda": require_cuda,
  }


def analyze(request):
  plan = request_options(request)
  repo_root = plan["repo_root"]
  environment = plan["environment"]
  roots = plan["roots"]
  issues = []
  checks = {
    "interpreter": "pending", "inherited_dependencies": "pending" if roots else "not_requested",
    "lock": "pending", "import_origins": "pending" if roots else "not_requested",
    "torch": "pending" if roots else "not_requested", "cuda": "pending" if plan["require_cuda"] else "not_required",
    "combined_runtime": "pending",
  }
  plan.update({"issues": issues, "checks": checks, "base": None, "interpreter": None, "inherited": {}, "modules": set(), "torch": False, "requirements": {}, "project": {}})
  env = runtime_env(environment)
  plan["env"] = env
  plan["cache"] = configure_cache(request, repo_root, plan["state_dir"], env, plan["sync_args"])
  pyproject = repo_root / "pyproject.toml"
  lockfile = repo_root / "uv.lock"
  plan.update({"pyproject": pyproject, "lockfile": lockfile})
  if not pyproject.is_file() or not lockfile.is_file():
    add_issue(issues, "project_metadata", "uv environment preparation requires pyproject.toml and uv.lock; run uv lock first")
  else:
    try:
      with pyproject.open("rb") as handle:
        plan["project"] = tomllib.load(handle)
      plan["lock_sha256"] = file_digest(lockfile)
      plan["pyproject_sha256"] = file_digest(pyproject)
    except (ValueError, OSError) as error:
      add_issue(issues, "project_metadata", str(error))
  try:
    interpreter = base_python(environment, repo_root, env)
    plan["interpreter"] = interpreter
    base = inspect_python(interpreter, repo_root, env, include_packages=bool(roots))
  except (RuntimeErrorDetail, ValueError, OSError) as error:
    add_issue(issues, "interpreter", str(error))
    checks["interpreter"] = "failed"
    return plan
  plan["base"] = base
  checks["interpreter"] = "passed"
  python_requirement = plan["project"].get("project", {}).get("requires-python")
  if python_requirement:
    check_python_requirement({"name": "project", "requires_python": python_requirement}, base["marker_env"], issues)
  before_closure = len(issues)
  inherited = inherited_closure(base, roots, issues=issues)
  plan["inherited"] = inherited
  if roots:
    checks["inherited_dependencies"] = "passed" if len(issues) == before_closure else "failed"
  if roots and base["python_prefix"] != base["base_prefix"]:
    add_issue(issues, "base_environment",
      "base_python points to another virtual environment; --system-site-packages inherits its "
      "base installation, not that virtual environment's packages; select conda/system Python"
    )
  torch = "torch" in inherited
  modules = {module for name in inherited for module in base["packages"][name]["modules"]}
  plan.update({"modules": modules, "torch": torch})
  if inherited:
    try:
      probe = inspect_python(interpreter, repo_root, env, modules=modules, include_packages=False)
      base["origins"] = probe["origins"]
      unavailable = [module for module in modules if base["origins"].get(module) is None or "error" in base["origins"][module]]
      for module in unavailable:
        add_issue(issues, "import_unavailable", f"reused module {module} cannot be located in the base environment", module)
      # Source paths are present during the eventual run, even though the base
      # inventory deliberately ignores ambient PYTHONPATH and the working dir.
      source = inspect_python(interpreter, repo_root, env, isolated=False, modules=modules, include_packages=False)
      mismatches = [module for module, origin in base["origins"].items() if source["origins"].get(module) != origin]
      for module in mismatches:
        add_issue(issues, "import_shadow", f"project or configured PYTHONPATH shadows reused module {module}", module)
      checks["import_origins"] = "failed" if mismatches or unavailable else "passed"
    except (RuntimeErrorDetail, ValueError, OSError) as error:
      add_issue(issues, "import_probe", str(error))
      checks["import_origins"] = "failed"
    checks["torch"] = "not_requested"
    if torch:
      try:
        probe = inspect_python(interpreter, repo_root, env, torch=True, require_cuda=plan["require_cuda"], include_packages=False)
        base["torch"] = probe["torch"]
        base["gpu_driver"] = probe.get("gpu_driver")
        checks["torch"] = "passed"
        if plan["require_cuda"]:
          checks["cuda"] = "passed"
      except (RuntimeErrorDetail, ValueError, OSError) as error:
        add_issue(issues, "cuda" if plan["require_cuda"] else "torch_probe", str(error), "torch")
        checks["torch"] = "failed"
        if plan["require_cuda"]:
          checks["cuda"] = "failed"
  elif roots:
    checks["import_origins"] = "unavailable"
    checks["torch"] = "unavailable"
  if "lock_sha256" in plan:
    arguments = ["uv", "export", "--project", str(repo_root), "--locked", "--format", "requirements.txt", "--no-hashes", "--no-header", "--no-annotate", "--no-emit-project"]
    arguments.extend(plan["sync_args"])
    if plan["operation"] == "doctor":
      arguments.append("--no-build")
    for extra in plan["extras"]:
      arguments.extend(["--extra", extra])
    arguments.extend(["--python", interpreter])
    before_lock = len(issues)
    try:
      exported = invoke(arguments, cwd=repo_root, env=env, capture=True)
      requirements = exported_requirements(exported, base["marker_env"], strict=bool(roots), issues=issues)
      plan["requirements"] = requirements
      validate_locked_reuse(base, inherited, requirements, issues=issues)
    except (RuntimeErrorDetail, ValueError, OSError) as error:
      add_issue(issues, "lock_export", str(error))
    checks["lock"] = "passed" if len(issues) == before_lock else "failed"
  else:
    checks["lock"] = "unavailable"
  if "lock_sha256" in plan:
    try:
      if file_digest(lockfile) != plan["lock_sha256"] or file_digest(pyproject) != plan["pyproject_sha256"]:
        add_issue(issues, "project_drift", "project metadata or uv.lock changed during preflight; retry with a stable checkout")
    except OSError as error:
      add_issue(issues, "project_drift", str(error))
  return plan


def environment_fingerprint(plan):
  base = plan["base"]
  return digest({
    "schema_version": 1,
    "lock_sha256": plan["lock_sha256"], "pyproject_sha256": plan["pyproject_sha256"],
    "base_python": base["python"], "marker_env": base["marker_env"],
    "inherited": {name: base["packages"][name] for name in sorted(plan["inherited"])},
    "selected_requirements": {name: str(value) for name, value in plan["requirements"].items()},
    "extras": plan["extras"], "sync_args": plan["sync_args"],
    "env_sha256": digest(plan["environment"].get("env", {})),
    "require_cuda": plan["require_cuda"], "install_project": plan["install_project"],
  })


@contextlib.contextmanager
def preparation_lock(path):
  with path.open("a") as handle:
    fcntl.flock(handle, fcntl.LOCK_EX)
    try:
      yield handle
    finally:
      # Closing alone can retain a lock through inherited or duplicated descriptors.
      fcntl.flock(handle, fcntl.LOCK_UN)


def _prepare(request):
  plan = analyze(request)
  if plan["issues"]:
    raise RuntimeErrorDetail("\n".join(issue["message"] for issue in plan["issues"]))
  operation = plan["operation"]
  repo_root, state_dir = plan["repo_root"], plan["state_dir"]
  environment, extras, sync_args = plan["environment"], plan["extras"], plan["sync_args"]
  install_project, require_cuda = plan["install_project"], plan["require_cuda"]
  project, pyproject, lockfile = plan["project"], plan["pyproject"], plan["lockfile"]
  lock_sha256, pyproject_sha256 = plan["lock_sha256"], plan["pyproject_sha256"]
  env, interpreter, base = plan["env"], plan["interpreter"], plan["base"]
  inherited, modules, torch, requirements = plan["inherited"], plan["modules"], plan["torch"], plan["requirements"]
  environment_dir = state_dir / "environment"
  environment_path = environment_dir / ".venv"
  manifest_path = environment_dir / "environment-state.json"
  owner_path = environment_dir / "owner.json"
  if environment_path.is_symlink():
    raise RuntimeErrorDetail("expri refuses to prepare a symlinked environment")
  if environment_path.is_relative_to(Path(base["python_prefix"])):
    raise RuntimeErrorDetail("expri environment state must be outside the base Python installation")
  environment_dir.mkdir(parents=True, exist_ok=True)
  with preparation_lock(environment_dir / ".prepare.lock"):
    owner = {"schema_version": 1, "repo_root": str(repo_root)}
    if owner_path.exists():
      if json.loads(owner_path.read_text()) != owner:
        raise RuntimeErrorDetail("environment directory is owned by a different project")
    elif environment_path.exists():
      raise RuntimeErrorDetail("expri refuses to modify an existing unowned environment")
    elif operation == "validate":
      raise RuntimeErrorDetail("environment is not prepared; run expri setup first")
    else:
      atomic_json(owner_path, owner)
    fingerprint = environment_fingerprint(plan)
    previous = json.loads(manifest_path.read_text()) if manifest_path.exists() else None
    current = previous and previous.get("fingerprint") == fingerprint and environment_path.is_dir()
    if operation == "validate" and not current:
      raise RuntimeErrorDetail("prepared environment does not match the current lock/runtime configuration; run setup again")
    project_env = {**env, "UV_PROJECT_ENVIRONMENT": str(environment_path)}
    python = environment_path / "bin" / "python"
    if not current:
      # A failed preparation must never leave a success manifest describing an
      # older, now partially changed environment.
      manifest_path.unlink(missing_ok=True)
      if environment_path.exists():
        if not environment_path.is_dir():
          raise RuntimeErrorDetail("owned environment path is not a directory")
        shutil.rmtree(environment_path)
      arguments = ["uv", "venv", "--python", interpreter]
      if inherited:
        arguments.append("--system-site-packages")
      arguments.append(str(environment_path))
      invoke(arguments, cwd=repo_root, env=env)
      arguments = ["uv", "sync", "--project", str(repo_root), "--locked", "--python", interpreter]
      arguments.extend(sync_args)
      for extra in extras:
        arguments.extend(["--extra", extra])
      if inherited:
        arguments.extend(["--no-install-project", "--no-build"])
        for name in sorted(inherited):
          arguments.extend(["--no-install-package", name])
      elif not install_project:
        arguments.append("--no-install-project")
      try:
        invoke(arguments, cwd=repo_root, env=project_env)
      except RuntimeErrorDetail as error:
        if inherited:
          raise RuntimeErrorDetail(
            f"{error}; reuse mode installs external dependencies from wheels only; provide a "
            "compatible wheel or use an environment without reuse for source builds"
          ) from error
        raise
      if inherited and install_project and is_packaged(project):
        combined = inspect_python(python, repo_root, env)
        ensure_build_requirements(project, combined)
        invoke(
          ["uv", "pip", "install", *installation_args(sync_args), "--python", str(python), "--no-deps", "--no-build-isolation", "--editable", str(repo_root)],
          cwd=repo_root, env=env,
        )
    combined = inspect_python(python, repo_root, env, isolated=False, modules=modules, torch=torch or require_cuda, require_cuda=require_cuda)
    if combined["base_prefix"] != base["base_prefix"] or combined["marker_env"] != base["marker_env"]:
      manifest_path.unlink(missing_ok=True)
      raise RuntimeErrorDetail("prepared environment uses a different base interpreter or Python version")
    own_project = install_project and is_packaged(project)
    validate_combined(
      combined, requirements, inherited, base,
      project_name=project.get("project", {}).get("name") if own_project else None,
      project_extras=extras,
      build_requirements=project.get("build-system", {}).get("requires", []) if own_project and inherited else (),
    )
    inherited_entry_points(environment_path, inherited, base, operation)
    # Snapshot the base again to verify the helper never changed its packages.
    after = inspect_python(interpreter, repo_root, env, include_packages=bool(plan["roots"]))
    if after["packages"] != base["packages"]:
      manifest_path.unlink(missing_ok=True)
      raise RuntimeErrorDetail("base environment changed while preparing; no runtime manifest was published")
    if file_digest(lockfile) != lock_sha256 or file_digest(pyproject) != pyproject_sha256:
      manifest_path.unlink(missing_ok=True)
      raise RuntimeErrorDetail("project metadata or uv.lock changed during preparation; retry with a stable snapshot")
    manifest = {
      "schema_version": 1,
      "fingerprint": fingerprint,
      "repo_root": str(repo_root),
      "environment_path": str(environment_path),
      "base_python": base["python"],
      "lock_sha256": lock_sha256,
      "pyproject_sha256": pyproject_sha256,
      "reuse_packages": sorted(inherited),
      "reuse_extras": {name: sorted(values) for name, values in inherited.items()},
      "base_manifest": base,
      "combined_manifest": combined,
      "selected_requirements": {name: str(value) for name, value in requirements.items()},
      "install_project": install_project,
      "env_sha256": digest(environment.get("env", {})),
      "cache": plan["cache"],
    }
    atomic_json(manifest_path, manifest)
    return {
      "environment_path": str(environment_path),
      "python": str(python),
      "manifest_path": str(manifest_path),
      "reused_packages": sorted(inherited),
      "cache": plan["cache"],
      "run_env": {
        **environment.get("env", {}),
        "UV_PROJECT_ENVIRONMENT": str(environment_path),
        "UV_CACHE_DIR": plan["cache"]["directory"],
        "PYTHONNOUSERSITE": "1",
        "PATH": os.pathsep.join([
          str(environment_path / "bin"), str(Path(base["python_prefix"]) / "bin"),
          env.get("PATH", ""),
        ]),
      },
      "env_remove": sorted(ENV_REMOVE),
    }


def planned_build_requirements(plan):
  issues = plan["issues"]
  checks = plan["checks"]
  if not plan["inherited"] or not is_packaged(plan["project"]):
    checks["editable_build"] = "not_requested"
    return
  if plan["checks"]["lock"] == "unavailable" or any(issue["kind"] == "lock_export" for issue in issues):
    checks["editable_build"] = "unavailable"
    return
  packages = dict(plan["base"]["packages"])
  for name, requirement in plan["requirements"].items():
    if name in plan["inherited"]:
      continue
    pins = list(requirement.specifier)
    if not requirement.url and len(pins) == 1 and pins[0].operator == "==":
      packages[name] = {"name": name, "version": pins[0].version}
  before = len(issues)
  for raw in plan["project"].get("build-system", {}).get("requires", []):
    requirement = checked_requirement(raw, "build-system.requires", issues)
    if not requirement or not marker_applies(requirement, plan["base"]["marker_env"]):
      continue
    try:
      require_installed(requirement, packages, "planned editable project build")
    except RuntimeErrorDetail as error:
      add_issue(issues, "build_requirement",
        f"{error}; add a compatible build dependency to the locked project or base environment; "
        "expri will not download isolated build dependencies in reuse mode",
        requirement.name,
      )
  checks["editable_build"] = "planned" if len(issues) == before else "failed"


def inspect_prepared(plan):
  checks = plan["checks"]
  environment_dir = plan["state_dir"] / "environment"
  environment_path = environment_dir / ".venv"
  owner_path = environment_dir / "owner.json"
  manifest_path = environment_dir / "environment-state.json"
  python = environment_path / "bin" / "python"
  if not python.is_file() or not manifest_path.is_file():
    checks["combined_runtime"] = "pending"
    return
  combined_issues = []
  checks["combined_issues"] = combined_issues
  try:
    if environment_path.is_symlink() or not owner_path.is_file() or json.loads(owner_path.read_text()) != {"schema_version": 1, "repo_root": str(plan["repo_root"])}:
      checks["combined_runtime"] = "unowned"
      return
    manifest = json.loads(manifest_path.read_text())
    if not isinstance(manifest, dict):
      raise RuntimeErrorDetail("prepared environment manifest must be an object")
    checks["prepared_fingerprint"] = "current" if not plan["issues"] and manifest.get("fingerprint") == environment_fingerprint(plan) else "stale"
    combined = inspect_python(
      python, plan["repo_root"], plan["env"], isolated=False, modules=plan["modules"],
      torch=plan["torch"] or plan["require_cuda"], require_cuda=plan["require_cuda"],
    )
    base = plan["base"]
    if combined["base_prefix"] != base["base_prefix"] or combined["marker_env"] != base["marker_env"]:
      add_issue(combined_issues, "combined_interpreter", "prepared environment uses a different base interpreter or Python version")
    own_project = manifest.get("install_project", False) and is_packaged(plan["project"])
    validate_combined(
      combined, plan["requirements"], plan["inherited"], base,
      project_name=plan["project"].get("project", {}).get("name") if own_project else None,
      project_extras=plan["extras"],
      build_requirements=plan["project"].get("build-system", {}).get("requires", []) if own_project and plan["inherited"] else (),
      issues=combined_issues,
    )
    inherited_entry_points(environment_path, plan["inherited"], base, "validate")
    checks["combined_runtime"] = "failed" if combined_issues else "passed"
  except (RuntimeErrorDetail, ValueError, OSError) as error:
    add_issue(combined_issues, "combined_runtime", str(error))
    checks["combined_runtime"] = "failed"


def doctor(request):
  # This path intentionally never calls preparation or its failure invalidator.
  # uv may use its cache, but project/base/run environment state stays untouched.
  try:
    plan = analyze(request)
    if plan["base"] is not None:
      planned_build_requirements(plan)
      checks = plan["checks"]
      checks["python_version"] = plan["base"]["marker_env"]["python_full_version"]
      checks["torch_details"] = plan["base"].get("torch")
      checks["gpu_driver"] = plan["base"].get("gpu_driver")
      checks["inherited_package_count"] = len(plan["inherited"])
      checks["locked_package_count"] = len(plan["requirements"])
      checks["locked_overlap_count"] = len(set(plan["inherited"]) & set(plan["requirements"]))
      inspect_prepared(plan)
    return {
      "compatible": not plan["issues"],
      "scope": "base_and_lock",
      "base_python": plan["base"]["python"] if plan["base"] else plan["interpreter"],
      "reused_packages": sorted(plan["inherited"]),
      "issues": plan["issues"], "checks": plan["checks"], "cache": plan["cache"],
    }
  except (RuntimeErrorDetail, KeyError, ValueError, OSError, TypeError) as error:
    return {
      "compatible": False, "scope": "base_and_lock", "base_python": None,
      "reused_packages": [], "issues": [{"kind": "configuration", "message": str(error)}],
      "checks": {"combined_runtime": "pending"},
      "cache": {"directory": request.get("cache_dir"), "link_mode": "default", "same_filesystem": None, "disabled": False},
    }


def prepare(request):
  if not isinstance(request, dict):
    raise RuntimeErrorDetail("environment request must be a JSON object")
  if request.get("operation") == "doctor":
    return doctor(request)
  try:
    return _prepare(request)
  except Exception:
    # Even validation of a previously prepared environment can discover drift.
    # Invalidate only a manifest owned by this exact project, never foreign state.
    try:
      repo_root = Path(request["repo_root"]).resolve()
      state_dir = Path(request.get("state_dir", ".expri"))
      if not state_dir.is_absolute():
        state_dir = repo_root / state_dir
      environment_dir = state_dir.resolve() / "environment"
      owner = environment_dir / "owner.json"
      if owner.exists() and json.loads(owner.read_text()) == {"schema_version": 1, "repo_root": str(repo_root)}:
        (environment_dir / "environment-state.json").unlink(missing_ok=True)
    except (KeyError, ValueError, OSError, TypeError):
      pass
    raise


def main():
  if len(sys.argv) != 2:
    raise RuntimeErrorDetail("usage: runtime.py '<JSON request>' (or request.json)")
  source = sys.argv[1]
  request = json.loads(source if source.lstrip().startswith("{") else Path(source).read_text())
  result = prepare(request)
  print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
  try:
    main()
  except (RuntimeErrorDetail, KeyError, ValueError, OSError) as error:
    print(f"error: {error}", file=sys.stderr)
    sys.exit(1)
