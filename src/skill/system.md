# Expri system profiles

Use system profiles for machine CPU/GPU, RAM, disk, and network observations.
This operates independently of experiment runs and project configuration.
New profile data lives in `~/.tokn/expri`; `--storage-dir` overrides
`EXPRI_STORAGE_DIR`, which overrides that default.

```sh
expri system sample
expri system status
expri system daemon --no-publish
expri system daemon --client-config /path/to/machine-client.toml
expri system history
expri system history --resolution hourly
```

`sample` writes an inventory and one observation, returning the stable machine
ID. Run the foreground `daemon` on each machine for continuous collection.
It defaults to ten-second sampling and holds a singleton lease. Ctrl-C stops
it; restart it after reboot. Commands run on the current machine and reject
`-T`. `status` and inspection of an empty store create no files.

## Publishing configuration

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
starting collection. Removing the profile database creates a new machine ID;
do not clone it onto another independently collecting machine.

## Read retention correctly

Raw detail is retained for 72 hours by observation time, locally and on the
server. Hourly summaries and their inventory revisions remain indefinitely.
Local summaries commit with each sample and upload as absolute versioned
snapshots. Retries do not inflate totals. An offline period longer than 72 hours
can still publish hourly history after its raw samples have expired.

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
expri service system list --config owner.toml
expri service system profile --config owner.toml machine-actual-id
expri service system history --config owner.toml machine-actual-id --resolution hourly
expri service system profile --config owner.toml machine-actual-id --inventory-revision 1
```

History uses bounded pages. Pass returned `next_cursor` through `--cursor`;
use timezone-qualified RFC3339 `--start`/`--end` to select a range. Read the
inventory revision referenced by a historical sample when device identities
changed. Existing run/asset/cache paths keep their current locations.

Use `expri system --help` and `expri service system --help` for exact flags.
