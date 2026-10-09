# Optional self-hosted experiment storage

Expri keeps execution and review local-first. Workers write the existing run
files, and a separate uploader forwards them to an optional service. The service
keeps acknowledged JSONL, logs, and versioned metadata on its own disk and
incrementally indexes metrics in SQLite. After a run finishes, it bundles the
received tracking files into `result.zip` in S3-compatible storage. Checkpoints
and private inputs remain separate S3 objects. Service outages leave training
running. Required private inputs must already be downloaded before an offline
run can start.

Runs can start their own uploader when service publishing is configured. Explicit
service commands remain available for existing runs and selected checkpoints.
The service does not schedule experiments.

## Service configuration

Keep this configuration on the server:

```toml
owner_token_env = "EXPRI_SERVICE_OWNER_TOKEN"

[[workers]]
project_id = "vision"
origin = "gpu-1"
token_env = "EXPRI_SERVICE_GPU_1_TOKEN"

[storage]
bucket = "my-expri-artifacts"
region = "us-east-1"
# Omit endpoint for AWS S3; set it for an S3-compatible server.
endpoint = "https://s3.example.net"
path_style = true
prefix = "expri"
```

Provide distinct random bearer tokens of at least 24 printable characters using
the named environment variables. Each worker token is restricted to its project
and origin; workers can download that project's inputs but cannot publish inputs.
Only the owner token has unrestricted access.

