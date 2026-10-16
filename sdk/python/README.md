# Expri Python SDK

The `expri-sdk` distribution imports as `expri`. It supports Python 3.10 or newer
on Linux and macOS, with no runtime dependencies. Version 0.1.6 aligns with the
CLI and Rust SDK. After publication, install it into the experiment environment:

```sh
python -m pip install expri-sdk==0.1.6
```

For development, use `python -m pip install ./sdk/python` from this repository.
The separate `expri` distribution provides the native CLI, available as
`python -m pip install expri==0.1.6` after publication. It can be installed
alongside the SDK when artifact registration needs a local CLI.
Automated PyPI publication is not yet enabled; see the
[release checklist](../../docs/releasing.md) before publishing these packages.

```python
import expri

with expri.init() as run:
  initial_step = run.initial_step
  run.params({"learning_rate": 0.001, "seed": 42})
  for step in range(100):
    loss = train_step()
    run.log(step, {"train/loss": loss})
  run.set_status("waiting")
  checkpoint = save_checkpoint()  # Finish and close the file before registering.
  run.register_artifact(checkpoint, labels=["latest"])
  run.set_status("running")
```

`init()` attaches instrumentation to the current task's `EXPRI_OUTPUT_DIR` and
`EXPRI_RUN_DIR`; it does not start a task or a daemon. Outside a managed task,
use `expri.init(output_dir="outputs")` for metrics and parameters. To attach to an
existing managed run's status and outbox, pass `run_dir`; its `outputs` directory
is the default. `run_dir` does not reconstruct the recorded task attempt or its
step origin. Without the task's inherited continuation environment, metrics use
a standalone session starting at the existing maximum step plus one.
An independent explicit output directory cannot change the containing task's
status or register artifacts for it. An explicit `run_dir` and `output_dir` must
refer to the same run.

`initial_step` is zero for empty history, otherwise the maximum existing step
plus one. It stays fixed for the session. `run.log(10, values)` writes
`initial_step + 10`; `run.log(1200, values, step_mode="global")` writes exactly
1200. Append complete historical events before initialization to continue their
history. Managed task attempts share a persisted base, including across logger
instances. Parameters are immutable within an attempt. Checkpoint continuation
keeps the same metric history and stores new attempt parameters separately.

`set_status("running" | "waiting")` uses the existing local state protocol and
requires a live supervisor. It works without a native binary. Artifact paths
are absolute or relative to this run's output directory, such as
`"checkpoint-1000.pt"`. Artifact registration invokes `EXPRI_BIN` (falling back
to `expri` on `PATH`) with an argument array. It returns the CLI's JSON report
after the local handoff; it does not wait for upload. Labels accept `best` and
`latest`. Keep registered files immutable while the shared worker service uploads
them. The SDK itself has no network client or credentials.

Every metric event is flushed locally before `log` returns. `flush()` does not
wait for publishing. `close()` and context exit release local resources;
the supervisor continues to own task completion and cancellation. A handle must
not be reused after `fork`. For distributed training, log from rank zero.

Existing scripts can continue using `from expri_metrics import MetricsLogger`
and `from expri_run import set_run_status` after installation, or copy those
same standalone source files without installing a package. They are the canonical
implementations used by the SDK. See [STANDALONE.md](STANDALONE.md) for file
formats, validation, concurrency, continuation, and compatibility details.

Run the SDK and standalone suites from this directory:

```sh
PYTHONDONTWRITEBYTECODE=1 python -m unittest discover -s . -p '*_tests.py'
python -m pip wheel --no-deps --wheel-dir dist .
PYTHONDONTWRITEBYTECODE=1 python tests/installed_wheel.py dist/expri_sdk-0.1.6-py3-none-any.whl
```

The wheel smoke test installs the wheel into a temporary virtual environment
without accessing a package index, then runs from outside the source tree.
