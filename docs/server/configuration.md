# Configuration

This page lists Bookclerk's settings: what lives in `config.toml`, what
lives in the library database, and the environment variables that
override or seed them.

## Where settings live

Bookclerk keeps settings in three places:

- **`config.toml`**, in `$BOOKCLERK_FILES_DIR` (or the path in `--config`
  or `BOOKCLERK_CONFIG`). The annotated template is
  [`config/config.example.toml`](../../config/config.example.toml).
- **The library database**, for settings every host shares:
  `core.events`, `core.output`, host enrollment, and every plugin's
  settings.
- **Bootstrap files** the process needs before it can open the database;
  see [Bootstrap files](#bootstrap-files).

To see the values in effect:

```sh
bookclerk config show
bookclerk config get <key>
bookclerk config paths
```

> [!NOTE]
> There are no per-run overrides: `library acquire -o key=value` was
> removed. Change a setting with `bookclerk config set`, or the plugin's
> settings, before you acquire.

Classic Libation setting names work as aliases where
[Libation parity](../contributing/research/libation-parity.md) says so.

## Tables in `config.toml`

| Table | What it sets |
| --- | --- |
| `[library]` | Auto-acquire, scan interval, Audible matching, storage layout fix |
| `[database]`, `[database.sqlite]`, `[database.d1]`, `[database.postgres]` | The library database plugin (`sqlite` by default, Cloudflare D1 or PostgreSQL) |
| `[auth]` | `[auth].password` to wrap `master.key` (prefer `BOOKCLERK_AUTH_PASSWORD`), and `[auth.oidc]` sign-in providers |
| `[media]` | The codec worker pool: concurrency and confinement; see [Media worker](media.md) |
| `[plugins]` | How external plugins are jailed; see [Isolation modes](plugins.md#isolation-modes) |
| `[output]` | The first-run seed of the `core.output` document, and the host's own `in_progress` scratch folder |
| `[daemon]` | Listen addresses, JSON logs, trusted proxies, and `[daemon.node_api]` transport |
| `[integrations]` | `public_origin`, and claim-ticket and portal session lifetimes |
| `[discovery]` | Recommendations, embeddings, Open Library, listening sync |
| `[diagnostics]` | Opt-in report sharing |
| `[jobs]`, `[jobs.concurrency]` | Job queue admission, leases and network concurrency |
| `[events]` | The first-run seed of `core.events`: outbox retention and delivery workers |

Stores, destinations and integrations are **not** in this file. Each is
a plugin instance in the database; see
[Plugin instances](#plugin-instances).

## Environment variables

Environment variables win over `config.toml`. Settings the database owns
(`core.output`, `core.events`) take them only as a first-run seed.

| Variable | Role |
| --- | --- |
| `BOOKCLERK_FILES_DIR` | The state root: database, plugins, cache and so on |
| `BOOKCLERK_CONFIG` | Path to `config.toml` |
| `BOOKCLERK_DATABASE_PLUGIN` | The database plugin: `sqlite` or `d1` |
| `BOOKCLERK_DATABASE_SQLITE_PATH` | The SQLite file path |
| `BOOKCLERK_D1_ACCOUNT_ID`, `BOOKCLERK_D1_DATABASE_ID` | Cloudflare D1 ids |
| `BOOKCLERK_D1_API_TOKEN` or `CLOUDFLARE_API_TOKEN` | D1 API token (environment only) |
| `BOOKCLERK_DAEMON_LISTEN` | The daemon's listen addresses |
| `BOOKCLERK_LOG`, `RUST_LOG` | Log filter |
| `BOOKCLERK_AUTH_PASSWORD` | Wraps `master.key` at rest (preferred over `[auth].password`); needed for legacy `json-encrypted` rows |
| `BOOKCLERK_OIDC_<ID>_CLIENT_SECRET` | Client secret of sign-in provider `<ID>` (hyphens become underscores) |
| `BOOKCLERK_DISCOVERY_EMBEDDINGS_ENABLED` | Local ONNX embeddings on or off |
| `BOOKCLERK_DISCOVERY_OPENLIBRARY_ENABLED` | Open Library enrichment on or off |
| `BOOKCLERK_DISCOVERY_RECOMMEND_LIMIT` | How many recommendations by default |
| `BOOKCLERK_JOBS_MAX_PENDING` | Cap on pending and running jobs (default 32) |
| `BOOKCLERK_JOBS_LEASE_SECONDS` | Worker lease length (default 60) |
| `BOOKCLERK_JOBS_MAX_ATTEMPTS` | Claims before a job fails for good (default 3) |
| `BOOKCLERK_JOBS_RETENTION_DAYS` | Days to keep finished jobs (default 7) |
| `BOOKCLERK_JOBS_TEMP_QUOTA_BYTES` | Acquire scratch quota (default 2 GiB) |
| `BOOKCLERK_JOBS_CONCURRENCY_NETWORK` | Network workers on this node, and running network jobs across the library (default 1) |
| `BOOKCLERK_JOBS_CONCURRENCY_PLUGIN_INSTANCE` | Running jobs per required plugin instance, across the library (default 1) |
| `BOOKCLERK_MEDIA_WORKERS` | Concurrent codec jobs (`0` means one per core, up to 8) |
| `BOOKCLERK_MEDIA_ISOLATION` | `required`, `best-effort` or `off` |
| `BOOKCLERK_MEDIA_WORKER` | Path to `bookclerk-media-worker` |
| `BOOKCLERK_PLUGIN_DIRS` | Extra plugin search folders (an OS path list) |
| `BOOKCLERK_PLUGIN_ISOLATION` | `required`, `best-effort` or `off`, for plugins |
| `BOOKCLERK_PLUGIN_JAIL` | Path to `bookclerk-jail` |
| `BOOKCLERK_PLUGIN_WORKERD` | Path to `bookclerk-workerd`, which every plugin spawns through |
| `BOOKCLERK_WORKERD_BIN` | Path to the pinned Cloudflare `workerd` (default: beside `bookclerk-workerd`) |
| `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` | The diagnostics collector (at build time or run time) |

> [!IMPORTANT]
> Plugin settings and enablement have no environment overrides. The old
> `BOOKCLERK_SOURCE_<ID>_ENABLED`, `BOOKCLERK_OUTPUT_*`, `BOOKCLERK_S3_*`,
> `BOOKCLERK_AWS_*`, `BOOKCLERK_GA_*`, `BOOKCLERK_ABS_*` and
> `BOOKCLERK_*_PASSWORD` variables are gone; those values live in the
> database.

The `BOOKCLERK_OUTPUT_*` packaging variables that remain (format,
Widevine, naming, sidecars) only seed `core.output` on the first
enrollment; see [Output (shared)](#output-shared).

## Library

```toml
[library]
auto_acquire = false
scan_interval_minutes = 60
enrich_from_audible = true
enrich_min_confidence = 90
# fix_storage_layout = false
```

## Jobs

```toml
[jobs]
max_pending = 32
lease_seconds = 60
max_attempts = 3
retention_days = 7
# temp_quota_bytes = 2147483648

[jobs.concurrency]
network = 1          # workers here, and running network jobs across the library
plugin_instance = 1  # running jobs per required plugin instance, across the library
```

[Jobs](../internals/jobs.md) explains the states, the `409` and `429`
admission answers, and crash recovery.

## Events

```toml
[events]
retention_days = 7
dead_letter_retention_days = 30
concurrency = 1
```

`[events]` was the first settings domain moved into the database, as
`core.events`. See [ADR: Control plane](../adr/control-plane.md).

### The database is the authority

- On the first enrollment, the TOML and `BOOKCLERK_EVENTS_*` values are
  imported once.
- After that, the database wins. Startup, reload,
  `bookclerk config set events.*` and later environment variables don't
  overwrite it.
- `bookclerk config show` prints `events.authority = database` and
  `events.revision`.
- Writes are compare-and-swap. A write that loses returns a revision
  conflict and changes nothing.

```sh
bookclerk config get events.retention_days
bookclerk config set events.retention_days 14
```

Allowed values: retention `1` to `3650` days, concurrency `1` to `32`.

### Over HTTP

- `GET`/`PUT /api/config/domains/core.events` takes `expected_revision`
  and the three fields.
- `PATCH /api/settings` accepts `events.*` through the same
  compare-and-swap, as long as the request doesn't also change file-backed
  keys. A mixed request is rejected before anything is written.
- The Settings page saves file-backed keys separately from `events.*`.
- `GET /api/settings` shows `events.revision` under `effective`.

### When changes apply

Running processes re-read the document at startup and whenever a
`configuration.changed` message reaches their node queue
(`node.<node_id>`). Every committed write sends one to each registered
node, and a pass every 60 seconds catches a missed one.

- A reload reads the document before it replaces the library connection
  or operator auth. If the read fails, the previous runtime stays.
- Plugins bound to an instance keep running across a reload. If the
  database connection itself changed, every plugin is retired, and the
  reconciler brings this host's present deployments back.
- An older snapshot never replaces a newer revision of the same library.
- Retention and the in-flight cap apply on the next dispatcher tick. The
  number of local delivery tasks is fixed at process start.

The shared `[output]` table follows the same model; see
[Output (shared)](#output-shared). `[library]`, `[jobs]`, media, plugins
and diagnostics are still TOML and environment settings.

### Retention and concurrency

- Acknowledged or rejected deliveries are kept for `retention_days`.
  Parent events with no live deliveries left are kept just as long, so a
  late node can still catch up.
- Dead letters are kept for the longer `dead_letter_retention_days`.
- Cleanup runs at daemon start and hourly, not on every dispatcher tick.
- `concurrency` is the number of local delivery workers **and** the
  library-wide cap on `running` deliveries per
  `(plugin_id, resource_class)`. Claims still filter by loaded plugin
  ids, and a portable `bookclerk_slots` row keeps that count.

See [Jobs](../internals/jobs.md) and [Managing plugins](plugins.md).

### Bootstrap files

These stay out of the database, because the process needs them before it
can read `core.events` and `core.output`:

| File or setting | Role |
| --- | --- |
| `[database]` and the `BOOKCLERK_DATABASE_*`, D1 and Postgres variables | Which library database to open |
| `master.key`, with `BOOKCLERK_AUTH_PASSWORD` or `[auth].password` | The library's data-encryption key |
| `node-identity.json` | This host's stable id, and the library it joined |

**Adding a second host to an existing database:**

- Copy `master.key` and the database settings. The host must have the
  same `master.key`; Bookclerk never mints a new key when the database
  already has a secret-root fingerprint.
- Leave out `node-identity.json` so the process becomes a new host.
  Copying it reuses the host id, and a file bound to a different library
  fails without changing either side.

The CLI aligns the key when a command opens the library, and never mints
or caches a key before the command runs. `bookclerk config master-key
status` and `bookclerk config master-key wrap` don't open the database,
and `wrap` fails when `master.key` is missing.

## Host nodes

A **host node** is another Bookclerk install that joins this library
with a single-use link and runs its scans and acquires. This section is
the library side's configuration. Joining, consent, uploads and the
protocol are in [Libraries and branches](cluster-hosts.md), and the
design is in
[ADR: Control plane](../adr/control-plane.md#host-node-enrollment-and-transport).

### Let hosts join

Host enrollment is a library-wide document, `core.host_enrollment`, and
it is on by default. In the web app it is **Settings → Branches → Let new
branches join**.

- The operator turns it off with the CLI:

  ```sh
  bookclerk config get host_enrollment.enabled
  bookclerk config set host_enrollment.enabled false
  ```

- An Owner (or the operator) can use
  `PUT /api/config/host-enrollment` with `{"enabled": false}`.
- Any signed-in user can read it with `GET /api/config/host-enrollment`.

While it is on, a branch can join with a sign-in (**Join a library**).

### Branch invites

Any active user can mint a branch invite for themselves, with **Add a
branch** or `POST /api/config/node-invites`. The operator or an Owner can
mint one for any active user. An invite is shown once and never cached.

```sh
bookclerk nodes invite --user riley --ttl 24h
bookclerk nodes invites list
bookclerk nodes invites revoke <link_id>
bookclerk nodes revoke <node_id>
```

The first command prints the link once; the last revokes an enrolled
host. Any signed-in user can also revoke a host they own with
`DELETE /api/config/node-credentials/<node_id>`, and the operator or an
Owner can revoke any host.

**What the link looks like:**
`<public_origin>/enroll#bcn1.<link_id>.<secret>[.<spki_pin>]`. The pin is
there only in `self_signed` mode.

- The secret is in the URL fragment, so proxies and access logs never see
  it. The database keeps only its hash.
- Set `integrations.public_origin` so the link points somewhere a host
  can reach.
- A link lasts 24 hours by default (5 minutes to 7 days) and works once.
- The host's operator redeems it in **Join a library → I have an
  invite**, or with `bookclerk clusters join`, which reads it from stdin
  or a prompt, never from the command line.

### Node API transport

`[daemon.node_api]` decides which connections may reach the node API:
`POST /api/node/v1/enroll` and the host socket `GET /api/node/v1/connect`.
Like `daemon.listen`, it is bootstrap configuration, so a change needs a
daemon restart.

```toml
[daemon]
trusted_proxies = ["10.0.0.2"]   # peers allowed to set X-Forwarded-*

[daemon.node_api]
mode = "loopback"                # loopback | proxy | acme | self_signed
# acme_contact = "mailto:ops@example.com"  # required for acme
# tls_listen = "0.0.0.0:8443"              # required for acme and self_signed
# acme_directory = "https://acme-staging-v02.api.letsencrypt.org/directory"
#                                          # default: Let's Encrypt production
# acme_directory_ca = "/etc/bookclerk/acme-ca.pem"
#                                          # extra roots for a private ACME CA
```

| Mode | Allows | Needs |
| --- | --- | --- |
| `loopback` (default) | Only connections where both ends are loopback, with no forwarding header | Nothing |
| `proxy` | Plain HTTP from a trusted proxy that sends exactly one `X-Forwarded-Proto: https` | An https `public_origin` and at least one entry in `daemon.trusted_proxies` |
| `acme` | The built-in TLS listener, with a certificate issued over TLS-ALPN-01 | `acme_contact`, `tls_listen`, and an https `public_origin` with a DNS name |
| `self_signed` | The built-in TLS listener, with a generated P-256 certificate whose pin travels in each link | `tls_listen`, and an https `public_origin` naming that listener |

Notes on each mode:

- **Loopback** is allowed in every mode, even on a wildcard
  `daemon.listen` (`0.0.0.0` or `[::]`). The check reads each
  connection's own local address, not the bind address.
- **`proxy`** checks every connection from a trusted proxy this way,
  even one with no forwarding headers. So with a proxy on the same host
  (`127.0.0.1` trusted), loopback enrollment goes through the proxy too.
- **`acme`**: forward port 443 of the `public_origin` name to
  `tls_listen`. The account and certificate are cached in
  `$BOOKCLERK_FILES_DIR/acme/` (mode 0700) and renewed automatically.
- **`self_signed`** is for a private network without ACME or a proxy.
  The certificate is generated once in
  `$BOOKCLERK_FILES_DIR/node-api/self-signed/` (mode 0700), and
  re-issued under the same key if the public host name changes, so the
  pin stays valid. Its SPKI pin (`spki-pin` there) is added to every
  invite link, and the host trusts exactly that key. Until the daemon has
  written the pin, minting an invite fails rather than make an unpinned
  link.

Everything else gets `403 {"error": "transport_insecure"}`. Plain HTTP
from a LAN address is refused, and forwarding headers from a peer that
isn't a trusted proxy are ignored. A mode missing a required setting
refuses every connection.

The TLS listener serves the web app and the node API routes. In `acme` and
`self_signed` mode, the node API stays unmounted on every listener,
loopback included, until the TLS listener has a certificate. Until then
(or if it couldn't bind), node API routes answer
`404 {"error": "node_api_unmounted"}`. There is no plain-HTTP fallback,
and ACME keeps retrying in the background.

**Testing ACME locally.** Run a [Pebble](https://github.com/letsencrypt/pebble)
server with `PEBBLE_VA_ALWAYS_VALID=1`, set
`BOOKCLERK_TEST_ACME_DIRECTORY=https://localhost:14000/dir` and
`BOOKCLERK_TEST_ACME_CA=<path to pebble.minica.pem>`, then run
`cargo test -p bookclerkd acme_issues_from_pebble`. Without them the test
skips.

## Plugin instances

A store, destination or integration plugin declares its settings itself,
in `plugin.toml` `[[settings]]`. Bookclerk stores them per **plugin
instance** in the database.

Enrollment (by the CLI or the daemon) pins that schema, creates the
plugin's default instance with the declared defaults, and records a
deployment for this host. A plugin is **enabled** on a host when its
deployment there is `desired = present`.

| Task | Command |
| --- | --- |
| List instances, deployments and what is running | `bookclerk plugins list` |
| Run a plugin on a host | `bookclerk plugins deploy s3 [--node <id>]` |
| Stop it there (the plugin is retired) | `bookclerk plugins undeploy s3 [--node <id>]` |
| Show settings, with schema defaults | `bookclerk plugins config get s3` |
| Change a setting | `bookclerk plugins config set s3 bucket my-audiobooks` |
| Set a secret (from stdin, sealed) | `bookclerk plugins config set s3 secret_access_key --secret` |
| Clear a setting | `bookclerk plugins config unset s3 prefix` |
| Add an instance | `bookclerk plugins instances create s3 --name "Backup bucket"` |
| Add an instance on one node only | `bookclerk plugins instances create local --name "NAS disk" --node <node-id>` |

### Over HTTP

The daemon's operator API mirrors this:

- `/api/config/plugin-instances`: list, and create with an optional
  `node_id`;
- `GET`/`PUT …/{id}/config` with `expected_revision`;
- `PUT …/{id}/deployments/{node_id}`;
- `/api/config/nodes`;
- `/api/config/plugins`: installed versions, with the pinned schema in
  `GET …/{key}`.

**Settings → Plugins** is built from the same schema.
`PATCH /api/settings` no longer accepts plugin keys.

### Rules

- Writes are compare-and-swap on the document revision, and are checked
  against the pinned schema. A stale write or an unknown key is rejected.
- Secrets (`type = "secret"`) are sealed into `encrypted_secrets` and
  never shown again.
- One plugin can have several instances, even on one host. A bare plugin
  key means the default instance; `library scan|acquire --instance` picks
  another.
- Store accounts belong to the plugin key, not an instance, so every
  instance serves the same accounts.

Details: [Managing plugins](plugins.md#enabling-and-settings-database),
[Output and destinations](output.md), [Stores](stores.md),
[Integrations](integrations.md).

## Sign-in providers (`[auth.oidc]`)

Optional. These providers sign users in to Bookclerk, which stays the
OIDC issuer for Audiobookshelf. Configure them in `config.toml` or in
**Settings → Sign-in** (Owner or operator). Mapping a provider's role to
`operator` is rejected.

See [Integrations](integrations.md#single-sign-on-from-another-provider)
and `config/config.example.toml`.

## Media worker pool

```toml
[media]
workers = 0             # 0 derives one per core, capped at 8
isolation = "required"  # required | best-effort | off
# worker_bin = "/usr/local/bin/bookclerk-media-worker"
```

Decoding, encoding and packaging run in `bookclerk-media-worker` child
processes, each confined to the paths its job declared. `required`
refuses media work when the jail doesn't engage, including when the
worker isn't installed. A reload applies changes to new jobs and lets the
old pool drain, so no restart is needed. See [Media worker](media.md).

## Plugin guests

```toml
[plugins]
isolation = "required"  # required | best-effort | off
# jail_bin = "/usr/local/bin/bookclerk-jail"
```

`bookclerk-jail` starts each external plugin, confined to its own
install, data, scratch and cache folders. `required` refuses to load a
plugin it can't jail, including when the launcher isn't installed.

This table is about **how** plugins run. Which plugins run on a host, and
their settings, live in the database (plugin instances and deployments).
See [Managing plugins](plugins.md).

`bookclerk config show` shows both tiers: `media.isolation`,
`media.worker_bin`, `plugins.isolation` and `plugins.jail_bin`. A helper
path reads `-` when it is found beside the running executable rather than
configured.

## Output (shared)

```toml
[output]
format = "enriched_m4b"
widevine = false
xhe_aac = false
naming_profile = "audiobookshelf"
download_cover = true
download_pdf = true
fixup_metadata = true
multi_destination = "sync_missing"
```

The shared output table is the second settings domain in the database,
`core.output`: one document every host applies. It holds the format,
Widevine and CDM provider, naming profile and templates, sidecars, cover
and chapter options, LAME settings, timestamps, path sanitization and
replacement rules, file name length, and the multi-destination policy.

It follows the same rules as `core.events`:

- **Seeding.** The `[output]` table and the `BOOKCLERK_OUTPUT_*`
  packaging variables (`BOOKCLERK_OUTPUT_FORMAT`,
  `BOOKCLERK_OUTPUT_WIDEVINE` and so on) are imported once, when a host
  enrolls against a database without a `core.output` document. A seed
  value out of bounds (such as `split_mp3_max_mb = 0`) is replaced by its
  default, with a warning naming the `output.*` key.
- **Authority.** After that, the document wins. Startup, reload and later
  TOML or environment values don't change it; edit it with a
  compare-and-swap write.
- **Showing it.** `bookclerk config show` prints
  `output.authority = database`, `output.revision` and every `output.*`
  value. If the stored document can't be read (unsupported version,
  corrupt, database unreachable), it prints
  `output.authority = unavailable` and `output.error` instead of the seed.
- **Checks.** Writes need the operator, and are checked before any SQL:
  `cover_size` is `native` or 1 to 10000, `chapter_layout` is `tree`,
  `flat` or `both`, `split_mp3_max_mb` is 1 to 10240, templates are at
  most 1 KiB without control characters, and there are at most 128
  replacement rules. A lost revision is a conflict and changes nothing.
- **Applying.** Running hosts apply a new revision when the
  `configuration.changed` wakeup reaches their node queue (with the
  60-second backstop). An acquire or convert already running keeps the
  settings it started with.

```sh
bookclerk config get output.naming_profile
bookclerk config set output.format single_mp3
bookclerk config set output.replacement_characters '[{"find":":","replace":"-"}]'
```

### Over HTTP

- `GET`/`PUT /api/config/domains/core.output` takes
  `{"expected_revision": n, "document": {…}}`, with every field of the
  schema-1 document.
- `PATCH /api/settings` accepts `output.*` keys through the same
  compare-and-swap, one revision per request. `GET /api/settings` lists
  the `output.*` values, with the revision they came from as top-level
  `output_revision`. Send that back as `expected_output_revision`; a stale
  value returns 409 and writes nothing. The web app always sends it.
- `output.revision` under `effective` is the revision this host has
  applied, which can lag until the host's next wakeup.
- A request mixing `output.*` with `events.*` or file-backed keys is
  rejected before anything is written.

The web app edits this document in **Settings → Output**.

### What stays per host

- `in_progress`, the acquire scratch folder, isn't part of the document.
  It names a path on one host, is needed before the database opens, and
  stays in `config.toml`.
- A relative `widevine_cdm` path resolves under each host's
  `BOOKCLERK_FILES_DIR`. When the first host seeds the document, a CDM
  path inside its files directory is stored as relative, so other hosts
  find it under their own.
- Each destination's root, prefix, naming overrides and credentials are
  `local` and `s3` plugin settings.

## Next steps

- [Output and destinations](output.md): destinations and naming.
- [Stores](stores.md): each store's settings.
- [Integrations](integrations.md): Audiobookshelf and sign-in providers.
