# Expri source pushing

Use `push` to send repository source and configuration to a worker checkout.
Use `expri skill config` to choose its target and transport.

```sh
expri -T gpu push --dry-run
expri -T gpu push
expri -T gpu push -- src scripts
expri -T gpu push --watch
```

The full push transfers committed Git history plus tracked local edits and
untracked source files. It removes previously pushed files missing from the
new tree while preserving unrelated worker-generated files. Tool state is
under `.expri/`. Large reusable inputs belong in managed assets rather than
source bundles; see `expri skill assets`.

Paths after `--` select the path-scoped rsync mode. That mode transfers only
files listed by `git ls-files` under the selected paths. It is not the same as
the complete history-and-dirty-source push. `--watch` cannot be combined with
path selections.

## File ownership

In `expri.toml`, use `[push]` to define exceptions to source selection:

```toml
[push]
remote_managed = ["uv.lock"]
include_ignored = ["config/experiment.toml"]
```

`remote_managed` preserves worker-owned files even if they are tracked or
locally modified. `include_ignored` selects explicit ignored source paths for
pushing and local snapshots. Managed asset payloads stay excluded; their
sidecars carry content identity.

## Relationship to running and fetching

`expri -T gpu run TASK` pushes automatically before launching. Use
`run --no-push TASK` only when intentionally executing the existing worker
checkout. Active managed runs retain frozen code snapshots, so a later push or
`push --watch` does not change their copied code files.

`fetch --watch` independently receives service results; it does not push source.
Use `expri skill service` for that workflow and `expri skill runs` to retrieve
files directly from a worker. `expri node ...` commands are worker-side protocol
operations; normal workstation workflows use the controller commands above.

Use `expri push --help` for exact flags.
