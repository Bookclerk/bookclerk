# Architecture

Bookclerk is a Cargo workspace of library crates plus binaries:

| Binary | Crate | Role |
| --- | --- | --- |
| `bookclerk` | `bookclerk-cli` | One-shot operator CLI |
| `bookclerkd` | `bookclerkd` | Scheduled jobs + authenticated HTTP API / GUI |
| `bookclerk-media-worker` | `bookclerk-media-worker` | Confined child process running one codec job |
| `bookclerk-jail` | `bookclerk-jail` | Applies a confinement policy, then `exec`s a plugin guest |
| `bookclerk-workerd` | `bookclerk-workerd` | Jailed Cloudflare workerd isolate launcher for script plugins |
| *(lib)* | `bookclerk-tray` | In-process tray linked into `bookclerkd` (not a default-member) |

Both share the same core: sources, library DB, acquire pipeline, storage, and
integrations.

## Data flow

```text
┌──────────────┐     scan      ┌────────────┐
│ ContentSource│ ────────────► │ library.db │
│ (per store)  │               │  (SQLite)  │
└──────┬───────┘               └─────┬──────┘
       │ fetch_title                 │ title ids / status
       ▼                             ▼
┌──────────────┐   pack (plain)   ┌────────────┐   put    ┌──────────────┐
│ cache/ temp  │ ───────────────► │  acquire   │ ───────► │ Destinations │
│ (plugin DRM) │                  │  pipeline  │          │ local / S3   │
└──────────────┘                  └─────┬──────┘          └──────────────┘
                                        │ book_acquired
                                        ▼
                                  ┌────────────┐
                                  │Integrations│  (ABS scan, portal, …)
                                  └────────────┘
```

1. **Scan** — each enabled source upserts owned titles into `library.db`.
2. **Enrich** (optional) — non-Audible rows may gain an Audible ASIN via public
   catalog search (`library.enrich_from_audible`).
3. **Acquire** — fetch Plain → package/encode → name → write every enabled
   destination. Decode, encode, and packaging do not run in the host: they are
   dispatched to a bounded pool of `bookclerk-media-worker` processes, each
   confined to the paths its job declared, so the C codecs never share an
   address space with the master key or `library.db`. See [media.md](media.md).
4. **Integrations** — receive `book_acquired` (and related) events; may trigger
   remote library scans or portal identity flows.
5. **Daemon** — runs scan/auto-acquire on an interval and exposes the control plane.

## Plugin kinds

Bookclerk uses four first-class plugin roles (in-process and/or external):

| Kind | Trait / host | Examples |
| --- | --- | --- |
| **Source** | `ContentSource` | `audible`, `libro`, `chirp`, `graphicaudio` |
| **Output / destination** | storage backends under `[output.*]` | `local`, `s3` |
| **Database** | SeaORM connection under `[database]` | `sqlite` (default), `d1` |
| **Integration** | `Integration` | `audiobookshelf`, SPA portal claim helpers |

Third-party plugins are separate executables discovered via `plugin.toml` and
spoken to over Workers RPC (`api_version = 1`) on stdio (native) or via
`bookclerk-workerd` (script isolates). Each guest is started by
`bookclerk-jail`, which confines it to its own install directory (read-only),
`plugins/<id>/data`, `plugins/<id>/tmp`, and — for source/output/database
operations — a **per-call** filesystem grant (Unix descriptor on fd 3; never the
download cache root) before becoming the plugin, so a storefront parsing hostile
input cannot reach `master.key` or the files-dir root. See [plugins.md](plugins.md).
Database backends are selected via `[database].plugin` (see [database.md](database.md));
external `kind = "database"` guests are loaded when staged under `plugins/`, with
in-process fallback when a platform guest is missing.

## Workspace crates (by concern)

