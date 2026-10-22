# Expri task launch

Select a task already defined under `[tasks]` and check whether its effective
environment is configured. Use `expri skill config` if either is unclear.

```sh
expri run --dry-run train --epochs 3
expri run train --epochs 3
expri -T gpu run train --epochs 3
expri -T gpu run --detach --json train --epochs 3
expri -T gpu run --smoke --detach --json train --epochs 1
expri -T gpu run --no-push train --epochs 3
expri run --queue --project-id vision --cpus 4 --memory 16GiB --gpus 1 --json train --epochs 3
```

Put all expri run options before the task name. In
`expri run train --detach`, `--detach` is passed to training; it does not detach
the run. Put the top-level `-T` before `run`.

Remote runs push source first unless `--no-push` is selected. Local runs execute
locally even if the project defines a single target. `--dry-run` previews the
workflow instead of launching the task.

`--queue` freezes and uploads source, then waits for a confirmed task start before
returning a job receipt. While resources are unavailable, its state is `pending`.
It requires a configured environment and an explicit memory reservation in the
CLI or task defaults. CPU defaults to one logical slot and GPUs to zero.
Queued runs use an explicitly enrolled foreground execution worker, with automatic
placement or `--worker`. Read `expri skill queue` for setup and recovery. The job
ID and eventual run ID are separate. Use `--queue --detach` to return a job
receipt immediately. Ctrl-C stops waiting without cancelling the durable job;
use `expri queue cancel <job_id>` for cancellation.

## Experiment membership at launch

For a managed run with automatic service publishing, choose one option before
the task name:

```sh
expri run --experiment exp-abc123 --detach train
expri run --new-experiment "Translation model v2" --detach train
expri -T gpu run --join-run run-abc123 --join-origin workstation --detach train
```

`--experiment` takes an existing experiment ID. `--new-experiment` creates a
named group. `--join-run` uses an existing published run in the configured
project: it joins that run's group, or creates one containing both runs if the
reference is ungrouped. Omit `--join-origin` only for a unique run ID. These
options are mutually exclusive and also apply to queued launches.

The publishing credential needs owner permissions; origin-scoped worker tokens
cannot manage grouping. Known permission/reference errors fail preflight.
Temporary connectivity failures let direct runs start and retain the request for
background retry after initial metadata publication. Launch success therefore
does not prove membership is already visible; check publishing status and the
hosted experiment. Successful retries do not duplicate groups or undo later
manual regrouping.
Queued launches still need the server to accept and schedule their submission.

Grouping preserves each run's history and does not imply lineage. Use
`expri skill runs` for the website's group and relationship controls.

## Managed runs and outputs

With `[environment]` configured, each run gets a frozen source snapshot, uv
environment, logs, and records under `.expri/runs/<run_id>/`. An empty environment
table is sufficient for ordinary uv isolation. Legacy tasks without an
environment execute in the checkout and do not support detach.

`--smoke` also requires a configured environment. It marks the run explicitly
and generates a `smoke-run-…` ID. With automatic publishing and an updated
server, every finished smoke run is archived on the server, including failed,
cancelled, and lost runs. Existing 15-day retention applies; local/worker files
remain available. `--no-publish` records the smoke mark without server archival.

Inside a managed task:

| Variable | Meaning |
| --- | --- |
| `EXPRI_BIN` | Current executable on the native worker |
| `EXPRI_RUN_ID` | Actual run identifier |
| `EXPRI_RUN_DIR` | Absolute path to this run's directory |
| `EXPRI_OUTPUT_DIR` | Absolute path to this run's outputs directory |

The task's working directory is its code snapshot. Write checkpoints and other
generated results to `EXPRI_OUTPUT_DIR`; a relative `outputs/` path otherwise
lands inside the code snapshot. Metrics use `expri skill metrics`, and finalized
checkpoint registration uses `expri skill artifacts`.

## Task-reported phase

Inside a managed task, use the local executable to report waiting and resume:

```sh
"$EXPRI_BIN" runs set-status waiting
"$EXPRI_BIN" runs set-status running
```

These operations use `EXPRI_RUN_DIR` without loading project or server settings.
They require a live running/waiting task and are idempotent. Waiting retains
elapsed time, publication, system capture, and cancellation; the supervisor
publishes preparation and final states. Tasks cannot change finished runs.
The dependency-free `sdk/python/expri_run.py` helper provides
`set_run_status("waiting")` and `set_run_status("running")` on both worker paths;
copy it into task source before importing it.

## Detached execution

`--detach` returns a table with the run ID and directory. Add `--json` before
the task name to capture a structured start receipt once its
supervisor can accept requests. Environment preparation continues in the
background. Capture the receipt's actual run ID, then inspect:

```sh
expri -T gpu runs status run-abc123 --json
expri -T gpu runs logs run-abc123 --stream stderr --tail 50
```

A detached run survives terminal/SSH disconnection but does not restart after
reboot. Task status and publisher status are separate; successful launch does
not imply completed training or uploaded outputs. `--no-publish` before the
task name disables automatic publishing for that run while retaining asset
download credentials.

New native Linux detached runs can upgrade their per-run supervisor with
`runs restart-supervisor` while the same task keeps running. Shared service or
component restarts do not upgrade it. Older supervisors and the Python fallback
cannot hand off a live task; arbitrary supervisor crashes are not recovered.

Read `expri skill runs` for logs, cancellation, supervisor upgrades, and result retrieval,
`expri skill environment` for preparation failures, and `expri skill service`
for publishing. Use `expri run --help` for exact flags.
