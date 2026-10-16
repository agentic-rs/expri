# Internal crate layout

The Cargo workspace builds one `expri` executable. Controllers, workers, the
local dashboard, and the hosted service keep the same commands, configuration,
run directories, and wire formats.

Directories under `crates/` omit the `expri-` prefix; Cargo package names retain it.

| Package | Responsibility | Internal dependencies |
| --- | --- | --- |
| `expri-core` | Protocol and service contracts, validated configuration values, identifiers, errors, filesystem leases | None |
| `expri-system` | Machine inventory collection, private global profile storage, 72h detail retention, hourly history | `expri-core` |
| `expri-runs` | Run catalogs, metric readers, artifact inventories | `expri-core` |
| `expri-client` | Publishing, durable transfer queues, checkpoint registration, downloads | `expri-core`, `expri-runs` |
| `expri-assets` | Git-tracked asset descriptors, acquisition, verified cache, binding inputs | `expri-core`, `expri-client` |
| `expri-worker` | Python environments, snapshots, execution, supervision, logs, job controls | `expri-core`, `expri-runs`, `expri-client`, `expri-assets` |
| `expri-dashboard` | Shared dashboard views, tables, charts, previews, HTTP routes, embedded frontend | `expri-core`, `expri-runs` |
| `expri-server` | Hosted API, authentication, SQLite tracking and machine profiles, S3, project management, retention | `expri-core`, `expri-runs`, `expri-dashboard`, `expri-system` |
| `expri` | CLI, repository configuration loading, SSH/controller orchestration, local adapters and composition | All eight libraries |

Shared contracts flow from core into data and subsystem implementations; the
executable composes those implementations. Libraries never depend on the CLI.
The client does not depend on the hosted server, and the shared dashboard does
not depend on either worker execution or server storage.

The local dashboard adapter resolves repository and cached-run paths. It adds
publisher status through the CLI's run inspection adapter and implements the
same `DashboardView` as the hosted adapter. Shared routes consume that interface
without loading CLI configuration or invoking an SSH controller.

Embedded resources live with the subsystem that uses them: `catalog.py` belongs
to run data; Python environment, logging, supervision, and metrics producer
scripts belong to the worker; the complete `dashboard_web` directory belongs
to the dashboard. Its generated `app.js` stays committed, so building or
installing the Rust CLI does not require Node.js.

Public task instrumentation packages live separately under `sdk/<lang>`.
`sdk/rust` is the `expri-sdk` crate, with its own version and lightweight
dependencies; the CLI does not depend on it. `sdk/python` packages the `expri`
Python API and owns the standalone metrics/status helper sources. The worker's
Python copies are generated with `scripts/sync_python_sdk.py` so its Cargo
package can embed them without referring outside its package directory.

## Validation and packaging

Check every workspace library and the existing CLI integration tests:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo +1.89.0 build --locked --bin expri
```

The Python, frontend, host/worker, and self-hosted S3 suites continue to exercise
the existing workflow. The container builder copies all crates; deployment
builds receive the complete Git source archive. Dashboard build commands and AB
bundles use `crates/dashboard/dashboard_web`.

Internal dependencies specify both a local path and an exact version. Cargo
removes dependency paths when packaging, so an ordinary dependent-package dry
run needs the library versions to exist in the registry. Before their initial
publication, verify packaged sources using temporary local patches:

1. Package `expri-core` with `cargo +1.89.0 package --locked -p expri-core`.
2. Extract its `.crate` archive from `target/package/` into a temporary directory
   outside the checkout, so Cargo does not discover the parent workspace.
3. Package remaining libraries in the release order below. Supply patches for
   internal dependencies pointing to their already extracted archives, for
   example `--config 'patch.crates-io.expri-core.path="/absolute/extracted/expri-core-VERSION"'`.
4. Extract each new package before packaging its consumers, then package
   `expri` with all internal dependencies patched to those extracted sources.

Use `cargo +1.89.0 package -p PACKAGE` for each package. Temporary path patches
can change lockfile source entries, so these verification commands may need to
omit `--locked`; preserve the committed workspace lockfile byte-for-byte. For
uncommitted changes, add `--allow-dirty`. A shared `CARGO_TARGET_DIR` can reuse
compiled dependencies. Inspect normalized manifests and embedded resources,
run tests from extracted packages, and check the packaged CLI's `--version`.
These checks verify shipped sources without claiming a normal registry dry run
succeeded.

## Release

The libraries remain internal implementation boundaries, but must be published
so `cargo install expri --locked` can resolve them. Keep all package versions and
exact internal dependency versions aligned. The release following v0.1.5 needs
a new version because that CLI version is already published.

The independently versioned `expri-sdk` crate and Python distribution have
separate release lifecycles. The CLI tag workflow below publishes only the
internal crates and executable. SDK package validation runs in CI; introducing
their source directories does not publish either SDK to a registry.

Configure crates.io trusted publishing for each library and `expri`, using this
repository's `release.yml` workflow and `cargo` environment. The existing CLI
publisher registration does not authorize the new package names automatically.
New crate names need an initial manual publication before their trusted
publisher can be configured, as described in the
[Rust trusted publishing design](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html#trusted-publisher-configuration-on-cratesio).
Complete that one-time bootstrap separately before using the tag workflow;
versions selected for a tag must still be unpublished for every package.

The tag workflow publishes in dependency order:

```sh
cargo publish --locked -p expri-core
cargo publish --locked -p expri-system
cargo publish --locked -p expri-runs
cargo publish --locked -p expri-client
cargo publish --locked -p expri-assets
cargo publish --locked -p expri-worker
cargo publish --locked -p expri-dashboard
cargo publish --locked -p expri-server
cargo publish --locked -p expri
```

These explicit package commands work with Rust 1.89, where workspace-wide
publication is unstable. Crate extraction and packaging checks do not publish
anything.
