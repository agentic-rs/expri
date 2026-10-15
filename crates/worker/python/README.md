Copy `expri_metrics.py` into your experiment's source directory, then import it
from your training script. It uses the Python standard library on Linux and
macOS; no package installation or changes to `PYTHONPATH` are required.

```python
from expri_metrics import MetricsLogger

with MetricsLogger() as logger:
  initial_step = logger.initial_step
  logger.params({"learning_rate": 0.001, "batch_size": 64, "seed": 42})
  for local_step in range(100):
    loss, accuracy = train_step()
    logger.log(local_step, {"train/loss": loss, "train/accuracy": accuracy})
```

The default directory is `EXPRI_OUTPUT_DIR`, which expri sets for each managed
run. Outside expri, pass a directory explicitly: `MetricsLogger("outputs")`.
The helper writes `metrics.jsonl` and `params.json` directly into that directory.
Use the effective parameters after applying defaults and command-line overrides.
The first `params()` call saves them; subsequent calls allow identical values and
reject changes within the same task attempt.

`logger.initial_step` is available immediately after initialization: 0 for empty
history, otherwise the maximum existing step plus one. The base stays fixed for
the logging session. Each `log()` call accepts `step_mode="relative"` (the default)
or `step_mode="global"`:

```python
with MetricsLogger("outputs") as logger:
  # Existing history has maximum step 1000, so initial_step is 1001.
  logger.log(0, {"train/loss": 0.4})  # Writes step 1001.
  logger.log(10, {"train/loss": 0.3})  # Writes step 1011.
  logger.log(1200, {"train/loss": 0.2}, step_mode="global")  # Writes step 1200.
```

Relative steps always use `initial_step + step`, even when the first logged step
is nonzero. Global steps are written unchanged, which is useful when a checkpoint
restores an absolute counter. Relative rows include `local_step`; all helper rows
include `step_mode`. Existing callers that already supply global counters should
pass `step_mode="global"` when using this version of the helper.

To continue history without launching a checkpoint continuation, append complete
events to `metrics.jsonl` before creating the logger. The helper uses their steps
regardless of whether the events were copied from another experiment or prepared
manually. It validates existing history at initialization and preserves its bytes.
Initialization scans at most 128 MiB and rejects malformed or incomplete rows.
A new standalone logger starts a new session using the current history's maximum;
logger instances in a managed task attempt share a saved base.

For `expri runs continue`, use this version of the helper in the task's source.
Expri sets `EXPRI_ATTEMPT` and `EXPRI_STEP_OFFSET` for continued attempts. Managed
rows include `attempt`, and the session base is saved under
`outputs/attempts/<attempt>/step-origin.json` while holding the metrics file lock.
This keeps the mapping stable across logger instances and process restarts within
the same attempt. Global u64 overflow is rejected without changing metric bytes.
An older saved origin that anchored a nonzero first local step keeps its recorded
base. Use global mode for that existing attempt; relative mode rejects the
incompatible origin instead of silently changing its mapping.

Continued attempts save parameters in `outputs/attempts/<attempt>/params.json`,
allowing a new training limit or other changed settings without overwriting the
original parameters. Metric rows still append to the same `metrics.jsonl` file.
The task loads its checkpoint from `EXPRI_RESUME_CHECKPOINT` using its own training
framework. System readings and newly logged metric timestamps retain real
observation times; checkpoint continuation does not automatically fill the
inactive interval.

Each `log()` call appends one complete JSONL row and flushes it before returning.
Rows include `schema_version: 1`, a UTC `timestamp`, `step`, `step_mode`, and `metrics`.
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
