# Optional self-hosted experiment storage

Expri keeps execution and review local-first. Workers write the existing run
files, and a separate uploader forwards them to an optional service. The service
keeps acknowledged JSONL, logs, and versioned metadata on its own disk and
incrementally indexes metrics in SQLite. After a run finishes, it bundles the
received tracking files into `result.zip` in S3-compatible storage. Checkpoints
and private inputs remain separate S3 objects. Service outages leave training
running. Required private inputs must already be downloaded before an offline
run can start.

Native runs lazily start one shared worker service for the current user and
storage root. It collects system observations and schedules each run's durable
publication job with isolated credentials. Explicit
service commands remain available for existing runs and selected checkpoints.
The optional shared resource queue schedules frozen submissions across explicitly
configured execution workers. Existing direct runs and publishing remain available.

Each transfer has a direction and a named action:

| Data | Direction | Action |
| --- | --- | --- |
| Source and config | Laptop → worker checkout | `expri -T gpu-1 push` |
| Frozen queued source | Laptop → S3 → execution worker | `expri run --queue TASK` |
| Live tracking and registered checkpoints | Worker → server/S3 | Shared worker service, or `expri service publish` |
| Completed private input files | Owner → S3 | `expri service input upload` |
| Required private input files | S3 → worker asset cache | `expri assets add`/`download` and automatic run preparation |
| Tracking bundle (`result.zip`) | Server → S3 | Automatic server upload, or `expri service upload` |
| Run metadata and selected outputs | Server/S3 → laptop | `expri fetch --watch` |

For publishing, `--watch` observes the durable job and Ctrl-C stops only that
observer. Source pushing and result fetching run separately;
each has its own retry loop. The server coordinates file transfers, while signed
URLs let workers and laptops transfer large file bytes directly to or from S3.

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
The configured owner token and separately issued CLI login credentials have
unrestricted API access. Worker and machine credentials retain their bound scopes.

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
original tracking files under `tracking/`, staged result ZIPs under `archives/`, and
multipart receipts. The S3 bucket alone does not replace this directory.
Server uploads retain the hot files and SQLite data. Archived runs are removed
after their 15-day retention period; project deletion also removes hosted data.
Only one service process may use a data directory. Keep the storage
configuration stable for that directory. S3 endpoints used in signed URLs must
be reachable from the workers and laptops.

Opening this version upgrades the SQLite schema to version 8. Older binaries
refuse that directory because they cannot preserve durable queue allocations,
worker fencing, smoke archival, asset provenance, and run retention. Back up the entire
service directory before upgrading; restoring that backup is required for a
backend downgrade. Existing S3 runs remain readable after the upgrade.

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
The dashboard discovers published projects and provides run details,
parameters, metric comparisons, charts, bounded log tails, and a **Files** tab.
Select **Storage** in a project to browse completed cloud objects. Its
**Private inputs** list shows input IDs and sizes, while **Run outputs** shows
uploaded files, including checkpoints, with their machine and run. Search and
page within either list. Both lists come from finalized service records; the
browser does not list the bucket or read file contents to build them.
Choose a project at the top of the page. **All machines** initially shows its
runs across recorded publishing origins. The **Machine** tags filter that
history, and the built-in **Machine** column sorts by origin. A machine label
comes from the origin saved with a run; no additional registration is needed
for existing history to appear. Select runs across machines to compare them.
Equal run IDs from different origins remain distinct, with machine provenance
shown in the comparison. Logs, files, browser downloads, and generated fetch
commands retain each run's actual project, origin, and run ID. Existing worker
run links open the same exact run within the project view.
Choose up to eight parameter or metric columns using the **Columns** checkbox
tags. Click table headers to sort before paging, and use the table's
**Last / Min / Max** tags to choose its metric values. Missing values
stay at the end in either sort direction. At widths of at least 860 CSS pixels,
the project run browser and review remain side by side, including when columns
are added. The run browser widens and contains the table's horizontal scrolling.
Below 860 CSS pixels, the run browser stacks above the review with scrolling
still contained in **Runs**. Sorting and column changes preserve the current
review; these choices last for the current page.
Workers must publish a run before it appears. Files shows output names, sizes, and
availability without reading checkpoint contents. **Worker (reported)** reflects
the last published inventory; **Cloud** means a finalized file is downloadable.
The per-run **Files** tab can therefore show a checkpoint reported on its
worker before it appears in project **Storage**. Storage shows only completed
cloud objects for the selected project, across all its machines and runs.
The **Transfer status** column distinguishes **Registered**, **Uploading**,
**Available in cloud**, and **Needs attention**. Registration records the
completed checkpoint locally; the background publisher handles hashing and
transfer. A temporary upload failure shows **Retrying upload** with a concise
reason. An upload reported by the worker is not shown as cloud availability
until the service has a completed file record. Optional **best** and **latest**
labels appear beside immutable checkpoint names; moving a label does not copy
or upload the checkpoint again.
Labels marked **pending** still refer to a registered checkpoint awaiting cloud
confirmation. If an older cloud file exists at the same path, its download
remains available while the transfer status stays registered or uploading; it does
not confirm the label until its recorded size and SHA256 match the registration.
The hosted page cannot inspect your laptop's disk. Local dashboards show files
actually present in the run or review cache; cached cloud availability is a
record of the last fetch rather than a live storage check.
**Downloaded locally** requires a saved successful download receipt matching the
current recorded object and the local file's size and modification time.
The dashboard reads metadata rather than checkpoint contents. If a local file
has changed or its cloud object has been replaced, its local location remains
visible while it loses the downloaded status. Legacy caches without these
receipts show local presence without claiming a verified download.

