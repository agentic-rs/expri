# Language SDKs

Each language package lives in `sdk/<lang>` and instruments tasks through the
same local run files and artifact outbox. The existing worker service handles
publishing, retries, and system monitoring. SDK initialization attaches local
instrumentation; the supervisor owns task launch, completion, and cancellation.

| Directory | Package | Import | Requirements |
| --- | --- | --- | --- |
| [python](python/README.md) | `expri-sdk` | `import expri` | Python 3.10+, Linux/macOS |
| [rust](rust/README.md) | `expri-sdk` | `expri_sdk` | Rust 1.89+, Linux/macOS |
| [contract](contract/README.md) | Shared producer contract and interoperability tests | | |

These packages start at version 0.1.0, independently of the CLI. Install the
initial packages from source or locally built distributions; adding them here
does not publish them to PyPI or crates.io.

From this repository, install the Python package into your experiment environment:

```sh
uv pip install ./sdk/python
```

```python
import expri

with expri.init() as run:
  initial_step = run.initial_step
  run.params({"learning_rate": 0.001})
  run.log(0, {"train/loss": 0.42})
  run.log(1200, {"train/loss": 0.3}, step_mode="global")
  run.register_artifact("checkpoint.pt", labels=["latest"])
```

The task must finish and close a checkpoint before registering it. Relative
artifact paths are rooted in the run's output directory. Registration returns
after local handoff, independently of cloud availability.

Add the Rust package to an experiment using its source path:

```sh
cargo add expri-sdk --path /path/to/expri/sdk/rust
```

```rust
let run = expri_sdk::init()?;
let initial_step = run.initial_step();
run.log(0, [("train/loss", 0.42)])?;
run.log_with_mode(1200, [("train/loss", 0.3)], expri_sdk::StepMode::Global)?;
```

Both SDKs expose parameters, relative/global scalar logging, running/waiting
status, and finalized artifact registration. Inside a managed task they inherit
its output directory and attempt context. Standalone callers select an output
directory explicitly. Closing an SDK handle only releases its local resources.

The Python package also includes the dependency-free `expri_metrics.py` and
`expri_run.py` modules for existing imports and copy-in instrumentation. They are
the canonical sources; regenerate the worker crate's embedded copies after
editing them:

```sh
python3 scripts/sync_python_sdk.py
python3 scripts/sync_python_sdk.py --check
```

Generated copies keep the published worker crate self-contained. CI checks that
they match and tests both installed Python distributions and the packaged Rust
crate. The existing CLI tag release workflow remains separate from SDK releases.
