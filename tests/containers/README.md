# Host and worker CI tests

This suite runs the built expri binary against separate Linux Docker containers
using real uv, SSH and rsync. It requires a Docker daemon that runs Linux
containers and `ssh-keygen` on the invoking machine. Missing prerequisites fail
the suite. Ordinary `cargo test` ignores the container integration test; CI runs
it explicitly through this runner.

From the repository root:

```sh
tests/containers/run.sh
# Reuse previously built expri-ci-host and expri-ci-worker images:
tests/containers/run.sh --no-build
# Run one case, rebuilding images first:
tests/containers/run.sh dependency-mismatch
tests/containers/run.sh --no-build matched-python cuda-unavailable
```

Rebuild after changing expri, the integration test or container fixtures: the
images contain the compiled binary and test harness. The runner creates fresh
containers for each case and removes them and their internal network on exit.
Each host case has a three-minute limit with a ten-second kill grace period.
It retains build and case logs in the printed directory. Set
`EXPRI_CONTAINER_ARTIFACTS=/absolute/path` to choose that directory; CI uploads
the logs as `container-workflow-logs` on success or failure.

## Installed distributions and isolation

The host has fake Torch `2.9.0+cpu`; the worker normally has fake Torch
`2.10.0+cu128`, `nvidia-cuda-runtime-cu12==12.8.90` and
`nvidia-cublas-cu12==12.8.4.1`. The project lock selects the worker versions and
`fixture-extra==0.1.0`. All are tiny, real wheel distributions installed with
uv into system Python, with dependency metadata, RECORD files, importable
modules and a `torchrun` entry point. Runtime inventory is not monkeypatched.
NVIDIA wheels declare nested namespaces using slash and backslash forms to
exercise namespace metadata normalization.

Only ephemeral SSH keys are copied into the containers. SSH uses a non-root
account, public-key authentication and checked host keys. Project and run
directories have no shared volume: code and results must cross SSH/rsync.

Image builds need internet access for base images, apt packages, Rust crates and
the pinned real helper wheels `packaging==25.0` and `tomli==2.2.1`. The build
checks helper wheel contents against their PyPI SHA256 digests. At runtime an
internal Docker network prevents access to external package indexes. The worker
serves a static wheel index at `http://worker:8000/simple`; both roles use it,
including cold isolated uv helper environments. No ports are published to the
invoking machine.

## Cases and assertions

| Case | Worker configuration | Expected result |
|---|---|---|
| `matched-native` | Matching graph, `expri-node` protocol | Complete workflow succeeds |
| `matched-python` | Matching graph, `python` protocol | Complete workflow succeeds |
| `matched-auto` | Matching graph, `auto` with a legacy node lacking current capabilities | The current workflow, including run artifact selection, falls back to Python |
| `torch-mismatch` | Torch `2.10.0+cu126` and its CUDA 12.6 dependencies | Doctor identifies the Torch lock mismatch; preparation fails before task execution |
| `dependency-mismatch` | Matching Torch, CUDA runtime `12.8.91` | Doctor identifies the transitive lock mismatch; preparation fails before task execution |
| `cuda-unavailable` | Matching graph, fake CUDA availability disabled | CUDA-required preparation fails; the same stack succeeds when CUDA is optional |

The matching cases check sync, doctor, detached success and failure runs, exit
codes, working directories inside code snapshots, separate run environments and
a shared uv cache. They check that lightweight dependencies load from the
overlay while Torch and the inherited `torchrun` use the worker base packages.
Changing and syncing source preserves the earlier snapshot. Pruning removes
finished environments while preserving outputs and an active environment;
cancelling that active run reaches a terminal state. Host and worker base
package inventories remain unchanged by the workflow.

Selective pulls retrieve logs, parameters and metrics while leaving checkpoints
and environments on the worker. After removing target configuration, cached CLI
inspection and comparison still work. Dashboard HTTP responses agree with the
cached records, parameters, log tails and metric reductions; chart responses
contain SVG and stay below the configured size cap. Offline review preserves
the project/cache file contents, modes and modification times.

The fake Torch probe models availability, a one-element tensor operation and
device metadata. It performs no GPU computation and does not cover real
PyTorch binaries, CUDA drivers, ABI compatibility or Conda installations. This
suite does not exercise browser DOM interactions. The separate
[self-hosted service acceptance suite](../../docs/self-hosted-service.md#acceptance-tests)
injects service outages and lost multipart acknowledgments, using these same
host and worker images plus a service, a pinned MinIO fixture and a fault proxy.
It checks immutable private inputs, upload recovery, selective downloads and
offline review without shared project directories or published ports.
