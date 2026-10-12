# Expri Python environments

Use a top-level `[environment]` for local runs and as the default for targets.
An empty table selects normal uv isolation. Each managed run prepares its own
environment and records its source and interpreter provenance. Install uv on
the executing machine.

```sh
expri setup --dry-run
expri setup
expri env doctor --json
expri -T gpu push
expri -T gpu env doctor --json
expri -T gpu setup
```

Local setup requires a top-level environment to use the local preparation path.
Push updated project files before remote doctor/setup so the worker checks the
intended lock and configuration. Setup prepares configured steps and warms the
cache; each managed run still prepares and validates its own environment.

## Reuse an existing GPU stack

In the private target file, select the interpreter that owns the base packages:

```toml
[target.gpu.environment]
base_python = "/opt/conda/bin/python"
reuse_packages = ["torch"]
require_cuda = true
```

`reuse_packages` requires `base_python` and accepts package names without
versions/extras. An existing uv virtual environment cannot be adopted as the
base. The target table replaces the whole project environment table.

Expri checks the inherited dependency closure against the selected locked
requirements and validates the combined environment. A mismatch fails rather
than replacing the base stack. A successful CUDA smoke test does not establish
dependency compatibility; missing `nvidia-*` Python metadata can still fail
even if Conda supplies native CUDA libraries.

Use `environment.env` for machine-specific native-library/activation variables.
`UV_*`, `PYTHONHOME`, and `VIRTUAL_ENV` overrides are reserved. Expri does not
install GPU drivers or a CUDA toolkit.

Doctor checks the base stack against the selected lock without installing
project dependencies. Its helper/lock export may need network access. Setup or
run preparation performs project installation and combined validation. If a
detached run is preparing or fails, inspect stderr via `expri skill runs`.

## Reclaim old run environments

```sh
expri env prune --keep-last 1 --dry-run --json
expri -T gpu env prune --keep-last 1 --json
expri -T gpu env prune --keep-last 1 --apply --json
```

Pruning previews by default. `--apply` removes eligible expri-owned `.venv`
directories from finished runs, keeping the latest eligible environment by
default. Active runs, unsafe paths, and unowned environments are skipped.
Snapshots, outputs, records, setup environments, and shared uv cache remain.
Review the preview before applying; logical bytes do not measure physical disk
space reclaimed when packages are shared through hardlinks/clones.

Read `expri skill config` for effective configuration and `expri skill run` for
isolation. Use `expri setup --help`, `expri env doctor --help`, and
`expri env prune --help` for exact flags.
