# expri

`expri` is a repo-local experiment workflow tool. It syncs local code to a
target, prepares Python dependencies with uv, and runs configured tasks locally
or remotely.

Building expri requires Rust 1.89 or newer.

## Sync

Top-level commands are controller-side commands: they run from your workstation
and operate on a configured target. `expri node ...` is the target-machine
namespace for commands that run locally on a synced node.

Create an `expri.toml` in the repo you want to sync, and keep machine targets
in a sibling target file. The target filename follows the config filename:
`expri.toml` uses `expri.target.toml`, and `cs336.toml` uses
`cs336.target.toml`. Target files are local/private; add them to that repo's
`.gitignore`.

```toml
# expri.toml
[project]
name = "my-project"

[sync]
remote_managed = ["uv.lock"]

[download.mappings]
wandb = "wandb"

[tasks]
dev = ["pnpm", "dev"]
train = { command = ["python", "scripts/train.py"], uv = true }
```

```toml
# expri.target.toml
[target.runpod]
host = "user@example.com"
remote_dir = "~/my-project"
protocol = "auto"
node_bin = "expri"
```

Then run:

```sh
expri -T runpod sync --config cs336-assignment5-alignment/expri.toml
```

Or from inside that repo:

```sh
expri -T runpod sync
```

See `examples/cs336.toml` for a CS336-shaped starting point.

Targets default to `protocol = "auto"`, which tries `expri node sync-apply`
first and falls back to the Python sync protocol. Set `protocol = "expri-node"`
to require the node binary, or `protocol = "python"` for the fallback path.
`protocol = "ssh"` remains an alias for `"python"`. The protocol chooses how
sync is applied on the target; the transport chooses how commands and files
reach it.

For configured environments, `auto` also checks whether the installed node
supports environment preparation and isolated runs. An older node falls back
to the Python protocol. `protocol = "expri-node"` requires a node with those
capabilities and reports an error when an upgrade is needed.
The `env doctor` and `env prune` commands also check for maintenance support
and use the same automatic fallback for older nodes.

The sync algorithm uploads committed history with a git bundle, stages `HEAD`
plus a zip archive of local dirty and untracked files, then installs the staged
files on the remote. It removes previously synced files absent from the staged
tree and preserves unrelated remote-generated files. Remote tool state lives
under `.expri/`.

Use `sync.remote_managed` for repo-relative files that the target should own,
even if they are tracked by Git or appear in the dirty patch. For example,
`remote_managed = ["uv.lock"]` preserves the target's lockfile across syncs and
excludes local changes to that file from `patch.zip`.

For a path-scoped rsync, pass paths after `--`. Only files returned by
`git ls-files` under those paths are transferred:

```sh
expri -T runpod sync -- src scripts
expri -T runpod sync --pull -- outputs/checkpoints
```

## Transport

