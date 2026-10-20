# Run resource requirements

Disk requirements protect a direct native Linux detached run before its task
starts and while it executes. They are opt-in and describe minimum free space
on the task's worker. They do not reserve disk space or delete files.

Configure a task in `expri.toml`:

```toml
[tasks.train]
command = ["python", "train.py"]

[tasks.train.requirements]
recovery_seconds = 30

[[tasks.train.requirements.disk]]
path = "outputs"
min_free = "20GiB"
resume_free = "30GiB"

[[tasks.train.requirements.disk]]
path = "/mnt/checkpoints"
min_free = "50GiB"
resume_free = "60GiB"
```

`outputs` selects the filesystem containing this run's output directory. Other
paths must be absolute directories on the machine executing the task. Use explicit
paths for checkpoint, cache, and temporary storage on other filesystems. These
directories must exist; an unavailable path or unreadable filesystem blocks the run
and resets its recovery timer. A controller cannot infer free space on a
different machine where a task launches external work.

Each disk needs a positive `min_free` value. `resume_free` must be larger; when
omitted it defaults to 20% above `min_free`, rounded upward. Values accept integer
byte quantities such as `20GiB`, `20000MiB`, or `20GB`. There are no implicit
disk thresholds for tasks without requirements. At most 16 distinct paths are
allowed.

Override the configured disk list when launching a run:

```sh
expri -T gpu-worker run --detach \
  --disk-free 20GiB --disk-path outputs --disk-resume-free 30GiB \
  --disk-free 50GiB --disk-path /mnt/checkpoints --disk-resume-free 60GiB \
  --disk-recovery-seconds 30 train
```

Place options before the task name. Repeated sizes and paths pair in their
respective order. When paths or resume sizes are supplied, provide one for each
`--disk-free`. A single omitted path defaults to `outputs`. Explicit disk flags
replace the task's disk list; otherwise the full configured list is retained.
`--disk-recovery-seconds` can also override the task's recovery delay alone.

## Lifecycle behavior

- Before task launch, an unmet requirement puts the run in `pending`, with its
  reason and observed free space. The supervisor stays active and Stop remains
  available. This applies to directly launched runs as well as the existing
  server queue's separate pending state.
- While running or waiting, low disk space suspends the owned task process
  group in memory and reports `paused`. The run ID, metrics, and attempt remain
  attached to the same task.
- All configured disks must reach their recovery limits continuously for
  `recovery_seconds` before automatic resume. The default is 30 seconds; zero
  allows immediate recovery. Lost observations and changed mounts reset that
  interval. Separate thresholds prevent repeated pause/resume around one limit.
- Manual pause has independent ownership. **Keep paused** retains a resource
  pause after space recovers. **Resume** clears manual pause intent but still
  respects requirements. Stop and cancellation take precedence over recovery.
- Continue and Restart inherit the run's current requirements, including website
  edits. A replacement that lacks disk space remains pending and can be stopped
  or have its requirements edited while it waits.

The guard samples the task host locally, normally every five seconds, and works
without publishing or a hosted-server connection. The existing supervisor owns
enforcement; no additional daemon is started. Sampling cannot prevent another
process from filling a disk between checks, so leave headroom for the task's
largest writes and expri's control metadata.
If control metadata cannot be committed, the existing supervisor failure path
terminates and reaps its owned task group rather than leave an unrecorded frozen
task behind. This is a headroom guard, not a filesystem quota or a guarantee
against a completely full disk.

Pausing does not reclaim disk space, RAM, or GPU memory. Resume depends on
cleanup or additional capacity restoring free space. This first implementation
does not treat RAM or GPU memory as automatically recoverable requirements.

## Website

On the primary hosted dashboard, open a supported run's **Requirements** editor
to add paths and change thresholds or recovery time. Changes apply after an
explicit save and worker acknowledgement. Removing all disk paths disables the
guard. The inspector shows the blocking paths, free bytes, limits, and recovery
state; run rows show a compact disk-space reason. AB previews remain read-only.

Queued jobs, foreground runs, Python fallback, and workers without the resource
capability reject configured requirements. Older live supervisors keep their
existing lifecycle behavior but cannot accept requirement edits until safely
upgraded. Installing a new CLI alone does not change an older supervisor's
capabilities.
