Copy `expri_metrics.py` into your experiment's source directory, then import it
from your training script. It uses the Python standard library on Linux and
macOS; no package installation or changes to `PYTHONPATH` are required.

```python
from expri_metrics import MetricsLogger

with MetricsLogger() as logger:
  logger.params({"learning_rate": 0.001, "batch_size": 64, "seed": 42})
  for step in range(100):
    loss, accuracy = train_step()
    logger.log(step, {"train/loss": loss, "train/accuracy": accuracy})
```

The default directory is `EXPRI_OUTPUT_DIR`, which expri sets for each managed
run. Outside expri, pass a directory explicitly: `MetricsLogger("outputs")`.
The helper writes `metrics.jsonl` and `params.json` directly into that directory.
Use the effective parameters after applying defaults and command-line overrides.
The first `params()` call saves them; subsequent calls allow identical values and
reject changes within the same task attempt.

For `expri runs continue`, use this version of the helper in the task's source.
Expri sets `EXPRI_ATTEMPT` and `EXPRI_STEP_OFFSET`: the helper maps the first local
step to the previous history's maximum global step plus one, then preserves local
step spacing. Continued rows include `local_step` and `attempt` alongside `step`.
The step origin is saved under `outputs/attempts/<attempt>/step-origin.json` while
holding the metrics file lock, so separate logger instances agree on the mapping.
Steps below that attempt's first local step and global u64 overflow are rejected
without modifying the metric history. Attempt 1 keeps the original step behavior.

Continued attempts save parameters in `outputs/attempts/<attempt>/params.json`,
allowing a new training limit or other changed settings without overwriting the
original parameters. Metric rows still append to the same `metrics.jsonl` file.
The task loads its checkpoint from `EXPRI_RESUME_CHECKPOINT` using its own training
framework. System readings and metric timestamps retain real observation times;
the inactive interval is never backfilled with synthetic values.

Each `log()` call appends one complete JSONL row and flushes it before returning.
Rows include `schema_version: 1`, a UTC `timestamp`, `step`, and `metrics`.
Steps are nonnegative integers up to `2**64 - 1`; they may repeat or arrive out of
order. Metric values must be finite real numbers, with booleans excluded. Integer
values retain their exact representation when they fit i64 or u64; larger finite
values use floating-point JSON numbers. Metric names remain unchanged, must be
nonempty after trimming, and cannot contain control characters or exceed 256
UTF-8 bytes. Events and parameter metadata are limited to 1 MiB each.

For distributed training, write from rank 0. Threads and cooperating local Unix
processes can append without interleaving rows, but this does not establish an
ordering between independent writers. Create a logger in each writer process;
an existing logger must not be reused after `fork`. Use separate output directories for
independent experiments. The helper rejects symlink destinations and refuses to
append after an incomplete row; it preserves existing bytes for inspection.

Parameter values can contain nested JSON objects, lists, strings, numbers,
booleans, and nulls. Object keys must be strings, integers must fit i64 or u64,
and NaN, infinity, tuples, and arbitrary Python objects are rejected.
Parameters may contain at most 64 nested object or array levels, including the
parameter root, so the stored file remains readable by expri.


Copy `expri_run.py` into your experiment's source directory when a managed task
needs to report that it is waiting for input or a resource. This independent
helper uses the Python standard library on Linux and macOS and does not require
the native expri binary to be installed or on `PATH`.

```python
from expri_run import set_run_status

set_run_status("waiting")
resource = wait_for_resource()
set_run_status("running")
train(resource)
```

The default directory is `EXPRI_RUN_DIR`, which expri sets for each managed run.
Pass `run_dir` explicitly only to select an existing managed run directory.
`waiting` is an active status: the task stays alive and cancellable, and logs,
system capture, publishing, and total elapsed time continue. It does not suspend
the process or impose a timeout. The helper preserves the original start time,
capture information, and all other run metadata.

`set_run_status()` returns a dictionary with `run_id`, `previous_status`,
`status`, and `changed`. Repeating the current status returns `changed: False`
without rewriting `run-state.json`. Only live, unfinished `running` or `waiting`
runs with valid schema and start metadata can change status; preparing, finished,
or lost runs are rejected. A status request cannot revive a completed run.

The helper and supervisor serialize state changes with the short
`.run-state.lock`, separate from the supervisor's continuously held `.run.lock`.
The lock wait is bounded to five seconds; a busy state lock raises `TimeoutError`
and can be retried. Invalid metadata raises `ValueError`, and filesystem failures
raise `OSError`. Run directories, parent components, metadata files, lock files,
and owned code/output directories must be real paths without symlinks. State
metadata is limited to 256 KiB and updates use a flushed atomic replacement.
