# Running Bookclerk

This page runs Bookclerk as a server: the daemon, putting it behind a TLS
proxy, systemd, Docker and logs. To try Bookclerk on your own computer
first, see [Getting started](../guide/getting-started.md).

## Daemon (`bookclerkd`)

`bookclerkd` is the long-running server. Start it with a files directory:

```sh
export BOOKCLERK_FILES_DIR=/var/lib/bookclerk
bookclerkd
```

At start, the daemon:

1. reads the bootstrap settings from `config.toml` (or `BOOKCLERK_CONFIG`);
2. opens the library database;
3. enrolls a stable host id (`node-identity.json`);
4. loads `[events]` and `[output]` from the database. The other sections
   still come from TOML.

It then runs two schedules:

- a library scan every `library.scan_interval_minutes`;
- auto-acquire, when `library.auto_acquire = true`.

> [!WARNING]
> Keep `library.auto_acquire = false` until you are happy with where
> files land.

### Listen addresses

By default the daemon listens on **both** loopbacks:
`["127.0.0.1:8787", "[::1]:8787"]`.

- Change it with `daemon.listen` or `BOOKCLERK_DAEMON_LISTEN`.
- TOML takes a string or an array. The environment and CLI take one
  address or a comma-separated list.
- Write IPv6 in brackets: `[::1]:8787`.
- The daemon binds each address, and skips failures as long as one
  succeeds.