Provide S3 credentials to the service through `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, and optional `AWS_SESSION_TOKEN`. Workers and laptops
use service tokens and temporary signed URLs rather than bucket credentials.
Signed URLs are credentials; keep them out of logs.

```sh
expri service serve --config server.toml --data-dir /var/lib/expri
```

The listener defaults to `127.0.0.1:8787`. Terminate HTTPS at your own reverse
proxy when serving other machines. Use `--listen` to select the private bind
address. `--create-bucket` explicitly provisions a private bucket; it is omitted
when using an existing bucket.

Persist and back up the entire service data directory. It contains SQLite,
original tracking files under `tracking/`, staged archives under `archives/`, and
multipart receipts. The S3 bucket alone does not replace this directory.
Archiving retains the hot files and SQLite data; expri does not delete them
automatically. Only one service process may use a data directory. Keep the storage
configuration stable for that directory. S3 endpoints used in signed URLs must
be reachable from the workers and laptops.

Opening this version upgrades the SQLite schema to version 2. Older binaries
refuse that directory because they cannot read tracking records. Back up the
entire service directory before upgrading; restoring that backup is required
for a backend downgrade. Existing S3 runs remain readable after the upgrade.

The optional object prefix is limited to 512 bytes to leave room for scoped IDs
within S3's object-key limit.

## Hosted dashboard

Enable browser review in the server configuration:

```toml
[dashboard]
public_url = "https://expri.example.net"
password_env = "EXPRI_DASHBOARD_PASSWORD"
```

Provide a strong, separate `EXPRI_DASHBOARD_PASSWORD` containing 16 to 256
printable ASCII bytes. It must differ from all owner and worker bearer tokens.
Open the configured HTTPS URL and sign in with this dashboard password.
The dashboard discovers synced project/worker sources and provides run details,
parameters, metric comparisons, charts, bounded log tails, and a **Files** tab.
Choose up to eight parameter or metric columns using the **Columns** checkbox
tags. Click table headers to sort before paging, and use the table's
**Last / Min / Max** tags to choose its metric values. Missing values
stay at the end in either sort direction. Adding columns places the run table
above the review workspace, with horizontal scrolling contained in the table
on narrow screens. Sorting and column changes preserve the current review;
these choices last for the current page.
Workers must push a run before it appears. Files shows output names, sizes, and
availability without reading checkpoint contents. **Worker (reported)** reflects
the last published inventory; **Cloud** means a finalized file is downloadable.
The hosted page cannot inspect your laptop's disk. Local dashboards show files
actually present in the run or review cache; cached cloud availability is a
record of the last pull rather than a live storage check.

Select up to 64 files. **Download selected files** provides individual browser
download links; the browser handles file bytes directly. Cloud downloads require
your dashboard session and use short-lived attachment links with no referrer.
Browser restart recovery depends on your browser; use the generated CLI pull
command for expri's durable resume. Enter the path to an existing service client
configuration on the computer running that command. No token is copied from the
browser into the command. Private inputs and checkpoint uploads remain CLI
operations.

The hosted dashboard listens for small server-sent revision notifications and
checks for changes every five seconds as recovery. Notifications carry no file
bytes; changed views fetch their data through the existing incremental requests.
Saved revisions and stream lengths make unchanged checks independent of S3. Changed views fetch bounded previews and preserve selection, filters,
the active tab, zoom, and hidden curves. **Auto refresh** pauses updates;
hidden/offline pages pause automatically, and connection failures retry more
slowly while keeping the current view. Configure automatic publishing or run
`push --watch` for live forwarding; dashboard refresh does not initiate an upload.
The run detail displays archive progress separately from training status, and
provides an authenticated **Download archive** link after upload succeeds.
A recovery export is visibly marked as a partial archive.

Browser access is read-only. Sign-in issues an eight-hour Secure, HttpOnly,
SameSite=Strict cookie; the browser never receives an owner/worker API token.
Logout revokes the session. Restart the service after changing its password;
restarts invalidate existing sessions. The CLI continues to use its existing
bearer authentication.

Keep the backend on loopback behind HTTPS. The reverse proxy must preserve the
browser's `Host`, `Origin`, `Cookie`, and `Sec-Fetch-Site` headers. Keep the
dashboard's `Referrer-Policy: same-origin` response header: `no-referrer` makes
native login/logout forms send `Origin: null`, which the service rejects.
If forwarding an explicit header list, also retain `Authorization` and
`Content-Type` for CLI requests. Buffer request bodies, use HTTP/1.0 upstream,
and strip `Expect` so the service receives fixed-length requests. Configure per-client rate limiting
for `/login`; a limit of ten requests per minute with a small burst is suitable
for this single-user setup. Do not cache authenticated responses or log request
bodies, credentials, or signed S3 URL queries.

Allow long-lived `/api/events` responses: disable proxy buffering and use a read
timeout above the 15-second heartbeat interval. For nginx, add this location
alongside your existing proxy configuration, preserving the same auth headers:

```nginx
location = /api/events {
  proxy_pass http://127.0.0.1:8787;
  proxy_set_header Host $host;
  proxy_buffering off;
  proxy_read_timeout 60s;
}
```

Live connections have a separate pool capped at eight; they do not occupy the
four ingestion request workers. Hidden/offline pages and paused refresh close
their connection. Polling continues if notifications are unavailable.

Hosted previews are bounded for small servers: the source catalog shows up to
1,000 project/worker sources, browsing and filters cover the 500 runs most
recently updated in the service per source, and cold overview reads have a
30-second time budget. Column discovery and sorting cover that same selection;
sorting does not expand it to older runs. Runs start ordered by their start
time, with parameter, metric, run ID, and status sorting available through table
headers. Existing catalogs reconstruct update order from upload records and show a
warning until fresh uploads establish service activity. Overview records are
cached against the run-state file digest or tracking revision. Warnings identify
missing or incomplete previews; refresh to retry. New tracking runs use the
incremental SQLite projection, including files larger
than 16 MiB. Original timestamps and repeated or reset steps are preserved.
Malformed or oversized metric rows stay in the raw file and produce preview
warnings. Charts retain bounded sampled points, and log tails read at most
64 KiB. Last/min/max summaries use every valid point. Legacy S3 metric previews
retain their 16 MiB limit; pull larger legacy files for local review.
The local CLI remains available for complete files and older runs.

For main and branch UIs sharing this service, see [dashboard deployments](deployment.md).

## Worker uploads

Create a client configuration on the worker, outside the synced source repo:

```toml
url = "https://expri.example.net"
token_env = "EXPRI_SERVICE_GPU_1_TOKEN"
```

To publish each recorded run automatically, add this to the project's
`expri.toml` (or `[target.gpu.service]` for a particular worker):

```toml
[environment]

[service]
client_config = "/etc/expri/worker.toml"
project_id = "vision"
origin = "gpu-1"
dashboard_url = "https://expri.example.net/"
```

`client_config` is an absolute path on the machine executing the run. Keep that
file outside the source checkout and provide its token through the named worker
environment variable. The run stores the file reference and scope, never the
token or signed URLs. Targets inherit the complete top-level service configuration
unless they supply their own service table. Use distinct origins for workers.
`dashboard_url` is optional and must be a public HTTP(S) root URL without credentials,
query parameters, or fragments.

`expri run train` and `expri run --detach train` start an independent native
publisher before environment preparation. It forwards metadata, metrics, and
logs, including preparation failures and cancellation. The task's exit code is
independent of publishing: service outages leave queued work retrying after
training finishes. Checkpoints are still selected explicitly. A lost supervisor
is recorded as `lost` once its run lease is released; surviving unsupervised
children are not considered a completed experiment.

Inspect publishing separately from the task result:

```sh
expri runs status run-abc123
expri runs show run-abc123
expri service resume --run-dir .expri/runs/run-abc123
```

`service resume` runs on the worker and restarts publishing from its saved intent
and queue after a publisher stops or a configuration/token problem is corrected.
It does not rerun training. The queue lives at the original checkout's
`.expri/service-sync`; keep it and the original run files until publication is
acknowledged. Publishers survive terminal closure and task completion, but must
be resumed after a worker reboot. For new tracking runs, `synced` means all
terminal tracking bytes were acknowledged and the archive job was accepted;
it does not mean the independent S3 archive upload has finished. Archive
failures retain tracking data and retry without restarting training. For legacy
queues, `synced` retains its original upload-completion meaning. A completed task
alone does not imply that its results reached the service. The optional dashboard link opens the run after sign-in and waits for
its first publication.

Automatic publishing requires a configured environment (an empty table selects
ordinary uv execution), a Unix native expri worker advertising `run-publishing-v1`,
and no Python fallback. Upgrade older workers or use `expri run --no-publish train`
for an individual run. Runs without service configuration keep their existing
execution and fallback behavior.

For existing runs, live publishing can also be started explicitly:

```sh
expri service push --config worker.toml \
  --project-id vision --origin gpu-1 \
  --run-dir .expri/runs/run-abc123 --watch
