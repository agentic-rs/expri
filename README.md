# expri

`expri` is a repo-local experiment workflow tool. It pushes local code to a
target, prepares Python dependencies with uv, and runs configured tasks locally
or remotely.

Building expri requires Rust 1.89 or newer.

## Install

Install or update the released CLI with Cargo:

```sh
cargo install expri --version 0.1.5 --locked --force
expri --version
```

See the [release notes](CHANGELOG.md) for features and migration instructions.
Install the same version on native workers when upgrading their capabilities.

## Agent guidance

`expri skill` prints a short agent skill with essential behavior and a topic
index. Load deeper instructions only for the workflow at hand:

```sh
expri skill
expri skill run
expri skill assets
expri skill query
expri skill login
expri skill --help
```

User-facing reports print readable tables by default. Add `--json` for scripts
and agents; use `--quiet` to suppress a human report. For a detached launch, put
`--detach --json` before the task name to capture its start receipt.

Topics cover configuration, server login, source pushing, task launch, run inspection,
metrics, artifacts, assets, environments, server queries, service publishing/fetching,
and machine system profiles.
The Markdown is embedded in the CLI, works offline without project
configuration, and does not install files.

## Machine system profiles

Record CPU/GPU, memory, disk, and network observations independently of
experiment runs:

```sh
expri system sample
expri service start
expri service status
expri system status
expri system history --resolution hourly
```

New machine configuration and profile history live in `~/.tokn/expri`.
Native runs start one shared worker service lazily and reuse it across runs.
It samples every ten seconds and stays running until `expri service stop` or a
worker restart. Use `expri service start --foreground` to run it in a terminal.
Optional machine-scoped
publishing runs independently, so server outages do not delay collection.
Both local and server stores keep 72 hours of machine detail and retain historical
hourly summaries indefinitely. New runs also preserve their exact system observations
for as long as the run is retained. Existing project configuration, caches, and run
storage keep their current paths. See [system profiles](docs/system-profiles.md)
for GPU support, service authentication, publishing, and history queries.

## Push source and config

Source and worker commands such as `push`, `setup`, and `-T ... run` operate
on a configured target from your workstation. `fetch` reads results from the
service instead. `expri node ...` runs commands locally on a worker.

Create an `expri.toml` in the repo you want to push, and keep machine targets
in a sibling target file. The target filename follows the config filename:
`expri.toml` uses `expri.target.toml`, and `cs336.toml` uses
`cs336.target.toml`. Target files are local/private; add them to that repo's
`.gitignore`.

```toml
# expri.toml
[project]
name = "my-project"

[push]
remote_managed = ["uv.lock"]

[download.mappings]
wandb = "wandb"

[tasks]
dev = ["pnpm", "dev"]
train = { command = ["python", "scripts/train.py"], uv = true }
```

```toml
# expri.target.toml
[target.runpod]
host = "user@example.com"
remote_dir = "~/my-project"
protocol = "auto"
node_bin = "expri"
```

Then run:

```sh
expri -T runpod push --config cs336-assignment5-alignment/expri.toml
```

Or from inside that repo:

```sh
expri -T runpod push
```

See `examples/cs336.toml` for a CS336-shaped starting point.

Targets default to `protocol = "auto"`, which tries `expri node push-apply`
first and falls back to the Python source push protocol. Set `protocol = "expri-node"`
to require the node binary, or `protocol = "python"` for the fallback path.
`protocol = "ssh"` remains an alias for `"python"`. The protocol chooses how
source changes are applied on the target; the transport chooses how commands and files
reach it.

For configured environments, `auto` also checks whether the installed node
supports environment preparation and isolated runs. An older node falls back
to the Python protocol. `protocol = "expri-node"` requires a node with those
capabilities and reports an error when an upgrade is needed.
The `env doctor` and `env prune` commands also check for maintenance support
and use the same automatic fallback for older nodes.

The source push uploads committed history with a git bundle, stages `HEAD`
plus a zip archive of local dirty and untracked files, then installs the staged
files on the remote. It removes previously pushed files absent from the staged
tree and preserves unrelated remote-generated files. Remote tool state lives
under `.expri/`.

Use `push.remote_managed` for repo-relative files that the target should own,
even if they are tracked by Git or appear in the dirty patch. For example,
`remote_managed = ["uv.lock"]` preserves the target's lockfile across pushes and
excludes local changes to that file from `patch.zip`.

For a path-scoped rsync, pass paths after `--`. Only files returned by
`git ls-files` under those paths are transferred:

```sh
expri -T runpod push -- src scripts
```

