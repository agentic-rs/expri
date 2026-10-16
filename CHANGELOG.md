# Changelog

## 0.1.6 — 2026-10-16

The CLI and the first packaged Python and Rust SDKs are prepared at version
0.1.6. This release adds a prebuilt CLI installable with pip, shared worker
services, resource scheduling, machine monitoring, and continuation of existing
runs. Registry publication remains a separate step; see the
[release checklist](docs/releasing.md).

- **CLI and SDK packages:** install the native CLI with `pip install expri`,
  or build it with Cargo. `sdk/python` packages `expri-sdk`, imported as
  `expri`; `sdk/rust` packages the lightweight `expri-sdk` crate. Both SDKs
  expose initial steps, parameters, scalar metrics, running/waiting status,
  and finalized artifact registration through the same local producer contract.
  Python's dependency-free `expri_metrics` and `expri_run` modules remain available.
- **Shared worker service:** native runs lazily start one service per worker
  for collection, publication, and queue execution. Components can be paused,
  resumed, and restarted independently; saved choices and durable jobs survive
  service restarts. `service publish` queues ongoing publication and returns a
  receipt; `--wait` waits quietly and `--watch` observes progress. Ctrl-C stops
  observation while publication continues. Jobs pin their destination, options,
  and private credential snapshot.
- **Resource scheduling:** `run --queue` freezes source and blocks in server-owned
  `pending` until the assigned worker starts the task. `--queue --detach` returns
  a durable receipt immediately. The server reserves logical CPU slots, memory,
  and exclusive whole GPUs across live workers, with worker/device/model pins.
  Worker budgets default to detected logical CPUs, 90% of RAM, and selectable
  GPUs; `gpus = []` selects CPU-only execution. Omitted or empty `project_ids`
  permits any project allowed by the worker credentials. Live NVIDIA checks
  exclude busy or unverified GPUs; the per-process memory threshold defaults to
  64 MiB. Reports older than 30 seconds block new GPU placement while accepted
  reservations stay held. Durable claims and launch journals prevent duplicate
  launches; uncertain attempts retain their reservations until reconciliation.
- **Run phases and smoke tests:** tasks can switch between `running` and
  `waiting` without releasing resources, changing the start time, or interrupting
  captures. Server-owned `pending` means execution has not started. `run --smoke`
  marks test runs; updated servers automatically archive finished published
  smoke runs, including failures and cancellations, with 15-day retention.
  Worker files remain available.
- **System monitoring:** a dedicated dashboard page and CLI browse machine
  inventory plus CPU, memory, GPU, disk, and network history. Detailed machine
  history lasts 72 hours, with hourly summaries retained indefinitely. Run
  captures align exact readings to each run's period and retain them for the
  run's lifetime. Readings describe the whole machine, including for overlapping
  runs, rather than attributed per-process usage.
- **Dashboard and run annotations:** a redesigned workspace keeps full run
  history beside inspection and comparison, with compact filters, run-specific
  errors, task names, and live durations. Source selection moves to the header;
  refresh controls move to the status bar. Hosted runs support editable nicknames
  and multiple tags through the dashboard and CLI, preserved across publication.
  Chart responses use bounded JSON while retaining exact coordinates.
- **Live supervisor upgrades:** capable native Linux detached runs can replace
  the supervisor executable while retaining the same task process, run ID, logs,
  metrics, and artifacts. Guarded requests record reasons and generations;
  failed target checks leave training running.
- **Checkpoint continuation:** direct detached runs on native Linux can continue
  from a finalized checkpoint with updated source or a longer training limit.
  The run keeps its ID and metric/log history; separate attempts record source,
  command, checkpoint digest, timestamps, and outcome. Running, waiting,
  completed, failed, and cancelled runs are supported. Queued runs remain outside
  this operation because their allocations belong to the server scheduler.
- **Metric step modes:** SDK initialization exposes a fixed `initial_step` of
  `MAX(existing step) + 1`, or zero for empty history. Logging defaults to
  `initial_step + step`; explicit global mode preserves the supplied step.
  Existing valid JSONL history can be appended before initialization without
  creating a checkpoint or execution attempt. Managed attempts share a durable
  step origin; each standalone initialization begins a new logging session.
- **CLI and architecture:** server login saves revocable API credentials;
  query commands inspect published runs directly. Reports use tables by default,
  with `--json` for scripts. Progressive `expri skill` guides work offline. The
  executable is split into eight internal subsystem crates, packaged with exact
  aligned versions so source installation remains supported.

### Migration

- **Upgrade the server first.** Back up its entire data directory before opening
  SQLite schema 10. Older binaries refuse the upgraded directory; downgrading
  requires restoring the backup. Updated archive support accepts all nine
  tracking files, including system captures. Then upgrade workstation and worker
  binaries together. Existing S3 runs remain readable.
- **Structured CLI output is explicit.** Add `--json` to scripts consuming query,
  configuration, login, service, system, asset, artifact, transfer, or detached
  start reports. Use `run --detach --json TASK` for a start receipt. Worker
  protocols and foreground task streams retain their existing formats.
- **Review metric counters when updating helpers or adopting an SDK.** Default
  relative mode now adds the fixed initial step in all logging sessions. A task
  restoring an already-global checkpoint counter must pass Python's
  `step_mode="global"` or Rust's `StepMode::Global`. Relative steps are zero-based:
  with history through 1000, supplied steps 0 and 10 become 1001 and 1011.
  Legacy persisted origins with a nonzero first-local-step anchor reject new
  relative logging rather than silently changing coordinates; global mode works.
