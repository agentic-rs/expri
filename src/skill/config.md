# Expri configuration

Reports show tables by default. Agents and scripts should add `--json` to read
structured results; `--quiet` suppresses human reports.

Read the existing project configuration before adding tasks or selecting a
worker. Most controller commands load `expri.toml` from the current directory;
they do not search ancestor directories.

## Global server connection

Authenticate with the primary dashboard password to save the global connection:

```sh
expri login --url https://expri.example.net
expri config server show
```

The prompt hides the password. Login saves a separate issued API token, without
saving the password or printing credentials. `expri login` later uses the saved
URL; `expri logout` revokes its issued token and keeps the URL for future login.
Read `expri skill login` for stdin input, token lifecycle, and server requirements.

Query and service client commands use `[server]` in `~/.tokn/expri/config.toml`.
Existing manual credentials remain supported:

```toml
[server]
url = "https://expri.example.net"
token = "your-server-token"
```

`expri config server set --url URL --token-stdin` stores a token supplied through
a pipe or redirected file. Updates preserve system-profile settings and use
private, atomic writes. `config server show` hides credentials;
`config server unset` removes the connection without revoking a remote token.
`EXPRI_STORAGE_DIR` overrides the global root; config, login, and logout also
accept `--storage-dir`.

Query and service client `--config PATH` selects another global or standalone
connection file. Existing `token_env` files remain supported. `expri service
serve --config PATH` selects the daemon's own settings. Project controller
`--config` still selects `expri.toml`, as described below.

## Paths and private targets

- `--config /path/to/expri.toml` selects the config. Its parent directory is the
  default repository root.
- `--repo /path/to/checkout` overrides the repository root, but does not change
  which config file is loaded. Pass both flags when those locations differ.
- Targets are loaded from the sibling target file: `expri.toml` pairs with
  `expri.target.toml`; `experiment.toml` pairs with `experiment.target.toml`.
  Keep the target file Git-ignored in the experiment repository.
- Put `-T TARGET` before the command. Local `run` ignores implicit target
  selection: omitting `-T` means local execution. For remote-only commands such
  as `push`, explicitly selecting a target also avoids ambiguity.

## Minimal managed experiment

In the experiment repository's `expri.toml`:

```toml
[project]
name = "my-project"

[environment]

[tasks]
train = ["python", "scripts/train.py"]
```

An empty `[environment]` enables ordinary uv isolation, recorded runs, and
detach. Install uv on the executing machine. Without an environment, legacy
array tasks run directly in the repository root; the object form
`train = { command = ["python", "scripts/train.py"], uv = true }` uses `uv run`
but does not enable managed run records or detach by itself.

In the Git-ignored `expri.target.toml`:

```toml
[target.gpu]
host = "user@example.com"
remote_dir = "~/my-project"
protocol = "auto"
node_bin = "expri"
```

SSH is the default transport. For an existing ctl host, set `transport = "ctl"`
and use its saved name or ID in `host`. `protocol` selects the worker
implementation: `auto` uses native expri when capable and falls back to Python
for supported operations; `expri-node` requires a capable native worker.
Managed assets and automatic publishing require native capabilities.

Targets inherit the top-level environment and service tables when no target
override exists. A target override replaces the entire table; fields are not
merged. Store credentials in ignored machine-local client configuration, using
the formats in `expri skill assets` and `expri skill service`.

```sh
expri run --config /path/to/expri.toml --dry-run train
expri -T gpu push --config /path/to/expri.toml --dry-run
```

Read `expri skill environment` for package reuse and setup, `expri skill run`
for task launch, and `expri push --help` or `expri run --help` for exact flags.