Select up to 64 files. **Download selected files** provides individual browser
download links; the browser handles file bytes directly. Cloud downloads require
your dashboard session and use short-lived attachment links with no referrer.
Browser restart recovery depends on your browser; use the generated CLI fetch
command for expri's durable resume. It uses the computer's global server connection;
optionally enter a client configuration path to override it. No token is copied from the
browser into the command. Browser downloads in Storage use the same authenticated
attachment flow. For large checkpoints, use the generated CLI fetch command for
durable resume. Private input and checkpoint uploads remain CLI operations.

The hosted dashboard listens for small server-sent revision notifications and
checks for changes every five seconds as recovery. Notifications carry no file
bytes; changed views fetch their data through the existing incremental requests.
Saved revisions and stream lengths make unchanged checks independent of S3.
Storage refreshes the current search and page when an input or output is
published; unchanged checks skip fetching the file list. Changed views fetch
bounded previews and preserve selection, filters, the active tab, zoom, and
hidden curves. **Auto refresh** pauses updates;
hidden/offline pages pause automatically, and connection failures retry more
slowly while keeping the current view. Configure automatic publishing or run
`expri service publish --watch` for live forwarding; dashboard refresh does not initiate an upload.
The run detail displays server upload progress separately from training status, and
provides an authenticated **Download result.zip** link after upload succeeds.
A recovery export is visibly marked as a partial result.

The primary dashboard allows archiving and restoring finished runs. Project
deletion requires explicit configuration. Preview dashboards remain read-only.
CLI login credentials have unrestricted API authority, including project deletion;
`dashboard.allow_project_deletion` governs the browser controls.
Sign-in issues an eight-hour Secure, HttpOnly,
SameSite=Strict cookie; the browser never receives an owner/worker API token.
Logout revokes the session. Restart the service after changing its password;
restarts invalidate existing sessions. The CLI continues to use its existing
bearer authentication, including separately issued CLI login tokens.

Keep the backend on loopback behind HTTPS. The reverse proxy must preserve the
browser's `Host`, `Origin`, `Cookie`, and `Sec-Fetch-Site` headers. Keep the
dashboard's `Referrer-Policy: same-origin` response header: `no-referrer` makes
native login/logout forms send `Origin: null`, which the service rejects.
If forwarding an explicit header list, also retain `Authorization` and
`Content-Type` for CLI requests. Buffer request bodies, use HTTP/1.0 upstream,
and strip `Expect` so the service receives fixed-length requests. Configure per-client rate limiting
for `/login` and `/v1/login`; a limit of ten requests per minute with a small burst is suitable
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
1,000 recorded project/worker sources. **All machines** covers up to 500 runs most
recently updated in that project across machines. Selecting a machine chooses
its own 500 most recently updated runs before applying search, task, status, or
sort choices. Cold overview reads have a 30-second time budget. Sorting happens
within the chosen history window before paging; it does not expand that window
to older runs. Column discovery uses the unfiltered project window. Runs start ordered by their start
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
retain their 16 MiB limit; fetch larger legacy files for local review.
The local CLI remains available for complete files and older runs.

For main and branch UIs sharing this service, see [dashboard deployments](deployment.md).

## Worker uploads

Create a client configuration on the worker, outside the pushed source repo:

```toml
url = "https://expri.example.net"
project_id = "vision" # Required by repository-relative private asset references.
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

`expri run train` and `expri run --detach train` start or reuse the shared worker
service before recording machine/capture metadata and preparing the environment.
Local readiness waits for a committed machine observation, independent of
network and credentials. Startup failure warns and records an error while
training continues. The service forwards metadata, metrics, and
logs, including preparation failures and cancellation. The task's exit code is
independent of publishing: service outages leave queued work retrying after
training finishes. Training registers finalized checkpoints for live upload.
A lost supervisor
is recorded as `lost` once its run lease is released; surviving unsupervised
children are not considered a completed experiment.

Inspect publishing separately from the task result:

```sh
expri runs status run-abc123
expri runs show run-abc123
expri service resume --run-dir .expri/runs/run-abc123
```

`service resume` runs on the worker and retries the matching saved job and queue.
It does not rerun training or create a duplicate job. Endpoint, credentials and
upload options are pinned at registration; conflicting registration is rejected.
Each local run binds to one publishing destination because checkpoint handoffs
and publisher status belong to that run.
Only the required token is snapshotted in private worker storage outside the
checkout. The service never swaps process environment variables between jobs.
The queue lives at the original checkout's
`.expri/publish`; keep it and the original run files until publication is
acknowledged. The shared service survives terminal closure and task completion.
After reboot, the next run, publication, or `service start` recovers saved jobs;
expri does not install an OS startup service. For new tracking runs, `published` means all
terminal tracking bytes and registered checkpoints were acknowledged and the server upload job was accepted;
it does not mean the independent result ZIP upload has finished. Upload
failures retain tracking data and retry without restarting training. For legacy
queues, `published` retains its original upload-completion meaning. A completed task
alone does not imply that its results reached the service. The optional dashboard link opens the run after sign-in and waits for
its first publication.

Automatic publishing requires a configured environment (an empty table selects
ordinary uv execution), a Unix native expri worker advertising `run-publishing-v1`,
and no Python fallback. Upgrade older workers or use `expri run --no-publish train`
for an individual run. Runs without service configuration keep their existing
execution and fallback behavior.

Deploy the updated server before upgrading workers to automatic collection.
Its archive limit includes all nine supported tracking files, including the
system capture, so complete native runs can seal and upload their result bundle.

Manage the shared worker process on the worker itself:

```sh
expri service start
expri service start --foreground
expri service status
expri service stop
```

`start` is idempotent in background mode; `--foreground` refuses a duplicate.
`status` never starts the service or creates files. `stop` leaves training alive
and preserves queued work. The next run or publishing request starts it again.
The process stays running while idle until explicitly stopped. Collection,
machine-history publication and run publication have separate status and loops.
Machine publishing needs its own configuration and scoped token; run tokens
alone do not enable it. See [system profiles](system-profiles.md).

For existing runs, publication can also be queued explicitly:

```sh
expri service publish --config worker.toml \
  --project-id vision --origin gpu-1 \
  --run-dir .expri/runs/run-abc123
