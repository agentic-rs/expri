# Machine system profiles

Expri records system inventory and utilization independently of experiment runs.
Each machine stores this new data under `~/.tokn/expri`:

```text
~/.tokn/expri/
  config.toml        # Optional machine-level system-profile settings
  profiles.sqlite3  # Stable machine ID, inventories, samples, hourly history, upload receipts
  collector.lock    # Live collector lease
  publishing.json   # Last publisher status and error
```

Existing project configuration, assets, uv caches, runs, and experiment
publishing queues keep their existing locations. System-profile storage is
selected by `--storage-dir`, then `EXPRI_STORAGE_DIR`, then the home-directory
default. Owned directories are private (`0700`) and files are private (`0600`)
on Unix. Resolving paths and inspecting an empty store creates no files.

## Start collection on each machine

```sh
expri system sample
expri system status
expri system daemon --no-publish
```

`sample` saves one inventory and observation and prints JSON, including the
stable `machine_id`. `daemon` runs in the foreground and samples every ten
seconds by default. Run it on every machine you want to record. It operates
without `expri.toml` or an active experiment and allows one collector per storage
directory. Stop it with Ctrl-C. Start it again after a restart; expri does not
install an OS service. These commands execute on the current machine and reject
`-T`; invoke them on the worker itself for worker measurements.

For another interval, use `--interval-seconds 30` or create global configuration:

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
expri system daemon
expri system daemon --client-config /path/to/machine-client.toml
expri system publish --client-config /path/to/machine-client.toml
```

The daemon collects even while publishing fails. Its independent publisher
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
overwrite a newer one. An outage longer than 72 hours loses raw detail while
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

The CLI can also inspect the same local or hosted history:

```sh
expri system history
expri system history --resolution hourly
expri system history --resolution hourly --start 2026-10-01T00:00:00Z --end 2026-10-12T00:00:00Z
expri service system list --config owner.toml
expri service system profile --config owner.toml machine-actual-id
expri service system history --config owner.toml machine-actual-id --resolution hourly
```

Commands print JSON. History defaults to 72 hours of detail or 30 days of hourly
data; specify RFC3339 `--start`/`--end` timestamps with a timezone to select a
different range. Reads are bounded and return `next_cursor`; pass that value
with `--cursor` to continue. `service system profile --inventory-revision N`
retrieves a historical inventory referenced by a sample or summary.

`status` reports lease-based collector liveness, durable sample/hour counts,
current inventory, and the last publisher status. Publisher status includes its
observation timestamp and may describe an earlier attempt when the collector is
stopped. All machine-profile data stays in SQLite; it does not create experiment
tracking streams or upload profile data to S3.
