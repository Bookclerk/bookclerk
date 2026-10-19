# Configuration

Primary file: `$BOOKCLERK_FILES_DIR/config.toml` (or `--config` /
`BOOKCLERK_CONFIG`). Annotated template:
[`config/config.example.toml`](../config/config.example.toml).

Inspect resolved values:

```bash
bookclerk config show
bookclerk config get <key>
bookclerk config paths
```

Classic Libation setting names are accepted as aliases where documented in
[libation-parity.md](libation-parity.md). Runtime overrides on acquire:
`-o key=value` (repeatable).

## Top-level tables

| Table | Purpose |
| --- | --- |
| `[library]` | Auto-acquire, scan interval, enrichment, storage layout fix |
| `[database]` / `[database.sqlite]` / `[database.d1]` | Library DB plugin (`sqlite` default, Cloudflare D1) |
| `[auth]` | Optional `[auth].password` wrapping `master.key` (prefer `BOOKCLERK_AUTH_PASSWORD`); optional `[auth.oidc]` identity broker |
| `[media]` | Codec worker pool: concurrency and confinement (see [media.md](media.md)) |
| `[plugins]` | How external plugin guests are jailed (see [plugins.md](plugins.md)) |
| `[output]` | Format, Widevine, naming, sidecars, multi-destination policy (shared; per-destination settings are plugin settings) |
| `[daemon]` | Listen address, JSON logs, trusted proxies, `[daemon.node_api]` transport |
| `[integrations]` | Portal: `public_origin`, claim-ticket and portal session TTLs |
| `[discovery]` | Recommendations, embeddings, Open Library, listening sync |
| `[diagnostics]` | Opt-in report sharing |
| `[jobs]` / `[jobs.concurrency]` | Durable queue admission, leases, network-class concurrency |
| `[events]` | Outbox retention and local delivery-worker concurrency |

