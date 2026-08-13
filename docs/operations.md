# Operations

## Daemon (`bookclerkd`)

```bash
export BOOKCLERK_FILES_DIR=/var/lib/bookclerk
bookclerkd
```

Reads `config.toml` from the files dir (or `BOOKCLERK_CONFIG`). Schedules:

- library scan every `library.scan_interval_minutes`
- auto-acquire when `library.auto_acquire = true` (keep **false** until ready)

HTTP control plane (default `127.0.0.1:8787`):

| Endpoint | Auth | Purpose |
| --- | --- | --- |
| `GET /health` | no | Liveness |
| `POST /api/auth/login` | no | Operator token → session cookie |
| `POST /api/auth/bootstrap` | operator | Create first Owner (once) |
| `POST /api/auth/elevate` | owner + password / passkey / OIDC step-up | Short-lived elevated operator session |
| `GET` / `POST` / `PATCH` `/api/users…` | provisioner | List/create/patch users; remint claim tickets |
| `GET` / `POST` `/api/plugins/{id}/consent` | operator | Plugin grant status / approve (widen or narrow; host-capped) |
| `GET` / `PATCH` `/api/settings` | operator | Daemon, library, plugins, confinement knobs |
| `GET /api/status` (also `/status`) | yes | Status snapshot |
| `POST /api/library/scan` (also `/scan`) | yes | Queue scan (`Content-Type: application/json`) |
| `POST /api/library/acquire` (also `/acquire`) | yes | Queue acquire |
| `GET /api/library/books` | yes | Book rows for the GUI |
| `GET /api/jobs` (also `/jobs`) | yes | Job list |
| `/` static UI | no | Built React SPA when `ui/dist` is present |

Override listen with `BOOKCLERK_DAEMON_LISTEN` or `daemon.listen`. Defaults to
**both** loopbacks: `["127.0.0.1:8787", "[::1]:8787"]`. TOML accepts a string or
an array; env/CLI accept a single address or a comma-separated list. IPv6 uses
bracketed form (`[::1]:8787`). The daemon binds each address (skips failures if
at least one succeeds). Changing `daemon.listen` and reloading
(`POST /api/config/reload` or SIGHUP) **rebinds without restarting**. Reload also
rebuilds operator auth, sources, integrations, destinations, and (when needed)
the database plugin as one transactional swap — auth is published **before** any
listen rebind so a public listener never outruns middleware. The tray opens
`http://localhost:<port>` and leaves resolution to the OS.

Operator auth defaults **on** (`[daemon.auth]`). The token is sealed in
`encrypted_secrets` (legacy `operator.token` files are imported once then
deleted). Optional override: `BOOKCLERK_OPERATOR_TOKEN`. Show or rotate with
`bookclerk daemon token` / `bookclerk daemon token rotate`. Browser operator
sessions are stored hashed in `operator_sessions` (survive restart; logout
revokes server-side). The system tray **Copy operator token** menu copies to
the clipboard and never prints the value.

User provisioning is role-scoped: Administrators may manage Members only;
non-elevated Owners may manage Members and Administrators (not Owners);
Operator tokens and elevated Owners may assign any role, including Owner.
Last-active-Owner demote/disable/delete is refused. Changing an existing
password requires the current password (invite/bootstrap first-password setup
does not). Elevated Operator sessions are revoked when the origin Owner is
demoted, disabled, deleted, or has their password changed.

The Owner role is greenfield. Testing/dev hosts that already have a
`library.db` from before this change should recreate it (`cargo reset --yes`)
and re-bootstrap; there is no Admin→Owner upgrade migration.

When exposing the daemon behind TLS, set `integrations.public_origin =
"https://…"` so session cookies gain the `Secure` flag. List reverse-proxy
peers in `daemon.trusted_proxies` (IP or CIDR) before login throttling will
honor `X-Forwarded-For` — empty means always use the direct TCP peer. Do not
expose publicly without TLS (reverse proxy) and a protected token. Details:
[gui.md](gui.md).

### Reverse proxy + TLS

Terminate TLS at the proxy and forward to loopback `bookclerkd`. Set
`integrations.public_origin` to the **external** `https://` origin (no trailing
slash). Cookie-authenticated `POST` / `PATCH` / `DELETE` under `/api/*` require
a matching `Origin` (or `Referer`) host; login / redeem / password paths are
exempt. Example nginx:

```nginx
server {
  listen 443 ssl http2;
  server_name bookclerk.example.com;
  # ssl_certificate …; ssl_certificate_key …;

  location / {
    proxy_pass http://127.0.0.1:8787;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_http_version 1.1;
  }
}
```

```toml
[daemon]
trusted_proxies = ["127.0.0.1", "::1"]

[integrations]
public_origin = "https://bookclerk.example.com"
```

### Hot-reloadable settings

