# Expri server login

Reports show tables by default. Agents and scripts should add `--json` to read
structured results; `--quiet` suppresses human reports.

Use the primary hosted dashboard password to authenticate this machine:

```sh
expri login --url https://expri.example.net
expri config server show
expri query runs
```

The interactive terminal prompt hides the password. Login exchanges it for a
separate API token with unrestricted server access and saves that token in
`[server]` in `~/.tokn/expri/config.toml`. It never saves the dashboard password
or prints credentials. Configuration files use mode `0600` on Unix. Writes are
private and atomic and preserve system-profile
settings. A connection changed during authentication is not overwritten.

Once the URL is saved, `expri login` defaults to it. To provide a password through
a pipe or redirected file, use `--password-stdin`:

```sh
printf '%s' "$EXPRI_DASHBOARD_PASSWORD" | expri login --url https://expri.example.net --password-stdin
```

Login and logout run on the current machine. `--storage-dir PATH` overrides
`EXPRI_STORAGE_DIR` and the default global directory. The server must support
`POST /v1/login` and `POST /v1/logout`; upgrade it if these endpoints are missing.
Use the main configured dashboard host. Preview and unknown hosts cannot issue
CLI credentials.

Issued tokens have no expiry and survive server restarts. Changing the dashboard
password, unrestricted server API token, or primary dashboard authority and
restarting the server invalidates them. Each client has a separate issued token;
logging out one client leaves other tokens and browser sessions active.

```sh
expri logout
expri config server show
expri login
```

Logout revokes the current issued token and clears its saved credential while
keeping the URL and project selection. An already rejected/revoked token is
cleared with a warning. Other server or transport errors preserve the credential
for retry. With no saved credential, logout does nothing. A saved URL without a
credential reports `token_source: null` in `--json` output and requires login before client queries.

Existing manually configured tokens and `token_env` files still work. Logout
cannot revoke static credentials; `expri config server unset` removes that local
connection without changing the remote token. Service daemon settings and
worker-local publishing configuration remain separate.

Continue with `expri skill query` to inspect published runs/files and
`expri skill service` to fetch selected results. Use `expri login --help` and
`expri logout --help` for exact options.