```

The uploader checks every five seconds and forwards metadata snapshots,
parameters, complete metric rows, and stdout/stderr bytes. It sends appended
bytes in batches of at most 64 KiB. The server syncs original bytes to disk and
commits offsets and metric indexes before acknowledging a batch. Retries compare
overlapping bytes rather than duplicating them. Terminal publication also sends
an unfinished final metric line and captures exact file revisions and lengths.
The training process does not make service requests.

If a training machine disappears, the server can recover its last acknowledged
prefix. Data still waiting on the worker can be lost. Pull the received files
normally, or use an owner configuration to create a recovery archive:

```sh
expri service archive --config owner.toml --project-id vision --origin gpu-1 \
  --run-id run-abc123 --partial
```

`--partial` captures received document versions and stream lengths without
sealing a still-active run or changing its training status. Omit `--partial` to
archive a terminal run. Normal archives require current revisions and lengths;
refresh and retry if they changed during the request. Partial exports can use
captured completed document revisions and stream prefixes while publication
continues. Network silence does not mark a run as lost or trigger a partial
archive.

The server builds `result.zip` from its acknowledged files and includes a
`manifest.json` identifying scope, completeness, paths, revisions, and lengths.
The ZIP contains run state, snapshot/environment metadata, parameters, artifact
inventory, metrics JSONL, stdout and stderr as available. It excludes datasets
and checkpoints. Persistent jobs and multipart receipts resume archive uploads
after a service restart or storage outage. No tracking file is deleted after
archiving.

The client negotiates `tracking-v1` and pins that protocol in its durable queue.
Existing queues that already published files remain on their legacy protocol;
older completed S3 runs remain readable. An older server's unknown-capability
response permits legacy publishing, while authentication and connection failures
leave negotiation pending for retry.

Large result files are selected explicitly after the run is terminal:

```sh
expri service push --config worker.toml \
  --project-id vision --origin gpu-1 \
  --run-dir .expri/runs/run-abc123 --artifact outputs/checkpoint.pt
```

Multipart upload IDs and acknowledged parts survive retries and service
restarts. A lost acknowledgement is reconciled with the service's stored receipt.
The client queue defaults to `.expri/service-sync`; use `--queue-dir` to keep its
location stable across invocations. A one-shot failure exits nonzero and retains
the queue. Watch mode retries service outages. Keep original output files until
upload acknowledgement; expri never removes them automatically.

Only finalized files belong in explicit artifacts. Inputs and artifacts use
ordinary IDs and one file SHA256 for integrity. The service checks successful
S3 completion and object size; downloading clients verify the full file hash.
ETags remain opaque multipart receipts. Incomplete multipart uploads and old
object revisions need an operator-selected S3 retention/lifecycle policy.
The service uses up to 1,000 parts per file, from 8 MiB to 5 GiB per part, and
accepts files up to 1,000 × 5 GiB. This keeps resumable-upload receipts below the
1 MiB control-response limit. Live stream batches are limited to 64 KiB. New
tracking runs retain raw files
and their SQLite projection after archiving. Legacy publishers continue removing
completed stream records after their individual S3 objects are published.

## Local download and dashboard

Use an owner client configuration with the same URL and the owner token's
variable name. Pull metadata, metrics, parameters, and logs by default, and
select checkpoints separately:

```sh
expri service list --config owner.toml --project-id vision --origin gpu-1
expri service pull --config owner.toml --project-id vision --origin gpu-1 \
  --run-id run-abc123 --repo .
expri service pull --config owner.toml --project-id vision --origin gpu-1 \
  --run-id run-abc123 --repo . --artifact outputs/checkpoint.pt
expri service pull --config owner.toml --project-id vision --origin gpu-1 \
  --run-id run-abc123 --repo . --artifact result.zip
