# Managed assets

An asset is a reusable file: a dataset, model file, or checkpoint. Training reads
an ordinary relative path such as `data/train.parquet`. Commit its adjacent
`data/train.parquet.expri.toml` descriptor. The payload, cache, transfer receipts,
and private repository settings remain Git-ignored.

```text
data/train.parquet             # Existing or downloaded payload; Git-ignored
data/train.parquet.expri.toml  # Small pinned descriptor; commit this
.expri/assets/<sha256>/file    # Verified content reused by this checkout's runs
.expri/config.toml            # Private repository service context; never commit
```

There is no input list in `expri.toml`. Version 2 descriptors identify exact bytes
with size and SHA256, their original source, optional private availability, and
optional producing-run provenance. Private descriptors contain input IDs and
short producer references, without server URLs, project IDs, credentials, or
absolute machine paths.

## Add an asset

```sh
# Track an existing local file, offline. This does not upload it.
expri assets add data/train.parquet

# Add and download an external named file.
expri assets add https://example.net/train.parquet data/train.parquet
expri assets add hf://example/model@v1/config.json models/config.json
expri assets add hf://datasets/example/corpus@main/data/train.parquet data/train.parquet

# Add an immutable private input from the configured repository service.
expri assets add expri://./inputs/dataset-v1 data/private.bin
```

`add <path>` hashes and caches the existing file, creates the sidecar, and adds
ignore rules. It preserves the original file and its permissions. The descriptor
has a local source without an absolute path. Before upload, another machine can
prepare it only if those exact bytes are already in its cache. A cache miss
explains that the asset must be uploaded first.

`add <url> <path>` downloads and verifies the payload, pins the source, and binds
the file read-only at the destination. Public HTTP(S), public Hugging Face named
files, and registered expri inputs are supported. Whole repositories, arbitrary
directory assets, gated Hugging Face access, and direct access to other private
S3 buckets are outside this version.

Use `--repo /path/to/checkout` to select a checkout. Otherwise commands use the
current Git root, or the current directory when outside Git. Asset paths are
relative to that root. `--json` prints machine-readable reports.

Commit the sidecar and ignore rules. Expri reopens narrowly scoped Git ignore
exceptions for descriptors beneath ignored data directories, preserving neighboring
files' exclusion. Nested ignore rules that still hide a sidecar must be adjusted.
Adding refuses an existing descriptor, an occupied URL destination, and data paths
already tracked by Git. Data in Git history remains in Git bundles; ignoring a
file cannot remove that history.

## Declare without downloading

```sh
# Registered inputs already have size/SHA256 metadata.
expri assets add expri://./inputs/dataset-v1 data/train.bin --no-download

# Supply the exact published identity when a public source lacks it.
expri assets add https://example.net/audio.tar.zst data/audio.tar.zst \
  --no-download --sha256 <64-digit-sha256> --size <bytes>
```

`--no-download` writes the descriptor and ignore rules without fetching the
payload or populating the content cache. Expri can query small source metadata.
It requires a trustworthy SHA256 and byte size; an HTTP ETag alone is insufficient.
Hugging Face revisions are resolved to repository commits. If file checksum
metadata is unavailable, provide `--sha256` and `--size` together.

Declare on a laptop, commit the descriptor, and let the executing worker download
the pinned version before training. A worker never chooses unrecorded content on
its first download.

## Private repository settings and uploads

Create this ignored, machine-local `.expri/config.toml` in each checkout that
needs private storage:

```toml
url = "https://service.example.net"
project_id = "example-project"
token_env = "EXPRI_ASSET_TOKEN"
```

Use a suitable existing owner or worker token through the named environment
variable. Owners can upload/import; workers can read their project's inputs.
Workers and laptops transfer payloads with temporary signed URLs, without bucket
credentials. Signed URLs and bearer values never enter descriptors.

All `expri://./` references resolve against this repository context. It is shared
by the assets commands in that checkout, rather than repeated in each command or
sidecar. Configure each machine separately; never push the local config or its
credentials. Expri rejects `.expri/config.toml` if it is tracked by Git.

