# Coordinated releases

The next prepared release is **0.1.6** for the CLI, all workspace crates, and both
SDKs. Preparation builds distributions and validates installation; it does not
publish packages or reserve registry names.

**Do not push `v0.1.6` yet.** The existing `release.yml` still publishes the nine
original Rust packages on `v*` tag pushes. It does not wait for the new distribution
checks, publish the Rust SDK, or upload Python packages. Coordinated publishing
must be explicitly approved and activated before using a release tag.

## Packages and supported platforms

| Registry | Project | Release contents |
| --- | --- | --- |
| PyPI | `expri` | Precompiled CLI wheels; installs the `expri` executable |
| PyPI | `expri-sdk` | Pure Python wheel and source distribution; imports as `expri` |
| crates.io | `expri` | CLI source package |
| crates.io | `expri-sdk` | Rust task instrumentation library |
| crates.io | `expri-core`, `expri-system`, `expri-runs`, `expri-client`, `expri-assets`, `expri-worker`, `expri-dashboard`, `expri-server` | CLI workspace libraries |

The Python distributions require Python 3.10 or newer. The CLI wheels contain no
Python import package, so `expri` and `expri-sdk` can be installed together. CLI
wheels support Linux with glibc 2.28 or newer on x86-64 and ARM64, and macOS 11 or
newer on Intel and Apple Silicon. Windows and musl-based Linux do not have
precompiled wheels in this release. The CLI has no source distribution on PyPI;
source installation remains available through Cargo.

## Build and review before publishing

Run the metadata guard with Python 3.11 or newer:

```sh
python3 -B scripts/release.py check
python3 -B tests/release_tests.py
```

The guard checks every workspace package version, internal dependency pins,
`Cargo.lock`, both Python project names, the Python SDK runtime version, and the
generated worker SDK helpers. A tag reference must be exactly `v<version>`.
Its `eligible_release_event` output identifies an actual matching tag push;
manual dispatch is always ineligible, even when its selected ref is a tag. This
output does not activate a publisher.

`.github/workflows/distributions.yml` runs on release-related pull requests and
can be dispatched manually for a selected branch. It has read-only repository
permissions and no publishing credentials or OIDC access. It performs:

1. Coordinated metadata and event validation.
2. Python SDK tests, isolated wheel/source builds, and strict distribution checks.
3. Four native CLI builds: Linux x86-64 and ARM64 in manylinux 2.28 containers,
   macOS Intel, and macOS Apple Silicon.
4. Strict CLI wheel metadata checks and offline CLI/SDK co-installation in a fresh
   virtual environment on each platform, including independent uninstallation.

The workflow saves `python-sdk-distributions` and one
`cli-wheel-<Rust target>` artifact for each platform. Download artifacts from the
same successful run when reviewing a candidate. A distribution workflow run does
not create a GitHub Release, tag, registry project, or deployment.

The native wheel builder pins
[maturin-action v1.49.4](https://github.com/PyO3/maturin-action/releases/tag/v1.49.4)
to commit `86b9d133d34bc1b40018696f782949dac11bd380` and uses
[maturin v1.11.5](https://github.com/PyO3/maturin/releases/tag/v1.11.5).
Linux explicitly requests `manylinux_2_28` compatibility. The macOS Intel runner
is `macos-15-intel`; `macos-14` supplies Apple Silicon. These are native runner
architectures in the [GitHub runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

## Registry setup still required

As checked on 2026-10-16, PyPI has no `expri` or `expri-sdk` project. crates.io has
`expri` 0.1.5, while the eight workspace libraries and `expri-sdk` have not been
published. The repository has a GitHub environment named `cargo`; a `pypi`
environment still needs to be created. No first publication or account setup is
performed by this preparation.

### PyPI

An authorized PyPI owner must create a pending trusted publisher for **each** new
project, using these exact values:

| Field | Value |
| --- | --- |
| PyPI project name | `expri`, then separately `expri-sdk` |
| GitHub owner | `agentic-rs` |
| GitHub repository | `expri` |
| Workflow filename | `release.yml` |
| GitHub environment | `pypi` |

Create the GitHub `pypi` environment with the intended release protections before
activation. A pending publisher can create its project at the first approved
upload; registering it does not reserve the name. PyPI documents both
[pending project publishers](https://docs.pypi.org/trusted-publishers/creating-a-project-through-oidc/)
and [publisher configuration](https://docs.pypi.org/trusted-publishers/adding-a-publisher/).

After approval, the dedicated PyPI publishing job should download the validated
artifacts from its build jobs and use
[`pypa/gh-action-pypi-publish@release/v1`](https://docs.pypi.org/trusted-publishers/using-a-publisher/)
under environment `pypi`, with `id-token: write` scoped to that job. Both projects
must trust `release.yml`, not the build-only `distributions.yml`. Builds and smoke
tests must keep their current read-only permissions.

### crates.io and the first coordinated release

crates.io trusted publishing requires an initial manual publication of each new
crate before its publisher can be configured. This differs from PyPI pending
publishers. See the [Rust team's trusted publishing announcement](https://blog.rust-lang.org/2025/07/11/crates-io-development-update-2025-07/).

The first coordinated 0.1.6 release therefore needs an explicitly approved manual
Cargo publication from the reviewed release commit, using credentials belonging
to an authorized owner. Publish these exact 0.1.6 packages in dependency order:

1. `expri-core`
2. `expri-system`
3. `expri-runs`
4. `expri-client`
5. `expri-assets`
6. `expri-worker`
7. `expri-dashboard`
8. `expri-server`
9. `expri-sdk`
10. `expri`

The first nine create new crate names; the last updates the existing CLI crate.
Each successful upload consumes that package's 0.1.6 version. Do not publish
placeholder versions or attempt to upload 0.1.6 again through the legacy tag
workflow. Once the new crates exist, their owners can configure each trusted
publisher with owner `agentic-rs`, repository `expri`, workflow `release.yml`, and
environment `cargo`. Review the existing `expri` publisher at the same time.

Later automated releases should keep Cargo publishing in environment `cargo`
with job-scoped OIDC, publish these ten packages in order, and gate it on exact
version agreement and all distribution checks. A manual dispatch must remain
build-only. The first manual 0.1.6 Cargo publication and its later tag must be
handled deliberately so already-uploaded versions are verified and skipped, or
the legacy tag publisher is replaced before the tag is pushed. This behavior is
not implemented by the preparation workflow.

## Approval and activation boundary

Before approving publication, review the successful four-platform build run,
the release notes, registry ownership, pending PyPI publishers, and the first
Cargo publication plan. Then enable coordinated publishing as a separately
reviewable change. Creating or pushing a tag, configuring registry publishers,
and uploading packages are separate actions from the preparation completed here.
