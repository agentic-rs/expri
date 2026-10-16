# expri CLI on PyPI

`expri` runs remote experiments, transfers artifacts, and monitors workers. After
version 0.1.6 is published, its Python distribution installs a prebuilt native
`expri` command:

```sh
python -m pip install expri==0.1.6
expri --version
```

Installation requires Python 3.10 or newer. The CLI does not require a Rust
compiler or any Python runtime packages. Release wheels support Linux with glibc
2.28 or newer and macOS 11 or newer, on x86_64 and ARM64.
Windows and musl-based Linux distributions are not supported by these wheels.
The release supplies only prebuilt CLI wheels; an unsupported platform cannot fall back
to compiling a source distribution during installation.

For task instrumentation, install the separate Python SDK:

```sh
python -m pip install expri-sdk==0.1.6
```

`expri` owns the command, and `expri-sdk` owns the `import expri` Python package.
They can be installed, upgraded, and removed independently. Installing the CLI
does not install the SDK. See the [project guide](https://github.com/agentic-rs/expri#readme)
and [SDK guide](https://github.com/agentic-rs/expri/tree/main/sdk/python) for usage.

## Building and checking wheels

The root `pyproject.toml` uses [maturin's binary bindings](https://www.maturin.rs/bindings#bin).
The wheel contains the native executable in its scripts installation scheme;
there is no Python wrapper or Python package in the CLI distribution. Its
version comes from the root Cargo package.

```sh
maturin build --release --locked --out dist/cli
uv build --wheel --out-dir dist/sdk sdk/python
python3 scripts/pip_cli_tests.py \
  --cli-wheel dist/cli/expri-0.1.6-py3-none-<platform>.whl \
  --sdk-wheel dist/sdk/expri_sdk-0.1.6-py3-none-any.whl \
  --expected-version 0.1.6
```

Replace `<platform>` with the wheel's actual platform tag. On Linux, build inside
the matching manylinux container and pass `--compatibility manylinux_2_28`;
maturin checks and repairs external library dependencies. Keep those checks
enabled. The Linux build uses glibc so system monitoring can load the host's
NVIDIA management library dynamically when available. Driver libraries are not
bundled with the wheel. See [maturin's distribution guide](https://www.maturin.rs/distribution).

The smoke check inspects wheel metadata and contents, installs both wheels into
a temporary virtual environment without an index, checks the installed command
and SDK, and verifies that uninstalling either distribution leaves the other
usable. Use `--inspect-only` when inspecting a wheel for another architecture;
the release workflow also executes installation checks on matching runners.
