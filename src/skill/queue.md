# Expri shared resource queue

Use the configured server and an existing task with a managed environment:

```sh
expri run --queue --project-id vision --cpus 4 --memory 16GiB --gpus 1 --json train --epochs 3
expri queue list --project-id vision --json
expri queue show job-abc123 --json
expri queue cancel job-abc123 --json
expri queue workers --json
```

Put run options before the task name. Submission freezes and uploads source
before returning a job ID, so the laptop can disconnect. The source ZIP and
extracted contents each have a 512 MiB limit; use managed assets for large inputs.
Only execution links an actual run ID. Use that run ID for published dashboard
results, `query files`, and worker run inspection.

Memory must be supplied with `--memory` or task defaults. CPU defaults to one
logical slot and GPU count to zero. Task defaults use:

```toml
[tasks.train]
command = ["python", "train.py"]

[tasks.train.resources]
cpus = 4
memory = "16GiB"
gpus = 1
gpu_model = "A100"
```

If the task uses an array shorthand such as `train = ["python", "train.py"]`,
convert it to this table before adding resources.

`--gpu-id` selects an exact device and can be repeated. `--gpu-model` matches a
model substring; `--gpu-memory 24GiB` requires dedicated memory per GPU.
`--worker gpu-1` pins placement. `-T gpu-1` uses that target's environment and
pins the target name, without SSH pushing. CUDA-required environments need a
GPU request and an NVIDIA worker. CPU and memory are reservations among queued
jobs, with driver hints rather than hard OS limits. Whole GPUs are exclusively
reserved among queued jobs; other processes can still use the machine.
Queued workloads must keep child processes in the supervisor's process groups
so terminal cleanup can release the allocation safely.

## Execution worker

Authenticate using the dashboard password with `expri login` on the worker.
Preserve its `[server]` in `~/.tokn/expri/config.toml` and add:

```toml
[queue_worker]
worker_id = "gpu-1"
project_ids = ["vision"]
```

Capacity is detected when the executor starts: all logical CPUs, 90% of installed
RAM (10% reserved as headroom), and selectable whole GPUs. Each field below is an
optional override; omitted fields remain inferred:

```toml
[queue_worker.capacity]
cpus = 16
memory_bytes = 68719476736

[[queue_worker.capacity.gpus]]
device_id = "GPU-your-device-uuid"
model = "NVIDIA A100"
memory_bytes = 42949672960
vendor = "nvidia"
selector = "GPU-your-device-uuid"
```

Use `gpus = []` under `[queue_worker.capacity]` to make a worker CPU-only.
Explicit GPU entries replace the inferred GPU list. Supply real NVIDIA UUIDs or AMD ROCr
selectors; Apple GPUs use a cooperative whole-device reservation with dedicated
memory omitted. Defaults use host inventory; use explicit limits for constrained
containers or shared machines. GPU devices without verified selectors, including
partitioned GPUs or unavailable partition status, are omitted from automatic whole-GPU capacity.
Capacity is a reservation budget, not current system utilization.
Execution workers require native Linux or macOS.
The project allowlist enables execution in the shared worker service:

```sh
expri service start --foreground
expri service status --json
```

`service start` also supports background operation.
Without `[queue_worker]`, the service collects and publishes without executing
server jobs. `service stop` stops queue polling while existing training continues;
its reservations remain held until the next start reconciles completion.
Execution has its own supervised thread. Pause or resume it independently:

```sh
expri service stop --component queue-execution
expri service status --component queue-execution --json
expri service start --component queue-execution
expri service restart --component queue-execution
```

Other collection and publishing components keep running. Paused execution stops
heartbeats; accepted attempts retain allocations even after the worker lease
expires. Its saved choice in `~/.tokn/expri/service.json` uses `"enable"` or
`"disable"` under `components.queue_execution` and survives restart and automatic
startup. Use `service restart --component queue-execution` to enable execution
and reload worker settings and inferred capacity after its old thread has joined.
Peers stay live. Whole `service restart` preserves saved component choices,
training, and held allocations. Component failure does not stop peers.
`--foreground` starts the whole supervisor and cannot
be combined with `--component`.
If shutdown interrupts an accepted task before launch, its journal stays held
for confirmed stopped-workload recovery rather than being relaunched automatically.

Only unrestricted server/login tokens support queue operations. Scoped publishing
and machine tokens cannot enable execution. Worker journals and sessions use
the new global `queue/` directory; existing project storage stays in place.

## Scheduling, retries, and recovery

The server assigns queued jobs across online workers using their reported budgets
and held reservations. CPU-only jobs prefer CPU-only workers; optional worker and
GPU constraints still apply. Workers poll for their assignments and accept them
before preparing source or executing. Smaller fitting jobs may pass blocked jobs.
`queue workers` shows available/total CPU, memory and GPUs, assigned work and active
attempts. JSON also includes device identities and reserved resources.
`queue list` includes assigned workers and pending reasons. Pages default to 20 records, with
`--limit` up to 100 and `--cursor` for the next page.

If a submission response is lost, repeat an identical submission with the
reported `--submission-id`. Source or specification changes require a new ID.
Restarting queue execution reconciles durable journals without relaunching a job.
There is no automatic execution retry or preemption.

An assignment that has not been accepted is released when its worker lease expires
and can be placed elsewhere. Losing the 60-second worker lease or supervisor after
acceptance marks an attempt uncertain and
holds its reservation. Known live supervisors can reconcile their linked run.
Do not release an unknown attempt merely because a worker is unreachable.
Independently stop and verify the workload before using:

```sh
expri queue recover attempt-abc123 --confirmed-stopped --json
```

Recovery releases the allocation and marks the attempt lost without rerunning it.
Active cancellation retains resources until the worker confirms completion.
`--no-publish` disables run publishing; otherwise ordinary publishing and smoke
archival apply. The queue itself is currently inspected through the CLI.