expri dashboard
```

Downloads populate `results/service-vision-gpu-1/runs/run-abc123/` by default.
`--source` changes the cache source name; `--results-dir` changes its root and
should match `[download].results_dir` in the project's `expri.toml`. Downloads
stage and verify files before publication and retain artifacts selected earlier.
The existing cached CLI and dashboard read this directory without contacting
any service or worker.
Selected checkpoint downloads retry bounded 8 MiB ranges and save progress under
`results/<source>/.service-pull/<run_id>/`, outside the visible review cache.
After an interruption, run the same `service pull` command again. Unchanged
object records continue from their last durable range; new signed URLs are
obtained for each request, and the existing full-file SHA256 check must pass
before publication. Versioned tracking metadata refreshes when its revision
changes; tracking
metrics and logs append from their durable saved offsets, including across
successful pulls. Legacy mutable metadata and logs refresh on each invocation.
Previous cached files remain available during transfer, and checkpoints selected
earlier stay in the cache. Pull reports include resumed file/byte counts. Keep
the private staging directory to retain interrupted download progress; no tokens
or signed URLs are saved there.

The publisher reports regular output file names and sizes in the reserved
`outputs/.expri-artifacts.json` metadata file. It does not upload those file
contents unless explicitly selected. Inventory discovery skips hidden files,
symlinks, and environment/cache internals, and is limited to 200 files, 64 KiB
of metadata, and a bounded directory scan. The dashboard warns when the list is
incomplete. Keep the reserved file for expri; it is downloaded as metadata and
excluded from the user-facing artifact list. If outputs are read-only or linked,
inventory generation is skipped while metadata and log synchronization continue.

## Private datasets and files

Input IDs are immutable within a project. Publish a changed dataset with a new
ID rather than replacing bytes under the old ID:

```sh
expri service input put --config owner.toml --project-id vision \
  --input-id dataset-v1 --file /private/data/train.bin
expri service input get --config worker.toml --project-id vision \
  --input-id dataset-v1 --destination .expri/inputs/dataset-v1.bin
```

Pass the downloaded path to the experiment as needed. Keep input files outside
Git and code snapshots. This version handles datasets/files; credentials and
secret injection are outside its scope.

## Acceptance tests

```sh
python3 -B tests/containers/service_workflow.py
# Reuse built expri and S3 fixture images:
python3 -B tests/containers/service_workflow.py --no-build
```

The acceptance suite uses separate host, worker, service, S3, browser and fault-proxy
containers on an internal network. No host ports or project volumes are shared.
The fixture builds pinned official MinIO source; set `EXPRI_TEST_S3_IMAGE` to use
an existing compatible MinIO image instead. Image builds require network access;
the acceptance workflow itself uses only the internal network.
It runs an actual uv experiment with fake installed Torch and a private input,
starts publishing automatically, interrupts the service during training, and
loses metric and multipart part acknowledgements. It kills/resumes the publisher
from its saved queue, restarts the service, resumes the selected checkpoint, and
reviews downloaded data offline. It verifies independent server ZIP completion,
original tracking bytes without per-file S3 uploads, recovery of an acknowledged
prefix while the worker is offline, and owner-requested partial archives. Failed and cancelled runs also finish publishing
their original task status and logs without selecting a checkpoint.
An isolated Firefox image submits the native login and logout forms over an
internal HTTPS fixture. It reproduces the rejected null origins under
`no-referrer`, then checks successful forms under `same-origin`, secure cookie
flags and session revocation. The test browser accepts the generated certificate
through its test-only WebDriver configuration. The browser also reviews the
uploaded runs: direct selection/comparison, keyboard tabs, deferred log loading,
refresh and filters. It checks parameter and metric checkbox columns, table
summary tags, native sortable headers, and retained review state. It verifies
desktop and 320/360/500 CSS pixel layouts, including table scrolling without
page overflow, saving screenshots for diagnostics. A second HTTPS hostname also
checks branch assets against the same catalog, cookie replay rejection, and
independent login/logout sessions.
An authenticated run link also survives native password sign-in, opens the exact
run, and lets the user select another run without later refresh overriding it.
Both hosts also exercise chart hover, drag zoom, legend toggles, and keyboard
inspection through native browser input. The checks preserve repeated samples,
verify chart script blocking, and require no additional metric requests during
interaction. Narrow layouts and chart reloads retain working controls.
A separate live-update check publishes appended bytes from an active fixture
through the real CLI while Firefox stays open. It verifies five-second probes, unchanged
snapshots skipped, new samples and summaries, drag deferral, preserved zoom and
hidden curves, pause/resume, live log tails, and retained script blocking.
Browser request logs contain method, path, origin, status and response policy,
without credentials or cookies.
