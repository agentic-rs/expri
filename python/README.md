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
reject changes, including when resuming an existing run.

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
