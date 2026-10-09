# Main and branch dashboard deployments

The main site and the AB preview share one running service, SQLite catalog,
Backblaze/S3 storage, and recorded run data. Each hostname has its own browser
sessions. The dashboard password is shared, but signing in or out on one site
does not sign in or out on the other.

AB previews the selected branch's `dashboard_web` HTML, JavaScript, and CSS.
The API and Rust chart renderer come from the main service. Interactive chart
controls are bundled into `app.js` and enhance the sampled SVG returned by that
service, so an AB asset deployment can preview those controls too. Do not start
a second service against the same data directory: the service deliberately
holds an exclusive lease on its store.

Auto refresh uses `/api/updates` when the shared backend supports it. A branch
preview against an older main backend falls back to bounded snapshots every
30 seconds and displays that cadence. Merge and deploy main to enable the
five-second change probes on both sites; an AB asset deployment does not upgrade
the backend.

Time axes require the corresponding Rust chart renderer on the shared backend.
Deploy the merged main release before publishing branch assets that offer
elapsed or wall-clock charts; an AB asset deployment alone cannot add an API
query option or change SVG coordinates.

## Configure the preview once

Keep the existing `[dashboard]` settings and add a preview to `server.toml`:

```toml
[dashboard]
public_url = "https://expri.example.net"
password_env = "EXPRI_DASHBOARD_PASSWORD"

[[dashboard.previews]]
public_url = "https://ab.expri.example.net"
assets_dir = "/opt/expri/dashboard/ab"
```

The preview directory must contain a valid initial bundle before restarting the
service. Each site needs a distinct HTTPS origin; wildcards are not accepted in
the application configuration. Up to eight preview sites can share the service.

A bundle contains `index.html`, `login.html`, `app.js`, `styles.css`, and
`deployment.json`. The manifest records the full Git commit and source branch:

```json
{"commit":"0123456789abcdef0123456789abcdef01234567","branch":"feature/layout"}
```

The directory layout is:

```text
/opt/expri/dashboard/ab/
  current -> releases/0123456789abcdef0123456789abcdef01234567
  releases/
    0123456789abcdef0123456789abcdef01234567/
      index.html
      login.html
      app.js
      styles.css
      deployment.json
```

The web files must be UTF-8 regular files no larger than 2 MiB each. The manifest
is limited to 16 KiB. Keep `<!-- LOGIN_ERROR -->` in the login template. Publish
bundles as immutable directories owned by the deployment user and readable by
the service account. The service resolves `current` for each page, so changing
that symlink updates the preview without restarting the API. HTML pins asset
URLs to its commit; retain old releases so pages opened before a deployment can
still load the matching assets.

## DNS, HTTPS, and Nginx

An `A` record for `*.expri.example.net` can point every preview name at the server;
keep the `expri.example.net` record for the main site. DNS routing does not create
application sites or HTTPS certificates. Configure the named sites explicitly
and reject unknown hosts. Individual certificates for main and AB are sufficient;
a wildcard certificate is optional and requires DNS validation with Let's Encrypt.

Use a separate Nginx vhost for the preview and preserve `Host`, `Origin`,
`Cookie`, and `Sec-Fetch-Site`. Both sites proxy dashboard requests to the same
loopback listener. Retain the existing buffering, timeout, login rate limit,
`Referrer-Policy: same-origin`, and no-cache settings described in
[self-hosted service setup](self-hosted-service.md). Do not rewrite the preview's
Host or Origin to the main site's values.

The preview vhost should deny the writable CLI endpoint:

```nginx
location = /v1/request {
  return 404;
}

location = /login {
  limit_req zone=expri_login burst=5 nodelay;
  limit_req_status 429;
  include /etc/nginx/snippets/expri-proxy.conf;
}

location / {
  include /etc/nginx/snippets/expri-proxy.conf;
}
```

The application also denies CLI requests addressed to a configured preview host.
Workers and uploaders continue using the main service URL.

## Release and recovery

Deployment is an explicit operator action. Selecting `main` deploys the remote
main commit and its service binary; selecting another branch updates the one AB
UI slot. No GitHub SSH secrets, server-side build, or permanent CI runner is
required. Build and transfer exact committed source, not a dirty working tree.

For the Vultr installation, run these commands from a clean checkout. The local
machine needs Python 3.11.8 or newer and an authenticated `ctl` host named
`vultr-2`. Main builds also need Docker. Set `--ctl /path/to/ctl` when using a
development build of the host client. The server needs Python 3.11 or newer,
and `ctl exec` must run the installer as root. The main build targets x86-64
Linux with glibc.

```sh
# Inspect a feature deployment without contacting the server.
python3 scripts/deploy.py --ref codex/dashboard-layout --dry-run

# Publish the committed assets from this local feature branch to AB.
python3 scripts/deploy.py --ref codex/dashboard-layout

# Fetch origin/main, build its Linux binary, and update the main service.
python3 scripts/deploy.py --ref main
```

Omitting `--ref` selects the current named branch. Feature branches resolve
locally; `main` always fetches `origin/main` before a real deployment. A main
dry run reports the cached remote commit and does not fetch. Commit and build
the TypeScript and TSX changes before deploying; the release uses the committed
`dashboard_web/app.js`, which CI checks against its source.
React is bundled into that asset; neither AB nor the main service needs a
Node.js runtime or a frontend dependency installation.

The installer verifies the transferred archive, takes a deployment lock, and
switches the release symlink atomically. Reusing a commit through another branch
name preserves the original bundle's build provenance. The scripts target this
installation's hostnames, systemd service, and paths; adapt those constants when
setting up another server.

A main deployment restarts the service and expires existing browser sessions.
An AB asset deployment leaves the API, worker uploads, and existing sessions
running. Keep the previous binary and preview release for rollback. The installer
reads `/var/lib/expri/metadata.sqlite3`'s schema version through a read-only SQLite
connection before and after a main deployment, including committed WAL changes.
After a failed startup or health check, it stops the new service before checking
the schema, so a process still starting cannot migrate during rollback. It rolls
back a failed backend only when that version is unchanged. If the new process
changed the schema, or the schema cannot be read after startup, it keeps the new
binary selected with the service stopped and reports that operator recovery is
required. Failure to stop the service also prevents automatic rollback. This
avoids starting an older binary against data it cannot read. AB asset failures
still restore the previous asset pointer without restarting the backend.

Before a schema upgrade, check the actual systemd `--data-dir`, free disk space,
and the current binary link. Stop the service and back up the entire data
directory while it is stopped: include SQLite and its WAL/SHM files, tracking
files, archive staging files, and multipart receipts. Keep the backup outside
the data directory, restrict it to root, and verify that it can be read before
continuing. A SQLite-only copy is insufficient. Restart the existing service if
the release is still being prepared; the installer checks configured previews
before activation. Record the backup time and old binary revision so a recovery
has a known boundary.

For the version 2 upgrade, older binaries refuse the upgraded directory. Prefer
repairing or restarting the new release while retaining received data. A backend
downgrade requires an explicit operator decision: stop the service, preserve
the upgraded directory separately, restore the complete pre-upgrade backup with
its ownership and permissions, select the compatible old binary, and restart.
This loses uploads received after the backup. The deployment scripts never
restore or overwrite service data automatically. During an ordinary code
rollback with an unchanged schema, keep the current data directory.

Both origins must be checked after setup: native login/logout, independent
sessions, shared catalogs and run values, correct asset revisions, and rejected
cross-origin requests. The container acceptance suite exercises this with
Firefox and two HTTPS hostnames on its internal network.
