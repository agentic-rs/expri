# Expri Rust SDK

The `expri-sdk` crate instruments Unix training tasks. It writes metrics and
parameters locally and hands finalized artifacts to the existing worker
service. It does not start an uploader, request credentials, or own task
completion.

Version 0.1.6 aligns with the CLI and Python SDK. After publication, add the
released crate to your experiment:

```toml
[dependencies]
expri-sdk = "0.1.6"
serde_json = "1"
```

For development before publication, replace the dependency with
`expri-sdk = { path = "/path/to/expri/sdk/rust" }`.

```rust,no_run
use expri_sdk::{StepMode, TaskStatus};
use serde_json::json;

let run = expri_sdk::init()?;
let initial_step = run.initial_step();
run.params(&json!({"learning_rate": 0.001, "seed": 42}))?;
run.log(0, [("train/loss", 0.5)])?;
run.log(10, [("train/loss", 0.4)])?;
run.log_with_mode(1200, [("validation/loss", 0.35)], StepMode::Global)?;
run.set_status(TaskStatus::Waiting)?;
run.set_status(TaskStatus::Running)?;
run.register_artifact("checkpoint-1200.pt", ["best", "latest"])?;
run.close()?;
# Ok::<(), expri_sdk::Error>(())
```

`init()` defaults to `EXPRI_OUTPUT_DIR`. For a standalone process, use
`Run::builder().output_dir("outputs").init()?`. For an explicit managed run,
use `Run::builder().run_dir(".expri/runs/run-id").init()?`; the output directory
is that run's `outputs/`. An explicit output directory cannot conflict with an
explicit run directory.

An explicit `run_dir` outside the task's managed environment selects that run
for status and artifact operations. Metrics still start a standalone logging
session at `MAX(step) + 1`; this does not reconstruct an execution attempt from
the saved run state. Initialize inside the running task to share its managed
attempt context.

The session's `initial_step()` is available immediately. Empty history starts
at zero; otherwise it is the maximum recorded step plus one. Relative logging
adds the supplied zero-based step to that fixed base, preserving gaps. With
history through 1000, `log(0, ...)` records 1001 and `log(10, ...)` records 1011.
Use `StepMode::Global` when the training loop already supplies global steps.
It preserves the supplied step exactly. Copying or appending valid history to
`metrics.jsonl` before initialization needs no checkpoint or execution attempt.

Cooperating Python and Rust SDK writers share Unix file locks and managed
attempt step origins. A standalone logger creates a new session at each
initialization; a managed attempt reuses its durable base. Legacy origins with
a nonzero first-local-step anchor accept global logging and reject relative
logging, avoiding a silent coordinate change.

Metric names must be nonempty, at most 256 UTF-8 bytes, and contain no control
characters. Values support signed and unsigned integers and finite floats;
booleans are excluded. Parameters accept a JSON object and remain immutable
within an attempt; repeated identical values are allowed. Continued attempts
store their own `params.json` under `outputs/attempts/N/`.

Status and artifact methods require the local expri CLI: the builder captures
`EXPRI_BIN` (or `expri` on PATH), with `cli_binary(...)` available for an explicit
binary. Commands receive the captured absolute `--run-dir`, so later environment
changes do not redirect them to another run. Relative artifact paths start at
the output directory. Finalize and close the file first; CLI validation checks
the path and hands it to the independent publisher. The methods return the CLI's
JSON report and propagate failures. Standalone sessions support metrics and
parameters; status and artifact operations require a managed run directory.

Dropping or closing a `Run` only releases local SDK resources. Each metric event
is visible before `log` returns; `flush()` additionally syncs metric bytes to
local storage without waiting for publication. Initialize independently after forking; use
rank zero for distributed training metrics. This crate is prepared for version
0.1.6; automated SDK publication is not yet enabled. See the
[release checklist](../../docs/releasing.md) before publishing.