```

This registers ongoing publication through terminal state and capture drain,
then returns a durable receipt. Add `--wait` to wait quietly or `--watch` to show
progress until completion. Ctrl-C stops observation while the shared service
continues uploading. Add `--json` for a structured receipt or final report.
Waiting exits unsuccessfully if a job is rejected or the service stops before
publication completes; the report preserves its saved status.

The uploader checks every five seconds and forwards metadata snapshots,
parameters, complete metric rows, and stdout/stderr bytes. It sends appended
bytes in batches of at most 64 KiB. The server syncs original bytes to disk and
commits offsets and metric indexes before acknowledging a batch. Retries compare
overlapping bytes rather than duplicating them. Terminal publication also sends
an unfinished final metric line and captures exact file revisions and lengths.
The training process does not make service requests.

Register a checkpoint only after closing the file (and any writer process):

```sh
"$EXPRI_BIN" artifact register "$EXPRI_OUTPUT_DIR/checkpoint-1000.pt" --label best --label latest
expri artifact list --run-dir .expri/runs/run-abc123
```

`--run-dir` defaults to the task's `EXPRI_RUN_DIR`. Relative paths start with
`outputs/` and are relative to the run directory. Registration saves a small
local outbox; it does not read checkpoint contents, hash them, or contact the
service. The independent publisher hashes and uploads through its existing
resumable multipart queue. Object transfers have a separate lane so a slow
checkpoint does not delay metrics and logs. Registrations made while offline
wait for the publisher to reconnect or resume.
`EXPRI_BIN` identifies the executing native worker binary, including a custom
`target.node_bin` outside the task's PATH.

Keep each registered file unchanged until its status is **cloud**. Use a new
filename for each completed checkpoint. `best` and `latest` are movable aliases
in the inventory; they select existing files and create no extra S3 objects.
Changing a registered file produces **needs attention**, which prevents the
publisher from claiming completed publication. Register the corrected file under a new
path, then explicitly remove the failed handoff with
`expri artifact unregister outputs/bad.pt --run-dir .expri/runs/run-abc123`.
This leaves the file, cloud objects, and saved multipart receipts untouched.
Active and successful registrations cannot be unregistered.

The inventory closes before terminal result sealing. Exact repeated
registrations remain idempotent, but adding files or moving aliases after
closure is rejected. Use `service file-upload` for post-run uploads. A training
exit does not discard pending checkpoint transfers.

If a training machine disappears, the server can recover its last acknowledged
prefix. Data still waiting on the worker can be lost. Pull the received files
normally, or use an owner configuration to request a recovery upload:

```sh
expri service upload --project-id vision --origin gpu-1 \
  --run-id run-abc123 --partial
```

`--partial` captures received document versions and stream lengths without
sealing a still-active run or changing its training status. Omit `--partial` to
upload a terminal run. Normal uploads require current revisions and lengths;
refresh and retry if they changed during the request. Partial exports can use
captured completed document revisions and stream prefixes while publication
continues. Network silence does not mark a run as lost or trigger a partial
upload.

The server builds `result.zip` from its acknowledged files and includes a
`manifest.json` identifying scope, completeness, paths, revisions, and lengths.
The ZIP contains run state, snapshot/environment metadata, parameters, artifact
inventory, metrics JSONL, stdout and stderr as available. It excludes datasets
and checkpoints. Persistent jobs and multipart receipts resume result ZIP uploads
after a service restart or storage outage. Tracking files remain after upload
until run retention or project deletion removes them.

The client negotiates `tracking-v1` and pins that protocol in its durable queue.
Existing queues that already published files remain on their legacy protocol;
older completed S3 runs remain readable. An older server's unknown-capability
response permits legacy publishing, while authentication and connection failures
leave negotiation pending for retry.

For a run without a saved publication job, select finalized result files at
its first manual registration. Selections remain pinned for subsequent retries:

```sh
expri service publish --config worker.toml \
  --project-id vision --origin gpu-1 \
  --run-dir .expri/runs/run-abc123 --artifact outputs/checkpoint.pt
