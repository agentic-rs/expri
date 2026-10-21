# Machine system profiles

Expri records system inventory and utilization independently of experiment runs.
Each machine stores this new data under `~/.tokn/expri`:

```text
~/.tokn/expri/
  config.toml       # Server connection and optional system-profile settings
  service.json      # Component enable/disable choices
  profiles.sqlite3  # Stable machine ID, inventories, samples, hourly history, upload receipts
  collector.lock    # Live collector lease
  collector.json    # Local collection status
  publishing.json   # Last publisher status and error
  worker-service/   # Service lease, status and private durable run jobs
```

Existing project configuration, assets, uv caches, runs, and experiment
publishing queues keep their existing locations. System-profile storage is
selected by `--storage-dir`, then `EXPRI_STORAGE_DIR`, then the home-directory
default. Owned directories are private (`0700`) and files written by expri are
private (`0600`) on Unix. Resolving paths and inspecting an empty store creates no files.

## Start collection on each machine

```sh
expri system sample
expri system status
expri service start
expri service status
expri service restart
expri service stop
```

`sample` saves one inventory and observation and prints a table, including the
stable machine ID. Add `--json` to obtain the complete machine-readable report.
Native runs start the shared worker service before recording their machine
identity and capture period. Concurrent and later runs reuse it. `service start`
starts it in the background or reuses the existing process; `--foreground` runs
it in the current terminal and refuses a second owner. The service samples every
ten seconds by default and operates without `expri.toml` or an active experiment.
It stays running until stopped and starts again on the next run or publication.
`service stop` stops collection and uploads, retaining saved work and leaving
training alive. Ctrl-C stops a foreground service. After reboot, the next run,
publication, or explicit start recovers saved jobs; expri does not install an OS
service. Commands execute on the current machine and reject `-T`; invoke them on
the worker itself. `service status` creates no files and reports collection,
machine publishing, capture export, run publishing, and queue execution separately.
The public components are `collection`, `publishing`, `queue-execution`, and `run-control`.
Collection supervises sampling and capture export; publishing supervises machine
history and run uploads. Each group supervises its own threads; stopping or
failing one does not stop the others. Control the relevant component with:

```sh
expri service stop --component collection
expri service start --component collection
expri service status --component publishing
expri service stop --component publishing
expri service start --component publishing
expri service restart --component publishing
```

