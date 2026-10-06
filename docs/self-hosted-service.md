# Optional self-hosted experiment storage

Expri keeps execution and review local-first. Workers write the existing run
files, and a separate uploader forwards them to an optional service. The service
uses SQLite for its catalog and upload receipts, and S3-compatible storage for
finalized files. Service outages leave training running. Required private inputs
must already be downloaded before an offline run can start.

This first version provides explicit service commands. It does not schedule runs
or automatically start an uploader with `expri run`. Launch the uploader beside
a detached experiment when live forwarding is wanted.

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

Persist and back up the entire service data directory. SQLite contains logical
file mappings, stream data, and multipart receipts; the S3 bucket alone does not
replace it. Only one service process may use a data directory. Keep the storage
configuration stable for that directory. S3 endpoints used in signed URLs must
be reachable from the workers and laptops.
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
parameters, metric comparisons, charts, and bounded log tails. Workers must push
a run before it appears. Private inputs and checkpoint contents are not loaded
by the dashboard; continue to use the CLI for uploads and selected downloads.

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

Hosted previews are bounded for small servers: the source catalog shows up to
1,000 project/worker sources, browsing and filters cover the 500 runs most
recently updated in the service per source, and cold overview reads have a
30-second time budget. Within that selection, runs are displayed by their start
time. Existing catalogs reconstruct update order from upload records and show a
warning until fresh uploads establish service activity. Overview records are
cached against the existing run-state file digest. Warnings identify missing
or incomplete previews; refresh to retry. Hosted metric files are limited to
16 MiB per run; larger files require local review. Charts retain bounded sampled
points, and log tails read at most 64 KiB. Last/min/max summaries use every point
in a supported metric file.
The local CLI remains available for complete files and older runs.

For main and branch UIs sharing this service, see [dashboard deployments](deployment.md).

## Worker uploads

Create a client configuration on the worker, outside the synced source repo:

```toml
url = "https://expri.example.net"
token_env = "EXPRI_SERVICE_GPU_1_TOKEN"
```

```sh
expri service push --config worker.toml \
  --project-id vision --origin gpu-1 \
  --run-dir .expri/runs/run-abc123 --watch
```

The uploader forwards metadata, parameters, complete metric rows, and stdout /
stderr bytes. Stream batches resume at acknowledged offsets; retries do not add
duplicate bytes. At terminal run states the completed streams become S3 objects.
The training process does not make service requests.

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
1 MiB control-response limit. Live stream batches are limited to 64 KiB;
completed live stream records are removed after their S3 object is published.
SQLite reuses their freed pages; this does not immediately shrink the database.

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
expri dashboard
```

Downloads populate `results/service-vision-gpu-1/runs/run-abc123/` by default.
`--source` changes the cache source name; `--results-dir` changes its root and
should match `[download].results_dir` in the project's `expri.toml`. Downloads
stage and verify files before publication and retain artifacts selected earlier.
The existing cached CLI and dashboard read this directory without contacting
any service or worker.
Object downloads retry bounded 8 MiB ranges and preserve previous cache files
on failure. Restarting the pull command currently restarts its staged downloads;
upload receipts remain durable across command restarts.

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
interrupts the service during training, loses metric and multipart part acknowledgements,
restarts the service, resumes the checkpoint, and reviews downloaded data offline.
An isolated Firefox image submits the native login and logout forms over an
internal HTTPS fixture. It reproduces the rejected null origins under
`no-referrer`, then checks successful forms under `same-origin`, secure cookie
flags and session revocation. The test browser accepts the generated certificate
through its test-only WebDriver configuration. The browser also reviews the
uploaded runs: direct selection/comparison, keyboard tabs, deferred log loading,
refresh and filters. It checks the desktop layout and a narrow layout at a
verified 500 CSS pixel viewport, saving screenshots for diagnostics. This Firefox
check does not cover 320/360 pixel mobile widths. A second HTTPS hostname also
checks branch assets against the same catalog, cookie replay rejection, and
independent login/logout sessions.
Browser request logs contain method, path, origin, status and response policy,
without credentials or cookies.