Changing `daemon.listen` and reloading (`POST /api/config/reload` or
SIGHUP) **rebinds without a restart**. See
[Changing settings without a restart](#changing-settings-without-a-restart).

The tray opens `http://localhost:<port>` and leaves name resolution to the
OS.

### HTTP API

| Endpoint | Sign-in | Purpose |
| --- | --- | --- |
| `GET /health` | None | Liveness |
| `POST /api/auth/login` | None | Operator token to session cookie |
| `POST /api/auth/bootstrap` | Operator | Create the first Owner (once) |
| `POST /api/auth/elevate` | Owner, with password, passkey or OIDC step-up | A short-lived elevated operator session |
| `GET`/`POST`/`PATCH /api/users…` | Provisioner | List, create and change users; mint new claim tickets |
| `GET`/`POST /api/plugins/{id}/consent` | Operator | A plugin's grant status, or approve it (widen or narrow, within host caps) |
| `GET`/`PATCH /api/settings` | Operator | Daemon, library, plugin and confinement settings. `events.*` and `output.*` are compare-and-swap against the database (one authority per request) |
| `GET`/`PUT /api/config/domains/core.events` | Operator | The authoritative `[events]` document and revision |
| `GET`/`PUT /api/config/domains/core.output` | Operator | The authoritative shared `[output]` document and revision (`{expected_revision, document}`) |
| `GET`/`PUT /api/auth/mfa-policy` | Owner or operator | `require_second_factor`: password sign-in needs TOTP or a passkey |
| `GET /api/status` (also `/status`) | Yes | Status snapshot |
| `POST /api/library/scan` (also `/scan`) | Yes | Queue a scan |
| `POST /api/library/acquire` (also `/acquire`) | Yes | Queue an acquire |
| `GET /api/library/books` | Yes | Book rows for the web app |
| `GET /api/jobs` (also `/jobs`) | Yes | The durable job list: progress, attempts, times |
| `POST /api/jobs/{id}/cancel` | Yes | Cancel pending work, or ask running work to stop |
| `/` | None | The web app, when `ui/dist` is present |

- `POST` bodies need `Content-Type: application/json`.
- Scan and acquire return `409` if one is already pending or running, and
  `429` if the queue is full.
- `GET /api/settings` returns `settings` (configured) and `effective` (the
  live auth flag and the loaded plugin ids). `GET /api/status` includes
  `auth_enabled`.

The table lists the common routes. The daemon describes every route in an
OpenAPI document, with no sign-in needed:

- `GET /api/openapi.json` is OpenAPI 3.1. Open it in a viewer such as
  [Scalar](https://scalar.com) or Swagger UI, or generate a client from it.
- `GET /api/openapi-3.0.json` is the same API restated as OpenAPI 3.0.3,
  for tools that only read 3.0. Use it for
  [request validation at the proxy](#validate-api-requests-at-the-proxy).

### The CLI against a running daemon

These commands send the Bearer token when auth is on:

```sh
bookclerk daemon health
bookclerk daemon status
bookclerk login
bookclerk daemon token
bookclerk daemon token rotate
bookclerk daemon jobs
bookclerk daemon scan [--account <id>]
bookclerk daemon acquire [--asin <id>] [--account <id>]
```

Every command and option is in the [CLI reference](cli-reference.md).

## Operator sign-in

Operator auth is on by default (`[daemon.auth]`).

### The operator token

- The token is sealed in `encrypted_secrets`. Legacy `operator.token`
  files are imported once, then deleted.
- `BOOKCLERK_OPERATOR_TOKEN` overrides it.
- Show the durable Bearer token with `bookclerk daemon token`, and replace
  it with `bookclerk daemon token rotate`.
- `Authorization: Bearer` API access always works.

Once an Owner exists, the web sign-in form refuses the token
(`POST /api/auth/login` returns 403). Use a sign-in link or Owner
elevation instead.

### Sign-in links

For a browser session, use the tray's **Copy sign-in link**, or run
`bookclerk login`. Either gives a one-time loopback URL:
`http://localhost:<port>/api/auth/tray-handoff?code=…`.

Under the hood, the CLI calls `POST /api/auth/tray-handoff/prepare` with
the Bearer token, then opens `GET /api/auth/tray-handoff?code=…` with the
one-time code. The durable token never appears in the URL.

The handoff is refused unless:

- the TCP peer is loopback;
- `Host` is a single, well-formed `localhost`, `127.0.0.1` or `::1`
  authority;
- there are no `X-Forwarded-*`, `Forwarded`, `Via` or `X-Real-IP`
  headers.

So a reverse proxy on the same host can't mint an operator cookie on the
public address.

More about the code:

- It is stored hashed in memory, registered for log redaction, and the
  redirect sets `Referrer-Policy: no-referrer`.
- It lasts `[daemon.auth] tray_handoff_ttl_secs`: **180** seconds by
  default, clamped to 30–900. The variable is
  `BOOKCLERK_DAEMON_AUTH_TRAY_HANDOFF_TTL_SECS`.
- The tray copies the URL for that long, then removes that exact URL from
  the clipboard if it is still there. It never copies the durable token.
- **Copy sign-in link** and **Open Bookclerk** each mint a new code, and
  invalidate any unused earlier one.

Browser operator sessions are stored hashed in `operator_sessions`. They
survive a restart, and logging out revokes them on the server.

### Who can manage whom

- Administrators manage Members only.
- Owners who haven't elevated manage Members and Administrators, not
  Owners.
- The operator token and elevated Owners can assign any role, including
  Owner.
- Demoting, disabling or deleting the last active Owner is refused.
- Changing a password needs the current one (setting the first password
  from an invite or bootstrap doesn't).
- An elevated session ends when its Owner is demoted, disabled, deleted
  or has their password changed.

Details: [Web app and sign-in](sign-in.md).

## Reverse proxy + TLS

To reach Bookclerk from other computers, terminate TLS at a reverse proxy
and forward to `bookclerkd` on loopback.

> [!CAUTION]
> Don't expose Bookclerk without TLS and a protected operator token.

### What the public address does

Set `integrations.public_origin` to the **external** `https://` address,
with no trailing slash. It is required off loopback. It becomes:

- the OIDC issuer for players;
- the WebAuthn relying party for passkeys;
- the base for invite links and SSO callbacks;
- the reason session cookies get the `Secure` flag.

Without it, Bookclerk accepts only a **bound loopback** `Origin` or
`Host` (matching the `daemon.listen` ports), and otherwise uses
`http://localhost:<ui-port>`. Hostile host names, unbound ports and
forwarded headers (`X-Forwarded-*`, `Forwarded`, `Via`) are never used as
the issuer. **Settings → Sign-in** can set or clear the address without
editing TOML.

### Requests the proxy must allow

- Cookie-authenticated `POST`, `PATCH` and `DELETE` under `/api/*` need a
  matching `Origin` (or `Referer`) host. Sign-in, redeem and password
  paths are exempt.
- The web app's live socket (`GET /api/live`) is a WebSocket, as is the
  host-node socket (`GET /api/node/v1/connect`) in `proxy` mode. The proxy
  must forward the upgrade.
- The live socket also needs `Origin` to equal `public_origin` exactly.
  Without a working socket, the web app falls back to polling.

### Example: nginx

```nginx
map $http_upgrade $connection_upgrade {
  default upgrade;
  ''      close;
}

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
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection $connection_upgrade;
    proxy_read_timeout 120s;   # sockets ping every 20 s
  }
}
```

```toml
[daemon]
trusted_proxies = ["127.0.0.1", "::1"]

[integrations]
public_origin = "https://bookclerk.example.com"
```

### Validate API requests at the proxy

A proxy that validates requests against an OpenAPI schema can refuse
malformed API calls before they reach the daemon. Cloudflare API Shield's
schema validation is one; it only reads OpenAPI 3.0.

1. Download the OpenAPI 3.0 document from your daemon:

   ```console
   $ curl -o bookclerk-openapi-3.0.json https://bookclerk.example.com/api/openapi-3.0.json
   ```

2. Upload it as the schema, and scope the rule to your Bookclerk hostname.
3. Watch the violations in log-only mode before you block anything.

> [!NOTE]
> Download the document again after each Bookclerk upgrade, since routes
> and fields change between versions. Validators check request bodies only
> up to a size limit, and file uploads (`application/octet-stream`,
> `image/*`) are not validated at all.

### Trusted proxies

List your proxies in `daemon.trusted_proxies` (IP addresses or CIDRs).
Only then does sign-in throttling honor `X-Forwarded-For`; an empty list
means the direct TCP peer is always used.

The tray's operator handoff ignores this list. **Open Bookclerk** always
talks to `http://localhost:<listen-port>` directly. So browse the proxied
host name as a user, and use localhost or Owner elevation for operator
work.

## Changing settings without a restart

Reload with `POST /api/config/reload` or SIGHUP. A reload rebuilds
operator auth, stores, integrations, destinations and (when needed) the
database plugin in one transactional swap. Auth is published **before**
any listen rebind, so a public listener never runs ahead of its
middleware.

| Setting | On reload |
| --- | --- |
| `daemon.listen` | Rebinds the listeners, after the auth swap. Free ports are bound first, and a failed rebind rolls back to the last good bind |
| `daemon.auth.*`, operator token | Rebuilds `OperatorAuthState`. Database sessions stay; the old token works for about 60 seconds after a rotation or reload |
| `sources.*`, `integrations.*`, `output.*` | Rebuilds the registries; integration watchers stop and start again |
| `database.plugin` | Reopens the library and destinations |
| `[media]` | Swaps the media worker pool |
| `[events]` | Compare-and-swap in the database. A reload overlays the committed revision and never copies TOML back over it. Each committed write wakes every node through its `node.<node_id>` queue, with a 60-second backstop for a missed wakeup |

## Host nodes

A Bookclerk install can join another library as a host node, and run its
scans and acquires. The web app calls this a **branch**. The full guide
is [Libraries and branches](cluster-hosts.md).

In the web app, join from **Settings → Library → Join a library** (or the
first-start welcome screen), with a sign-in on the library. The
connection starts without a restart.

On the command line:

| Task | Command |
| --- | --- |
| Join, signing in through a browser | `bookclerk clusters join --url https://owner.example` |
| Join with a branch invite | `bookclerk clusters join` (paste the invite, or pipe it on stdin) |
| See what the library asks to run here | `bookclerk clusters consents` |
| Approve a request | `bookclerk clusters approve <plugin_key> --request <digest> [--deny-domain …] [--deny-binding …]` |
| Revoke an approval | `bookclerk clusters revoke <plugin_key>` |
| Leave | `bookclerk clusters leave` (tells the library, which revokes the node, then deletes the node keys) |

What a host needs:

- `bookclerk-workerd` and the pinned `workerd` beside the daemon, like any
  plugin host;
- outbound HTTPS to the library's `public_origin`.

A host keeps its own database and `master.key`, and never sees the
library's. On the library, `bookclerk nodes list` shows the host
(`kind = host`) with its heartbeats, and `bookclerk nodes revoke
<node_id>` disconnects it for good.

## systemd

A sample unit is in
[`packaging/systemd/bookclerkd.service`](../../packaging/systemd/bookclerkd.service).

1. Install `bookclerkd`, `bookclerk-media-worker` and `bookclerk-jail` to
   `/usr/local/bin`, from the same directory.
2. Create the user and folders.
3. Enable and start the service:

   ```sh
   sudo systemctl enable --now bookclerkd
   ```

> [!IMPORTANT]
> `bookclerkd` looks for its helpers beside itself. By default it refuses
> media work, and won't load an external plugin, rather than run either
> unconfined.

What the sample unit sets:

- `Environment=BOOKCLERK_FILES_DIR=/var/lib/bookclerk`
- `ProtectSystem=strict` with `ReadWritePaths=/var/lib/bookclerk`

Give `master.key` a password with `BOOKCLERK_AUTH_PASSWORD` (or
`[auth].password`), kept outside the files directory. To wrap an
existing BCK1 key later, run `bookclerk config master-key wrap`, or
reload `bookclerkd` after setting the password (SIGHUP or
`POST /api/config/reload`).

If your books live outside the files directory, set an absolute root and
add it to `ReadWritePaths`:

```sh
bookclerk plugins config set local root /srv/audiobooks
```

## Docker

The Dockerfile is
[`packaging/docker/Dockerfile`](../../packaging/docker/Dockerfile). Build
it from the repository root:

```sh
docker build -f packaging/docker/Dockerfile -t bookclerkd .
```

The image holds the same bundle as a platform release
(`cargo package-platform`), under `/opt/bookclerk`: `bookclerk`,
`bookclerkd`, `bookclerk-media-worker`, `bookclerk-jail`,
`bookclerk-workerd`, the pinned `workerd`, the web app, and the `sqlite`
and `local` platform plugins in `/opt/bookclerk/plugins/`.

### Published images

Prebuilt images are published to the GitHub Container Registry for `amd64`
and `arm64`:

```sh
docker pull ghcr.io/bookclerk/bookclerk:edge
```

Until the first release, only `edge` (the newest build from `main`) and
`sha-<commit>` tags exist. Releases add `<version>`, `<major>.<minor>`,
`<major>` and `latest`. Pin the digest in production
(`ghcr.io/bookclerk/bookclerk@sha256:…`), and check where an image was
built before running it:

```sh
gh attestation verify oci://ghcr.io/bookclerk/bookclerk:edge --owner bookclerk
```

### Run it

```sh
export BOOKCLERK_AUTH_PASSWORD
docker run --rm \
  -v bookclerk-config:/config \
  -v bookclerk-data:/data \
  -e BOOKCLERK_AUTH_PASSWORD \
  bookclerkd
```

Set `BOOKCLERK_AUTH_PASSWORD` in your shell or an env file, never on the
command line.

| Path or variable | Role |
| --- | --- |
| `/config` | `BOOKCLERK_FILES_DIR`: `config.toml`, `library.db` (with `encrypted_secrets`), `master.key`, installed `plugins/` |
| `/data` | The books root (`bookclerk plugins config set local root /data/Audiobooks`) |
| `/opt/bookclerk` | The read-only bundle: hosts, helpers, pinned `workerd`, web app, platform plugins |
| `BOOKCLERK_DAEMON_LISTEN` | Loopback inside the container by default |

A starting config is in the image at
`/etc/bookclerk/config.example.toml`.

### Publish the web app

Keep operator auth on, and terminate TLS at a proxy:

```sh
docker run -d --name bookclerk \
  -v bookclerk-config:/config \
  -v bookclerk-data:/data \
  -e BOOKCLERK_AUTH_PASSWORD \
  -e BOOKCLERK_DAEMON_LISTEN=0.0.0.0:8787 \
  -p 8787:8787 \
  bookclerkd
docker exec bookclerk bookclerk daemon token
```

The second command prints the sealed operator token.
`BOOKCLERK_OPERATOR_TOKEN` overrides it; pass it by name
(`-e BOOKCLERK_OPERATOR_TOKEN`), never with its value on the command line.

### Platform plugins on start

On every start, before it looks for the database plugin, `bookclerkd`
(and the CLI) installs each platform plugin from the `plugins/` folder
beside its executable. It skips a plugin when the files directory already
holds a trusted, byte-identical copy, and records each install in
`install-ledger.json`.

- A fresh `/config` volume boots with a working database and local output.
- Upgrading (or rolling back) the image brings the plugins to the build
  that image shipped with.
- A folder of the same name (`sqlite`, `local`) owned by another install
  is left alone, with a warning.

The same applies to an unpacked platform archive run from its own folder.

### Isolation in Docker

Plugin and media isolation keep their default, `"required"`, in the
image. Both confined tiers work under Docker's default seccomp profile,
which has allowed the `landlock_*` syscalls since Docker 20.10.14. An
older engine refuses those syscalls, and `bookclerkd` says so at startup
instead of quietly running codecs and plugins unconfined.

> [!CAUTION]
> Don't fix that with `--security-opt seccomp=unconfined`: it removes a
> boundary instead of restoring one. Upgrade the engine, or lower
> `isolation` on purpose.

For day-to-day building and testing with a known OpenSSL and Node
toolchain, use the [Dev Container](../contributing/devcontainer.md)
instead.

## Logging

- **`bookclerkd`** writes structured JSON to stderr
  (`daemon.json_logs = true` by default), and to the OS facility:
  journald, macOS `os_log` or the Windows Event Log.
- Lines from plugin jails and media workers are captured and re-emitted
  as tracing events. ANSI codes from piped plugins are stripped, so the
  JSON holds no `\u001b` escapes.
- Spawn-stage text is the same kind of event (`bookclerk::spawn`). It
  goes to raw stderr only when `BOOKCLERK_SPAWN_DIAG` is `1`, `true` or
  `stderr`.
- The **CLI** (`bookclerk`) prints command output only. Tracing is off
  until `-v`, `-vv` or `-vvv` (or `BOOKCLERK_LOG` or `RUST_LOG`).
- The level comes from `BOOKCLERK_LOG`, then `RUST_LOG`, then the daemon
  default `bookclerk=info,warn`.
- Secrets are redacted, and a diagnostics upload is abandoned if a
  registered secret remains.

Opt-in error reports: [Diagnostics](diagnostics.md).

## Safe live testing

When testing with real store accounts:

1. Keep `library.auto_acquire = false`.
2. After signing in, run `bookclerk library set-scan <account> --scan false`.
3. Acquire **one** title (`--asin`, `--isbn` or a UUID), never the whole
   library.
4. Use the CLI rather than `POST /scan` or `/acquire`, so nothing queues
   in bulk.
5. To sync once without turning scans back on, run
   `bookclerk library scan --account <id>`.

## Next steps

- [Security checklist](security.md): before the server goes public.
- [Configuration](configuration.md): every setting.
- [Backups, updates, import and export](backups.md): recovery points and
  updates.
