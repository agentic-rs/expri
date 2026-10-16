# Expri metrics

Write scalar training events to `EXPRI_OUTPUT_DIR/metrics.jsonl` and effective
experiment parameters to `EXPRI_OUTPUT_DIR/params.json`. Managed run outputs
live under `.expri/runs/<run_id>/outputs/`; the task works inside its code
snapshot, so a relative `outputs/` path is a different location.

## Instrument the experiment

Each JSONL line is a complete event:

```json
{"schema_version":1,"step":12,"metrics":{"train/loss":0.42,"train/accuracy":0.87}}
```

The parameter file has this shape:

```json
{"schema_version":1,"params":{"learning_rate":0.001,"batch_size":64,"seed":42}}
```

Capture effective parameters after applying defaults and CLI overrides. Treat
saved parameters as immutable for that run. Steps are nonnegative 64-bit
integers; metric values must be finite numbers. Booleans are not metric values.
Optional timestamps are RFC3339 strings. Repeated steps and step resets remain
in logging order; readers report malformed or incomplete rows as warnings.

If you have an expri source checkout, its optional helper is
`sdk/python/expri_metrics.py`. Copy it into the experiment's source
directory before importing it:

```python
from expri_metrics import MetricsLogger

with MetricsLogger() as logger:
  initial_step = logger.initial_step
  logger.params({"learning_rate": 0.001, "batch_size": 64, "seed": 42})
  logger.log(12, {"train/loss": 0.42, "train/accuracy": 0.87})
```

The helper uses the standard library, flushes each event, and rejects parameter
changes. Outside a managed run, use `MetricsLogger("outputs")` explicitly.
`initial_step` is available at initialization: 0 for empty history, otherwise the
maximum existing step plus one. `log(step, metrics)` defaults to relative steps
and writes `initial_step + step`; `log(step, metrics, step_mode="global")` writes
the supplied step unchanged. With history through 1000, relative 0 writes 1001
and relative 10 writes 1011. Use global mode when restoring an absolute counter.
Append any existing complete events before initialization to continue their
history. The base is fixed for the logger session; a managed task attempt shares
its saved base across logger instances. A new standalone logger starts a new
session from the current history.
Existing trainers can write the same files directly. Flush complete JSONL rows
for live inspection; in distributed training write from rank 0 and create the
logger within that process.

Task phases are independent of scalar metrics. Copy the optional
`sdk/python/expri_run.py` helper into task source and use
`from expri_run import set_run_status` to switch between `waiting` and `running`.
Waiting is an active phase: elapsed time and system readings continue, and the
supervisor still owns completion and cancellation.

## Inspect and compare

```sh
expri runs metrics run-abc123 --json
expri runs metrics run-abc123 --metric train/loss
expri runs compare run-abc123 run-def456 --metric train/loss --reduction min --json
expri runs compare run-abc123 run-def456 --chart .expri/charts/comparison.html
```

`metrics` reports sample count, last value, min/max, and last step. Comparison
requires at least two distinct run IDs and uses `last` by default; choose `min`
or `max` deliberately for the question being evaluated. Repeat `--metric` to
select multiple names; omitting it includes all metrics. Missing values remain
explicit rather than becoming zero. JSON includes parameters and warnings.

Charts are standalone local HTML files that work offline. Summary statistics
use all samples; long plotted series are reduced to at most 2,000 points per
run and metric. A chart's status is a recorded snapshot.

## Inspect worker results and offline caches

```sh
expri -T gpu runs metrics run-abc123 --json
expri -T gpu runs fetch run-abc123 --metrics
expri -T gpu runs compare run-abc123 run-def456 --cached --metric train/loss
```

Remote metric inspection fetches only fixed metadata and metric files into
`results/<target>/runs/`; it skips logs and checkpoints. `--cached` requires
`-T` and reads saved files without contacting the target. Cached files can be
older than the worker's current state. Use `runs status` for a live status.
Service-origin results are fetched through `expri fetch` or `service fetch`;
use their cache source with `-T <source> ... --cached` for offline inspection.

Continue with `expri skill run` for execution or `expri skill service` for
service result fetching. Use `expri runs metrics --help` and
`expri runs compare --help` for exhaustive flags.