```

An automatically registered run already has a saved job. Adding `--artifact`
to a later `publish` request conflicts with that job's options. Register live
checkpoints before its inventory closes, or upload additional files afterward
with their original run scope:

```sh
expri service file-upload --config worker.toml \
  --target '{"kind":"run","scope":{"project_id":"vision","origin":"gpu-1","run_id":"run-abc123"},"path":"outputs/checkpoint.pt"}' \
  --file .expri/runs/run-abc123/outputs/checkpoint.pt
```

`file-upload` uploads the selected file independently of the saved run job and
does not change its artifact selection or seal another result bundle.

Multipart upload IDs and acknowledged parts survive retries and service
restarts. A lost acknowledgement is reconciled with the service's stored receipt.
The client queue defaults to `.expri/publish`; use `--queue-dir` to keep its
location stable across invocations. Accepted jobs retain their queues and retry
service outages independently of the CLI observer. Registration errors exit
nonzero; accepted work survives a startup failure and its receipt includes a
warning. Keep original output files until
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
and their SQLite projection after uploading. Legacy publishers continue removing
completed stream records after their individual S3 objects are published.

## Local download and dashboard

To continuously receive metadata and selected checkpoints, configure the
laptop's client separately from the worker's publishing credentials:

```toml
[fetch]
client_config = "/home/me/.tokn/expri/config.toml"
project_id = "vision"
origins = ["gpu-1", "gpu-2"]
labels = ["best", "latest"]
# artifacts = ["outputs/checkpoint-1000.pt", "result.zip"]
```

```sh
expri fetch --watch
expri -T gpu-1 push --watch
```

`fetch` uses the listed service origins and runs on the laptop. `push` uses a
selected worker target and updates its source/config checkout. Source changes
do not modify a running experiment's frozen snapshot. `fetch` never pushes
source changes, and `push` never downloads service results. Path-scoped source
transfers cannot be combined with `push --watch`.

The watcher polls every five seconds, slows retries during outages, and keeps
verified local files available. Missing labels or checkpoints are pending until
the completed matching object arrives; partial bytes never enter the review
cache. Unchanged catalogs skip transfers; restarting reuses verified checkpoint
files and durable tracking offsets. The watcher retains previously selected
files when aliases move. Stop it with Ctrl-C; training and publishing continue.
Omit `--watch` for one fetch pass, or add `--dry-run` to inspect selections without
publishing files locally. Dry-run reports identify labels that need the run's
inventory to resolve; they do not treat those labels as unavailable files.
Reports are bounded and watch summaries stay compact.

## CLI report formats

User-facing reports use tables by default. Add `--json` to query, service,
system, queue, config, login/logout, asset, artifact, and fetch commands for structured
output. Existing runs and metrics commands also support `--json`. Scripts
that parse command output must select this flag explicitly. For a detached
start receipt, use `expri run --detach --json TASK`; put these options before
the task name. `--quiet` suppresses human reports, while explicit JSON remains
available. Foreground task output and logs continue to stream normally.

## Shared resource queue

Queue a task with an explicit memory budget and a configured environment:

```sh
expri run --queue --project-id vision --cpus 4 --memory 16GiB --gpus 1 train
expri queue list --project-id vision
expri queue show job-abc123 --json
expri queue cancel job-abc123
expri queue workers --json
```

CPU defaults to one logical slot and GPUs to zero. Task-level
`[tasks.train.resources]` accepts `cpus`, `memory`, `gpus`, `gpu_ids`, `gpu_model`,
and `gpu_memory`; CLI values override the corresponding defaults. These apply
only to queued runs. Memory sizes accept positive integers with binary units
such as `GiB` or decimal units such as `GB`. `--project-id` can default from the
selected project's `[service]`. `--worker gpu-1` pins placement; `-T gpu-1` also
uses that target's environment and pins its name, without an SSH source push.

Each submission uploads a verified source ZIP through the existing private-input
path before creating a durable job. The compressed and extracted source are each
limited to 512 MiB and 100,000 entries. Large datasets and models should use managed
assets. The job pins the uploaded object and SHA256 rather than reading later
checkout changes. Once submission returns, the laptop may disconnect. Retrying an
uncertain response with the same `--submission-id` and identical source/specification
returns the same job; changing that submission is rejected.
Source selection includes `push.include_ignored` files, like ordinary managed
local snapshots. `--no-push` and SSH control options cannot be combined with
`--queue`, which always submits its own frozen source.

On each execution machine, authenticate with `expri login` and add its explicit
budget to `~/.tokn/expri/config.toml`:

```toml
[queue_worker]
worker_id = "gpu-1"
project_ids = ["vision"]

[queue_worker.capacity]
cpus = 16
memory_bytes = 68719476736