`expri -T runpod push --watch` keeps the worker checkout up to date while active
runs retain their frozen snapshots. Separately, configure `[fetch]` and run
`expri fetch --watch` on the laptop to receive live run metadata and selected
finalized checkpoints through the service. Training registers ready
files with `expri artifact register "$EXPRI_OUTPUT_DIR/checkpoint-1000.pt" --label best`.
See [managed assets](docs/assets.md) for public URL, Hugging Face, and private
input sidecars, and [result fetching](docs/self-hosted-service.md#local-download-and-dashboard)
for laptop selection, offline reuse, and resumable transfers.

## Managed assets

Track large files with small `<path>.expri.toml` sidecars and keep the file bytes
out of Git. `assets add` accepts an existing local file or a public URL/Hugging
Face named file. `assets import` makes a hosted run output reusable without
downloading or copying its S3 object. `assets upload` makes a local asset
available to other workers; `download`, `update`, and `status` reproduce, refresh,
and inspect the tracked version.

```sh
expri assets add data/train.bin
expri assets upload data/train.bin
expri assets add https://example.net/model.bin models/base.bin
expri assets add expri://./inputs/dataset-v1 data/remote.bin --no-download
expri assets import expri://./worker-1/run-abc123/outputs/best.pt models/best.pt
expri assets download
expri assets status
expri assets update data/train.bin
```

`--no-download` requires a known SHA256 and size from metadata or explicit
arguments. Private references use repository-relative IDs; configure the service
URL, project ID, and token environment variable in ignored `.expri/config.toml`.
Sidecars retain content identity and optional producing-run provenance. Native
workers require `assets-v2`; verified cached assets remain usable offline.
See the [managed assets guide](docs/assets.md) for configuration and examples.

## Transport

Targets use SSH by default. Set `transport = "ctl"` on an individual target to
use [ctl](https://github.com/tokn-ai/ctl):

```toml
# expri.target.toml
[target.work]
transport = "ctl"
host = "work"
remote_dir = "~/my-project"
protocol = "auto"
# ctl_bin = "/path/to/ctl/target/debug/ctl"
# ctl_method = "vpn"
```

`host` selects a saved ctl host name or ID, or an SSH alias/destination.
Use the saved host ID if its name contains `:` or `/`, since rsync uses these
characters to parse destinations. For IPv6, configure the address in a saved ctl
host or SSH alias, then use its name, ID, or alias here.
`ctl_bin` defaults to `ctl` on your `PATH`; set it to a binary built from a local
ctl checkout if needed. `ctl_method` optionally selects a connection method
name or ID; otherwise ctl uses the saved host's preferred method.

Remote commands run through `ctl ssh`, and transfers retain rsync with
`rsync -e 'ctl ssh'` (including the configured binary and method). Expri does
not need `ctl-agent` for this transport. Rsync must be installed locally and on
the target. The target still needs a Unix shell and the usual tools required by
the selected source push protocol and commands.

The `[ssh]` control settings apply only to the SSH transport. With ctl, ctl
manages connections and reuse. A target's explicit `port` is passed as `-p`;
ctl treats that as a connection override and skips broker connection reuse.
Prefer a saved host with its account configured in ctl. With GNU rsync, a
`user@host` destination also overrides the account and skips broker reuse.

## Task

`expri run <name>` runs a configured command alias:

```toml
[tasks]
dev = ["pnpm", "dev"]
train = { command = ["python", "scripts/train.py"], uv = true }
```

Without an environment configuration, the array form runs exactly that command
in the repo root. The object form supports `uv = true`, which prefixes the
command with `uv run`. Existing task definitions keep this behavior.

With an `[environment]` or target environment configured, every task uses uv
and runs from its own code snapshot. Expri stores the snapshot, run environment,
and output directory under `.expri/runs/<run_id>/`. A later source push cannot change
the copied code files of an active run. Base packages remain shared when reuse
is enabled, and uv's cache avoids repeated downloads when it creates each run's
environment.

Run environments are retained under `.expri/runs/`. Expri keeps a shared uv
cache in the original checkout's `.expri/cache/uv`, so each new code snapshot
uses the same cached packages. An explicit setup `--cache-dir` is resolved from
the original checkout and remains stable across runs; otherwise an ambient
`UV_CACHE_DIR` override takes precedence over the project default. Environments,
snapshots, and outputs still accumulate; use `expri env prune` to review old run
environments for removal.

Local snapshots include explicit file paths listed in `push.include_ignored`,
such as a Git-ignored experiment configuration, alongside the usual tracked,
dirty, and untracked source files. Other Git-ignored files stay excluded.

```sh
expri run dev
expri run train --epochs 3
expri -T runpod run train --epochs 3
expri -T runpod run --no-push train --epochs 3
expri -T runpod run --detach train --epochs 3
```

When `-T/--target` is provided, `run` pushes the repo to the target before
executing the task there. Pass `--no-push` before the task name to skip that
source push. Run options must appear before the task name; arguments after the task
name are passed through to the task.

An optional `[service]` or `[target.<name>.service]` starts a separate publisher
for each recorded run. It forwards metadata, metrics, and logs through a durable
queue while training continues independently. `expri runs status <run_id>` shows
both task and publishing status; `--no-publish` disables it for one run.
See [automatic publishing and worker credentials](docs/self-hosted-service.md#worker-uploads)
for configuration, dashboard links, and resuming a stopped publisher.

Use `expri run --smoke train --epochs 1` for a smoke test. This requires a
configured environment and records an explicit smoke mark with a generated
`smoke-run-…` ID. It also works with `--detach`. Once the finished run state
reaches an updated server, the server automatically archives the run, including
failed or cancelled runs, using the existing 15-day retention period. Local and
worker run files remain available. The dashboard shows a **Smoke** badge.

### Shared resource queue

With an updated server and an explicitly configured execution worker, queue a
managed task instead of choosing when to start it:

```sh
expri run --queue --project-id vision --cpus 4 --memory 16GiB --gpus 1 train --epochs 3
expri queue list
expri queue show job-abc123 --json
expri queue cancel job-abc123
expri queue workers
```

Submission freezes and uploads the source before returning a job ID, so the
laptop can disconnect. Workers report their budget and heartbeat to the server,
which assigns fitting jobs across online workers and reserves their resources.
CPU-only jobs prefer CPU-only workers. CPU counts are logical slots and memory is a reservation
among queued jobs; whole GPUs are assigned exclusively among these jobs.
`--gpu-id` selects exact devices, while `--gpu-model` and `--gpu-memory` constrain
automatic selection. `--worker` pins placement. Options belong before the task
name, and `--json` selects a structured receipt or report.

Defaults can live alongside a task's command:

```toml
[tasks.train]
command = ["python", "train.py"]

[tasks.train.resources]
cpus = 4
memory = "16GiB"
gpus = 1
gpu_model = "A100"
```

These defaults apply to `run --queue`. Memory must be supplied in the task or
CLI; CPU defaults to one slot and GPUs to zero. Each worker needs a local
`[queue_worker]` declaration and the shared `expri service start` runtime; use
`--foreground` to run it in a terminal.
Capacity defaults to detected logical CPUs, 90% of RAM, and selectable whole GPUs.
Optional capacity fields override these defaults; `gpus = []` disables GPU allocation.
`queue workers` reports available/total resources and pending assignments. Read
[worker setup and recovery](docs/self-hosted-service.md#shared-resource-queue)
or `expri skill queue`. The job receives a run ID only after dispatch; published
runs then appear in the existing dashboard.

Each configured-environment run receives these variables:

| Variable | Meaning |
| --- | --- |
| `EXPRI_RUN_ID` | The run's identifier |
| `EXPRI_RUN_DIR` | Absolute path to the run directory |
| `EXPRI_OUTPUT_DIR` | Absolute path to the run's output directory |

The task's working directory is its code snapshot. Relative output paths such
as `outputs/checkpoint.pt` therefore live under the snapshot. Have the task
write to `EXPRI_OUTPUT_DIR` when you want all outputs in the dedicated output
directory. The existing `download` mappings still resolve from the target repo
root. Use `runs fetch` for files stored within an individual run.

Configured-environment runs save task stdout and stderr to
`.expri/runs/<run_id>/logs/stdout.log` and `logs/stderr.log` while streaming both
to the terminal. Environment-preparation diagnostics also go to the stderr log;
the helper's private JSON response stays out of task stdout. Run records include
the command, timestamps, status, exit code, and log paths. Older run directories
remain readable when optional records or logs are missing.

Execution stays in the foreground by default. On Unix, `run --detach <task>`
starts a separate supervisor session and prints a start table; add `--json`
before the task name for a structured receipt containing the
run ID and directory after the supervisor owns the run and can accept requests.
Environment preparation then continues in the background; inspect status and
stderr for its progress. The run survives a terminal or SSH disconnect. Detach
requires a configured environment, including an empty `[environment]` table
for ordinary uv isolation; legacy tasks are rejected before a remote source push.

Detached runs use the same snapshot, environment, outputs, and log layout.
They do not restart after a machine reboot or a lost supervisor. `runs status`
reports `lost` when a detached run has an active recorded status but no live
supervisor. Foreground runs retain their recorded status when their runner is
stopped. If a descendant keeps a pipe open after the child exits, logging stops
waiting after a one-second grace period. This and other capture failures are
recorded as `logging_error`, while `task_exit_code` preserves the task's exit
status. Buffered output from closed pipes is drained completely.

Check a detached run, follow its saved logs, or request cancellation:

```sh
expri -T runpod run --detach train --epochs 3
expri -T runpod runs status run-abc123 --json
expri -T runpod runs logs run-abc123 --tail 100 --follow
expri -T runpod runs logs run-abc123 --stream stderr --tail 50
expri -T runpod runs cancel run-abc123
```

These commands also work locally without `-T`. `logs` emits the selected log's
raw bytes, defaults to stdout and the last 100 lines, and stops following once
the run finishes or its supervisor is lost. `--tail 0 --follow` reads only new
output. Interrupting or disconnecting a log reader leaves the run running.
Cancellation requests are asynchronous: the supervisor sends a graceful
termination signal to its current preparation or task process group, then
escalates after two seconds if needed. Inspect status again to confirm
`cancelled`. Cancelling an already finished run leaves its result intact;
active foreground or lost runs cannot be cancelled through this command.

## Run records and selective fetching

List local runs or inspect the configured target directly:

```sh
expri runs list
expri -T runpod runs list --task train --status failed --limit 5
expri -T runpod runs show run-abc123 --json
```

`list` defaults to the 20 newest records. Its status filters are `preparing`,
`running`, `completed`, `failed`, `cancelled`, and `unknown`. `show` includes the run state,
source snapshot provenance, and recorded Python environment when available.
Missing, malformed, or unsupported records produce warnings; incomplete runs
can still be inspected. These commands read fixed records rather than scanning
an environment's installed packages.

Fetch a target run's available metadata and logs into
`results/<target>/runs/<run_id>/`:

```sh
expri -T runpod runs fetch run-abc123
expri -T runpod runs fetch run-abc123 --metrics
expri -T runpod runs fetch run-abc123 --outputs --dry-run
expri -T runpod runs fetch run-abc123 --artifact 'outputs/model one.pt'
expri -T runpod runs fetch run-abc123 --artifact code/out/jobs
```

The default selection contains `run-state.json`, `snapshot.json`, the
environment-state manifest, and regular files under `logs/`. Outputs and files
under the code snapshot are opt-in: `--outputs` selects the dedicated output
directory, and repeatable `--artifact` selects a file or directory beneath
`outputs/` or `code/`. Expri excludes environments, caches, and symlinked files
or directories from these transfers. Explicit selections into excluded paths
fail. The configured `[download].results_dir` replaces the default `results`
directory.

A fetch stages the selected files before updating its owned destination and
writes `pull-state.json` after publication. Repeating a metadata-only fetch
refreshes the records and logs while retaining artifacts downloaded earlier.
A failed transfer leaves the previous cache intact. Missing optional metadata
is skipped, so partial records can still be fetched.

`runs fetch --dry-run` contacts the target to query the actual selection, then
prints the destination and files without writing locally or transferring them.
Use cached inspection to review previously fetched records without contacting
the target:

```sh
expri -T runpod runs list --cached
expri -T runpod runs show run-abc123 --cached --json
```

With `protocol = "auto"`, run queries and logging use capability checks and
fall back to Python on older nodes. Requiring `protocol = "expri-node"` reports
an upgrade error when the installed node lacks the requested capability.

## Metrics and comparison charts

Copy [expri_metrics.py](crates/worker/python/expri_metrics.py) into your experiment's
source directory. The helper uses the Python standard library and works in the
run's uv environment. Log effective parameters after applying configuration
defaults and command-line overrides, then record scalar metrics during training:

```python
from expri_metrics import MetricsLogger

with MetricsLogger() as logger:
  logger.params({"learning_rate": 0.001, "batch_size": 64, "seed": 42})
  for step in range(100):
    loss, accuracy = train_step()
    logger.log(step, {"train/loss": loss, "train/accuracy": accuracy})
```

By default, the helper writes `metrics.jsonl` and optional `params.json` into
`EXPRI_OUTPUT_DIR`: `.expri/runs/<run_id>/outputs/`. Outside a managed run, pass
an explicit directory to `MetricsLogger("outputs")`. Parameters are immutable:
repeating identical values is allowed, while changing saved parameters fails.
Each event is flushed before `log()` returns. For distributed training, log from
rank 0 and create the logger within that process. See [Python metrics guide](crates/worker/python/README.md)
for the complete producer contract.

Inspect one run or compare runs locally:

```sh
expri runs metrics run-abc123
expri runs metrics run-abc123 --metric train/loss --json
expri runs compare run-abc123 run-def456 --metric train/loss --reduction min
expri runs compare run-abc123 run-def456 --chart .expri/charts/comparison.html
```

`metrics` reports each series' sample count, last value, minimum, maximum, and
last step. `compare` uses the last recorded value by default; `--reduction min`
or `max` selects an extremum. Missing metrics remain explicit rather than
becoming zero. Repeat `--metric` to select several names, or omit it to include
all recorded metrics. JSON output includes parameters, warnings, and metric
points for further analysis.

The chart is a standalone local HTML document with inline SVG curves, run
metadata, parameter differences, and full-series summaries. Open it in a local
browser; it works offline. Curves preserve logging order, including repeated
steps and step resets. For long series, the plot keeps at most 2,000 points per
run and metric using minimum/maximum buckets; summaries still use all samples.

Remote inspection fetches only the fixed metric files and run metadata into
the existing `results/<target>/runs/<run_id>/` cache. It skips logs and other
outputs, including checkpoints. `[download].results_dir` can change the cache
location. Use `--cached` to read that cache without contacting the target:

```sh
expri -T runpod runs metrics run-abc123 --json
expri -T runpod runs compare run-abc123 run-def456 --chart .expri/charts/remote.html
expri -T runpod runs fetch run-abc123 --metrics
expri -T runpod runs compare run-abc123 run-def456 --cached --metric train/loss
```

`runs fetch --metrics` selects metadata, `outputs/metrics.jsonl`, and
`outputs/params.json`. Combine it with `--outputs` or `--artifact` when additional
files are needed. Selective fetches retain artifacts downloaded earlier. Online
inspection ignores retained metric files absent from the current remote
selection; cached inspection reads the retained files. Recorded status in a
chart or cached response is a snapshot; use `runs status` for a live check.
Older nodes automatically fall back to the Python catalog for metric selection
when the target uses `protocol = "auto"`.

Existing trainers can write the same files without the helper. Each JSONL row
has this shape (`schema_version` and `timestamp` may be omitted):

```json
{"schema_version":1,"step":12,"timestamp":"2026-10-03T01:00:00Z","metrics":{"train/loss":0.42}}
```

Steps must be nonnegative 64-bit integers, and metric values must be finite
numbers. Optional timestamps must be RFC3339 strings of at most 128 bytes.
`params.json` holds `{"schema_version":1,"params":{...}}`; a plain
parameter object is also accepted. Readers report malformed rows or incomplete
final writes and preserve valid samples. Each row and parameter file is limited
to 1 MiB. A metrics file is limited to 128 MiB, with at most 1,000,000 selected
points per run; select fewer metrics if the point limit is reached. Inspection
reads the file extent present when it opens, so a live writer cannot prolong
the read indefinitely.


## Local dashboard

Start the read-only run dashboard from your project:

```sh
expri dashboard
expri dashboard --port 0             # Choose an available local port
expri -T runpod dashboard            # Open the cached runpod source first
```

Open the printed `http://127.0.0.1:<port>` URL. The dashboard lists local runs
from `.expri/runs/` and downloaded runs from `results/<target>/runs/` (or your
configured results directory). Cached sources remain available without target
credentials. It never contacts a target or changes run files. Use the CLI to
fetch remote metrics and logs; the dashboard detects updated selected runs:

```sh
expri -T runpod runs fetch run-abc123 --metrics
expri -T runpod runs fetch run-abc123             # Refresh metadata and logs
```

The header contains source/project selection, **Runs / System / Storage** navigation,
and appearance controls. The bottom status bar contains Refresh, Auto refresh,
and freshness information. **Appearance** offers Light, Dark, and System
themes and remembers your choice in this browser. Full run history stays in the
left panel above 1000 CSS pixels, including while Storage is open. Inspection,
comparison, or Storage uses the remaining width; narrower screens stack history
above this workspace. Opening a run or choosing
**Compare runs** moves to the review on compact screens; closing it returns to
the run browser. Run names and status stay at the start of the table, followed
by machine, task, and start time. History shows two tags per run and a count of
additional tags; inspection shows all tags. Choose up to eight parameter or
metric columns with the **Columns** checkbox tags. Wider tables scroll
horizontally inside history, with selection and run identity pinned. Nested
parameters remain distinct, including keys containing dots or slashes. Missing values show
an em dash. The table's **Last / Min / Max** tags choose the metric
summary, independently of the chart comparison summary. Click a column header
to sort; clicking again reverses its direction. Sorting applies before paging,
and missing values stay at the end in either direction. Column and sort changes
preserve selected runs, the active review tab, and chart exploration. Status
filters use single-click tags. These choices last for the current page;
saved workspace views are a separate roadmap step.

The **System** page browses machine inventory and CPU, memory, GPU, disk, and
network history independently of the selected project. Choose a machine,
period, and Detail or Hourly resolution with native controls. Detail retains
72 hours of readings; hourly charts show means over measured coverage.
Counter readings become rates over their measured intervals, and missing
observations remain gaps. The bottom status bar refreshes the visible monitor
without loading hidden run logs or charts. See [system profiles](docs/system-profiles.md)
to start and publish collection.

The run inspector's **System metrics** tab automatically maps captured whole-machine
readings to the run's start and finish times. Select Run elapsed or Wall clock,
compare Last / Min / Max / Mean summaries, or open the System page in Run period
mode. Exact samples and referenced inventories remain in the run's
`outputs/system-profile.jsonl`, independently of the 72-hour machine-history window.
Overlapping runs share machine readings; these measurements describe the machine,
not resource usage attributed to an individual process. Runs without a capture
show an explicit unavailable state. Deploy the updated hosted server first so
its archive limit accepts all nine tracking files. Then upgrade workers and stop
any older standalone collector before starting new captures; native runs start
the shared worker service automatically.

Opening a run starts with its curves in a compact metric grid. **Overview** holds
its command, provenance, parameters and metric summaries; **Logs** loads
stdout/stderr tails when opened. **Files** lists regular outputs, sizes, and
local or reported worker/cloud availability. Select files for individual browser
downloads, or generate a service fetch command for resumable checkpoint downloads.
Files loads only when opened and keeps selection during refresh. Selecting runs updates the charts directly:
one run opens its curves, and two to eight runs from one source show a comparison.
Compare last, minimum or maximum metric values, and expand parameter differences
below the curves. Changing search/task/status filters clears the selection;
paging, table sorting, column changes, and Refresh preserve it. Status reflects
the saved records;
`runs status` remains the live check.

Hosted runs have an optional nickname and multiple tags. Open a run and use
**Nickname and tags** to edit them, then **Save**. Names and tags appear in the
run browser and comparisons; Search also matches them. The run ID remains visible
and continues to identify files and links. Edits are stored in the service and
survive worker publishing. Local and cached dashboards remain read-only.
Older hosted servers show the nickname and tag fields disabled until the server
is upgraded, so branch previews can still show the new layout.

The service CLI reads or replaces the same values using credentials with owner permissions:

```sh
expri service run annotations --project-id vision --origin gpu-1 --run-id run-1
expri service run annotate --project-id vision --origin gpu-1 --run-id run-1 \
  --nickname "Baseline" --tag baseline --tag model-a
expri service run annotate --project-id vision --origin gpu-1 --run-id run-1 --clear
```

`annotate` replaces both fields: omitted nickname or tags are cleared. Nicknames
support up to 128 characters; each of up to 16 tags supports 48 characters.
Whitespace is trimmed and duplicate tags are removed, preserving their order.

On the hosted dashboard, switch to **Storage** to browse finalized S3 objects
for the selected project. **Private inputs** shows immutable input IDs and
sizes; **Run outputs** shows uploaded files, including checkpoints, with their
machine and run. Search and page through either list, then download a file
through the authenticated dashboard. Storage records input IDs rather than
original local filenames. Input and checkpoint uploads remain CLI operations;
use `expri service fetch` for resumable downloads of large checkpoints.

**Auto refresh** checks every five seconds while the page is visible. Turn it
off to pause updates; hidden tabs and offline browsers pause automatically.
Connection failures keep the current view and slow retries to at most once a
minute. Updates retain the current page, filters, selected runs, review tab,
metric choices, zoom, and hidden curves. **Refresh** checks immediately.
While Storage is open, unchanged checks skip fetching the file list; new
uploads refresh the current search and page.
Local source discovery and run lists refresh every 30 seconds; selected-run
changes are checked every five seconds. A recovery refresh runs every five
minutes to catch changes missed by file metadata.

Charts support point inspection, range zoom, and run visibility controls. Hover
over a curve to read the nearest displayed samples, or focus the plot and use
the arrow keys. The **X-axis** tags select **Step** (the default), **Elapsed time**
since each run's first timestamped metric event, or **Date & time**. Date & time
defaults to your browser's local timezone, named beside the **Local / UTC** tags.
Range and sample readouts show the UTC offset at each displayed instant,
including daylight saving changes. Switching the display timezone preserves
zoom and hidden runs, changes no recorded timestamps, and makes no API requests.
The elapsed origin includes all metrics in the run and remains stable when you
filter metrics or the preview samples points. Time views omit points without
timestamps and report the omitted count; they do not infer timestamps from steps.
Drag across the x-axis to zoom; use the zoom buttons or
**Reset zoom** to change the range. Click a run in the legend to show or hide its
curve. These interactions use the displayed preview and do not download more
metric data. Point readouts retain the recorded step, value, and available timestamp, including
repeated steps and step resets. **Open chart** opens the standalone static view;
its Date & time axis uses UTC.
Changed charts replace their bounded preview because sampling can change older
displayed points. Updates wait until a drag finishes. A zoomed range stays at
the same coordinates; the full range follows newly recorded points. Switching
axes resets zoom while retaining hidden runs. After an initial
bounded refresh establishes the current view, unchanged probes do not download
metrics or reload the chart; recovery refreshes still check the full preview.

Browser responses stay bounded: details preview large parameters and metadata,
show up to 50 metric summaries, and omit package/source-file inventories. Log
tails read at most 64 KiB and 1,000 lines. Charts initially show four metrics;
enter up to six exact metric names to choose others. Curves retain extrema with
at most 600 points per run/metric and 4,800 points across the chart. Chart previews
use bounded JSON rendered in the dashboard, with exact step/time coordinates
carried as decimal strings. Chart JSON and standalone HTML reports are capped
at 2 MiB; other JSON responses are capped at 512 KiB. An oversized response
reports an error so you can narrow the selection. Original artifacts stay
intact. Summary tables do not retain point arrays; chart reads retain the
existing 128 MiB file and 1,000,000 selected-point limits.

The dashboard uses React and TypeScript with TSX components. Esbuild bundles the
UI into one JavaScript asset embedded in the Rust binary, so viewing runs requires
no Node.js, CDN, or hosted service. For frontend development:

```sh
pnpm --dir crates/dashboard/dashboard_web install --frozen-lockfile
pnpm --dir crates/dashboard/dashboard_web check
pnpm --dir crates/dashboard/dashboard_web test
pnpm --dir crates/dashboard/dashboard_web build
```

Commit the generated `crates/dashboard/dashboard_web/app.js` with its
TypeScript and TSX source.
The controller owns requests and refresh scheduling; React owns the workspace
and chart markup, while the chart controller keeps interactive state in a stable
preview container. **Open chart** retains the standalone HTML report for sharing
and offline review.
Frontend tests use jsdom, and container acceptance tests exercise the production
bundle in Firefox. CI checks that rebuilding produces the checked-in asset.

## Optional self-hosted storage

Use `expri service` to forward run metadata, metrics and logs to a service backed
by durable tracking files and SQLite, upload private input files, and fetch selected
checkpoints into the local review cache. One worker service collects machine
history and retries publication jobs while training continues.
`expri service publish` queues ongoing publication of an existing run and
returns; `--wait` waits quietly and `--watch` shows progress until completion.
Jobs retain their initial artifact selection; use `service file-upload` for
additional post-run files. Uploads keep durable multipart receipts; downloads
verify file SHA256 digests before publishing downloaded files. The server bundles and uploads
acknowledged tracking files into `result.zip` in S3; checkpoints and inputs
remain separate objects.

An optional hosted dashboard reviews published runs through HTTPS with a separate
dashboard password and authenticated browser sessions. Choose a project to review
its runs across recorded machines; **All machines** is the default. Machine
tags filter by the publishing origin, and the sortable **Machine** column keeps
that provenance visible. Runs from different machines can be compared even
when their recorded run IDs match. Their logs, files, browser downloads, and
generated CLI commands keep each run's original project and machine scope.
Existing worker run links continue to open the exact recorded run.
Finished hosted runs can be archived on the primary dashboard. They disappear
from the normal list and can be restored from the **Archived** view for 15 days.
After that deadline, durable cleanup removes their hosted data and unreferenced
S3 outputs, preserving private inputs, shared objects, and worker/laptop copies.
Server management commands are `expri service run archive`, `restore`, and `status`;
`service upload` still produces result ZIPs. AB remains read-only. See
[run retention](docs/self-hosted-service.md#run-archival-and-retention).
The project's **Storage** view lists completed private inputs and uploaded
outputs across its machines and runs; the per-run **Files** tab also shows
worker-reported outputs that have not yet been uploaded.
Storage usage shows **S3 storage**, counting each recorded object once including
retained uploads, and **Local storage** for server tracking files and staged
result ZIPs. Pending upload sizes appear separately. Optional project deletion
requires a preview, the exact project name, and the dashboard password; durable
cleanup retries interruptions and removes eligible S3 versions. AB remains
read-only. See [storage management](docs/self-hosted-service.md#storage-usage-and-project-deletion)
for configuration and server management commands.
The local dashboard remains available for offline review after downloading
results.

Sign in with the primary dashboard password to save this machine's server connection:

```sh
expri login --url https://expri.example.net
expri config server show
```

The interactive password prompt is hidden. Login exchanges the password for a
separate API token with unrestricted server access and saves it privately in `[server]` in
`~/.tokn/expri/config.toml`; it never stores the password or prints the token.
Use `--password-stdin` for a pipe or redirected password file. Later `expri login`
uses the saved URL. Query and service client commands use this connection by
default; `--config PATH` selects an explicit override.

Issued tokens have no expiry and survive service restarts. `expri logout`
revokes the current issued token and clears its local credential while keeping
the URL and project selection. Changing the dashboard password, unrestricted
server API token, or primary dashboard authority invalidates issued tokens after
restarting the service. Upgrade the server to enable CLI login and logout.

Existing manual tokens and `token_env` client files remain supported. Use
`expri config server set --url URL --token-stdin` to store a supplied token;
`expri config server show` hides it. Connection updates preserve system-profile
settings. Read [login and configuration](docs/self-hosted-service.md#configure-the-clis-server-connection)
or `expri skill login` for details.

Browse published records without a project checkout:

```sh
expri query runs
expri query runs --project-id vision --origin gpu-1
expri query files --run-id run-abc123
```

File queries resolve a run ID across accessible active runs and archived runs
within their restore window. Add `--project-id` and/or `--origin` only when the
ID is ambiguous, or supply both to select the scope directly.

These commands show tables by default. Add `--json` for complete bounded pages
with `next_cursor`; table output shows the next cursor when more rows are available.
To pin later file pages to the same scope, optionally pass the resolved project
and origin with the cursor.
File rows distinguish received data from reported outputs still awaiting upload. Execution status is
the last published state. See [server browsing](docs/self-hosted-service.md#browse-published-runs-and-files).

See the [self-hosted service guide](docs/self-hosted-service.md) for server and
worker configuration, browser sign-in, input files, selective downloads, and offline dashboard
review. Configure `[service]` for automatic publication with `expri run`, or use
explicit service commands for existing runs.

## Python environment

Configure uv for local tasks and as the default for targets with a top-level
environment table:

```toml
# expri.toml
[environment]
```

An empty table selects a normal uv environment. For a remote GPU machine that
already has a suitable Conda or system Python installation, select its Python
and the packages you want to reuse:

```toml
# expri.target.toml
[target.runpod.environment]
base_python = "/opt/conda/bin/python"
reuse_packages = ["torch"]
require_cuda = true

[target.runpod.environment.env]
# Add the native-library variables your existing installation needs:
LD_LIBRARY_PATH = "/opt/conda/lib"
```

The target environment table replaces the entire top-level environment table;
fields are not merged. A target without that table inherits the top-level
configuration. Use an empty `[target.<name>.environment]` table to select a
normal uv environment even when the project default reuses base packages.

`base_python` identifies an existing interpreter. `reuse_packages` contains
package names, without versions or extras, and requires `base_python`. Expri
validates their installed dependency closure and requires exact locked versions
where inherited packages occur in the target's selected lock graph. It then
validates the combined environment and keeps the base installation unchanged.
A mismatch fails with an explanation instead of replacing PyTorch or its
dependency stack. Unrelated Conda packages are not compatibility-certified.

For reuse, point `base_python` at the Conda or system interpreter that owns the
installed packages. An existing uv virtual environment is not adopted as a base
or as a run environment; reuse from another virtual environment is rejected.
Passing a Torch/CUDA smoke test alone does not establish lock compatibility:
the inherited dependency closure and the combined environment must also pass
validation against the selected locked requirements.

PyTorch reuse requires a compatible Python distribution graph as well as working
native libraries. If `uv.lock` selects `nvidia-*` CUDA wheel packages, those
distributions must already belong to the inherited stack. A Conda installation
with CUDA libraries but missing that Python package metadata fails explicitly;
expri avoids downloading a second CUDA wheel stack into the overlay. Select a
base installation and lockfile that describe the same dependency stack.

`require_cuda` defaults to `false`. Set it to `true` alongside PyTorch reuse to
require an available CUDA device and a small GPU computation during environment
validation. When PyTorch is reused, its CUDA version and availability are
recorded even if CUDA is not required. Native-library and other activation
variables can be supplied in `env`; use the values appropriate to that machine.
`UV_*`, `PYTHONHOME`, and `VIRTUAL_ENV` overrides are reserved for expri's
environment selection. Expri does not install CUDA drivers or a CUDA toolkit.

Install uv on each execution machine before preparing an environment. The
reuse helper fetches small pinned `packaging` and `tomli` dependencies through
uv. Project dependencies are also fetched as needed through uv. Reuse mode
installs external dependencies from wheels, so missing compatible wheels fail
explicitly. The current project is installed as an editable package in the run
environment using compatible build dependencies already available there.

## Environment checks and storage

Before preparing a rented machine's project environment, check its existing
Python stack against the selected lockfile:

```sh
expri -T runpod push
expri -T runpod env doctor
expri -T runpod env doctor --json
```

For a top-level `[environment]`, run `expri env doctor` locally. The report
identifies the base Python, inherited package closure, failed checks, and cache
configuration. Its compatibility verdict covers the base stack and selected
lock graph. It does not certify a rental provider, GPU performance, or a future
project build. If an owned prepared environment already exists, the report
checks it separately without rewriting its manifest. Project dependency
installation and combined-environment validation happen during setup or run
preparation. The helper may fetch its small pinned dependencies and the lock
export may need package-index access.

If the report identifies a locked-version mismatch, select a base image and
lockfile that agree. A missing `nvidia-*` wheel metadata issue means Conda
native CUDA libraries alone do not describe the requested wheel stack.
`require_cuda = true` additionally checks GPU availability and a small CUDA
computation; successful CUDA execution does not waive dependency checks.

The report includes the effective cache directory, link mode, whether the cache
and run environments share a filesystem, and whether caching is disabled.
Sharing cached package files through hardlinks or filesystem clones can reduce
physical disk use on a compatible filesystem. Copy mode, cross-filesystem
storage, source builds, editable projects, and generated outputs can still add
per-run bytes. A directory's logical file size is not the amount of disk space
that pruning will reclaim.

Preview pruning before applying it:

```sh
expri -T runpod env prune
expri -T runpod env prune --keep-last 1 --apply
# Local project:
expri env prune --keep-last 1 --dry-run
```

Pruning defaults to a preview and keeps the latest eligible run environment.
It removes only expri-owned `.venv` directories from finished runs. Active or
preparing runs, unsafe paths, and unowned environments are skipped. Code
snapshots, outputs, run records, setup environments, and the shared cache remain
available. The report lists each run's action and reason, logical bytes, and
the number of environments removed. Pruning does not happen automatically.

## Setup

`expri -T <target> setup` runs repo-configured setup steps on the target. Built-in
steps are `uv`, `hf`, and `script`; scripts are resolved relative to the remote
repo root. When the target has an environment configured, setup also validates
the base installation, prepares a dependency overlay, warms uv's cache, and
saves a validation manifest under `.expri/`. Each run prepares its own overlay
under its run directory and validates the base stack again. Setup does not
guarantee that a later run's project build or downloads will succeed offline.

With a top-level `[environment]`, `expri setup` also prepares the local project
without a target. A target-only configuration requires `-T <target>`. Setup
scripts before the first environment-preparation step run in the ambient
environment; scripts after preparation run through uv in the selected
environment. Place a `uv` setup step before scripts that need project
dependencies. The project itself is installed in each isolated run environment.

Prepare the target after pushing its project files:

```sh
expri -T runpod push
expri -T runpod setup
expri -T runpod run train --epochs 3
```

The run records the base interpreter and inherited package versions so results
can be traced back to the environment used. Preparation inspects the current
base stack and checks reused packages against the project lock. Compatible
changes produce a new recorded fingerprint; incompatible changes fail.

## Download

`expri -T <target> download` downloads configured result mappings into
`results/<target>/`. Mappings are declared in `expri.toml`:

```toml
[download]
ignore = ["*.pt"]

[download.mappings]
wandb = "wandb"
jobs = "out/jobs"
```

That example downloads the remote repo's `wandb/` directory into
`results/<target>/wandb/`, and `out/jobs/` into `results/<target>/jobs/`.
Ignore patterns are passed to rsync as excludes relative to each mapping root.
Pass mapping names after `--` to download a subset:

```sh
expri -T runpod download -- wandb
```

Use `--dry-run` to print the selected transport's commands without executing them.

## Testing

The single `expri` executable uses internal workspace crates. See
[crate layout](docs/crate-layout.md) for boundaries and release ordering.

Run Rust checks with `cargo test --workspace --all-targets --all-features --locked`. CI also
runs the Python runtime/metrics tests and dashboard frontend checks.

The [host and worker suite](tests/containers/README.md) runs real uv, SSH and
rsync between isolated Docker containers with different installed fake Torch
versions. It covers dependency reuse and incompatibility, detached runs,
snapshot isolation, pruning, cancellation, selective fetching and cached dashboard
review across native and Python protocols. Run it from the repository root:

```sh
tests/containers/run.sh
```

Image builds require internet; test execution uses an internal fixture index.
The fake GPU API tests workflow decisions and metadata, without verifying real
GPU computation or PyTorch/CUDA binary compatibility.

The [self-hosted S3 suite](docs/self-hosted-service.md#acceptance-tests) additionally
tests private inputs, service outages during training, lost multipart
acknowledgments, restart recovery, selective checkpoint downloads and offline
review through separate service, S3, host and worker containers. Its reusable
asset phase also checks deferred addition, local upload, output import without
an extra S3 object, portable sidecars, worker downloads, and hosted provenance:

```sh
python3 -B tests/containers/service_workflow.py
```