Component choices are saved in `~/.tokn/expri/service.json` across process restarts.
For example, `{"components":{"publishing":"disable"}}` disables background
publishing; values are `"enable"` or `"disable"`, and omitted components default
to enabled. Manual file edits take effect on whole-service startup or restart.
Automatic startup and a whole-service start respect paused choices. Paused
collection leaves gaps in
observations and retains committed capture readings in the durable outbox.
Terminal run supervisors can still finalize their own captures. A component
start reloads its settings after its preceding threads have stopped. Use
`service restart --component collection` or `service restart --component publishing`
to enable that group and reload its settings while other components continue.
Component requests return before
draining completes; inspect status while it is stopping. `--foreground` starts
the whole shared process and cannot be combined with `--component`.
See [all component names and stop behavior](self-hosted-service.md#independent-worker-components).
Whole `service restart` preserves saved choices, pending data, and training.

The old `expri system daemon` command is a deprecated foreground compatibility
alias for the shared worker service. Its `--no-publish` disables only machine
history publication for that invocation. A run's `--no-publish` disables only
that run's publication. Local collection and any configured machine publisher
continue independently.

When upgrading a worker with an older standalone collector already running,
stop that collector through its terminal or process manager before starting the
new service. Its collection lease prevents a second collector from taking over.
Existing training and durable capture data remain available.

For another interval, create global configuration:

```toml
# ~/.tokn/expri/config.toml
[system_profile]
interval_seconds = 10
```

Collection includes CPU inventory/utilization/load/frequency, system RAM and
swap, local disk capacity, and network interface counters. CPU/counter rates
need a previous observation; warmup, reset, unsupported, and failed readings
are omitted and accompanied by warnings instead of becoming zero. Removed
devices disappear from inventory and readings. Long pauses and clock changes
start a new measured interval rather than
filling the missing time. Measurements describe the system visible to the
collector, rather than a training process or its container limits.

GPU providers are optional at runtime: NVIDIA uses NVML, Apple uses IOKit, and
Linux AMD uses available sysfs sensors. Device/provider limits remain explicit.
Apple GPU memory is shared system memory; it is not dedicated VRAM. NVIDIA
utilization is a sampled driver reading, not a continuous ten-second GPU trace.
Linux disk I/O uses observable diskstats counters. macOS disk I/O and Apple GPU
temperature/power are currently unavailable; other profile readings continue.

## Publish to an existing server

Upgrade the server to a version advertising `system-profile-v1`. Give each
machine its own token and bind it to the machine ID printed by `sample`. Add
the binding to the existing server configuration:

```toml
[[machines]]
machine_id = "machine-actual-id-from-sample"
token_env = "EXPRI_LAPTOP_MACHINE_TOKEN"
```

Provide the token through that environment variable on the server. On the
machine, create a private service client file, for example
`~/.tokn/expri/machine-client.toml`:

```toml
url = "https://expri.example.net"
token_env = "EXPRI_MACHINE_TOKEN"
```

Provide the matching token in the machine's named environment variable. Keep
the client file private. Configure its absolute path on the machine:

```toml
[system_profile]
interval_seconds = 10

[system_profile.publish]
client_config = "/home/me/.tokn/expri/machine-client.toml"
```

```sh
expri service start
expri system publish --client-config /path/to/machine-client.toml
```

Restart the affected component after changing its configuration:

```sh
expri service restart --component publishing
```

Use `collection` instead to apply a changed sampling interval. These restarts
leave other component threads running. Machine publishing requires the explicit saved setting above;
a general `[server]` connection alone leaves it `unconfigured` in service status.
Run publication and machine history use their own scoped credentials.

The worker service collects even while publishing fails. Its independent publisher
retries bounded batches and stores acknowledgements per endpoint. `publish`
sends one pending batch without starting a collector. The owner can read all
machines; machine credentials can publish/read only their own machine. Existing
project-scoped worker tokens cannot submit machine profiles. System history is
independent of project/run archival or deletion.

The machine identity lives in its profile database. Restarting a collector
retains the identity and sequence. Removing that database creates a new identity
that must receive its own server binding. Do not clone a machine's live database
onto another machine and run both under the same ID.

## Retention and hourly summaries

Both local and server stores retain raw samples for a rolling **72 hours by
observation time**. Expiry runs during collection, publishing, and server
maintenance; queries also hide expired details. Hourly summaries and referenced
inventory revisions are retained indefinitely.

Each local sample and its hourly updates commit atomically before raw expiry.
The publisher uploads absolute, versioned hourly snapshots. A retry of the
same snapshot does not add its counts twice, and an older snapshot cannot
overwrite a newer one. An outage longer than 72 hours loses uncaptured machine detail while
its hourly history can still be uploaded later. The server accepts those old
summaries without reconstructing them from expired raw data.

Hours use UTC boundaries. Gauges keep valid sample count, captured min/max/last,
measured coverage, and `sum = value * covered milliseconds`; divide by coverage
for the mean. Counter summaries keep interval-change totals; divide the total
by covered seconds for average throughput. Cumulative counters remain exact
decimal strings in raw JSON. Intervals crossing an hour boundary are split;
counter attribution across that boundary is proportional. Missing time is not
counted as zero utilization or extended coverage.
Means and throughput are unavailable when coverage is zero, including a lone
warmup observation from `sample`.

Hourly summaries have bounded metric and byte budgets. If many transient
devices exhaust a bucket's budget, existing metrics continue updating and new
metrics are omitted with explicit warnings in the affected summary and sample.
Raw samples keep their valid readings.

After a clock correction, records more than five minutes in the future are
quarantined and excluded from uploads; affected recent hours are rebuilt from
valid retained samples. Quarantine keeps raw identities and reasons, not raw
payloads. `status` exposes quarantine counts. Within a running process,
monotonic elapsed time bounds destructive retention after a forward clock
jump; the wall clock at process startup is still the initial time reference.

## Inspect local and hosted history

Open the dashboard's **System** page to browse machine inventory and utilization.
Machine choices are independent of project selection. Period presets, Detail or
Hourly resolution, and Local or UTC time controls share the chart workspace.
Detailed charts show counter rates over their measured intervals; hourly charts
show means or throughput over measured coverage. Missing readings remain gaps.
The displayed measurements describe the whole system visible to the collector.

Hosted dashboards read profiles through the authenticated browser session and
do not require machine credentials in the browser. Local dashboards read an
existing profile store using `EXPRI_STORAGE_DIR` or the normal home-directory
location. Opening the page does not start collection or create a profile store.

New native and Python fallback runs record the existing store's stable
`machine_id` in `run-state.json` when it is available at launch. Start collection
or run `expri system sample` before launching a run to establish that identity,
and use the same `EXPRI_STORAGE_DIR` on both commands if overriding storage.
Missing or unreadable profiles leave the run without a mapping and do not stop
the experiment. Existing runs do not acquire an inferred machine identity from
their hostname or worker label.

## Retained system metrics for runs

Deploy the updated server first so complete captures fit its nine-file tracking
archive limit. Then upgrade run workers and stop any older standalone collector
before launching new runs. Native runs start the shared service automatically.
Native and Python launchers reserve `outputs/system-profile.jsonl` when existing
local profile storage is available. They record `system_capture_id` alongside `machine_id` in
run state. An existing user file at that path is never replaced.

For each capture, the collector keeps the first matching observation in each
wall-clock minute and its inventory in a durable outbox. Selection commits in
the same transaction as collection and survives export, restart, and clock
corrections. Live machine collection and disk-space checks retain their normal
cadence. The exporter appends the selected original records to the run file,
flushes them to disk, and checkpoints the committed byte prefix before releasing
outbox rows. Selected readings survive interruptions longer than 72 hours.
The run file, hosted tracking stream, and result archive follow normal run
retention and deletion. Rolling machine detail and hourly history remain separate.

The mapping uses the run's existing period: `started_at` includes environment
preparation, and observations satisfy `start <= observed_at < finish`. Overlapping
runs capture the same whole-machine readings. Original intervals, sequence,
session, inventory revisions, warnings, and decimal counter strings remain intact.
Each point is one actual observation, not a minute average or a minute's total
counter activity. Minute buckets do not impose a uniform 60-second spacing:
readings near opposite minute boundaries may be close together. Continuation
starts a separate capture, so each attempt can retain its first reading even
when two attempts occupy the same minute.
Counter charts show rates over the full measured interval, including intervals
that begin before the run. They do not prorate a boundary interval into exact
per-process usage. Missing observations and clock changes remain visible.

At task completion the capture remains pending until the collector commits past
the finish boundary or its lease proves collection stopped. The task can return
while capture publication finishes. A collector that cannot acknowledge the
boundary yields an explicit incomplete capture without changing the task exit
code. Recorded readings remain available; no boundary sample is invented.

Run captures use the existing experiment publisher and credentials. The server
must advertise `run-system-metrics-v1` for automatic stream uploads. Unsupported
servers still receive normal experiment data; publishing status explains that
the retained capture remains local. New browser APIs read only the run's own uploaded
capture, never arbitrary global machine history inferred from run metadata.

Open **System metrics** in a run or comparison to inspect mapped values, retained
summaries, and Run elapsed or Wall clock charts. The System page's Run period
mode uses retained captures, so older runs can be inspected after machine detail
expires. Pre-existing runs without a capture remain explicitly unavailable;
hourly averages cannot reconstruct exact short-run readings. Browser pages and
chart previews are bounded with visible truncation notes. The full raw file is
included by default when fetching a run from the service.

Older files may already contain several readings per minute. Browser charts and
summaries select the first eligible reading per minute of each capture, while
the original upload offsets and archive bytes stay valid. New capture writes
use the minute policy; existing committed local prefixes are never rewritten.
On a worker upgrade, large active captures seed their minute selections in
bounded background passes before export catches up. Training and machine
collection continue. Allow this migration to finish before treating temporarily
unchanged capture-file lengths as an exporter failure.
The hosted backend can reclaim space from older high-rate streams using
[lossless system-capture storage compression](self-hosted-service.md#system-capture-storage-compression).
This private storage format reconstructs the original bytes for downloads,
upload retries, continuation, and archives. It does not alter training metrics.

The CLI can also inspect the same local or hosted history:

```sh
expri system history
expri system history --resolution hourly
expri system history --resolution hourly --start 2026-10-01T00:00:00Z --end 2026-10-12T00:00:00Z
# Service inspection uses [server] in ~/.tokn/expri/config.toml by default.
expri service system list
expri service system profile machine-actual-id
expri service system history machine-actual-id --resolution hourly
# Use complete JSON reports in scripts or agents.
expri system history --resolution hourly --json
expri service system list --json
```

Reports default to human-readable tables; `--json` preserves the complete
structured report for automation. `--quiet` suppresses tables, while an explicit
`--json` still prints the report. History defaults to 72 hours of detail or 30 days of hourly
data; specify RFC3339 `--start`/`--end` timestamps with a timezone to select a
different range. Reads are bounded and return `next_cursor`; pass that value
with `--cursor` to continue. `service system profile --inventory-revision N`
retrieves a historical inventory referenced by a sample or summary.

`status` reports lease-based collector liveness, durable sample/hour counts,
current inventory, and the last publisher status. Publisher status includes its
observation timestamp and may describe an earlier attempt when the collector is
stopped. Independent machine profiles stay in SQLite. Run-owned copies additionally
use experiment tracking streams and can be included in result archives in S3.
