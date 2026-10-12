---
name: expri
description: Run experiments and query published results with the expri CLI, including remote workers, isolated Python environments, metrics, managed assets, and result transfers.
---

# Expri for agents

Use expri to execute configured tasks locally or on a worker and inspect their
results. Read the relevant topic below when you need its workflow; each command
prints an embedded Markdown guide and works without project configuration.

## Choose a topic

| Command | Read when you need to |
| --- | --- |
| `expri skill login` | Authenticate to the server and revoke an issued CLI token |
| `expri skill config` | Configure the global server, define tasks, or select a target |
| `expri skill push` | Transfer source and config to a worker |
| `expri skill run` | Launch a task, including a detached experiment |
| `expri skill runs` | Inspect status/logs, cancel, or retrieve worker results |
| `expri skill metrics` | Instrument training or compare recorded metrics |
| `expri skill artifacts` | Hand finalized checkpoints to background publishing |
| `expri skill assets` | Pin datasets/models or reuse a producing run's output |
| `expri skill environment` | Prepare uv, check base-package reuse, or prune |
| `expri skill query` | Browse published server runs and file availability |
| `expri skill service` | Publish runs or fetch results from an existing service |
| `expri skill system` | Collect machine inventory/utilization and inspect system history |

## Essential behavior

- Read the project's existing `expri.toml` and sibling `expri.target.toml` before
  choosing a task or target. The default config is in the current directory;
  `--config` selects another file. Use `expri skill config` for path rules.
- `expri run TASK` executes locally. `expri -T TARGET run TASK` executes remotely
  and pushes source first. Put `-T` before the subcommand and run options before
  `TASK`; arguments after `TASK` belong to the task.
- A configured `[environment]` enables isolated, recorded runs. Detached runs,
  automatic publishing, and runs using managed assets require it. An empty
  table selects ordinary uv isolation.
- Write run outputs to `EXPRI_OUTPUT_DIR`. Capture the returned run ID for
  detached runs and use `runs status --json` to check task and publisher status.
- `push` sends source to a worker; `runs fetch` retrieves a worker run over
  SSH/ctl; `fetch` retrieves service results using `[fetch]`. These have different
  configuration and selection rules.
- `query runs` and `query files` read published server JSON through the global
  `[server]` connection without a repository or target. Use `expri skill query`
  for scope, pagination, and received/reported availability.
- Reports are human-readable tables by default. Prefer `--json` when reading reports. Use the installed CLI's
  `--help` for exact flags, for example `expri runs status --help`.

Topic examples use `gpu`, `train`, and `run-abc123` as placeholders. Substitute
the project's configured target/task and the actual run ID.
