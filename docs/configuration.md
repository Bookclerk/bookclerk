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
| `[output]` | Format, Widevine, naming, sidecars, multi-destination policy |
| `[output.local]` / `[output.s3]` | Destination plugins (`enabled`, roots, per-dest naming) |
| `[sources.<id>]` | Per-storefront enable + store knobs |
| `[daemon]` | Listen address, JSON logs |
| `[integrations]` / `[integrations.<id>]` | Portal + outbound integrations |
| `[discovery]` | Recommendations, embeddings, Open Library, listening sync |
| `[diagnostics]` | Opt-in report sharing |
| `[jobs]` / `[jobs.concurrency]` | Durable queue admission, leases, network-class concurrency |

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
| `BOOKCLERK_LIBRO_PASSWORD` | Libro.fm login |
| `BOOKCLERK_CHIRP_PASSWORD` | Chirp login |
| `BOOKCLERK_GA_PASSWORD` | GraphicAudio login |
| `BOOKCLERK_GA_ACCESS` | GraphicAudio access mode (`web`/`zip`/`device`) |
| `BOOKCLERK_ABS_API_KEY` | Audiobookshelf API key |
| `BOOKCLERK_DISCOVERY_EMBEDDINGS_ENABLED` | Local ONNX embeddings on/off |
| `BOOKCLERK_DISCOVERY_OPENLIBRARY_ENABLED` | Open Library enrichment on/off |
| `BOOKCLERK_DISCOVERY_RECOMMEND_LIMIT` | Default recommendation count |
| `BOOKCLERK_JOBS_MAX_PENDING` | Cap on pending+running daemon jobs (default 32) |
| `BOOKCLERK_JOBS_LEASE_SECONDS` | Worker lease length (default 60) |
| `BOOKCLERK_JOBS_MAX_ATTEMPTS` | Claims before a job fails terminally (default 3) |
| `BOOKCLERK_JOBS_RETENTION_DAYS` | Days to keep terminal job rows (default 7) |
| `BOOKCLERK_JOBS_TEMP_QUOTA_BYTES` | Acquire scratch quota (default 2 GiB) |
| `BOOKCLERK_JOBS_CONCURRENCY_NETWORK` | Network-class workers (default 1) |
| `BOOKCLERK_MEDIA_WORKERS` | Concurrent codec jobs (`0` = one per core, capped at 8) |
| `BOOKCLERK_MEDIA_ISOLATION` | `required` / `best-effort` / `off` |
| `BOOKCLERK_MEDIA_WORKER` | Path to `bookclerk-media-worker` |
| `BOOKCLERK_OUTPUT_LOCAL_ROOT` | Local destination root |
| `BOOKCLERK_OUTPUT_S3_*` / `BOOKCLERK_S3_*` | S3 destination settings (bucket/region/endpoint/…) |
| `BOOKCLERK_AWS_ACCESS_KEY_ID` / `BOOKCLERK_AWS_SECRET_ACCESS_KEY` | S3 credentials env override (optional `BOOKCLERK_AWS_SESSION_TOKEN`; wins over `encrypted_secrets` and SDK chain) |
| `BOOKCLERK_SOURCE_<ID>_ENABLED` | Force-enable/disable any source/plugin id (`<ID>` uppercased; e.g. `BOOKCLERK_SOURCE_ECHO_ENABLED=0`) |
| `BOOKCLERK_PLUGIN_DIRS` | Extra plugin search roots (OS path list) |
| `BOOKCLERK_PLUGIN_ISOLATION` | `required` / `best-effort` / `off` for plugin guests |
| `BOOKCLERK_PLUGIN_JAIL` | Path to `bookclerk-jail` |
| `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` | Diagnostics collector (build-time or runtime) |

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
network = 1
```

See [jobs.md](jobs.md) for the state machine, `409`/`429` admission, and crash
recovery.

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
launcher is not installed. This is about *how* guests run; which plugins load and
what they are configured with stays in `[sources.<id>]` /
`[integrations.<id>]`. See [plugins.md](plugins.md).

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

Destination and naming detail: [destinations.md](destinations.md).
Store-specific knobs: [sources.md](sources.md).
Integrations: [integrations.md](integrations.md).
