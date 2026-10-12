# Expri server queries

Use `expri query runs` and `expri query files` to inspect published server records
before downloading results. They run on the current machine without a project
checkout or SSH target. Tables are the default; add `--json` for bounded structured
pages when an agent or script reads the result. They read metadata, without
downloading run files or filling the local review cache.

The connection defaults to `[server]` in `~/.tokn/expri/config.toml`;
`EXPRI_STORAGE_DIR` selects another global root. Use `expri login --url URL`
with the primary dashboard password to configure it; read `expri skill login`
for authentication and `expri skill config` for path rules. `--config PATH` selects another global or standalone client file.
Credentials determine access: unrestricted credentials can query all scopes,
worker tokens are bound to one project/origin, and machine-profile tokens cannot
query runs. The server must advertise `service-catalog-v1`.

## Find published runs

```sh
expri query runs --json
expri query runs --project-id vision --origin gpu-1 --archival all --limit 20
expri query runs --config /etc/expri/client.toml --project-id vision
```

The default selects active runs and up to 50 rows. `--archival archived` selects
archived runs within their restore window; `all` selects both. Expired runs are
unavailable. Results order by descending run ID, then ascending project and
origin. Keep each run's full `scope`: identical run IDs can exist on different
workers or projects.

Execution status is the last published state. Missing or unavailable metadata
produces bounded warnings. Use worker `runs status`, `runs logs`, and `runs cancel`
for live supervision; `service run status` describes archival and cleanup.

## Inspect file availability

```sh
expri query files --project-id vision --origin gpu-1 --run-id run-abc123 --json
expri query files --project-id vision --origin gpu-1 --run-id run-abc123 --prefix outputs/ --limit 50
```

Files require the complete project/origin/run ID scope and default to up to
100 rows. `--prefix` matches literal path text; `%` and `_` are ordinary characters.
Rows combine authoritative `received` metadata with the worker's last `reported`
inventory. `availability` distinguishes `received`, `reported_only`, and
`received_different_version` when known sizes or checksums disagree. Received
tracking data can be an unsealed prefix of a growing file.

Reported files may still await upload. `best`/`latest` labels remain pending
until a received object matches the reported size and checksum; inspect
`labels_pending`. An absent checksum cannot confirm a label or prove a version
mismatch. The reported inventory is bounded to 200 files and 64 KiB, with
`reported_inventory_truncated` and warnings exposing omissions or unavailability.
Received records still appear when the inventory is unavailable.

Both queries accept `--limit` from 1 to 100. Tables print a next-cursor hint. In `--json` output, copy the response's `next_cursor`
exactly into `--cursor` and keep the same scope and filters. Continue until
`next_cursor` is null; byte bounds can make a page shorter than its requested
limit. Run cursors identify full scopes; file cursors are raw paths.

Queries return no signed URLs. Download selected files with `expri service fetch`
and continue with `expri skill service` for resumable transfers. Legacy
`expri service list` keeps its existing behavior. Use `expri query runs --help`
and `expri query files --help` for exact flags.