[[queue_worker.capacity.gpus]]
device_id = "GPU-your-device-uuid"
model = "NVIDIA A100"
memory_bytes = 42949672960
vendor = "nvidia"
selector = "GPU-your-device-uuid"
```

Keep the existing `[server]` URL/token in this file. CPU-only workers can omit the
GPU entries. `worker_id` must be unique, and `project_ids` is an explicit execution
allowlist. Capacity is the operator's schedulable budget, not free memory or CPU
utilization from system monitoring. Start the native Linux or macOS worker in the foreground:

```sh
expri queue daemon
```

This command enrolls the worker, renews its lease, and starts fitting jobs
automatically. CPU and memory reservations govern these queued jobs and do not
install operating-system limits. GPUs are reserved as whole devices and exposed
with driver selectors: NVIDIA uses `CUDA_VISIBLE_DEVICES`, AMD uses HIP/ROCR
selectors, and Apple GPUs use a cooperative whole-device reservation. Declare
NVIDIA UUIDs or AMD driver selectors accurately. Apple shared memory is part of
the machine budget; omit GPU `memory_bytes`. Multi-CPU and multi-GPU workers are
supported. GPU model matching is case-insensitive substring matching; `--gpu-memory`
requires that amount of dedicated memory on every assigned device. A CUDA-required
environment needs a GPU request and is placed only on NVIDIA devices.

Queued supervisors clean up their owned process groups before acknowledging a
terminal run. Workloads must keep children in these groups; processes that
deliberately escape with a new session/group are outside this cooperative
boundary. An unverified cleanup keeps the allocation uncertain.

Workers claim the oldest fitting job atomically, holding all resources during
source download, environment preparation, and execution. A job that does not fit
remains queued with a reason in `queue list`; smaller fitting jobs may pass it.
There is no preemption, multi-machine job, or automatic execution retry in this
version. Queue operations require unrestricted configured or issued login tokens;
existing scoped publishing and machine-profile tokens cannot enroll an executor.

The receipt contains a job ID. An actual run ID is linked after worker preparation,
and its normal run files, publishing, and smoke archival rules apply. Publishing
defaults on unless disabled by `--no-publish` or the selected service configuration.
Use queue commands for waiting work, and the existing dashboard/query commands for
published runs. Queue jobs themselves are currently visible through the CLI.

Worker state, claim receipts, and launch journals live under
`~/.tokn/expri/queue/`; existing project storage is not moved. A daemon restart
reconciles known runs without launching them again. Losing a 60-second worker lease
marks its attempts uncertain and keeps their reservations, because a disconnected
process may still run. Known supervisors can reconcile their linked run. If a
supervisor or journal is lost, independently stop and verify the workload before:

```sh
expri queue recover attempt-abc123 --confirmed-stopped
```

Recovery marks the attempt lost and releases its reservation; it does not rerun the
job. Cancelling an active job requests cooperative termination and retains its
reservation until the worker acknowledges completion. Deleting a project cancels
waiting jobs and requests cancellation of active ones, retaining active allocation
records until completion or confirmed recovery.

## Configure the CLI's server connection

Authenticate using the password for the primary hosted dashboard:

```sh
expri login --url https://expri.example.net
expri config server show
```

The interactive prompt hides the password. For automation, supply one password
line through a pipe or redirected file with `--password-stdin`:

```sh
printf '%s' "$EXPRI_DASHBOARD_PASSWORD" | expri login --url https://expri.example.net --password-stdin
```

Login exchanges that password for an independently issued API token with
unrestricted server access. The password is never saved. The token is stored in
`[server]` in `~/.tokn/expri/config.toml`, with a private directory and a
`0600` configuration file on Unix; command output and `config server show` hide it. Updates
are atomic and preserve system-profile settings. A concurrent connection change
during authentication prevents login from overwriting the newer selection. If
login reports that the token was saved but durability could not be confirmed,
inspect `expri config server show` before retrying; the saved token is not revoked.

Once a URL is saved, `expri login` defaults to it. Login and logout run on the
current machine and accept `--storage-dir`; `EXPRI_STORAGE_DIR` also selects the
global root. `expri logout` revokes the current issued credential and clears its
saved token while retaining the URL and project selection for the next login.
Other clients' issued tokens and browser sessions remain active.

Issued API tokens have no expiry and survive server restarts. Restart the service
after rotating the dashboard password, unrestricted API token, or primary
dashboard authority; this invalidates previously issued credentials. Disabling
the primary dashboard at startup also revokes them. Restoring an older password
or API secret does not resurrect invalidated tokens. An already
rejected or revoked credential can be cleared by logout with a warning. If
revocation cannot be confirmed because of another server or transport error,
logout keeps the saved credential for retry. Static server/worker tokens cannot
be revoked through logout; use `expri config server unset` to remove a manually
configured connection locally.

Upgrade the server to enable `POST /v1/login` and `POST /v1/logout`. The primary
configured dashboard host accepts these endpoints; preview and unknown hosts do
not. The reverse proxy must forward these paths and preserve `Host`,
`Content-Type`, and `Authorization` for logout. Login does not use browser
cookies. The server allows ten login attempts per minute and at most 256 active
issued credentials; reaching the limit rejects a new login without evicting
other clients. Revoke unused credentials before creating more.

Manual API tokens and existing environment-based client files remain supported.
A manually configured global connection looks like:

```toml
[server]
url = "https://expri.example.net"
token = "your-server-token"
```

Use `expri config server set --url URL --token-stdin` with a supplied token
through a pipe or redirected file. `expri config server unset` removes only the
connection and preserves system settings; it does not revoke a remote token.
`config server show` reports a retained URL without credentials using
`token_configured: false` and `token_source: null` in `--json` output. Inspection of missing
configuration creates no files.

Query and service client commands use this connection by default. `--config PATH`
overrides it with another global config or a standalone client file containing
`url` and exactly one of `token` or `token_env`. A global `[server]` may retain
only a URL after logout; authenticate again before issuing client requests.
The server determines permissions from the token. `service serve --config`
still selects the server daemon's configuration. Remote run publishing keeps
its explicit worker-local client path. Read `expri skill login` for the agent
workflow.

## Browse published runs and files

After configuring the global server connection, browsing
on the current machine requires no `expri.toml` or project checkout and
does not download run bytes or populate a local review cache:

```sh
expri query runs
expri query runs --project-id vision
expri query runs --project-id vision --origin gpu-1 --archival all
expri query files --run-id run-abc123
expri query files --run-id run-abc123 --prefix outputs/
```

The server must advertise `service-catalog-v1`. Credentials with unrestricted
server access see all projects and origins by default; optional `--project-id` and `--origin` narrow
the selection. Worker credentials see only their bound project and origin;
machine-profile credentials cannot browse runs. Run lists select active runs by default.
`--archival archived` selects archived runs still within their restore window;
`--archival all` selects both. Expired/deleting runs are unavailable.

File queries need only `--run-id` when exactly one accessible run has that ID.
Resolution includes active runs and archived runs within their restore window.
If multiple scopes match, the command reports the conflict; add `--project-id`
and/or `--origin` to narrow the selection. Providing both selects the scope
directly. Resolution uses the existing `service-catalog-v1` catalog and may page
through newer runs; supplying both scope fields avoids that lookup for large catalogs.

Run pages include the full `scope`, task, last published execution status,
timestamps and bounded warnings when metadata is missing or unavailable. A run
ID can appear on more than one worker, so keep its project and origin with it.
Results order by descending run ID, then ascending project and origin; this is
a stable catalog order rather than a sort by live activity. Query the worker
with `runs status`, `runs logs`, or `runs cancel` for live supervision.
`service run status` continues to describe archival and cleanup.

Commands print tables by default; `--json` returns the complete page for
automation. Run pages default to 50 rows and file pages to 100;
`--limit` accepts 1 to 100. Pass the returned `next_cursor` through `--cursor`,
keeping the same scope and filters. File pages report their resolved project and
origin; optionally pass both on later pages to pin that scope if another run
with the same ID is published. Received catalogs support pagination beyond
the old 500-run and 1,000-file listing limits. The original scoped
`expri service list` command keeps its existing behavior.

File pages combine authoritative `received` metadata with the worker's last
published `reported` inventory. `availability` is `received`, `reported_only`,
or `received_different_version` when known sizes or checksums disagree. Received
storage identifies an object, a legacy stream, or versioned tracking data;
unsealed tracking data describes the received prefix of a growing file.
Reported files can still be waiting for upload. `best`/`latest` labels remain
pending until a received object matches the reported size and checksum.
An absent checksum cannot confirm a label or prove that two versions differ.

`--prefix` matches a literal path prefix; `%` and `_` are ordinary characters.
No checkpoint bytes or signed download URLs are returned. The reported inventory
is bounded to 200 outputs and 64 KiB; `reported_inventory_truncated` and warnings
expose incomplete or unavailable inventories. Received records still appear
when that inventory is unavailable. Use `service fetch` to download selected
files. Read the progressive agent guide with `expri skill query` and inspect
exact flags with `expri query --help`.

## Download selected server files

Use server credentials with access to the requested project. Fetch metadata,
metrics, parameters, and logs by default, and
select checkpoints separately:

```sh
expri service list --project-id vision --origin gpu-1
expri service fetch --project-id vision --origin gpu-1 \
  --run-id run-abc123 --repo .