| Concern | Crates |
| --- | --- |
| Config / paths / logging | `bookclerk-config` |
| Source trait + registry | `bookclerk-source` |
| Store adapters (plugins) | `bookclerk-plugins/optional/source-{audible,libro,chirp,graphicaudio}` (lib + guest bin) |
| ABS integration (plugin) | `bookclerk-plugins/integration-audiobookshelf` (lib + guest bin) |
| Clear-media packaging | `bookclerk-media` (remux / fixup / MP3; no DRM) |
| MP4 container plumbing | `bookclerk-mp4` (shared with store plugins; no cryptography) |
| Process confinement | `bookclerk-sandbox` (Landlock+seccomp / Seatbelt / AppContainer) |
| Guest jail launcher | `bookclerk-jail` (applies a policy, then `exec`s the guest) |
| Workerd isolate launcher | `bookclerk-workerd` (pinned Cloudflare workerd + bridge/egress) |
| Acquire orchestration | `bookclerk-acquire` |
| Naming templates | `bookclerk-naming` |
| Library DB | `bookclerk-library` (SeaORM plugins + rusqlite store) |
| Search | `bookclerk-search` |
| Discovery / recommendations | `bookclerk-discover` (via registered `ContentSource` catalog APIs) |
| Storage backends | `bookclerk-storage` |
| Catalog enrichment | `bookclerk-enrich` (shared HTTP helpers; Audible plugin owns Discover catalog) |
| Integrations framework + portal | `bookclerk-integrations` (traits / registry / portal; ABS lives in its plugin) |
| External plugin host | `bookclerk-plugin-host` |
| Libation migrate/export | `bookclerk-migrate` |

## Files directory layout

`$BOOKCLERK_FILES_DIR` (env or `--bookclerk-files`) is the unit of state:

```text
BookclerkFiles/
  config.toml
  library.db          # incl. encrypted_secrets (auth + Widevine CDM + S3 keys)
  cache/
  search_index/
  plugins/            # third-party plugin installs (see plugin-registry.md)
    <id>/data/        # one guest's state (its HOME inside the jail)
    <id>/tmp/         # one guest's scratch (its TMPDIR inside the jail)
  logs/               # reserved (Bookclerk does not rotate log files)
```

Runtime credentials are **not** stored as files under the files dir:
Audible/Libro.fm/Chirp/GraphicAudio auth, Widevine CDM blobs, and S3 destination
keys live in the `encrypted_secrets` DB table; bootstrap secrets (passphrase,
DB/API tokens) come from the environment. There is no `Accounts/` directory.

Relative `output.local.root` values resolve under this directory.

## Identity model

- Library rows are keyed by a stable **UUID**.
- Store identifiers (ASIN, ISBN, product id) are indexed attributes.
- `library acquire` / search accept UUID, ASIN, ISBN, or source product id.
- Scan inclusion is **per account** in SQLite (`auth set-scan`), not a TOML flag.

## Control plane (`bookclerkd`)

Default listen: both loopbacks
`["127.0.0.1:8787", "[::1]:8787"]` (`BOOKCLERK_DAEMON_LISTEN` / `daemon.listen`;
string or array / comma-separated). IPv6 uses bracketed form: `[::1]:8787`.

Operator auth (`[daemon.auth]`, DB-sealed token / `BOOKCLERK_OPERATOR_TOKEN`) gates
the API. Config reload swaps auth before listen rebind. See [gui.md](gui.md) and
[operations.md](operations.md).

| Method | Path | Auth | Notes |
| --- | --- | --- | --- |
| `GET` | `/health` | no | Liveness |
| `POST` | `/api/auth/login` | no | Operator token → session cookie |
| `GET` | `/api/auth/me` | yes | SPA bootstrap |
| `GET` | `/api/status`, `/status` | yes | Counts + listen |
| `GET` | `/api/jobs`, `/jobs` | yes | Job list |
| `POST` | `/api/library/scan`, `/scan` | yes | Queue scan |
| `POST` | `/api/library/acquire`, `/acquire` | yes | Queue acquire |
| `GET` | `/api/library/books` | yes | Paginated book rows |
| `GET` | `/api/library/books/{uuid}/cover` | yes | Best-effort local cover |
| static | `/` | no | Built React UI (`ui/dist`) when present |

Portal claim sessions use `bookclerk_portal_session` (`Path=/`) via `/api/portal/*`.
