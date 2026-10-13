# Changelog

## Unreleased

- `run --queue` freezes source and submits managed tasks to a shared server queue.
  Foreground `queue daemon` workers reserve logical CPU slots, memory, and whole
  GPUs from explicit budgets, with automatic placement or worker/device pins.
  `queue list`, `show`, `workers`, and `cancel` report tables or `--json`.
  Durable claims and launch journals prevent duplicate execution on retries;
  uncertain attempts retain reservations until reconciliation or confirmed recovery.
  Back up the service data directory before upgrading to SQLite schema 8.
- Native runs now start one shared worker service lazily for collection and run
  publication. `service start` runs in the background or reuses it;
  `--foreground` runs in the terminal. `service status` is read-only, and
  `service stop` preserves queued jobs and leaves training alive. Saved jobs
  recover on the next start after a reboot. `system daemon` is a deprecated
  foreground alias. Stop an older standalone collector before starting the new
  service after upgrading the worker binary.
- `service publish` now queues ongoing publication and returns a durable
  receipt. Use `--wait` to wait quietly or `--watch` to observe progress; Ctrl-C
  stops observation while publication continues. Jobs pin their destination,
  options and private credential snapshot; conflicting registration is rejected.
- Server archives now accept all nine supported tracking files, including run
  system captures. Deploy the updated server before upgrading workers that
  automatically capture system metrics.

- `run --smoke` marks managed runs and generates `smoke-run-…` IDs. An updated
  server automatically archives finished published smoke runs with the existing
  15-day retention, including failed and cancelled runs; worker files remain
  available. Local and hosted dashboards show a Smoke badge.

- User-facing query, configuration, login, service, system, asset, artifact,
  transfer, and detached-start reports now use tables by default. Scripts must
  request `--json` to retain structured output. Use `run --detach --json TASK`
  for a machine-readable start receipt. Worker protocols and foreground task
  streams retain their existing formats.

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
