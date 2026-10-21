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

## Experiment groups and lineage in the website

An experiment groups runs serving one goal, including work across machines or
implementations. The hierarchy is **project → experiment → run → task attempt**.
Each run can belong to one experiment or remain ungrouped. Preserve its full
identity: project, publishing origin, and run ID. Equal run IDs from different
origins are separate runs.

On the primary hosted dashboard, open the experiment manager in the run history
header to create a group and assign selected runs or a run from its inspector.
Grouping and lineage management currently use the website; there are no CLI
assignment commands. AB previews allow read-only browsing. Local, cached, and
older dashboards keep their existing browsing when grouping is unsupported.

The **Experiments** view starts collapsed. A group's summary uses its latest
started matching run when available. It is not a designated primary or best
version. Status and selected columns describe that run; retained member counts
stay full, while matching and active counts honor filters. Filters can change
the **Latest matching** representative. Sorting and top-level pagination operate
on whole groups. Expand a group to load its other matching runs newest first
with independent history pagination. Choose **Runs** for a flat sortable table,
newest first by default; its experiment filter includes older members beyond
the normal recent-run window. Selection and lifecycle controls always target
individual runs.

Membership does not require lineage. In the experiment editor, record directed
**Continued**, **Restarted**, or **Split** relationships between members to
describe resumed work, another execution, or parallel parts. Relationships must
stay within the group and cannot form cycles. Any relationship can include a
parent checkpoint reference and step; these record provenance without verifying
or loading the checkpoint or launching work. Existing Continue retains the run
ID and adds a task attempt, so membership follows that run. Assign and link new
runs, including Restart results, explicitly.

Every run retains its own metrics and step axis. Groups do not concatenate
history, shift steps, aggregate parallel metrics, or coordinate execution.
Comparison charts keep each run's series separate. Deleting a group preserves
its runs and data; expired members can remain as unavailable historical links.

## Task-controlled waiting

Run `"$EXPRI_BIN" runs set-status waiting` from a live task to report that it is
waiting for external work, then use `running` when work resumes. The operation
uses `EXPRI_RUN_DIR`; `--run-dir` selects an explicit local live run, and `--json`
prints the old/new status plus whether it changed. Omit `-T`: this operation
runs on the training machine and bypasses project/server configuration.
Only `running` and `waiting` can be requested. A waiting task remains alive,
keeps its original start time, and continues publishing and collecting system
readings. Repeated requests are idempotent; final states cannot be rewritten.

## Cancellation

```sh
expri -T gpu runs cancel run-abc123 --json
expri -T gpu runs status run-abc123 --json
```

Cancellation is an asynchronous request to a live detached supervisor. It
terminates preparation or the task gracefully, escalating after two seconds if
needed; check status afterward. Finished runs retain their result. Active
foreground runs and lost detached runs cannot be cancelled with this command.

## Live supervisor upgrades

New native Linux detached runs support an explicit supervisor handoff while
their task remains `running` or `waiting`:

```sh
expri -T gpu runs status run-abc123 --json
expri -T gpu runs restart-supervisor run-abc123 --reason upgrade --request-id upgrade-1 --expected-generation 1 --binary /opt/expri/bin/expri --json
```

Use the run's current `state.supervisor.generation` from status; generation starts
at 1 and advances only after a successful handoff. `--expected-generation`
protects against stale requests. `--binary` is an absolute path on the executing
worker, not a local file to upload. Omit it to use the native executable handling
the request. Omit `-T` for a local run.

The command waits for the result. Save its request ID and reuse it with the same
reason, binary option, and expected generation after an interrupted connection
or acknowledgement timeout. An identical retry returns the recorded result
without a second handoff; reusing an ID with changed arguments is rejected.
Without `--request-id`, the CLI generates and prints one. A recorded failure
produces a nonzero exit code, including with `--json`; inspect `restart.error`
and the event history. Failed target validation leaves the worker running.
Use a new request ID for a fresh attempt after fixing a recorded failure.

The task PID, run ID, start time, logs, metrics, and artifacts are preserved.
`run-state.json` retains generation/reason/outcome history and explicitly records
`worker_restarted: false`; dashboard Overview shows its newest bounded preview.
When run metadata approaches its size limit, new requests are refused rather
than dropping history. Old supervisors, Python fallback, foreground, preparing,
terminal, and cancellation-in-progress runs are unsupported. A service/component
restart does not upgrade per-run supervisors. This operation cannot recover or
adopt a task after an arbitrary supervisor crash or SIGKILL and never restarts a
worker from a checkpoint.

## Continue a task from a checkpoint

```sh
expri -T gpu runs continue run-abc123 --checkpoint outputs/checkpoint-1000.pt --reason extend-2000 --request-id extend-2000 --expected-attempt 1 -- --max-steps 2000
```

This starts another task attempt in the same direct, detached run on a native
Linux worker. Running or waiting tasks stop before the replacement starts;
completed, failed, and cancelled direct runs can also continue. Active runs need
a supervisor that advertises continuation support. Queued runs, foreground runs,
Python fallback, preparing runs, lost supervision, and cancellation in progress
are unsupported. The operation does not recover an arbitrary supervisor crash.

The command resolves the recorded task in the current configuration (`--task`
overrides it), pushes source, and prepares an isolated snapshot/environment before
stopping the old task. `--no-push` uses the existing worker checkout. Arguments
after `--` append to the configured command. Use a finalized checkpoint beneath
the run's `outputs/`; the task must load `EXPRI_RESUME_CHECKPOINT` through its own
training framework. Expri does not choose framework-specific loading arguments.

The run ID, original start time, metric/log prefixes, and registered artifacts
remain. Each attempt records code, checkpoint, reason, dates, and outcome. The
updated Python metrics helper exposes `logger.initial_step`, the earlier global
maximum plus one. Relative logging defaults to `initial_step + step`, with local
step 0 at that base; pass `step_mode="global"` to preserve an absolute counter.
The base is shared by logger instances within the same attempt. Direct JSONL
writers must apply their own global mapping using `EXPRI_STEP_OFFSET`;
`EXPRI_ATTEMPT` identifies the attempt. Continued parameters live in
`outputs/attempts/<attempt>/params.json`.
Real timestamps preserve downtime gaps; no observations are invented.

Save the request ID and retry an interrupted request with the same arguments.
`--expected-attempt` rejects a stale request after another attempt starts. A
recorded failure returns a nonzero exit code, including with `--json`; inspect
`continuation.error`. Failed source/checkpoint/environment preparation leaves an
active old task running.
After fixing a recorded failure, use a new request ID for a fresh attempt.

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
