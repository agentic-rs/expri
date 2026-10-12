# Expri artifacts

Reports show tables by default. Agents and scripts should add `--json` to read
structured results; `--quiet` suppresses human reports.

Use artifact registration to hand a finalized output to the run's independent
publisher while training continues. Registration writes a small durable local
outbox; the publisher hashes and uploads the bytes separately.

## Finalize before registering

Write each checkpoint to a temporary file, finish and close all writers, then
rename it to a unique final filename beneath `EXPRI_OUTPUT_DIR`. Register that
final file inside the managed task:

```sh
"$EXPRI_BIN" artifact register "$EXPRI_OUTPUT_DIR/checkpoint-1000.pt" --label best --label latest
"$EXPRI_BIN" artifact list
```

`EXPRI_BIN` selects the executing native binary, including a custom worker
`node_bin` outside the task's PATH. `--run-dir` defaults to `EXPRI_RUN_DIR`.
The command runs on the training machine; omit `-T`.

From that machine's original checkout, inspect or register an existing output
with an explicit run directory:

```sh
expri artifact register outputs/checkpoint-1000.pt --run-dir .expri/runs/run-abc123 --label best
expri artifact list --run-dir .expri/runs/run-abc123
```

Relative registration paths start with `outputs/` and are relative to the run
directory. Absolute paths must belong to that run's outputs directory. Hidden
path components, traversal, symlinks, caches, `metrics.jsonl`, and `params.json`
are excluded from checkpoint registration.

## Interpret progress

- `registered`: the durable handoff exists; file contents have not necessarily
  reached the service.
- `uploading`: the independent publisher is transferring the file.
- `cloud`: the completed object is acknowledged.
- `needs_attention`: inspect `sync_error` and correct the failed handoff.

Keep the file unchanged until its status is `cloud`. Prefer unique immutable
filenames even after upload. `best` and `latest` are movable aliases selecting
existing files; they create no duplicate object. Register another finalized
file with the same label to move the alias.

Offline registrations wait for the publisher to reconnect or resume. Check both
task and publishing status; completing training does not imply upload success:

```sh
expri runs status run-abc123 --json
expri service resume --run-dir .expri/runs/run-abc123
```

If a registered file changes, finalize corrected content under a new filename,
register it, then discard only the failed old handoff:

```sh
expri artifact unregister outputs/bad.pt --run-dir .expri/runs/run-abc123
```

`unregister` accepts only a `needs_attention` registration. It retains file and
cloud bytes, retires the old path, and cannot remove an active or successful
handoff. The inventory closes before terminal result sealing; after closure,
adding files and moving labels are rejected. Use the explicit
`service file-upload` workflow for post-run uploads.

## Retrieve selected outputs

```sh
expri -T gpu runs fetch run-abc123 --artifact outputs/checkpoint-1000.pt
expri service fetch --project-id vision --origin gpu-1 --run-id run-abc123 --artifact outputs/checkpoint-1000.pt
```

The first command reads directly from a worker; the second reads the existing
service. Service fetch selections require finalized uploaded objects. Configure
`[fetch].labels = ["best"]` for ongoing service fetching by alias.

Continue with `expri skill service` for publishing/fetch configuration or
`expri skill assets` to reuse an uploaded output in later runs. Use
`expri artifact --help` and `expri artifact <subcommand> --help` for all flags.
