# Managed assets

An asset is a reusable file: a dataset, model file, or a checkpoint registered
as a project input. Training reads an ordinary relative path such as
`data/train.parquet`. Git records the adjacent `data/train.parquet.expri.toml`
descriptor; the actual bytes and transfer state stay outside source snapshots.

```text
data/train.parquet             # Downloaded, read-only file; Git-ignored
data/train.parquet.expri.toml  # Small descriptor; commit this to Git
.expri/assets/<sha256>/file    # Verified content reused by this checkout's runs
```

There is no input list in `expri.toml`. Each descriptor identifies a source, a
size, and one SHA256 digest. A Hugging Face descriptor also records the resolved
repository commit and the requested branch/tag used for explicit updates.

## Import a named file

```sh
# Public HTTP(S) file.
expri assets import https://example.net/train.parquet data/train.parquet

# Public Hugging Face model or dataset file. Revision defaults to main.
expri assets import hf://example/model@v1/config.json models/config.json
expri assets import hf://datasets/example/corpus@main/data/train.parquet data/train.parquet

# An immutable private input already registered with the expri service.
expri assets import expri://expri.clouds56.top/vision/inputs/dataset-v1 \
  data/private.bin --client-config /etc/expri/worker.toml
```

Use `--repo /path/to/checkout` to select a checkout. Otherwise commands use the
current Git root, or the current directory when outside Git. Asset paths are
relative to that root. `--json` prints machine-readable reports.

Import downloads and verifies the file, creates its sidecar, and adds the data
path and `.expri/` cache to the root `.gitignore`. Commit the sidecar and ignore
rules. When a data directory is already ignored, import adds narrow exceptions
for the sidecar while keeping neighboring files ignored. Nested ignore rules
that still hide the sidecar must be adjusted before import can finish.
It refuses an existing destination or descriptor and refuses data paths
already tracked by Git. Never commit the large file itself. Data already present
in Git history is still part of Git bundles; ignoring it cannot remove that
history.

Initial sources are public URLs, public Hugging Face named files, and registered
expri inputs. Whole repositories, directory assets, gated Hugging Face access,
and direct access to other private S3 buckets are outside this version.

## Download, inspect, and update

```sh
expri assets download                    # Prepare all descriptors in the checkout
expri assets download data/train.parquet # Prepare one recorded version
expri assets status                      # Verify local files; no network requests
expri assets update data/train.parquet   # Resolve the source again; update Git metadata
```

`download` reproduces the descriptor's size and digest. It reuses verified
cached bytes offline and resumes interrupted network transfers when the source
has a usable validator. A server ignoring Range restarts safely; an unversioned
partial response without a strong ETag restarts instead of mixing versions.
Source changes never silently rewrite a descriptor. A missing or changed source
fails the download unless the exact bytes are already cached.

`update` follows the recorded URL or Hugging Face branch/tag, downloads its
current content, and changes the local file and sidecar after successful
verification. A descriptor imported at a Hugging Face commit remains at that
commit when updated. Review and commit descriptor changes as data changes.

Commands preserve locally modified files. Move your edits elsewhere before
updating, or explicitly use `expri assets download --force <path>` to replace
local bytes with the recorded version. Read-only assets can share an inode with
the cache: copy an asset to another file before editing it, rather than making
the shared file writable. Corrupt cached bytes fail verification; remove the
affected cache file before downloading again.

A descriptor for a public URL detects changes but does not keep old source
versions available. Preserve important versions in your private input storage
if the original host does not provide that guarantee.

## Private inputs and credentials

An `expri://server/project/inputs/input-id` reference identifies the service's
registered immutable input. The service authorizes access and provides temporary
S3 download URLs; workers do not need the server's bucket credentials. Those
temporary URLs and bearer values never enter the sidecar or run metadata.

`--client-config` points at an existing local expri client configuration. If
omitted, assets commands use the checkout's `[service].client_config` when
available. Its endpoint must match the descriptor's server. The reference
defaults to HTTPS; a matching explicit client endpoint can select HTTP for local
development and container fixtures. Credentials are resolved on the machine
performing the download.

Owners still upload and register private inputs with `expri service input
upload` and `expri service reference`. Workers retain project-scoped read access.
Give a checkpoint intended for future runs a persistent project input reference
before its original run expires; that reference preserves the shared S3 object.

## Run preparation

Runs using assets require `[environment]` and a native expri worker advertising
`assets-v1`. Expri captures selected sidecars in the isolated code snapshot,
prepares their exact content before environment setup and task launch, and binds
read-only files at the same relative paths within that snapshot. For example,
training opens `data/train.parquet` from its run code directory. Write derived
data to `EXPRI_OUTPUT_DIR`.

Only sidecars included in the run's source selection are prepared. Ignored or
excluded descriptors are not run dependencies. Native snapshots and source
patches exclude managed data bytes even when `push.include_ignored` selects
them. Source push rejects managed data tracked by Git because its Git bundle
would contain those bytes. Python workers reject asset-bearing runs with a
native-worker upgrade instruction.

Each run records asset paths, safe source references, sizes, and digests in
`run-state.json`. Existing runs keep their original content when a workspace
asset or descriptor is updated. Required assets that cannot be prepared fail
the run before training. Asset preparation supports cancellation.

The executing worker's `[service].client_config` supplies credentials for private
asset cache misses. Public/Hugging Face assets need no service configuration.
`--no-publish` disables uploads while retaining private download credentials.
Verified cached assets remain usable while their original service is unavailable.

The earlier `service.inputs` configuration and `EXPRI_INPUT_DIR` training
interface have been removed. Migrate each required file with `expri assets
import`, commit its sidecar, and read its ordinary relative path in training.
