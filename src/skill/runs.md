# Expri run inspection and worker results

Use the same target used to launch the run. Omit `-T` for locally recorded runs.
Prefer JSON reports for agent inspection.

For published server records, use `expri query runs` and `expri query files`.
They show tables by default; add `--json` for bounded JSON pages through the global `[server]` connection without
SSH or a local project. `expri query files --run-id run-abc123 --json` resolves a
unique accessible active or retained archived run; add `--project-id` and/or
`--origin` if the ID is ambiguous. Read `expri skill query` for resolved scopes,
pagination, and file availability. Their execution state is the last publication; the worker
commands below inspect live supervision.

```sh
expri runs list --json
expri -T gpu runs list --task train --status failed --limit 5 --json
expri -T gpu runs show run-abc123 --json
expri -T gpu runs status run-abc123 --json
expri -T gpu runs logs run-abc123 --stream stderr --tail 100
expri -T gpu runs logs run-abc123 --tail 0 --follow
```

`list` defaults to the newest 20 recorded runs. `show` includes source and
environment provenance; `status` also checks detached supervisor liveness and
reports publishing status when available. A detached run with an active record
but no live supervisor is `lost`. Missing or incomplete records can yield
warnings rather than a complete report.

Smoke runs launched with `run --smoke` carry a `smoke` mark in local metadata
and a generated `smoke-run-…` ID. The dashboard shows a Smoke badge. Finished
published smoke runs are archived automatically on an updated server; use
`expri query runs --archival archived --json` to include them. Local and worker
inspection remains available during and after the server's 15-day retention.

Logs default to stdout and its last 100 lines. Following ends when the run
finishes or its supervisor is lost. Interrupting the log reader leaves training
running. For bounded agent inspection, start with a tail rather than a follow.

## Cancellation

```sh
expri -T gpu runs cancel run-abc123 --json
expri -T gpu runs status run-abc123 --json
```

Cancellation is an asynchronous request to a live detached supervisor. It
terminates preparation or the task gracefully, escalating after two seconds if
needed; check status afterward. Finished runs retain their result. Active
foreground runs and lost detached runs cannot be cancelled with this command.

## Retrieve directly from a worker

```sh
expri -T gpu runs fetch run-abc123 --json
expri -T gpu runs fetch run-abc123 --metrics --json
expri -T gpu runs fetch run-abc123 --artifact outputs/best.pt --dry-run
expri -T gpu runs fetch run-abc123 --artifact outputs/best.pt
expri -T gpu runs fetch run-abc123 --outputs
expri -T gpu runs show run-abc123 --cached --json
```

`runs fetch` requires a target and uses SSH/ctl. By default it retrieves metadata
and logs into `results/<target>/runs/<run_id>/`; `[download].results_dir`
overrides `results`. `--metrics` retrieves metrics/parameters with metadata,
without logs or checkpoints. `--outputs` selects all dedicated outputs;
repeat `--artifact` for specific files/directories under `outputs/` or `code/`.
Environments, caches, and symlinked files are excluded.

Dry-run queries the worker's selection without transferring files or writing
the local cache. Subsequent metadata-only fetches preserve previously fetched
artifacts. `--cached` on `list`/`show` requires `-T` and reads that target's cache
without contacting it.

For configured worker-root mappings, use `expri -T gpu download -- NAME` instead.
For service-backed results, use `expri skill service`; top-level `fetch` is a
different path. `expri dashboard` browses local and cached runs without fetching
worker data. Read `expri skill metrics` for scalar summaries and comparison.
Use `expri runs --help` and `expri runs fetch --help` for exact flags.
