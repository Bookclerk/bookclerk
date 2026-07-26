# Architecture

Bookclerk is a Cargo workspace of library crates plus binaries:

| Binary | Crate | Role |
| --- | --- | --- |
| `bookclerk` | `bookclerk-cli` | One-shot operator CLI |
| `bookclerkd` | `bookclerkd` | Scheduled jobs + authenticated HTTP API / GUI |
| `bookclerk-tray` | `bookclerk-tray` | Optional tray → system browser (not a default-member) |

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
┌──────────────┐   decrypt/pack   ┌────────────┐   put    ┌──────────────┐
│ cache/ temp  │ ───────────────► │  acquire   │ ───────► │ Destinations │
└──────────────┘                  │  pipeline  │          │ local / S3   │
                                  └─────┬──────┘          └──────────────┘
                                        │ book_acquired
                                        ▼
                                  ┌────────────┐
                                  │Integrations│  (ABS scan, portal, …)
                                  └────────────┘
```

1. **Scan** — each enabled source upserts owned titles into `library.db`.
2. **Enrich** (optional) — non-Audible rows may gain an Audible ASIN via public
   catalog search (`library.enrich_from_audible`).
3. **Acquire** — fetch → decrypt/encode → name → write every enabled destination.
4. **Integrations** — receive `book_acquired` (and related) events; may trigger
   remote library scans or portal identity flows.
5. **Daemon** — runs scan/auto-acquire on an interval and exposes the control plane.

## Plugin kinds

Bookclerk uses three first-class plugin roles (in-process and/or external):

| Kind | Trait / host | Examples |
| --- | --- | --- |
| **Source** | `ContentSource` | `audible`, `libro`, `chirp`, `graphicaudio` |
| **Output / destination** | storage backends under `[output.*]` | `local`, `s3` |
| **Integration** | `Integration` | `audiobookshelf`, Connect portal helpers |

Third-party plugins are separate executables discovered via `plugin.toml` and
spoken to over newline-delimited JSON-RPC on stdio. See [plugins.md](plugins.md).

## Workspace crates (by concern)

| Concern | Crates |
| --- | --- |
| Config / paths / logging | `bookclerk-config` |
| Source trait + registry | `bookclerk-source` |
| Store adapters | `bookclerk-audible`, `bookclerk-libro`, `bookclerk-chirp`, `bookclerk-graphicaudio` |
| Decrypt / remux / MP3 | `bookclerk-decrypt` |
| Acquire orchestration | `bookclerk-acquire` |
| Naming templates | `bookclerk-naming` |
| Library DB | `bookclerk-library` |
| Search | `bookclerk-search` |
| Storage backends | `bookclerk-storage` |
| Catalog enrichment | `bookclerk-enrich` |
| Integrations + portal | `bookclerk-integrations` |
| External plugin host | `bookclerk-plugin` |
| Libation migrate/export | `bookclerk-migrate` |

## Files directory layout

`$BOOKCLERK_FILES_DIR` (env or `--bookclerk-files`) is the unit of state:

```text
BookclerkFiles/
  config.toml
  library.db
  Accounts/           # *.audible.auth, *.libro.auth, *.ga.auth, *.chirp.auth, *.s3.auth, *.wvd
  cache/
  search_index/
  plugins/            # third-party plugin installs
  logs/               # reserved (Bookclerk does not rotate log files)
```

Relative `output.local.root` values resolve under this directory.

## Identity model

- Library rows are keyed by a stable **UUID**.
- Store identifiers (ASIN, ISBN, product id) are indexed attributes.
- `library acquire` / search accept UUID, ASIN, ISBN, or source product id.
- Scan inclusion is **per account** in SQLite (`auth set-scan`), not a TOML flag.

## Control plane (`bookclerkd`)

Default listen: `127.0.0.1:8787` (`BOOKCLERK_DAEMON_LISTEN` / `daemon.listen`).

Operator auth (`[daemon.auth]`, token file / `BOOKCLERK_OPERATOR_TOKEN`) gates
the API. See [gui.md](gui.md) and [operations.md](operations.md).

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

Connect portal under `/connect` keeps its own claim-ticket session model.
