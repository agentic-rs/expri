# Local producer contract

The Python and Rust SDKs produce the same existing local files. They do not start
a service, upload data directly, or decide task completion. Closing an SDK logger
only releases its resources. The shared worker service and supervisor retain
those responsibilities.

- Metrics append complete UTF-8 JSONL rows to `outputs/metrics.jsonl`. Steps are
  u64 values; duplicate and out-of-order steps remain valid. Names are unchanged,
  nonempty after trimming, at most 256 UTF-8 bytes, and exclude control characters.
  Values are finite numbers, excluding booleans. Each row is at most 1 MiB.
- Initialization validates at most 128 MiB of existing history and takes its
  maximum step plus one, or zero for empty history. Relative logging uses this
  fixed `initial_step + step`; global logging preserves the supplied step. Rows
  contain `schema_version: 1`, `timestamp`, `step`, `step_mode`, and `metrics`.
  Only relative rows contain `local_step`; managed rows also contain `attempt`.
- Initialization and appending use an exclusive Unix `flock` on the metrics file
  itself. Managed attempts share their fixed base in
  `outputs/attempts/<attempt>/step-origin.json`: schema 2 contains exactly
  `schema_version`, numeric `attempt`, and canonical decimal string `step_offset`.
  Existing schema 1 origins also contain string `first_local_step`; nonzero
  anchors allow global logging and reject relative logging.
- Parameters are immutable JSON objects, limited to 1 MiB and 64 nested object
  or array levels including the root. Initial or standalone parameters use
  `outputs/params.json`; continued attempts use
  `outputs/attempts/<attempt>/params.json`. Identical subsequent values are
  accepted. Writers publish complete files without overwriting a competing one.
- Task phase requests use the separate `.run-state.lock` with a five-second
  acquisition limit and require an active supervisor holding `.run.lock`. Only
  unfinished `running` and `waiting` states can be changed. Preserve unknown
  metadata and update the current attempt with the run's phase atomically.
- Artifact registration delegates to the installed expri CLI and its existing
  finalized-file registry. SDKs do not implement a second upload protocol.

These packages currently target Linux and macOS, where the supervisor and SDK
share Unix file locking. Independent experiments require separate output
directories. Distributed trainers should log from rank zero and create a logger
in each writing process rather than inheriting one across `fork`.

## Cross-language verification

```sh
cargo build -p expri-sdk --example contract_probe
python3 sdk/contract/interop_tests.py
```

The harness starts actual Python and Rust processes against the same files. It
checks bidirectional base reuse, global and relative steps, standalone sessions,
mixed writers blocked by the same metrics lock, immutable parameters, legacy
origins, u64 overflow, and byte-preserving rejection of incomplete history. The
Rust example is a test adapter. `--probe PATH` selects its already built binary.
