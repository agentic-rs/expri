# expri

`expri` is a repo-local remote workflow tool. The first implemented command is
`sync`, which makes a remote working tree match local `HEAD` plus dirty and
untracked local files.

## Sync

Top-level commands are controller-side commands: they run from your workstation
and operate on a configured target. `expri node ...` is the target-machine
namespace for commands that run locally on a synced node.

Create an `expri.toml` in the repo you want to sync, then run:

```sh
expri sync runpod --config cs336-assignment5-alignment/expri.toml
```

See `examples/cs336.toml` for a CS336-shaped starting point.

Targets default to `protocol = "auto"`, which tries `expri node sync-apply`
first and falls back to the SSH protocol. Set `protocol = "expri-node"` to
require the node binary, or `protocol = "ssh"` for the fallback path.

## Setup

`expri setup <target>` runs repo-configured setup steps on the target. Built-in
steps are `uv`, `hf`, and `script`; scripts are resolved relative to the remote
repo root.

or from inside that repo:

```sh
expri sync runpod
```

The sync algorithm uploads committed history with a git bundle, checks out
`HEAD` on the remote, then overlays a zip archive of local dirty and untracked
files. Remote tool state lives under `.expri/`.

Use `--dry-run` to print the SSH/rsync commands without executing them.
