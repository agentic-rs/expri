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

The CLI and both SDK packages are prepared at version 0.1.6. The `expri`
PyPI distribution is the prebuilt CLI; `expri-sdk` is the Python instrumentation
API imported as `expri`. Install either or both as needed after publication.

After the release is published, install the Python SDK into your experiment
environment:

```sh
python -m pip install expri-sdk==0.1.6
# Optional native CLI for artifact registration:
python -m pip install expri==0.1.6
```

For development before publication, use `python -m pip install ./sdk/python`.

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

Add the released Rust package to an experiment:

```sh
cargo add expri-sdk@0.1.6
```

For local development, use `cargo add expri-sdk --path /path/to/expri/sdk/rust`.

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
crate. Distribution preparation builds and checks packages without publishing
them. SDK and PyPI publication is not yet enabled; see the
[release checklist](../docs/releasing.md) before activating a coordinated release.