- **Use the shared service controls.** Stop an older standalone collector after
  upgrading a worker, then use `expri service start` for background operation or
  `service start --foreground` for a terminal process. `system daemon` remains a
  deprecated foreground alias. `service status` is read-only; `service stop`
  preserves queued jobs and leaves training alive. Saved jobs recover on the
  next service start after a reboot; training itself does not restart.
- **Treat publication as a durable job.** `service publish` returns after local
  registration. Add `--wait` or `--watch` to observe it through completion.
  Shared service restarts leave existing run supervisors on their original
  executable; explicitly request a supported live supervisor upgrade when needed.

### Installation and update

After publication, install the prebuilt native CLI without a Rust compiler on
supported wheel platforms:

```sh
python -m pip install --upgrade expri==0.1.6
expri --version
```

Install task instrumentation separately:

```sh
python -m pip install expri-sdk==0.1.6
# Or add the Rust SDK to an experiment:
cargo add expri-sdk@0.1.6
```

The `expri` CLI distribution and `expri-sdk` Python distribution can coexist;
only `expri-sdk` provides the `import expri` instrumentation API. Source builds
require Rust 1.89 or newer:

```sh
cargo install expri --version 0.1.6 --locked --force
```

[Changes since v0.1.5](https://github.com/agentic-rs/expri/compare/v0.1.5...v0.1.6)

## 0.1.5

Expri now covers the experiment workflow from preparing a worker and starting a
run to reviewing metrics and retrieving selected results.

- **Managed runs and environments:** run locally or over SSH/ctl with uv,
  optionally reusing compatible Conda/system packages such as PyTorch. Each
  configured-environment run has a frozen code snapshot, environment records,
  outputs and logs. Detached runs survive SSH disconnects and support status,
  log following and cancellation; `env doctor` and `env prune` help maintain
  worker environments.
- **Metrics and dashboards:** a standard-library Python logger records
  parameters and timestamped metrics. Compare runs through the CLI, offline
  HTML charts, the local dashboard or a password-protected hosted dashboard.
  The React/TSX UI adds hover inspection, zoom, run visibility, step/elapsed/date
  axes, local/UTC time display, configurable columns and automatic refresh.
- **Optional self-hosted storage:** publish tracking independently of training
  to durable server files and incremental SQLite projections. S3-compatible
  storage holds checkpoints, private inputs and server-produced `result.zip`
  bundles. Hosted project views compare runs across machines; main and branch
  dashboard previews can share one backend.
- **Live checkpoint transfers:** explicitly register finalized outputs for
  background upload, with `best`/`latest` labels. `fetch --watch` continuously
  receives metadata and selected checkpoints. Durable transfer state, verified
  downloads and offline caches support interruptions and later retries; the
  dashboard also offers authenticated browser downloads.
- **Reusable assets:** Git-tracked `.expri.toml` sidecars describe named files
  from public URLs, Hugging Face or registered private inputs. Import, download,
  update and inspect assets with pinned sizes/digests, an immutable content
  cache and automatic preparation inside run snapshots.
- **Storage management and retention:** browse private inputs and run outputs,
  inspect project S3/local storage usage, and reference existing objects without
  duplicating bytes. Project deletion uses durable cleanup retries. Finished
  hosted runs can be archived and restored for 15 days before hosted cleanup,
  preserving worker/laptop copies, private inputs and shared objects.

### Migration

- **If upgrading from published v0.1.4:** replace `expri sync` with `expri push`,
  `[sync]` with `[push]`, `run --no-sync` with `run --no-push`, and `node sync-apply` with
  `node push-apply`. Update both project and private target files containing
  `[sync]`. Source pushing and result fetching are separate actions;
  `sync --pull` is removed. Use `download`
  for configured worker mappings, `runs fetch` for a worker run, or `fetch` for
  service results. See the
  [transfer directions](docs/self-hosted-service.md).
- **If upgrading from unreleased development builds:** replace `service.inputs`
  and `EXPRI_INPUT_DIR` with `expri assets import <source> <relative-path>`.
  Commit the adjacent sidecar
  and ignore rules; training reads the ordinary relative path. Replace
  `runs pull` with `runs fetch` and `[file_sync]` with `[fetch]`. The old service
  verbs `push`, `pull`, `archive` and `file-put` become `publish`, `fetch`,
  `upload` and `file-upload`; `service input put`/`get` become
  `service input upload`/`download`. See [managed assets](docs/assets.md).
  If running a development self-hosted service, back up its entire data directory
  before upgrading to SQLite schema 4; a backend downgrade requires restoring
  that backup. See the [service upgrade guidance](docs/self-hosted-service.md#service-configuration).
- Upgrade the workstation and workers together. Detached runs, automatic
  publishing and asset preparation require a configured environment. Automatic
  publishing and assets require a native worker advertising `run-publishing-v1`
  and `assets-v1` respectively. Supported operations, including detached runs,
  retain automatic Python fallback when `protocol = "auto"`.

### Installation and update

Building requires Rust 1.89 or newer. After this version is published:

```sh
cargo install expri --version 0.1.5 --locked
# Replace an existing installation explicitly:
cargo install expri --version 0.1.5 --locked --force
```

The same `expri` executable serves workstation, worker and hosted service roles.
Detached training and its publisher survive disconnects but do not automatically
restart after a machine reboot.

[Changes since v0.1.4](https://github.com/agentic-rs/expri/compare/v0.1.4...v0.1.5)