| Setting | On reload |
| --- | --- |
| `daemon.listen` | Rebind listeners (after auth swap); preflight-binds free ports and rolls back to the last successful bind if rebind fails |
| `daemon.auth.*` / operator token | Rebuild `OperatorAuthState` (DB sessions unchanged; previous token accepted for ~60s after rotate/reload) |
| `sources.*` / `integrations.*` / `output.*` | Rebuild registries; integration watchers stopped then restarted |
| `database.plugin` | Re-open library + destinations |
| `[media]` | Swap media worker pool |

`GET /api/settings` returns both `settings` (configured) and `effective`
(runtime auth flag + loaded plugin ids). `GET /api/status` includes
`auth_enabled` for the live middleware.

Talk to a running daemon from the CLI (sends Bearer when auth is enabled):

```bash
bookclerk daemon health
bookclerk daemon status
bookclerk daemon token
bookclerk daemon token rotate
bookclerk daemon jobs
bookclerk daemon scan [--account <id>]
bookclerk daemon acquire [--asin <id>] [--account <id>]
```

## systemd

Sample unit: [`packaging/systemd/bookclerkd.service`](../packaging/systemd/bookclerkd.service).

```bash
# after installing bookclerkd, bookclerk-media-worker, and bookclerk-jail to
# /usr/local/bin and creating user/dirs:
sudo systemctl enable --now bookclerkd
```

Install all three from the same directory. `bookclerkd` looks for its helpers
beside itself and, by default, refuses media work or declines to load an external
plugin rather than running either unconfined.

Highlights from the sample unit:

- `Environment=BOOKCLERK_FILES_DIR=/var/lib/bookclerk`
- `ProtectSystem=strict` + `ReadWritePaths=/var/lib/bookclerk`
- Prefer `BOOKCLERK_AUTH_PASSWORD` (or `[auth].password`) — not under the files dir.
  Wrap an existing BCK1 key later with `bookclerk config master-key wrap` or
  reload `bookclerkd` after setting the password (SIGHUP / `POST /api/config/reload`).

If acquired media lives outside the files dir, set an absolute
`output.local.root` and add that path to `ReadWritePaths`.

## Docker

Dockerfile: [`packaging/docker/Dockerfile`](../packaging/docker/Dockerfile)
(repo-root convenience symlink: `Dockerfile`).

For day-to-day compile/test with a controlled OpenSSL/Node toolchain (and host-
runnable `target/` binaries), use the [Dev Container](devcontainer.md) instead
of fighting missing `libssl-dev` on the host.

```bash
docker build -f packaging/docker/Dockerfile -t bookclerkd .
docker run --rm \
  -v bookclerk-config:/config \
  -v bookclerk-data:/data \
  -e BOOKCLERK_AUTH_PASSWORD='your-strong-passphrase' \
  bookclerkd
```

| Path / env | Role |
| --- | --- |
| `/config` | `BOOKCLERK_FILES_DIR` (`config.toml`, `library.db` incl. `encrypted_secrets`) |
| `/data` | Default books root (`BOOKCLERK_OUTPUT_LOCAL_ROOT=/data/Audiobooks`) |
| `BOOKCLERK_DAEMON_LISTEN` | Default loopback inside the container |

To publish the UI / API (keep operator auth enabled; terminate TLS at a proxy):

```bash
docker run … -e BOOKCLERK_DAEMON_LISTEN=0.0.0.0:8787 \
  -e BOOKCLERK_OPERATOR_TOKEN="$(bookclerk daemon token)" \
  -p 8787:8787 bookclerkd
```

(Prefer sealing the token in `library.db` under `/config` and omitting the env
override once the volume is initialized.)

Copy `/etc/bookclerk/config.example.toml` from the image as a starting config.

Both confined tiers work under the default Docker seccomp profile, which has
allowed the `landlock_*` syscalls since Docker 20.10.14. On an older engine the
syscalls are refused, and `bookclerkd` says so at startup instead of quietly
running codecs and plugin guests unconfined. Do not reach for
`--security-opt seccomp=unconfined` to fix it: that removes a boundary rather
than restoring one. Upgrade the engine, or lower `isolation` deliberately.

## Logging

- stderr + OS facility (journald / macOS `os_log` / Windows Event Log)
- `BOOKCLERK_LOG` → `RUST_LOG` → default `bookclerk=info,warn`
- CLI: `-v` / `-vv`
- Secrets are redacted; diagnostics uploads abort if a registered secret remains

Opt-in crash/error reporting: [diagnostics.md](diagnostics.md).

## Safe live testing

When exercising real store credentials:

1. Keep `library.auto_acquire = false`.
2. After login, `bookclerk auth set-scan <account> --scan false`.
3. Acquire **one** title (`--asin` / `--isbn` / UUID), not the whole library.
4. Prefer CLI over `POST /scan` / `/acquire` so nothing bulk-queues.
5. One-shot sync without re-enabling scan: `library scan --account <id>`.
