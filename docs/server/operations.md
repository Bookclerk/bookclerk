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
4. loads the settings the database owns: `core.events`, `core.output`,
   `core.cache` and every plugin's settings. `[library]`, `[jobs]`,
   `[media]`, `[plugins]`, `[daemon]` and the other tables still come
   from TOML.

It then runs these schedules:

- a library scan every `library.scan_interval_minutes`;
- auto-acquire, when `library.auto_acquire = true`;
- a daily check for better files, when `library.upgrade_quality = true`
  (see [Upgrading to better files](output.md#upgrading-to-better-files)).

> [!WARNING]
> Keep `library.auto_acquire = false` until you are happy with where
> files land.

Auto-acquire and other downloads the daemon starts itself follow each
store account's download pace: a daily budget, a gap between downloads
and a pause after the store throttles. A held job waits without spending
an attempt and picks up where it stopped. Change one account's pace with
`bookclerk library set-pace` or from **Accounts**; see
[Download pacing](../internals/jobs.md#download-pacing).

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
| `GET`/`PUT /api/auth/mfa-policy` | Owner or operator reads; operator changes | `require_second_factor`: password sign-in needs TOTP or a passkey |
| `GET /api/status` (also `/status`) | Yes | Status snapshot |
| `POST /api/library/scan` (also `/scan`) | Yes | Queue a scan. Any user may call `/api/library/scan`; a Member or an Administrator whose library-wide switch is off scans only the store accounts linked to them. The `/scan` alias stays with Administrators |
| `POST /api/library/acquire` (also `/acquire`) | Yes | Queue an acquire. Any user may call `/api/library/acquire`; a Member or an Administrator whose library-wide switch is off acquires only from the store accounts linked to them. `editions` queues editions from a work's detail. The `/acquire` alias stays with Administrators |
| `POST /api/library/convert` | Yes | Queue a convert: add a copy (`copy`, such as `{"format": "mp3", "quality": "64k-mono"}`) to stored books named by `titles` or `editions`, beside the original (see [Adding a copy to a stored book](output.md#adding-a-copy-to-a-stored-book)). Operator, Owners and Administrators only |
| `GET /api/library/works` | Yes | The library's works, one row per work, paged by cursor and filterable by media kind, kind, acquire status and account; `q` searches titles, people, series and store identifiers |
| `GET /api/library/works/{id}` | Yes | One work with its editions, store listings, holdings and the caller's own state |
| `GET`/`PUT /api/library/works/{id}/tags` | Provisioner | A work's library tags, or replace the ones set in Bookclerk (sidecar tags stay); `facet=library_tag` filters the works list by one |
| `GET`/`PUT /api/library/collections` | Provisioner | The library's collections (names and storage folder prefixes), or replace them; a change re-indexes the Library, and `facet=collection` filters the works list by one |
| `PUT`/`DELETE /api/library/listings/{id}/pin` | Provisioner | Pin a store listing to the edition it sells (it moves there at once, with its holdings and offers, and later scans keep it there), or remove the pin (the listing stays where it is) |
| `POST /api/library/works/{id}/split` | Provisioner | Split a wrongly merged work: up to 12 of its expressions move, with their editions, listings and holdings, to a new work that copies its title, credits, series and each person's own state; new listings with the same title keep joining the original |
| `GET /api/library/conflicts` | Provisioner | The identifier clashes and ambiguous matches the resolver left for an Administrator, paged by cursor |
| `GET /api/library/editions/{id}/cover` | Yes | The JPEG cover written beside an acquired edition's file, when this node runs a local destination |
| `GET /api/jobs` (also `/jobs`) | Yes | The durable job list: progress, attempts, times |
| `POST /api/jobs/{id}/cancel` | Yes | Cancel pending work, or ask running work to stop |
| `/` | None | The web app, when `ui/dist` is present |

An Owner can turn off single Administrator switches (see
[Sharing](../guide/sharing.md#limit-what-an-administrator-may-do)). The
routes a switch guards, such as user changes, pins, splits and job
cancels, then answer `403` to that Administrator.

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

### Cache headers

Every response says how long a browser or a caching proxy may keep it:

| Response | `Cache-Control` |
| --- | --- |
| Web app pages | `no-cache`; `/invite` is `no-store` |
| Web app files under `/assets/` (their names carry a content hash) | `public, max-age=31536000, immutable` |
| Other web app files (icons, manifest, fonts) | `no-cache` |
| Covers (`/api/covers/…`) | `private, max-age=31536000, immutable` |
| Other images (`/api/images/…`, Gravatars, store icons) | `private, max-age=3600, stale-while-revalidate=86400` |
| Store samples (`/api/samples/…`) | `private, max-age=86400` |
| API JSON, errors (HTML error pages too) and everything else | `no-store` |

Covers and other images also send an `ETag`. A browser that asks again
with `If-None-Match` gets `304 Not Modified` and no body when its copy is
current. A proxy in front of Bookclerk needs no cache rules of its own:
it may keep `/assets/` files and must not store the rest.

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
  branch-node socket (`GET /api/node/v1/connect`) in `proxy` mode. The proxy
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
Only then does sign-in throttling honor `X-Forwarded-For` (or RFC 7239
`Forwarded` when `X-Forwarded-For` is absent); an empty list means the
direct TCP peer is always used.

Each proxy appends the address it received the request from, so a client
can put anything it likes at the left of the header. Bookclerk reads the
addresses from the right, skips every one that is a trusted proxy, and
uses the first one that isn't. List every proxy in the chain (for
example both a CDN's ranges and your own nginx), or the throttle keys on
the innermost proxy you left out. If a hop it has to read isn't an IP
address, Bookclerk uses the direct peer instead.

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
| `sources.*`, `integrations.*` | Rebuilds the registries; integration watchers stop and start again |
| `database.plugin` | Reopens the library and destinations |
| `[media]` | Swaps the media worker pool |
| `[events]`, `[output]` | Compare-and-swap in the database (`core.events`, `core.output`). A reload overlays the committed revision and never copies TOML back over it. Each committed write wakes every node through its `node.<node_id>` queue, with a 60-second backstop for a missed wakeup |

## Branch nodes

A Bookclerk install can join another library as a branch node, and run its
scans and acquires. The web app calls this a **branch**. The full guide
is [Libraries and branches](branches.md).

In the web app, join from **Settings → Library → Join a library** (or the
first-start welcome screen), with a sign-in on the library. The
connection starts without a restart.

On the command line:

| Task | Command |
| --- | --- |
| Join, signing in through a browser | `bookclerk libraries join --url https://owner.example` |
| Join with a branch invite | `bookclerk libraries join` (paste the invite, or pipe it on stdin) |
| See what the library asks to run here | `bookclerk libraries consents` |
| Approve a request | `bookclerk libraries approve <plugin_key> --request <digest> [--deny-domain …] [--deny-binding …] [--deny-own-user-agent …]` |
| Revoke an approval | `bookclerk libraries revoke <plugin_key>` |
| Leave | `bookclerk libraries leave` (tells the library, which revokes the node, then deletes the node keys) |

What a host needs:

- `bookclerk-workerd` and the pinned `workerd` beside the daemon, like any
  plugin host;
- outbound HTTPS to the library's `public_origin`.

A host keeps its own database and `master.key`, and never sees the
library's. On the library, `bookclerk nodes list` shows the host
(`kind = branch`) with its heartbeats, and `bookclerk nodes revoke
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

Give `master.key` a password with `BOOKCLERK_AUTH_PASSWORD`,
`BOOKCLERK_AUTH_PASSWORD_FILE` or a systemd credential (see
[Keeping the auth password off disk](#keeping-the-auth-password-off-disk)),
or `[auth].password`, kept outside the files directory. To wrap an
existing BCK1 key later, run `bookclerk config master-key wrap`, or
reload `bookclerkd` after setting the password (SIGHUP or
`POST /api/config/reload`).

### Sealing the key with systemd-creds

A service like this has no OS keychain (no login session, no Secret
Service). On systemd 256 or later, `bookclerkd` can seal the key with
`systemd-creds` instead, to the machine's TPM2 when it has one and to the
host key otherwise:

1. Save a recovery copy first. A sealed key only opens on this machine:

   ```sh
   sudo -u bookclerk bookclerk config master-key export --out /root/bookclerk-recovery.bck
   ```

2. Set the store in `config.toml`:

   ```toml
   [auth]
   key_store = "systemd-creds"
   ```

3. Restart the service:

   ```sh
   sudo systemctl restart bookclerkd
   ```

On start, `bookclerkd` runs `systemd-creds --user encrypt` as its own
user, writes the sealed key beside `master.key` as
`master-key-<id>.cred`, reads it back, and only then replaces
`master.key` with a pointer (`BCK4`). `bookclerk config master-key
status` shows `store=systemd-creds`. To move it now instead, with the
service stopped, run `bookclerk config master-key move systemd-creds`.

> [!NOTE]
> If sealing fails for any reason (an older systemd, no access to the
> credential service), the key stays in `master.key`, wrapped with the
> auth password when one is set, and `bookclerkd` logs a warning and
> starts. Once the key is sealed, a credential that no longer opens (a
> replaced TPM or motherboard, a reinstalled system) stops `bookclerkd`
> from starting rather than making a new key. Restore it with `bookclerk
> config master-key import --from <recovery copy>` after moving the old
> `master.key` away.

### Sealing the key yourself

On an older systemd, or to keep the credential in the system credential
store, seal the key yourself and let the unit hand it to `bookclerkd`:

1. With the service stopped, pipe the key into `systemd-creds`. The
   command writes the key exactly as stored (wrapped with the auth
   password when one is set) and refuses to write to a terminal:

   ```sh
   sudo -u bookclerk bookclerk config master-key credential \
     | sudo systemd-creds encrypt --name=bookclerk-master-key - /etc/credstore.encrypted/bookclerk-master-key
   ```

2. Load it in the unit:

   ```ini
   LoadCredentialEncrypted=bookclerk-master-key
   ```

3. Set `key_store = "systemd-creds"` under `[auth]` and start the service.

On start, `bookclerkd` finds the same key in
`$CREDENTIALS_DIRECTORY/bookclerk-master-key`, adopts it without sealing
a copy of its own, and replaces `master.key` with a pointer. From then on
the key is only in that credential: keep the unit line and the
credential file, and keep the recovery copy.

### Keeping the auth password off disk

Without `systemd-creds`, the key stays in the password-wrapped file. Keep
the password itself off disk with an encrypted systemd credential, which
a TPM can seal to the machine:

```sh
systemd-creds encrypt --name=bookclerk-auth-password - /etc/credstore.encrypted/bookclerk-auth-password
```

Then load it in the unit:

```ini
LoadCredentialEncrypted=bookclerk-auth-password
```

`bookclerkd` and the CLI read `$CREDENTIALS_DIRECTORY/bookclerk-auth-password`
themselves when `BOOKCLERK_AUTH_PASSWORD` and
`BOOKCLERK_AUTH_PASSWORD_FILE` are unset. A credential that is present
but empty or unreadable stops them from starting, so a fresh key is never
minted unwrapped by mistake.

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

Hand the auth password to the container as a file, not an environment
variable. Anyone who can run `docker inspect`, or read
`/proc/<pid>/environ` inside the container, sees a variable's value; a
file mounted under `/run/secrets/` shows up only as its path.

1. Write the password to a file in a directory only you can read. The
   file must be readable by the container's `bookclerk` user:

   ```sh
   install -d -m 0700 ~/.config/bookclerk-secrets
   (umask 0022; read -rsp 'Bookclerk auth password: ' pw && printf '%s\n' "$pw" > ~/.config/bookclerk-secrets/auth-password)
   ```

2. Mount it read-only and point `BOOKCLERK_AUTH_PASSWORD_FILE` at it:

   ```sh
   docker run --rm \
     -v bookclerk-config:/config \
     -v bookclerk-data:/data \
     -v ~/.config/bookclerk-secrets/auth-password:/run/secrets/bookclerk-auth-password:ro \
     -e BOOKCLERK_AUTH_PASSWORD_FILE=/run/secrets/bookclerk-auth-password \
     bookclerkd
   ```

A container has no OS keychain, so the key stays in `master.key` on the
`/config` volume, wrapped with this password. Keep the password file off
that volume: a copy of `/config` alone then can't open the key.

> [!WARNING]
> `-e BOOKCLERK_AUTH_PASSWORD` still works, but the value is then part of
> the container's configuration. If you use it, set it in your shell or
> an env file, never on the command line.

`BOOKCLERK_AUTH_PASSWORD_FILE` and `BOOKCLERK_AUTH_PASSWORD` can't both be
set. A named file that is missing, empty, larger than 4 KiB or not UTF-8
stops `bookclerkd` from starting. Surrounding whitespace, such as a
trailing newline, is ignored.

#### Compose, Podman and Kubernetes secrets

Each of these mounts a secret as a file under `/run/secrets/`, which is
what `BOOKCLERK_AUTH_PASSWORD_FILE` reads.

Docker Compose:

```yaml
services:
  bookclerk:
    image: ghcr.io/bookclerk/bookclerk:edge
    volumes:
      - bookclerk-config:/config
      - bookclerk-data:/data
    environment:
      BOOKCLERK_AUTH_PASSWORD_FILE: /run/secrets/bookclerk-auth-password
    secrets:
      - bookclerk-auth-password
secrets:
  bookclerk-auth-password:
    file: ./secrets/auth-password
volumes:
  bookclerk-config:
  bookclerk-data:
```

Podman, from the command line or a Quadlet `.container` unit:

```sh
podman secret create bookclerk-auth-password ~/.config/bookclerk-secrets/auth-password
podman run --rm --secret bookclerk-auth-password \
  -e BOOKCLERK_AUTH_PASSWORD_FILE=/run/secrets/bookclerk-auth-password \
  -v bookclerk-config:/config -v bookclerk-data:/data bookclerkd
```

```ini
[Container]
Secret=bookclerk-auth-password
Environment=BOOKCLERK_AUTH_PASSWORD_FILE=/run/secrets/bookclerk-auth-password
```

Don't use Podman's `type=env` secrets: they put the value back in the
environment.

Kubernetes: mount a `Secret` as a volume (not `env.valueFrom`) and point
the variable at the mounted key:

```yaml
containers:
  - name: bookclerk
    env:
      - name: BOOKCLERK_AUTH_PASSWORD_FILE
        value: /run/secrets/bookclerk/auth-password
    volumeMounts:
      - name: auth-password
        mountPath: /run/secrets/bookclerk
        readOnly: true
volumes:
  - name: auth-password
    secret:
      secretName: bookclerk-auth-password
```

Bookclerk running straight under systemd (not in a container) reads a
`bookclerk-auth-password` credential instead; see
[Keeping the auth password off disk](#keeping-the-auth-password-off-disk).

| Path or variable | Role |
| --- | --- |
| `/config` | `BOOKCLERK_FILES_DIR`: `config.toml`, `library.db` (with `encrypted_secrets`), `master.key`, installed `plugins/` |
| `/data` | The books root (`bookclerk plugins config set local root /data/Audiobooks`) |
| `/opt/bookclerk` | The read-only bundle: hosts, helpers, pinned `workerd`, web app, platform plugins |
| `BOOKCLERK_AUTH_PASSWORD_FILE` | Path of a mounted secret holding the password that wraps `master.key` |
| `BOOKCLERK_DAEMON_LISTEN` | Loopback inside the container by default |

A starting config is in the image at
`/etc/bookclerk/config.example.toml`.

### Publish the web app

Keep operator auth on, and terminate TLS at a proxy:

```sh
docker run -d --name bookclerk \
  -v bookclerk-config:/config \
  -v bookclerk-data:/data \
  -v ~/.config/bookclerk-secrets/auth-password:/run/secrets/bookclerk-auth-password:ro \
  -e BOOKCLERK_AUTH_PASSWORD_FILE=/run/secrets/bookclerk-auth-password \
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
- A folder of the same name (`sqlite`, `local`, `embedder`) owned by another install
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
