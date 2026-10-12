# Expri service

Use an existing expri service to publish run metadata, metrics, logs, and selected
finalized checkpoints, then fetch them onto the reviewing machine. Worker
publishing credentials and laptop fetching credentials are separate.

For `service` commands, `--config` selects a service client file rather than
the project's `expri.toml`. `service fetch --repo` selects the review checkout
and defaults to the current directory; provide it when fetching from elsewhere.

## Configure the executing worker

Create a client file outside the pushed checkout, for example
`/etc/expri/worker.toml`, with the existing service URL and token variable:

```toml
url = "https://expri.example.net"
project_id = "vision"
token_env = "EXPRI_WORKER_TOKEN"
```

Provide that token in the named worker environment variable. Add the publishing
scope to `expri.toml`, or use `[target.gpu.service]` for one configured target:

```toml
[environment]

[service]
client_config = "/etc/expri/worker.toml"
project_id = "vision"
origin = "gpu-1"
```

`client_config` is an absolute path on the machine executing the task. Use a
distinct origin for each worker. Target service tables replace the entire
project service table; missing target tables inherit the project default.
Automatic publishing requires an environment and a Unix native worker with
`run-publishing-v1`; Python fallback cannot publish automatically.

```sh
expri -T gpu run --detach train --epochs 3
expri -T gpu runs status run-abc123 --json
```

The independent publisher starts before environment preparation. Service outages
leave queued work retrying while training continues. `run --no-publish train`
disables publishing for that run while retaining private asset download access.
Checkpoint contents need explicit artifact registration or selection.

## Resume or publish an existing run on the worker

```sh
expri service resume --run-dir .expri/runs/run-abc123
expri service publish --config /etc/expri/worker.toml --project-id vision --origin gpu-1 --run-dir .expri/runs/run-abc123 --watch
```

`resume` restores the saved automatic publisher without rerunning training.
Keep original run files and `.expri/publish` until acknowledgment; publishers
need resuming after a worker reboot. Inspect publishing separately from task
exit status. For tracking runs, `published` means tracking/checkpoint work was
acknowledged and the server ZIP job accepted; its ZIP can still be uploading.
`result.zip` contains tracking data, with datasets and checkpoints excluded.

## Fetch onto the reviewing machine

Use an existing owner client file with the same service URL and its owner token
variable. Configure the laptop's `expri.toml`:

```toml
[fetch]
client_config = "/home/me/.config/expri/owner.toml"
project_id = "vision"
origins = ["gpu-1"]
labels = ["best", "latest"]
```

```sh
expri fetch --dry-run
expri fetch --watch
expri service list --config owner.toml --project-id vision --origin gpu-1
expri service fetch --config owner.toml --project-id vision --origin gpu-1 --run-id run-abc123 --artifact outputs/checkpoint-1000.pt
expri dashboard
```

`fetch` and `service` run on the current machine and reject `-T`. Fetch receives
metadata, metrics, parameters, and logs by default; checkpoints are selected
separately by labels or paths. Missing labels remain pending during watching.
Downloads verify and stage files before publishing them into
`results/service-<project_id>-<origin>/runs/<run_id>/`. Repeat interrupted fetches
to resume; previously verified results remain available offline.

Continue with `expri skill artifacts` for live checkpoint handoffs,
`expri skill assets` for private inputs, or `expri skill metrics` for inspection.
Use `expri fetch --help`, `expri service --help`, and
`expri service <subcommand> --help` for exhaustive flags.
