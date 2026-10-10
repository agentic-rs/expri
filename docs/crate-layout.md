# Internal crate layout

The workspace builds one `expri` executable. Its commands, configuration, run
directories, and wire formats remain shared across controller, worker, and
server modes.

| Package | Responsibility | Internal dependencies |
| --- | --- | --- |
| `expri-core` | Shared protocol and configuration values, identifiers, errors, and filesystem leases | None |
| `expri-run-data` | Run catalogs, metric readers, and artifact inventories | `expri-core` |
| `expri` | CLI, configuration loading, controller and worker execution, transfers, dashboards, and hosted server | Both libraries |

Dependency direction is `expri` → `expri-run-data` → `expri-core`; the CLI also
uses core directly. Neither library depends on the CLI, HTTP clients, SQLite,
or S3. CLI parsing and chart rendering tests stay with the executable rather
than introducing dependencies from run data into those layers. The Python run
catalog is packaged with `expri-run-data`; other embedded Python and dashboard
assets remain with `expri`.

The CLI's run inspection adapter adds publisher status to the stored run report.
The data library reads records without depending on publisher execution.

Future extractions can separate the client and transfer queues, asset
acquisition, and worker execution, followed by dashboard presentation and the
hosted server. Preserve this dependency direction and the single executable
while moving each subsystem. Split shared contracts before their consumers;
avoid a generic utility crate that absorbs unrelated implementation details.

## Validation and packaging

Run all workspace libraries and the existing CLI integration tests:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo +1.89.0 build --locked --bin expri
```

The existing Python, frontend, host/worker, and self-hosted S3 suites still
exercise the executable. The container builder copies the workspace crates;
deployment builds already receive the complete Git source archive.

Internal dependencies specify both a local path and an exact version. Cargo
removes those paths when packaging, so an ordinary dependent-package dry run
needs the library versions to exist in the registry. Before their first
publication, verify packaged sources using temporary local patches instead:

1. Package `expri-core` with `cargo +1.89.0 package --locked -p expri-core`.
2. Extract its `.crate` archive from `target/package/` into a temporary directory
   outside the checkout, so Cargo does not discover the parent workspace.
3. Package `expri-run-data`, passing
   `--config 'patch.crates-io.expri-core.path="/absolute/extracted/expri-core-VERSION"'`.
4. Extract that package too, then package `expri` with patches pointing to both
   extracted libraries.

Use `cargo +1.89.0 package -p PACKAGE` in steps 3 and 4. Temporary path patches
can change lockfile source entries, so these verification commands may need to
omit `--locked`; keep the committed workspace lockfile unchanged. When checking
uncommitted changes, add `--allow-dirty`. Inspect each archive's manifest and
embedded assets, and run the packaged CLI's `--version`. This verifies shipped
sources locally without claiming a normal registry dry run succeeded.

## Release

The libraries remain internal implementation boundaries, but must be published
so `cargo install expri --locked` can resolve them. Keep all package versions and
their exact internal dependency versions aligned. The release following
v0.1.5 needs a new version because that CLI version is already published.

Configure crates.io trusted publishing for **each** of `expri-core`,
`expri-run-data`, and `expri`, using this repository's `release.yml` workflow
and `cargo` environment. The existing CLI publisher registration does not
authorize publishing the two new package names automatically.

New crate names need an initial manual publication before their trusted
publisher can be configured, as described in the
[Rust trusted publishing design](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html#trusted-publisher-configuration-on-cratesio).
Complete that one-time bootstrap separately before using the tag workflow;
the versions selected for a tag must still be unpublished for all three
packages.

The tag workflow publishes in dependency order:

```sh
cargo publish --locked -p expri-core
cargo publish --locked -p expri-run-data
cargo publish --locked -p expri
```

These explicit package commands work with Rust 1.89, where workspace-wide
publication is unstable. Publishing is a release action; crate extraction and
packaging validation do not publish anything.
