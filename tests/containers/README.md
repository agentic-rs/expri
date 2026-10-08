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
host/worker suite does not exercise browser DOM interactions. The separate
[self-hosted service acceptance suite](../../docs/self-hosted-service.md#acceptance-tests)
injects service outages and lost multipart acknowledgments, using these same
host and worker images plus a service, a pinned MinIO fixture, a fault proxy and
an independent Firefox image. The browser image uses Debian Firefox ESR and
SHA256-pinned Mozilla geckodriver 0.36.0 release artifacts for amd64 and arm64.
It checks immutable private inputs, upload recovery, selective downloads and
offline review without shared project directories or published ports.

Configured runs start their own native publishers in this suite. Training
finishes through a service outage; killing and resuming the publisher reuses
its saved intent and acknowledged stream/upload offsets. Failed and cancelled
runs also reach `synced`, with their original task status preserved in hosted
review. Checkpoints remain absent until explicitly selected.

The service suite also submits real browser login and logout forms over a
generated internal HTTPS certificate. It reproduces `Origin: null` failures
under `no-referrer`, then verifies successful forms under `same-origin`, cookie
security flags and session revocation. The test browser accepts the fixture's
certificate through its test-only WebDriver configuration. `browser-forms.log` records assertions;
`browser-requests.log` records method, path, origin, status and response policy
without passwords, cookies or request bodies.

After the worker uploads two runs, Firefox checks the workspace against those
results: selecting runs updates charts, tabs support the keyboard, logs load on
demand, Refresh preserves the comparison, and filters clear it. The suite checks
desktop columns and overflow at a verified 500 CSS pixel viewport, saving
`workspace-desktop.png` and `workspace-narrow.png` with the CI diagnostics.
The Firefox check covers a narrow layout rather than 320/360 pixel mobile widths.

The Files check opens the fourth tab with native keyboard input, selects an
uploaded checkpoint, generates a scoped CLI command, and downloads the actual
17 MiB attachment through Firefox. It verifies its SHA256 and saves
`workspace-files.png` and `workspace-files-narrow.png`. A test-only HTTPS object
proxy preserves the signed S3 path/query and streams attachments without forwarding
dashboard credentials; Firefox's normal insecure-download protection stays enabled.
The transfer acceptance
kills a CLI pull after the first durable 8 MiB range, then reruns the same command
and verifies that the saved prefix is reused while prior cache files stay intact.

The live-update check republishes new samples, metadata and logs from the
finalized second worker run while Firefox remains open. It checks five-second
probes without unchanged metric fetches, defers replacement during a native
drag, retains zoom/hidden runs/filters/selection, and exercises pause/resume and
the active Logs tab. `workspace-auto-refresh.png` captures the updated chart;
`browser-auto-refresh.log` records these assertions.

Native pointer and keyboard input also checks exact hover readouts, drag zoom,
zoom/reset controls, and per-run legend toggles on both dashboard hosts. Repeated
samples stay individually reachable. Changed previews remove the old chart
handlers; unchanged Refresh retains their controls. Interaction does not fetch
more metric data. Chart scripts remain blocked.
Hover and zoom screenshots are included in the CI diagnostics.

Time-axis checks on both dashboard hosts cover elapsed and date & time hover,
native drag zoom, single-click axis tags, native radio keyboard navigation, and
hidden-run preservation. Firefox runs in `Asia/Shanghai` to verify local timezone
names and exact offsets. Local/UTC tags change the display without refetching
experiment data, rebuilding plots, changing recorded timestamps, or moving the
zoom range. Summary tags also select Last, Minimum, and Maximum in one click.
An entirely legacy
series verifies that rows without timestamps stay in Step view, are counted
as omissions in time views, and retain legend choices when their empty time
view returns to Step. Live publications preserve exact elapsed and UTC
zoom ranges and the selected timezone. `workspace-elapsed.png` and `workspace-wall_clock.png` (plus their
AB counterparts) capture these controls.

The native run-link check starts logged out, follows a project/origin/run link
through the password form, and opens the exact run's Charts tab. Selecting a
different run and refreshing preserves that choice. `browser-deep-link.log`
records the assertions without credentials or request bodies.

The service workflow also opens an AB hostname in Firefox. It verifies distinct
branch asset URLs, the same uploaded runs and comparison values, rejected cookie
replay between hosts, and logout isolation. `workspace-ab.png` records the preview.
