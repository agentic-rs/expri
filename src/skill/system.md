# Expri system profiles

Use system profiles for machine CPU/GPU, RAM, disk, and network observations.
This operates independently of experiment runs and project configuration.
New profile data lives in `~/.tokn/expri`; `--storage-dir` overrides
`EXPRI_STORAGE_DIR`, which overrides that default.

```sh
expri system sample
expri system status
expri service start
expri service start --foreground
expri service status
expri service stop
expri system history
expri system history --resolution hourly
```

Reports default to human-readable tables. Add `--json` when an agent or script
needs complete structured fields, such as `expri system sample --json` or
`expri service system list --json`. `--quiet` suppresses tables; explicit
`--json` still prints the report. The foreground service keeps its operational
messages rather than producing a final report.

`sample` writes an inventory and one observation, returning the stable machine
ID. Native runs start one shared worker service lazily before capture metadata;
later runs reuse it. It defaults to ten-second sampling, stays running until
stopped and recovers saved publication jobs on the next start after reboot.
`start` runs in the background; `--foreground` runs in the terminal and refuses
a duplicate. `stop` leaves training alive and retains queued work. Ctrl-C stops
a foreground service. Commands run on the current machine and reject `-T`.
`service status` and inspection of an empty store create no files. `system daemon`
is a deprecated foreground alias; its `--no-publish` disables machine publishing
only. `run --no-publish` disables only that run's publication.

The public components are `collection`, `publishing`, `queue-execution`, and `run-control`.
Collection supervises sampling and capture export; publishing supervises machine
history and run uploads. Use:

```sh
expri service stop --component collection
expri service start --component collection
expri service status --component collection
expri service stop --component publishing
expri service start --component publishing
expri service restart --component collection
```

Saved enable/disable choices live in `~/.tokn/expri/service.json`, for example
`{"components":{"publishing":"disable"}}`. Missing components default to enabled.
Choices survive whole-service restart and automatic startup. Manual file edits
apply on whole-process startup or `service restart`. Collection pause creates
observation gaps and keeps unexported samples in the durable outbox,
and terminal supervisors can still finalize their run captures. Component
failures do not stop peers. Restart only the affected component to load changed
settings with `service restart --component NAME`; it enables the selected group
and waits internally for its old threads to finish while peers stay live. Status
can show stopping or starting while it completes. `--component` is available on
start/stop/restart/status and cannot accompany `start --foreground`. Whole
`service restart` preserves component choices, training, and queued work.

## Publishing configuration

Deploy the updated server before upgrading run workers; complete system captures
require its nine-file tracking archive limit. Stop an older standalone collector
before starting the shared worker service.

The existing server must advertise `system-profile-v1`. Configure a dedicated
machine token with `[[machines]]`, its actual `machine_id`, and `token_env` on
that server. Use matching machine service credentials locally. Project worker
tokens cannot publish machine profiles; owner credentials can inspect all
machines. Machine credentials can publish/read only their own ID.

The private machine client file uses the existing `url` and `token_env` fields.
Its absolute path can be selected per command or saved in global configuration:

```toml
# ~/.tokn/expri/config.toml
[system_profile]
interval_seconds = 10

[system_profile.publish]
client_config = "/home/me/.tokn/expri/machine-client.toml"
```

Collection continues while uploads retry. `status` includes the last publishing
error and timestamp. `expri system publish` sends one pending batch without
starting collection. This explicit command can use the global `[server]` connection
when no client override is selected. Configuring `[server]` alone does not enable
the worker service's background machine publishing; select machine credentials
through the profile publishing settings. Use `service restart --component publishing`
after changing those settings, or `collection` after changing the interval.
Removing the profile database creates a new machine ID;
do not clone it onto another independently collecting machine.

## Read retention correctly

Raw detail is retained for 72 hours by observation time, locally and on the
server. Hourly summaries and their inventory revisions remain indefinitely.
Local summaries commit with each sample and upload as absolute versioned
snapshots. Retries do not inflate totals. An offline period longer than 72 hours
can still publish hourly history after its raw samples have expired.

Run-owned captures have a separate policy: retain the first real observation
in each wall-clock minute of each capture for as long as the run is retained.
The minute selection does not slow machine collection or resource checks.
Original timestamps, measurement intervals, counter strings, inventory revisions,
and attempt boundaries stay intact. A selected point is not a minute average;
counter rates still use its original interval. Continuation starts a new capture.
Existing committed capture files keep their upload offsets. Hosted charts and
summaries apply the minute selection to older high-rate captures as well.
Hosted storage can compress those original bytes losslessly with
`expri service compact-system-captures --data-dir /var/lib/expri --json` to
preview, then `--apply` while the backend is stopped. Never truncate or rewrite
acknowledged capture files to thin history; publisher offsets and continuation
checkpoints depend on their original byte prefixes.

Hourly gauges expose measured coverage and weighted sum, plus count/min/max/last.
Divide sum by coverage milliseconds for the mean. Counter summaries sum measured
deltas; divide by coverage seconds for throughput. Raw counters serialize as
exact decimal strings. Hours are UTC, intervals crossing a boundary are split,
and downtime is not filled with zeros or extended coverage.
Means and throughput require nonzero coverage; a lone warmup `sample` has no
measured interval. Inspect summary warnings for bounded metric omissions.

GPU readings depend on available NVIDIA/Apple/AMD providers. Unsupported or
failed readings are omitted with warnings. Apple GPU memory is shared rather
than dedicated VRAM; GPU utilization is sampled. macOS disk I/O and Apple GPU
temperature/power are unavailable in this version. CPU/counter rates start with
warmup. A system profile is not a per-training-process measurement.

```sh
expri service system list
expri service system profile machine-actual-id
expri service system history machine-actual-id --resolution hourly
expri service system profile machine-actual-id --inventory-revision 1
```

History uses bounded pages. The table shows its next cursor, and `--json`
includes `next_cursor` in the structured report. Pass it through `--cursor`;
use timezone-qualified RFC3339 `--start`/`--end` to select a range. Read the
inventory revision referenced by a historical sample when device identities
changed. Existing run/asset/cache paths keep their current locations.

Use `expri system --help` and `expri service system --help` for exact flags.