Targets use SSH by default. Set `transport = "ctl"` on an individual target to
use [ctl](https://github.com/tokn-ai/ctl):

```toml
# expri.target.toml
[target.work]
transport = "ctl"
host = "work"
remote_dir = "~/my-project"
protocol = "auto"
# ctl_bin = "/path/to/ctl/target/debug/ctl"
# ctl_method = "vpn"
```

`host` selects a saved ctl host name or ID, or an SSH alias/destination.
Use the saved host ID if its name contains `:` or `/`, since rsync uses these
characters to parse destinations. For IPv6, configure the address in a saved ctl
host or SSH alias, then use its name, ID, or alias here.
`ctl_bin` defaults to `ctl` on your `PATH`; set it to a binary built from a local
ctl checkout if needed. `ctl_method` optionally selects a connection method
name or ID; otherwise ctl uses the saved host's preferred method.

Remote commands run through `ctl ssh`, and transfers retain rsync with
`rsync -e 'ctl ssh'` (including the configured binary and method). Expri does
not need `ctl-agent` for this transport. Rsync must be installed locally and on
the target. The target still needs a Unix shell and the usual tools required by
the selected sync protocol and commands.

The `[ssh]` control settings apply only to the SSH transport. With ctl, ctl
manages connections and reuse. A target's explicit `port` is passed as `-p`;
ctl treats that as a connection override and skips broker connection reuse.
Prefer a saved host with its account configured in ctl. With GNU rsync, a
`user@host` destination also overrides the account and skips broker reuse.

## Task

`expri run <name>` runs a configured command alias:

```toml
[tasks]
dev = ["pnpm", "dev"]
train = { command = ["python", "scripts/train.py"], uv = true }
```

Without an environment configuration, the array form runs exactly that command
in the repo root. The object form supports `uv = true`, which prefixes the
command with `uv run`. Existing task definitions keep this behavior.

With an `[environment]` or target environment configured, every task uses uv
and runs from its own code snapshot. Expri stores the snapshot, run environment,
and output directory under `.expri/runs/<run_id>/`. A later sync cannot change
the copied code files of an active run. Base packages remain shared when reuse
is enabled, and uv's cache avoids repeated downloads when it creates each run's
environment.

Run environments are retained under `.expri/runs/`. Expri keeps a shared uv
cache in the original checkout's `.expri/cache/uv`, so each new code snapshot
uses the same cached packages. An explicit setup `--cache-dir` is resolved from
the original checkout and remains stable across runs; otherwise an ambient
`UV_CACHE_DIR` override takes precedence over the project default. Environments,
snapshots, and outputs still accumulate; use `expri env prune` to review old run
environments for removal.

Local snapshots include explicit file paths listed in `sync.include_ignored`,
such as a Git-ignored experiment configuration, alongside the usual tracked,
dirty, and untracked source files. Other Git-ignored files stay excluded.

```sh
expri run dev
expri run train --epochs 3
expri -T runpod run train --epochs 3
expri -T runpod run --no-sync train --epochs 3
```

When `-T/--target` is provided, `run` syncs the repo to the target before
executing the task there. Pass `--no-sync` before the task name to skip that
sync. Run options must appear before the task name; arguments after the task
name are passed through to the task.

Each configured-environment run receives these variables:

| Variable | Meaning |
| --- | --- |
| `EXPRI_RUN_ID` | The run's identifier |
| `EXPRI_RUN_DIR` | Absolute path to the run directory |
| `EXPRI_OUTPUT_DIR` | Absolute path to the run's output directory |

The task's working directory is its code snapshot. Relative output paths such
as `outputs/checkpoint.pt` therefore live under the snapshot. Have the task
write to `EXPRI_OUTPUT_DIR` when you want all outputs in the dedicated output
directory. The existing `download` mappings still resolve from the target repo
root. Use `runs pull` for files stored within an individual run.

Configured-environment runs save task stdout and stderr to
`.expri/runs/<run_id>/logs/stdout.log` and `logs/stderr.log` while streaming both
to the terminal. Environment-preparation diagnostics also go to the stderr log;
the helper's private JSON response stays out of task stdout. Run records include
the command, timestamps, status, exit code, and log paths. Older run directories
remain readable when optional records or logs are missing.

Execution remains a foreground process. Saved logs do not make tasks survive
an SSH disconnect or provide detached-job supervision. A stopped runner can
leave a preparing or running record; expri preserves that recorded status.
If a descendant keeps a pipe open after the foreground child exits, logging
stops waiting after a one-second grace period. This and other capture failures
are recorded as `logging_error`, while `task_exit_code` preserves the task's
exit status. Buffered output from closed pipes is drained completely.

## Run records and selective pull

List local runs or inspect the configured target directly:

```sh
expri runs list
expri -T runpod runs list --task train --status failed --limit 5
expri -T runpod runs show run-abc123 --json
```

`list` defaults to the 20 newest records. Its status filters are `preparing`,
`running`, `completed`, `failed`, and `unknown`. `show` includes the run state,
source snapshot provenance, and recorded Python environment when available.
Missing, malformed, or unsupported records produce warnings; incomplete runs
can still be inspected. These commands read fixed records rather than scanning
an environment's installed packages.

Pull a target run's available metadata and logs into
`results/<target>/runs/<run_id>/`:

```sh
expri -T runpod runs pull run-abc123
expri -T runpod runs pull run-abc123 --outputs --dry-run
expri -T runpod runs pull run-abc123 --artifact 'outputs/model one.pt'
expri -T runpod runs pull run-abc123 --artifact code/out/jobs
```

The default selection contains `run-state.json`, `snapshot.json`, the
environment-state manifest, and regular files under `logs/`. Outputs and files
under the code snapshot are opt-in: `--outputs` selects the dedicated output
directory, and repeatable `--artifact` selects a file or directory beneath
`outputs/` or `code/`. Expri excludes environments, caches, and symlinked files
or directories from these transfers. Explicit selections into excluded paths
fail. The configured `[download].results_dir` replaces the default `results`
directory.

A pull stages the selected files before updating its owned destination and
writes `pull-state.json` after publication. Repeating a metadata-only pull
refreshes the records and logs while retaining artifacts downloaded earlier.
A failed transfer leaves the previous cache intact. Missing optional metadata
is skipped, so partial records can still be pulled.

`runs pull --dry-run` contacts the target to query the actual selection, then
prints the destination and files without writing locally or transferring them.
Use cached inspection to review previously pulled records without contacting
the target:

```sh
expri -T runpod runs list --cached
expri -T runpod runs show run-abc123 --cached --json
```

With `protocol = "auto"`, run queries and logging use capability checks and
fall back to Python on older nodes. Requiring `protocol = "expri-node"` reports
an upgrade error when the installed node lacks the requested capability.

## Python environment

Configure uv for local tasks and as the default for targets with a top-level
environment table:

```toml
# expri.toml
[environment]
```

An empty table selects a normal uv environment. For a remote GPU machine that
already has a suitable Conda or system Python installation, select its Python
and the packages you want to reuse:

```toml
# expri.target.toml
[target.runpod.environment]
base_python = "/opt/conda/bin/python"
reuse_packages = ["torch"]
require_cuda = true

[target.runpod.environment.env]
# Add the native-library variables your existing installation needs:
LD_LIBRARY_PATH = "/opt/conda/lib"
```

The target environment table replaces the entire top-level environment table;
fields are not merged. A target without that table inherits the top-level
configuration. Use an empty `[target.<name>.environment]` table to select a
normal uv environment even when the project default reuses base packages.

`base_python` identifies an existing interpreter. `reuse_packages` contains
package names, without versions or extras, and requires `base_python`. Expri
validates their installed dependency closure and requires exact locked versions
where inherited packages occur in the target's selected lock graph. It then
validates the combined environment and keeps the base installation unchanged.
A mismatch fails with an explanation instead of replacing PyTorch or its
dependency stack. Unrelated Conda packages are not compatibility-certified.

For reuse, point `base_python` at the Conda or system interpreter that owns the
installed packages. An existing uv virtual environment is not adopted as a base
or as a run environment; reuse from another virtual environment is rejected.
Passing a Torch/CUDA smoke test alone does not establish lock compatibility:
the inherited dependency closure and the combined environment must also pass
validation against the selected locked requirements.

PyTorch reuse requires a compatible Python distribution graph as well as working
native libraries. If `uv.lock` selects `nvidia-*` CUDA wheel packages, those
distributions must already belong to the inherited stack. A Conda installation
with CUDA libraries but missing that Python package metadata fails explicitly;
expri avoids downloading a second CUDA wheel stack into the overlay. Select a
base installation and lockfile that describe the same dependency stack.

`require_cuda` defaults to `false`. Set it to `true` alongside PyTorch reuse to
require an available CUDA device and a small GPU computation during environment
validation. When PyTorch is reused, its CUDA version and availability are
recorded even if CUDA is not required. Native-library and other activation
variables can be supplied in `env`; use the values appropriate to that machine.
`UV_*`, `PYTHONHOME`, and `VIRTUAL_ENV` overrides are reserved for expri's
environment selection. Expri does not install CUDA drivers or a CUDA toolkit.

Install uv on each execution machine before preparing an environment. The
reuse helper fetches small pinned `packaging` and `tomli` dependencies through
uv. Project dependencies are also fetched as needed through uv. Reuse mode
installs external dependencies from wheels, so missing compatible wheels fail
explicitly. The current project is installed as an editable package in the run
environment using compatible build dependencies already available there.

## Environment checks and storage

Before preparing a rented machine's project environment, check its existing
Python stack against the selected lockfile:

```sh
expri -T runpod sync
expri -T runpod env doctor
expri -T runpod env doctor --json
```

For a top-level `[environment]`, run `expri env doctor` locally. The report
identifies the base Python, inherited package closure, failed checks, and cache
configuration. Its compatibility verdict covers the base stack and selected
lock graph. It does not certify a rental provider, GPU performance, or a future
project build. If an owned prepared environment already exists, the report
checks it separately without rewriting its manifest. Project dependency
installation and combined-environment validation happen during setup or run
preparation. The helper may fetch its small pinned dependencies and the lock
export may need package-index access.

If the report identifies a locked-version mismatch, select a base image and
lockfile that agree. A missing `nvidia-*` wheel metadata issue means Conda
native CUDA libraries alone do not describe the requested wheel stack.
`require_cuda = true` additionally checks GPU availability and a small CUDA
computation; successful CUDA execution does not waive dependency checks.

The report includes the effective cache directory, link mode, whether the cache
and run environments share a filesystem, and whether caching is disabled.
Sharing cached package files through hardlinks or filesystem clones can reduce
physical disk use on a compatible filesystem. Copy mode, cross-filesystem
storage, source builds, editable projects, and generated outputs can still add
per-run bytes. A directory's logical file size is not the amount of disk space
that pruning will reclaim.

Preview pruning before applying it:

```sh
expri -T runpod env prune
expri -T runpod env prune --keep-last 1 --apply
# Local project:
expri env prune --keep-last 1 --dry-run
```

Pruning defaults to a preview and keeps the latest eligible run environment.
It removes only expri-owned `.venv` directories from finished runs. Active or
preparing runs, unsafe paths, and unowned environments are skipped. Code
snapshots, outputs, run records, setup environments, and the shared cache remain
available. The report lists each run's action and reason, logical bytes, and
the number of environments removed. Pruning does not happen automatically.

## Setup

`expri -T <target> setup` runs repo-configured setup steps on the target. Built-in
steps are `uv`, `hf`, and `script`; scripts are resolved relative to the remote
repo root. When the target has an environment configured, setup also validates
the base installation, prepares a dependency overlay, warms uv's cache, and
saves a validation manifest under `.expri/`. Each run prepares its own overlay
under its run directory and validates the base stack again. Setup does not
guarantee that a later run's project build or downloads will succeed offline.

With a top-level `[environment]`, `expri setup` also prepares the local project
without a target. A target-only configuration requires `-T <target>`. Setup
scripts before the first environment-preparation step run in the ambient
environment; scripts after preparation run through uv in the selected
environment. Place a `uv` setup step before scripts that need project
dependencies. The project itself is installed in each isolated run environment.

Prepare the target after syncing its project files:

```sh
expri -T runpod sync
expri -T runpod setup
expri -T runpod run train --epochs 3
```

The run records the base interpreter and inherited package versions so results
can be traced back to the environment used. Preparation inspects the current
base stack and checks reused packages against the project lock. Compatible
changes produce a new recorded fingerprint; incompatible changes fail.

## Download

`expri -T <target> download` downloads configured result mappings into
`results/<target>/`. Mappings are declared in `expri.toml`:

```toml
[download]
ignore = ["*.pt"]

[download.mappings]
wandb = "wandb"
jobs = "out/jobs"
```

That example downloads the remote repo's `wandb/` directory into
`results/<target>/wandb/`, and `out/jobs/` into `results/<target>/jobs/`.
Ignore patterns are passed to rsync as excludes relative to each mapping root.
Pass mapping names after `--` to download a subset:

```sh
expri -T runpod download -- wandb
```

Use `--dry-run` to print the selected transport's commands without executing them.
