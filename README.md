# expri

`expri` is a repo-local remote workflow tool. The first implemented command is
`sync`, which makes a remote working tree match local `HEAD` plus dirty and
untracked local files.

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

[download.mappings]
wandb = "wandb"
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

See `examples/cs336.toml` for a CS336-shaped starting point.

Targets default to `protocol = "auto"`, which tries `expri node sync-apply`
first and falls back to the SSH protocol. Set `protocol = "expri-node"` to
require the node binary, or `protocol = "ssh"` for the fallback path.

## Setup

`expri -T <target> setup` runs repo-configured setup steps on the target. Built-in
steps are `uv`, `hf`, and `script`; scripts are resolved relative to the remote
repo root.

or from inside that repo:

```sh
expri -T runpod sync
```

The sync algorithm uploads committed history with a git bundle, checks out
`HEAD` on the remote, then overlays a zip archive of local dirty and untracked
files. Remote tool state lives under `.expri/`.

For a path-scoped rsync, pass paths after `--`. Only files returned by
`git ls-files` under those paths are transferred:

```sh
expri -T runpod sync -- src scripts
expri -T runpod sync --pull -- outputs/checkpoints
```

## Download

`expri -T <target> download` downloads configured result mappings into
`results/<target>/`. Mappings are declared in `expri.toml`:

```toml
[download.mappings]
wandb = "wandb"
jobs = "out/jobs"
```

That example downloads the remote repo's `wandb/` directory into
`results/<target>/wandb/`, and `out/jobs/` into `results/<target>/jobs/`.
Pass mapping names after `--` to download a subset:

```sh
expri -T runpod download -- wandb
```

Use `--dry-run` to print the SSH/rsync commands without executing them.