expri service fetch --project-id vision --origin gpu-1 \
  --run-id run-abc123 --repo . --artifact outputs/checkpoint.pt
expri service fetch --project-id vision --origin gpu-1 \
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
After an interruption, run the same `service fetch` command again. Unchanged
object records continue from their last durable range; new signed URLs are
obtained for each request, and the existing full-file SHA256 check must pass
before publication. Versioned tracking metadata refreshes when its revision
changes; tracking
metrics and logs append from their durable saved offsets, including across
successful fetches. Legacy mutable metadata and logs refresh on each invocation.
Previous cached files remain available during transfer, and checkpoints selected
earlier stay in the cache. Fetch reports include resumed file/byte counts. Keep
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
expri service input upload --project-id vision \
  --input-id dataset-v1 --file /private/data/train.bin
expri service input download --config worker.toml --project-id vision \
  --input-id dataset-v1 --destination .expri/inputs/dataset-v1.bin
```

The hosted project's **Storage → Private inputs** view lists completed input IDs and
sizes and provides individual browser downloads. Expri stores the input ID,
size, and digest, not the original filename passed to `--file`; choose a
download destination when using the CLI. A worker may retrieve inputs in its
project, while publishing a new input requires a token with owner permissions.

Pass the downloaded path to the experiment as needed. Keep input files outside
Git and code snapshots. This version handles datasets/files; credentials and
secret injection are outside its scope.

For automatic preparation, add a registered input as a managed asset:

```sh
expri assets add expri://./inputs/dataset-v1 \
  data/train.bin --client-config /etc/expri/worker.toml