`--client-config` explicitly selects an existing client configuration. It must
include `project_id` for repository-relative asset references. If the ignored
repository config is absent, assets commands can use the checkout's
`[service].client_config`. Asset-only workflows do not require a publishing origin
or `[environment]` just to configure private storage.

```sh
expri assets add data/train.parquet
expri assets upload data/train.parquet
# Commit the descriptor after upload records its private availability.
```

`upload [path]` uploads the exact recorded content and records the persistent
private input ID only after success. It resumes interrupted transfers and supports
retrying a completed upload. A local modification requires explicit `update`
before upload. Public/Hugging Face assets retain their original source after
upload so future updates still follow it. Changing content clears availability
that belongs to the old bytes.

## Import a producing run's output

```sh
expri assets import \
  expri://./worker-1/run-abc123/outputs/best.pt \
  models/base.pt
```

The reference is the producing run's recorded origin, run ID, and file path
relative to the run directory. Expri queries the configured service. The output
must be finalized and uploaded; the producing run can still be active.

Import creates a persistent immutable private input, preserves producing-run
provenance, and writes a descriptor for the next experiment. It shares the existing
S3 object without downloading or copying its payload. Repeating the same import
reuses the canonical asset. Use `--input-id` to choose an explicit immutable input
ID. A conflicting existing identity or producer is rejected.

The asset keeps its bytes and captured provenance after the original run expires.
The dashboard shows Produced by and Used by relationships, and Copy reference
provides this repository-relative URI. Deleted producer runs retain their recorded
identity with an unavailable-run indication.

## Download, inspect, and update

```sh
expri assets download                    # Prepare all descriptors in the checkout
expri assets download data/train.parquet # Prepare one recorded version
expri assets status                      # Verify local files; no network requests
expri assets update data/train.parquet   # Explicitly select new content
```

`download` reproduces the descriptor's size and digest. It reuses verified cached
bytes offline and resumes interrupted transfers when the source has a usable
validator. A server ignoring Range restarts safely. Source changes never silently
rewrite descriptors. Corrupt cached content fails verification; remove the affected
cache file before downloading again.

For local assets, `update` hashes the current workspace file and updates its pinned
identity. For URL/Hugging Face assets, it resolves the original source again,
downloads its current content, and updates the file and descriptor after successful
verification. Hugging Face branches/tags follow their recorded requested revision;
a source pinned to a commit stays at that commit. Immutable expri inputs stay pinned.
Review and commit descriptor changes as data changes.

Commands preserve local modifications unless `update` explicitly selects a local
file's new bytes. To restore recorded content, preserve edits elsewhere or use
`assets download --force <path>`. Materialized read-only assets may share an inode
with the cache: copy before editing instead of making a shared inode writable.

`status` reports ready, missing, or modified local content, plus local-only or
downloadable availability. Checking status never contacts remote sources.

A public URL does not guarantee old versions remain available. Upload important
versions into private input storage if the original host cannot preserve them.

## Run preparation

Runs with assets require `[environment]` and a native worker advertising
`assets-v2`. Expri captures selected sidecars in the isolated source snapshot,
prepares their exact bytes before environment setup and task launch, and binds
read-only files at the same relative paths. Training can open `data/train.parquet`
from the run's code directory. Write derived data to `EXPRI_OUTPUT_DIR`.

Only sidecars selected in the run's source snapshot are prepared. Managed payloads
are excluded from snapshots and source patches even with `push.include_ignored`.
Source push rejects tracked managed data. Python workers reject asset-bearing runs
with a native-worker upgrade instruction.

Each run records consumed asset identities, availability, and producer references
in `run-state.json`. Updating a workspace descriptor cannot change existing runs.
Missing assets fail before training. Preparation supports cancellation.

Workers prefer their own ignored `.expri/config.toml`, then the executing worker's
`[service].client_config`, for private downloads. `--no-publish` retains download
credentials. Verified cache entries remain usable while the service is unavailable.

The earlier generic `assets import` command is now `assets add`; `import` is reserved
for run outputs. Descriptor version 1 and absolute private source references are
not supported. Re-add assets with version 2 descriptors and local service context.