Storefronts, destinations, and integrations are **not** in this file. Each
is a plugin instance in the database: its settings come from the plugin's
`[[settings]]` schema and "enabled" is a deployment on a host. See
[Plugin instances](#plugin-instances) below.

## Important environment variables

| Variable | Role |
| --- | --- |
| `BOOKCLERK_FILES_DIR` | State root (DB, plugins, cache, …) |
| `BOOKCLERK_DATABASE_PLUGIN` | Active DB plugin (`sqlite` / `d1`) |
| `BOOKCLERK_DATABASE_SQLITE_PATH` | SQLite path override |
| `BOOKCLERK_D1_ACCOUNT_ID` / `BOOKCLERK_D1_DATABASE_ID` | Cloudflare D1 identifiers |
| `BOOKCLERK_D1_API_TOKEN` / `CLOUDFLARE_API_TOKEN` | D1 API token (env-only) |
| `BOOKCLERK_CONFIG` | Config path override |
| `BOOKCLERK_DAEMON_LISTEN` | Control plane bind |
| `BOOKCLERK_LOG` / `RUST_LOG` | Log filter |
| `BOOKCLERK_AUTH_PASSWORD` | Wraps `master.key` at rest (preferred over `[auth].password`); required for legacy `json-encrypted` rows |
| `BOOKCLERK_OIDC_<ID>_CLIENT_SECRET` | Upstream OIDC/OAuth client secret for provider id `<ID>` (hyphens become underscores) |
| `BOOKCLERK_DISCOVERY_EMBEDDINGS_ENABLED` | Local ONNX embeddings on/off |
| `BOOKCLERK_DISCOVERY_OPENLIBRARY_ENABLED` | Open Library enrichment on/off |
| `BOOKCLERK_DISCOVERY_RECOMMEND_LIMIT` | Default recommendation count |
| `BOOKCLERK_JOBS_MAX_PENDING` | Cap on pending+running daemon jobs (default 32) |
| `BOOKCLERK_JOBS_LEASE_SECONDS` | Worker lease length (default 60) |
| `BOOKCLERK_JOBS_MAX_ATTEMPTS` | Claims before a job fails terminally (default 3) |
| `BOOKCLERK_JOBS_RETENTION_DAYS` | Days to keep terminal job rows (default 7) |
| `BOOKCLERK_JOBS_TEMP_QUOTA_BYTES` | Acquire scratch quota (default 2 GiB) |
| `BOOKCLERK_JOBS_CONCURRENCY_NETWORK` | Network-class workers on this node and running network jobs cluster-wide (default 1) |
| `BOOKCLERK_JOBS_CONCURRENCY_PLUGIN_INSTANCE` | Running jobs per required plugin instance, cluster-wide (default 1) |
| `BOOKCLERK_MEDIA_WORKERS` | Concurrent codec jobs (`0` = one per core, capped at 8) |
| `BOOKCLERK_MEDIA_ISOLATION` | `required` / `best-effort` / `off` |
| `BOOKCLERK_MEDIA_WORKER` | Path to `bookclerk-media-worker` |
| `BOOKCLERK_PLUGIN_DIRS` | Extra plugin search roots (OS path list) |
| `BOOKCLERK_PLUGIN_ISOLATION` | `required` / `best-effort` / `off` for plugin guests |
| `BOOKCLERK_PLUGIN_JAIL` | Path to `bookclerk-jail` |
| `BOOKCLERK_PLUGIN_WORKERD` | Path to `bookclerk-workerd` (the front door every plugin spawns through) |
| `BOOKCLERK_WORKERD_BIN` | Path to the pinned Cloudflare `workerd` (default: beside `bookclerk-workerd`) |
| `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` | Diagnostics collector (build-time or runtime) |

There are no environment overrides for plugin settings or enablement
(formerly `BOOKCLERK_SOURCE_<ID>_ENABLED`, `BOOKCLERK_OUTPUT_*`,
`BOOKCLERK_S3_*`, `BOOKCLERK_AWS_*`, `BOOKCLERK_GA_*`, `BOOKCLERK_ABS_*`,
`BOOKCLERK_*_PASSWORD`). Those values live in the database.

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
network = 1          # workers here, and running network jobs cluster-wide
plugin_instance = 1  # running jobs per required plugin instance, cluster-wide
```

See [jobs.md](jobs.md) for the state machine, `409`/`429` admission, and crash
recovery.

## Events

```toml
[events]
retention_days = 7
dead_letter_retention_days = 30
concurrency = 1
```

`[events]` is the first database-backed configuration domain (`core.events`).
See [ADR: Control plane](adr/control-plane.md).

On first enrollment the effective TOML and `BOOKCLERK_EVENTS_*` values are
imported once. After that row exists the database is the authority: startup,
reload, `bookclerk config set events.*`, and later environment variables do
not overwrite it. `bookclerk config show` prints `events.authority = database`
and `events.revision`. A compare-and-swap write that loses returns a revision
conflict and leaves the stored document unchanged.

```bash
bookclerk config get events.retention_days
bookclerk config set events.retention_days 14
```

The daemon operator API is `GET` / `PUT /api/config/domains/core.events`
(`expected_revision` plus the three fields). `PATCH /api/settings` accepts
`events.*` through the same compare-and-swap path when the request does not
also change file-backed keys. A mixed request is rejected before either
authority is written. The Settings page saves file-backed keys and does not
batch them with `events.*`. `GET /api/settings` includes `events.revision` in
`effective`.

Running processes re-read the document on startup and whenever a
`configuration.changed` message reaches their node queue (`node.<node_id>`;
every committed write publishes one to each registered node), with a
60-second backstop pass.
A reload reads that document before it replaces the library connection or
operator auth. If the read fails, the previous runtime stays in place. Guests
bound to a plugin instance keep running across a reload; when the database
connection itself changed, every guest is retired and the reconciler brings
this host's present deployments back. A newer revision of the same cluster is
not replaced by the older snapshot.
Retention and the in-flight cap apply on the next dispatcher tick. The number
of local delivery tasks is chosen at process start. Other `[events]` neighbors
in this file (`[library]`, `[jobs]`, the shared `[output]`, media, plugins,
diagnostics) are still TOML and environment configuration.

Acked/rejected deliveries use `retention_days`. Parent events with no remaining
live deliveries are kept until that same cutoff so a late node can still
reconcile. Dead letters use the longer `dead_letter_retention_days`. Cleanup
runs at daemon start and hourly, not on every dispatcher tick.
`concurrency` is the number of local delivery workers **and** the cluster-wide
max `running` deliveries per `(plugin_id, resource_class)` (claim still filters by
loaded plugin ids; a portable `bookclerk_slots` row serializes that
count). See [jobs.md](jobs.md) and [plugins.md](plugins.md).

Allowed writes: retention fields `1..=3650`, concurrency `1..=32`.

### Bootstrap files

These stay outside the database because the process needs them before
`core.events` can be read:

| File / setting | Role |
| --- | --- |
| `[database]` and `BOOKCLERK_DATABASE_*` / D1 / Postgres env | Which library database to open |
| `master.key` plus `BOOKCLERK_AUTH_PASSWORD` or `[auth].password` | Cluster data-encryption key |
| `node-identity.json` | Stable host id and the cluster id this files directory joined |

A second host joining an existing database must have the same `master.key`.
Bookclerk does not mint a new key when the database already has a secret-root
fingerprint. The CLI aligns that root when a command opens the library, and
does not mint or cache a key before command dispatch. `bookclerk config
master-key status` and `bookclerk config master-key wrap` do not open the
database; wrap fails when `master.key` is absent. Copy `master.key` and the
database settings. Omit
`node-identity.json` when the process should be a new host; copying that file
reuses the host id. A file bound to a different cluster id fails without
rewriting either side.

## Host nodes

A host node is a separate Bookclerk install that enrolls into this cluster
with a single-use link (see [ADR: Control plane](adr/control-plane.md#host-node-enrollment-and-transport)).

Enrollment is a cluster document, `core.host_enrollment`, on by default.
Only the operator or an Owner turns it off: the operator with the CLI, an
Owner (or the operator) over HTTP with `PUT /api/config/host-enrollment`
and `{"enabled": false}`. Any signed-in user may read it with
`GET /api/config/host-enrollment`.

```bash
bookclerk config get host_enrollment.enabled
bookclerk config set host_enrollment.enabled false
```

While it is on, any active user may mint a link for themselves (Settings
later; `POST /api/config/node-invites` today), and the operator or an Owner
may mint for any active user:

```bash
bookclerk nodes invite --user riley --ttl 24h   # prints the link once
bookclerk nodes invites list
bookclerk nodes invites revoke <link_id>
bookclerk nodes revoke <node_id>                # revoke an enrolled host
```

Any signed-in user may also revoke a host they own with
`DELETE /api/config/node-credentials/<node_id>`; the operator or an Owner
may revoke any host.

The link is `<public_origin>/enroll#bcn1.<link_id>.<secret>[.<spki_pin>]`
(the pin only in `self_signed` mode, below). The secret is in
the URL fragment, so proxies and access logs never see it; the database keeps
only its hash. Set `integrations.public_origin` so the link points somewhere a
host can reach. Links live 24 hours by default (5 minutes to 7 days) and
redeem once.

`[daemon.node_api]` is bootstrap configuration, like `daemon.listen`. It
decides which connections may reach the node API (`POST /api/node/v1/enroll`
and the host-node socket `GET /api/node/v1/connect`). Changing it needs a
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

| Mode | Allowed |
| --- | --- |
| `loopback` (default) | Only connections where both ends are loopback and no forwarding header is present. Loopback is allowed in every mode, including on a wildcard `daemon.listen` (`0.0.0.0` / `[::]`): the check reads each connection's own local address, not the bind address. |
| `proxy` | Plain HTTP from a `daemon.trusted_proxies` peer that sends exactly one `X-Forwarded-Proto: https`. Requires an https `integrations.public_origin` and at least one trusted proxy. Every connection from a trusted proxy is checked this way, even one with no forwarding headers, so with a same-host proxy (`127.0.0.1` trusted) loopback enrollment goes through the proxy too. |
| `acme` | The built-in TLS listener on `tls_listen` with a certificate for `integrations.public_origin`'s DNS name, issued over TLS-ALPN-01 (forward port 443 of that name to `tls_listen`). Account and certificate are cached under `$BOOKCLERK_FILES_DIR/acme/` (mode 0700) and renewed automatically. Requires `acme_contact` and an https `public_origin` with a DNS name. |
| `self_signed` | The built-in TLS listener on `tls_listen` with a P-256 certificate generated once under `$BOOKCLERK_FILES_DIR/node-api/self-signed/` (mode 0700). Its SPKI pin (`spki-pin` there) is appended to every enrollment link, and the host trusts exactly that key; until the daemon has written it, minting an invite fails rather than produce an unpinned link. Needs an https `integrations.public_origin` naming the TLS listener, since enrollment links carry that origin. For a private network without ACME or a proxy. |

Everything else gets `403 {"error": "transport_insecure"}`: plain HTTP from a
LAN address is refused, and forwarding headers from a peer that is not a
trusted proxy are ignored. A mode missing its required settings refuses
every connection.

The TLS listener serves only the node API routes. In `acme` and
`self_signed` mode the node API stays unmounted on every listener, loopback
included, until that listener has a certificate: while ACME has not issued
one (or the listener could not bind), node API routes answer
`404 {"error": "node_api_unmounted"}`. There is no plain-HTTP fallback;
ACME keeps retrying in the background.

To exercise ACME against a local [Pebble](https://github.com/letsencrypt/pebble)
server (run with `PEBBLE_VA_ALWAYS_VALID=1`), set
`BOOKCLERK_TEST_ACME_DIRECTORY=https://localhost:14000/dir` and
`BOOKCLERK_TEST_ACME_CA=<path to pebble.minica.pem>` and run
`cargo test -p bookclerkd acme_issues_from_pebble`; without them that test
skips.

## Plugin instances

Everything a storefront, destination, or integration plugin can be configured
with is declared by the plugin itself in `plugin.toml` `[[settings]]` and
stored per **plugin instance** in the database. Enrollment (CLI and daemon
alike) pins that schema, creates the plugin's default instance with the
declared defaults, and records a deployment row for this host. "Enabled"
means `desired = present` on a host.

```bash
bookclerk plugins list                               # instances, deployments, observations
bookclerk plugins deploy s3 [--node <id>]            # desired = present
bookclerk plugins undeploy s3 [--node <id>]           # desired = absent → guest retired
bookclerk plugins config get s3                      # schema defaults under the document
bookclerk plugins config set s3 bucket my-audiobooks
bookclerk plugins config set s3 secret_access_key --secret   # stdin, sealed
bookclerk plugins config unset s3 prefix
bookclerk plugins instances create s3 --name "Backup bucket"
```

The daemon's operator API mirrors this under `/api/config/plugin-instances`
(list / create, `GET`/`PUT …/{id}/config` with `expected_revision`,
`PUT …/{id}/deployments/{node_id}`), plus `/api/config/nodes` and
`/api/config/plugins` (installed versions; `GET …/{key}` carries the pinned
schema). **Settings → Plugins** is built from the same
schema. `PATCH /api/settings` no longer accepts plugin keys.

Writes are compare-and-swap on the document revision and validate against
the pinned schema, so a stale write or an unknown key is rejected. Secrets
(`type = "secret"`) are sealed into `encrypted_secrets` and never echoed
back. Several instances of one plugin may exist, and may run on one host; a
bare plugin key routes to the default instance, and `library scan|acquire
--instance` pins another. Store accounts are keyed by plugin key, not
instance, so every instance serves the same connected accounts. Details:
[plugins.md](plugins.md#enabling-and-settings-database),
[destinations.md](destinations.md), [sources.md](sources.md),
[integrations.md](integrations.md).

## Identity broker (`[auth.oidc]`)

Optional. Bookclerk remains the OIDC issuer for Audiobookshelf; these providers
sign Users in. Configure in `config.toml` or **Settings → Sign-in** (Owner or
Operator). See [integrations.md](integrations.md#optional-identity-broker-upstream-oidc--social)
and `config/config.example.toml`. Mapping any IdP role to `operator` is rejected.

## Media worker pool

```toml
[media]
workers = 0             # 0 derives one per core, capped at 8
isolation = "required"  # required | best-effort | off
# worker_bin = "/usr/local/bin/bookclerk-media-worker"
```

Decode, encode, and packaging run in `bookclerk-media-worker` child processes
confined to the paths each job declared. `required` refuses media work when the
jail does not engage, including when the worker binary is not installed. A reload
applies changes here to subsequent jobs and lets the previous pool drain, so no
restart is needed. See [media.md](media.md).

## Plugin guests

```toml
[plugins]
isolation = "required"  # required | best-effort | off
# jail_bin = "/usr/local/bin/bookclerk-jail"
```

External plugin guests are started by `bookclerk-jail`, which confines them to
their own install, data, scratch, and cache directories before becoming the
plugin. `required` refuses to load a plugin it cannot jail, including when the
launcher is not installed. This is about *how* guests run; which plugins run
on this host and what they are configured with is the database (plugin
instances and deployments). See [plugins.md](plugins.md).

Both confinement tiers are visible in `bookclerk config show`
(`media.isolation`, `media.worker_bin`, `plugins.isolation`, `plugins.jail_bin`);
a helper path reads `-` when it is found beside the running executable rather
than configured.

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

This shared table is still `config.toml`; moving it to a `core.output`
cluster document is tracked in [ADR: Control plane](adr/control-plane.md).
Per-destination roots, prefixes, naming overrides, and credentials are the
`local` / `s3` plugin settings.

Destination and naming detail: [destinations.md](destinations.md).
Store-specific knobs: [sources.md](sources.md).
Integrations: [integrations.md](integrations.md).