```

Commit `data/train.bin.expri.toml` and the generated ignore rule. Training reads
`data/train.bin` from its isolated code directory. Runs prepare the snapshotted
descriptor before environment setup and task launch, reusing verified bytes
under `.expri/assets/<sha256>/file`. Cached assets work offline and running
experiments keep their original bytes across later workspace updates. Private
cache misses prefer the executing worker's ignored `.expri/config.toml`, then
`service.client_config`. The selected client configuration must include the
project ID; neither it nor the server URL is stored in the committed descriptor.

See [managed assets](assets.md) for local/public/Hugging Face additions, deferred
downloads, output imports, explicit uploads and updates, and migration from
earlier descriptors.

Private input downloads retain range receipts beside the destination under
`.expri-input-downloads/`. Repeating a download verifies and reuses matching
bytes. An unavailable service can use the last acknowledged input record only
when the cached file still passes its size and full-file SHA256 check; rejected
credentials or a deleted project never permit that fallback. A failed download
preserves the previous destination. Keep the cache private and outside Git.
Asset preparation requires a native worker advertising `assets-v2`.
`--no-publish` disables uploads while retaining private download credentials.

### Reuse an uploaded file

The owner can reference a completed input or run output as another input or
output in the same project. The service reuses the existing S3 object without
transferring its bytes. Pin the source's recorded size and SHA256 digest; an
existing destination must already reference that same object.

For example, expose an uploaded checkpoint as a private input for later runs:

```sh
expri service reference \
  --source '{"kind":"run","scope":{"project_id":"vision","origin":"gpu-1","run_id":"run-1"},"path":"outputs/best.pt"}' \
  --target '{"kind":"input","project_id":"vision","input_id":"checkpoint-v1"}' \
  --size "$CHECKPOINT_SIZE" --sha256 "$CHECKPOINT_SHA256"
```

Set `CHECKPOINT_SIZE` and `CHECKPOINT_SHA256` to the source's saved values. References appear in
the project's Storage view and use the usual input and artifact downloads.
Workers may retrieve referenced inputs, but only the owner may create references.

To publish one file independently of run completion, use `expri service file-upload`:

```sh
expri service file-upload \
  --target '{"kind":"run","scope":{"project_id":"vision","origin":"gpu-1","run_id":"run-1"},"path":"outputs/best.pt"}' \
  --file /private/checkpoints/best.pt
```

This command does not seal the run or create `result.zip`. If the destination
already stores an object with matching size and digest, it returns `reused: true`
without uploading or creating a local queue. Otherwise it uses a resumable upload
queue under `.expri/publish/files/`, configurable with `--queue-dir`.

## Run archival and retention

Archiving hides a finished hosted run from the default run list and schedules
deletion **15 days from the time it was archived**. The deadline is displayed in
the Archived view. Only `completed`, `failed`, `cancelled`, and `lost` runs can be
archived; a running or incompletely received run must finish publishing its
authoritative run state first. Archiving does not stop training or create a ZIP.
`expri service upload` remains the server's result ZIP upload command.

Launch smoke tests with `expri run --smoke TASK`, or add `--detach` before the
task name for background execution. A configured environment is required. The
run records `smoke: true` and receives a generated `smoke-run-…` ID; the name
alone never enables archival. On an updated server, a complete terminal
run-state publication from the managed publisher automatically archives a smoke
run, including `failed`, `cancelled`, or `lost` runs. The 15-day period starts
when the server receives that state. Publishing can finish during retention,
and worker/laptop run files remain in place. Automatic archival happens once
per run: restoring it keeps it active through later metadata updates and
publisher retries. You can still archive it manually afterward. Raw legacy
object uploads do not trigger this policy. `--no-publish` keeps the smoke mark locally without
automatic server archival.

On the primary hosted dashboard, use **Archive** on a finished run. Select the
**Archived** view to review its deadline or **Restore** it before that deadline.
Restoring returns the run to the normal list and cancels retention. Archiving an
already archived run preserves its original deadline; restoring and later
archiving it starts a new 15-day period. Preview dashboards, including AB, can
review archived runs but cannot archive or restore them. These actions use the
existing signed-in dashboard session; they do not require enabling project
deletion or re-entering its password.

Owner tokens can manage the same lifecycle from the CLI:

```sh
expri service run archive --project-id vision \
  --origin gpu-1 --run-id run-1
expri service run status --project-id vision \
  --origin gpu-1 --run-id run-1
expri service run restore --project-id vision \
  --origin gpu-1 --run-id run-1
