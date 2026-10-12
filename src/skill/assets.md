# Expri assets

Reports show tables by default. Agents and scripts should add `--json` to read
structured results; `--quiet` suppresses human reports.

Use managed assets for reusable datasets, model files, and checkpoints. Training
opens an ordinary path; a committed adjacent `<path>.expri.toml` sidecar pins its
size and SHA256. Payloads and `.expri/assets/` cache files stay outside Git.

## Choose the source

```sh
expri assets add data/train.bin
expri assets add https://example.net/model.bin models/base.bin
expri assets add hf://example/model@v1/config.json models/config.json
expri assets add hf://datasets/example/corpus@main/data/train.parquet data/train.parquet
expri assets add expri://./inputs/dataset-v1 data/private.bin --no-download
```

- `add <path>` hashes and caches an existing local file without uploading it.
- `add <url> <path>` downloads one named file and pins its identity.
- `--no-download` records the descriptor without downloading the payload. It
  requires trustworthy SHA256 and size metadata, or explicit `--sha256` and
  `--size` together. Source metadata requests can still occur.
- Hugging Face branches/tags resolve to commits; whole repositories and gated
  Hugging Face sources are unsupported.

Run assets commands in the checkout that owns the files; omit `-T`. Paths are
relative to the current Git root, or current directory outside Git. Select
another checkout with `--repo /path/to/project`; use `--json` for reports.

## Share local data or reuse a run output

Private references use an existing service client configuration. Create ignored,
machine-local `.expri/config.toml` with the appropriate existing credentials:

```toml
url = "https://expri.example.net"
project_id = "vision"
token_env = "EXPRI_ASSET_TOKEN"
```

Provide the token in the named environment variable. Owners can upload/import;
workers can read their project's inputs. Keep this file and tokens out of Git.
`--client-config /path/to/client.toml` explicitly selects another client file;
it must include `project_id` for repository-relative `expri://./` references.

```sh
expri assets upload data/train.bin
expri assets import expri://./gpu-1/run-abc123/outputs/best.pt models/best.pt
```

`upload` shares the exact recorded bytes and updates private availability after
success. `import` requires an already finalized, uploaded producing-run output;
it creates a persistent private input without downloading or copying the object.
The URI uses the producing run's recorded service origin (`gpu-1` above), run ID,
and run-relative output path. The origin comes from `[service].origin` or its
target override and can differ from the SSH target name (`gpu`).
Commit sidecars and the generated ignore rules after these commands succeed.

## Reproduce or deliberately update

```sh
expri assets status --json
expri assets download
expri assets download data/train.bin
expri assets update data/train.bin
```

`status` verifies local files without contacting sources. `download` reproduces
the pinned version and reuses verified cache bytes. `update` selects current
workspace bytes for a local source, or resolves a URL/Hugging Face source again;
review and commit its descriptor changes. `download --force` replaces modified
local content, so preserve edits first. Copy read-only materialized files before
editing, because they can share an inode with the cache.

Runs containing sidecars require `[environment]` and a native worker supporting
`assets-v2`. The run prepares pinned bytes before task launch; later descriptor
updates cannot change an existing run. A local-only asset needs uploading before
a different machine can reproduce it on a cache miss. Private worker downloads
use that worker's ignored `.expri/config.toml`, then `[service].client_config`.

Continue with `expri skill run` for execution or `expri skill service` for client
and publishing configuration. Use `expri assets --help` and
`expri assets <subcommand> --help` for exhaustive flags.
