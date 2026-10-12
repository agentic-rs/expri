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
```

Put all expri run options before the task name. In
`expri run train --detach`, `--detach` is passed to training; it does not detach
the run. Put the top-level `-T` before `run`.

Remote runs push source first unless `--no-push` is selected. Local runs execute
locally even if the project defines a single target. `--dry-run` previews the
workflow instead of launching the task.

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
| `EXPRI_RUN_ID` | Actual run identifier |
| `EXPRI_RUN_DIR` | Absolute path to this run's directory |
| `EXPRI_OUTPUT_DIR` | Absolute path to this run's outputs directory |

The task's working directory is its code snapshot. Write checkpoints and other
generated results to `EXPRI_OUTPUT_DIR`; a relative `outputs/` path otherwise
lands inside the code snapshot. Metrics use `expri skill metrics`, and finalized
checkpoint registration uses `expri skill artifacts`.

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

Read `expri skill runs` for logs, cancellation, and result retrieval,
`expri skill environment` for preparation failures, and `expri skill service`
for publishing. Use `expri run --help` for exact flags.