```

At the deadline, the server removes only the run's hosted metadata, metrics,
logs, tracking files, staged result ZIPs, and unreferenced S3 outputs. Worker and
laptop copies remain. Project private inputs and objects still referenced by
other runs or inputs remain. Eligible S3 object versions and pending multipart
uploads are cleaned up using the same object safety checks as project deletion.
The server retains a small tombstone to reject late worker uploads; publishing
cannot resurrect an expired run. Use a new run ID for a new run.
Continuous fetch retains saved local files and continues watching the remaining
runs when one hosted run expires; deleting the project still stops its watcher.

Deletion begins automatically when the deadline is reached, or after the service
returns if it was offline then. Restore is unavailable at or after the deadline.
Cleanup has a durable queue independent of ingestion and result ZIP production.
Cleanup serializes with file publication in the same project, so slow storage
cleanup can delay that project's publishing; other projects use separate gates.
Failures retain the cleanup status and retry after server restarts or S3 outages.
Inspect `service run status` for `deleting`, `needs_attention`, the pending task
count and the last error; `deleted` means hosted cleanup has finished. Failed
cleanup does not extend the deadline or make the run restorable.

## Storage usage and project deletion

The project's **Storage** page shows usage across all of its service records,
independent of the selected list, search, or page. **S3 storage** counts each
recorded S3 key once, including retained completed uploads that no current file
uses. Referencing the same dataset as an input and output does not increase this
total. **Local storage** counts acknowledged tracking documents, metrics, logs,
and staged or retained `result.zip` files in the server's service data
directory. It does not measure files on workers or your laptop. Pending uploads
show declared file sizes separately, rather than bytes received.

These are catalog statistics, not the bucket's total billable storage. They do
not include unrelated or unrecorded objects, historical S3 versions, temporary
ZIP-building files, filesystem allocation overhead, or shared database
overhead. Projects that share an S3 key each include that key once in their own
total; adding project totals can still count cross-project shared objects more
than once. The deletion preview estimates reclaimable objects, excluding keys
referenced by another project. Inspect the same figures from the owner CLI:

```sh
expri service project stats --project-id vision
expri service project delete-preview --project-id vision
```

Project deletion removes its server-side runs, private inputs, tracking files,
staged result ZIPs, and eligible S3 objects. It does not stop training or remove
worker and laptop files. Stop publishers before deleting a project. The service
permanently rejects further writes to that project ID, including delayed retries
from old publishers; use a new project ID for subsequent experiments. Current
publishers stop on this permanent rejection, leaving training and local files
intact.

Browser deletion is disabled by default. Enable it in the existing dashboard
configuration:

```toml
[dashboard]
public_url = "https://expri.example.net"
password_env = "EXPRI_DASHBOARD_PASSWORD"
allow_project_deletion = true
```

Only the primary dashboard can delete; AB previews remain read-only. **Delete
project** opens a preview. Enter the exact project ID and the dashboard password
to submit it. Cancel sends no deletion request. If project data changed after
the preview, refresh and review the new preview before submitting again.

The owner CLI also requires the preview's revision and exact project ID:

```sh
expri service project delete --project-id vision \
  --revision 'REVISION_FROM_PREVIEW' --confirm-project vision
expri service project deletion --project-id vision
```

Metadata removal and a durable cleanup queue are committed together. Cleanup
aborts pending multipart uploads, removes eligible objects and local tracking
files, and retries after storage outages or a service restart. The dashboard
shows pending cleanup even after the project leaves the catalog; checking its
status does not submit another deletion.

Cleanup status distinguishes **Pending** work from **Needs attention**.
Network failures, throttling, and server errors retry after at least 30 seconds.
Storage permission, configuration, invalid provider response, or local filesystem
permission problems show a safe explanation and recheck after at least five
minutes. Correct the reported cause; the server resumes cleanup automatically.
Restart the service if its credentials or configuration changed. Each unfinished
task retains its own error, so completion of another task does not hide a problem.
Status becomes **Deleted** only when every cleanup task is complete, and its
error clears. Unfinished work and its status survive a service restart.

An accepted deletion cannot be undone: its project records are removed and some
files may already have been cleaned up. A failed browser request can have an
unknown outcome; use **Check status** before reviewing a fresh deletion preview.
The dashboard never automatically replays the deletion request.

For a versioned bucket, cleanup permanently removes all versions and delete
markers of each eligible exact object key. It does not sweep the bucket or an
arbitrary prefix. This matters for Backblaze, where a plain deletion can retain
older versions. The storage credentials need permission to inspect bucket
versioning, list versions, delete object versions, and abort multipart uploads,
in addition to the existing upload/download permissions. Missing permissions or
object retention leave cleanup unfinished with a **Needs attention** status and
periodic rechecks. See [Backblaze's deletion
behavior](https://www.backblaze.com/apidocs/s3-delete-object) and [AWS S3 deletion
semantics](https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObject.html).

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
reviews downloaded data offline. A finalized checkpoint registered during
training reaches a watching laptop while that run is still active. A later run
reuses its verified private input with the service offline. The suite verifies
independent server ZIP completion,
original tracking bytes without per-file S3 uploads, recovery of an acknowledged
prefix while the worker is offline, and owner-requested partial uploads. Failed and cancelled runs also finish publishing
their original task status and logs without selecting a checkpoint.
Versioned MinIO also exercises usage totals for shared references, retained
uploads, pending multipart uploads, and tracking bytes. An isolated project is
deleted during an S3 outage, then cleanup resumes after a service restart. The
test checks that all of its object versions and multipart uploads are removed
while another project's data survives. Native browser checks cover deletion
preview and cancel, password requirements, narrow layouts, and read-only AB.
An isolated Firefox image submits the native login and logout forms over an
internal HTTPS fixture. It reproduces the rejected null origins under
`no-referrer`, then checks successful forms under `same-origin`, secure cookie
flags and session revocation. The test browser accepts the generated certificate
through its test-only WebDriver configuration. The browser also reviews the
uploaded runs: direct selection/comparison, keyboard tabs, deferred log loading,
refresh and filters. It checks parameter and metric checkbox columns, table
summary tags, native sortable headers, and retained review state. A second
publishing origin uses its own scoped token and the same recorded run ID as an
active first-origin fixture. The project check exercises default All machines,
filtering, comparison, distinct metric streams, machine sorting, scoped
logs/files/downloads, and live updates from both origins without changing chart
exploration or table choices. The original native checks also exercise an older
backend's missing project catalog through a fixture-controlled fallback. It verifies
1440/1024/860 CSS pixel desktop layouts with Runs beside Review, including project
and added-column tables, plus a stacked layout at 859 CSS pixels and narrow
320/360/500 CSS pixel layouts with table scrolling without page overflow,
saving screenshots for diagnostics.
A second HTTPS hostname also checks branch assets against the same catalog,
cookie replay rejection, and independent login/logout sessions.
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
