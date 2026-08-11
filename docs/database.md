# Database plugins

Bookclerk stores library state (accounts, books, portal, discovery) in a
**database plugin**. Exactly one backend is active — unlike destinations, which
may fan out.

Default: local SQLite file (`$BOOKCLERK_FILES_DIR/library.db`). Optional:
Cloudflare D1 (remote SQLite via the Cloudflare HTTP API).

## ORM choice (Prisma-like maintainability)

Bookclerk previously used hand-written `rusqlite` SQL in
`bookclerk-library`. To keep schema and queries maintainable the way a
TypeScript app uses Prisma, we evaluated the Rust options:

| Option | What it is | Prisma-like DX | SQLite | Cloudflare D1 | Fit with Bookclerk |
| --- | --- | --- | --- | --- | --- |
| **SeaORM** | Async ActiveRecord-style ORM (entities, `ActiveModel`, relations, migrations) | Closest maintained match (TypeORM/Prisma ergonomics) | `sqlx-sqlite` | First-class `ProxyDatabaseTrait` (HTTP / Workers) | **Chosen** |
| **Diesel** | Sync compile-time query DSL | Strong types; steeper DSL; less “entity CRUD” feel | Excellent | `diesel-d1` is experimental, WASM Workers-only; HTTP API not ready | Strong for SQLite-only; weak for D1 from `bookclerk`/`bookclerkd` |
| **SQLx** | Compile-checked SQL, not an ORM | Far from Prisma | Yes | No first-class driver | Good toolkit, not the maintainability target |
| **prisma-client-rust** | Generated Prisma client for Rust | Most Prisma-like | Yes | Via Prisma ecosystem | **Unmaintained** — rejected |
| **Stay on rusqlite** | Thin SQLite wrapper | None | Native | Would be bespoke HTTP | Status quo; no ORM win |

### Why not Diesel?

Diesel’s compile-time guarantees are excellent, and Bookclerk’s current
`LibraryStore` is sync-friendly. D1 is the deal-breaker for an in-process
daemon/CLI:

- Official path for remote D1 from native binaries is the Cloudflare **HTTP**
  query API.
- SeaORM already supports custom backends through `Database::connect_proxy`.
- Diesel custom backends require
  `i-implement-a-third-party-backend-and-opt-into-breaking-changes` and a full
  `Backend`/`Connection` stack; `diesel-d1` today is Workers WASM + binding
  only.

### Why SeaORM 2

- Entity / `ActiveModel` workflow closest to Prisma Client usage patterns.
- Same `DatabaseConnection` for local SQLite and D1 (proxy).
- Migrations and schema stay SQL-first (compatible with existing
  `library.db` files) while queries move onto typed entities over time.

### `libsqlite3` alignment note

SeaORM’s `sqlx-sqlite` driver needs `libsqlite3-sys` `<0.38`. Upstream
`audible-rs` pins `rusqlite` 0.40 (`libsqlite3-sys` `^0.38`). Cargo cannot
link two `links = "sqlite3"` crates in one binary.

Bookclerk therefore:

- Pins workspace `rusqlite` to **0.37** and `rusqlite_migration` to **2.3** so
  they share `libsqlite3-sys` 0.35 with a single SQLite link.
- Vendors `audible-rs` under [`third_party/audible-rs`](../third_party/audible-rs)
  with only that dependency bump (see `BOOKCLERK_PATCH.md` there).
- Uses SeaORM’s **`proxy`** backend for both local SQLite (rusqlite wrapper) and
  D1 (HTTP). SeaORM 2.0’s `sqlx-sqlite` feature is not enabled: it currently
  fails to compile against `sea-query` 1.0.x (`Value` payload boxing change).

## Configuration

```toml
[database]
# Active plugin: "sqlite" (default) | "d1" | "postgres"
plugin = "sqlite"

[database.sqlite]
# Relative to BOOKCLERK_FILES_DIR, or absolute. Default: library.db
# path = "library.db"

[database.d1]
account_id = "your-cloudflare-account-id"
database_id = "your-d1-database-uuid"
# Token is read from BOOKCLERK_D1_API_TOKEN or CLOUDFLARE_API_TOKEN (env-only).
# api_base = "https://api.cloudflare.com/client/v4"

[database.postgres]
# Standard postgres:// connection URL. The URL contains credentials —
# prefer url_file or BOOKCLERK_DATABASE_POSTGRES_URL (registered for redaction).
# url = "postgres://user:password@localhost:5432/bookclerk"
# url_file = "/run/secrets/postgres_url"   # path to a file containing the URL
```

The Cloudflare API token is supplied via the environment only
(`BOOKCLERK_D1_API_TOKEN`, falling back to `CLOUDFLARE_API_TOKEN`); there is no
credentials file.

Environment overrides:

| Variable | Role |
| --- | --- |
| `BOOKCLERK_DATABASE_PLUGIN` | `sqlite`, `d1`, or `postgres` |
| `BOOKCLERK_DATABASE_SQLITE_PATH` | SQLite path override |
| `BOOKCLERK_D1_ACCOUNT_ID` | Cloudflare account id |
| `BOOKCLERK_D1_DATABASE_ID` | D1 database UUID |
| `BOOKCLERK_D1_API_TOKEN` / `CLOUDFLARE_API_TOKEN` | D1 API token |
| `BOOKCLERK_D1_API_BASE` | API base URL override |
| `BOOKCLERK_DATABASE_POSTGRES_URL` | Postgres connection URL (registered as secret) |
| `BOOKCLERK_DATABASE_POSTGRES_URL_FILE` | Path to file containing Postgres URL |

## Plugin kinds

Built-in **local SQLite** is a **platform-shipped guest**
(`plugins/sqlite/`, default `[database].plugin = "sqlite"`). The host passes
`library.db` over the side channel (fd 3); the guest runs with
`[sandbox].network = none`. The matching guest is **required** — there is no
in-process engine fallback.

External `kind = "database"` guests also load for **D1** and **Postgres** when
discovered and `[database].plugin` matches the plugin id. SeaORM proxy calls
(`db.query` / `db.execute`) forward through the guest; `master.key` never leaves
the host. Connect params are a tagged `DbConnectParams` shape; the guest returns
`{ "dialect": "sqlite" | "postgres" }` so the host builds the RPC proxy without
hardcoding backends.

First-party guests: `bookclerk-plugin-database-sqlite` (platform),
`bookclerk-plugin-database-d1` and `bookclerk-plugin-database-postgres` (optional).
Each is a full Workers-RPC guest with its own binary and `plugin.toml`. Install
platform sqlite under `$BOOKCLERK_FILES_DIR/plugins/sqlite/`; stage optional DB
guests with `cargo stage-plugins --optional`.

### Switching backends (opt-in migration)

Changing `[database].plugin` does **not** copy library data automatically.
After editing config (or `bookclerk plugins enable <id>`), use one of:

```bash
# Preview row counts
bookclerk config database migrate --from sqlite --to postgres --dry-run

# Copy library rows, then update config.toml
bookclerk config database migrate --from sqlite --to postgres --apply
```

Daemon operators can POST `/api/database/migrate` with JSON
`{ "from": "sqlite", "to": "postgres", "dry_run": false, "apply": true }`.
When `apply` is true, the daemon writes `[database].plugin` and **reopens the
library connection without restarting**. Config reload (`POST /api/config/reload`
or SIGHUP) also switches backends live when `[database].plugin` changes.

## Postgres plugin

`plugin = "postgres"` connects to a PostgreSQL server using the sqlx-postgres
driver inside SeaORM. The connection URL is standard libpq-style:
`postgres://user:password@host:5432/dbname`.

Configuration (at least one of `url` or `url_file` is required):

```toml
[database]
plugin = "postgres"

[database.postgres]
# Option A: inline URL (registered as secret for log redaction)
url = "postgres://bookclerk:secret@db.example.com/bookclerk"

# Option B: file containing the URL (preferred for production)
url_file = "/run/secrets/postgres_url"
```

Or via environment:

```
BOOKCLERK_DATABASE_PLUGIN=postgres
BOOKCLERK_DATABASE_POSTGRES_URL=postgres://user:pass@host/db
# Or:
BOOKCLERK_DATABASE_POSTGRES_URL_FILE=/run/secrets/postgres_url
```

**Schema migrations**: Fresh Postgres databases apply
[`latest_schema_postgres`](../crates/bookclerk-library/src/migrations.rs)
(the single greenfield DDL, with `BIGSERIAL` / `BIGINT` / `BYTEA` so integer
columns match the shared `i64` entities) and record version `1` in
`schema_migrations`. Every statement is `IF NOT EXISTS`, so re-application is a
no-op. Because the schema is greenfield rather than an incremental chain,
changing it means editing `latest_schema_postgres` and recreating (or manually
altering) an existing Postgres DB.

**Compiled features**: `sqlx-postgres` + `runtime-tokio-rustls` are enabled on
the `sea-orm` workspace dependency. `sqlx-sqlite` is intentionally excluded to
avoid the `libsqlite3-sys` link conflict with `rusqlite 0.37`.

## D1 caveats

- D1 is SQLite-compatible but accessed over HTTP; latency is higher than
  local `library.db`.
- Cloudflare D1 does not provide full interactive transaction semantics the
  way a local SQLite file does; prefer short statements and avoid relying on
  multi-statement ACID across the proxy.
- Schema migrations run in the D1 plugin module via
  `bookclerk_db_guest::apply_pending_migrations` (tracked in
  `schema_migrations`).

### Boundary: core vs database plugins

| Crate | Owns |
| --- | --- |
| [`bookclerk-library`](../crates/bookclerk-library) | Greenfield DDL ([`migrations`](../crates/bookclerk-library/src/migrations.rs)), SeaORM entities, [`LibraryStore`](../crates/bookclerk-library) CRUD (`from_connection` only) |
| `bookclerk-plugin-database-{sqlite,d1,postgres}` | Per-engine connect/migrate/proxy quirks and the jailed Workers-RPC guest |
| Host (`bookclerk-plugin-host`) | Spawn guest, mediate secrets into tagged `DbConnectParams`, forward SeaORM via RPC proxy |

Core stays database-agnostic: it only sees a migrated `DatabaseConnection`.
Hosts must install/stage the active database guest; missing guests are hard errors.

### LibraryStore status

The operator-facing [`LibraryStore`](../crates/bookclerk-library) is
**SeaORM-backed and async**: it holds a `DatabaseConnection` and every method is
an `async fn` returning `Result<…>`. Production opens go through
`bookclerk_plugin_host::open_library_store` (external guest required). Tests may
use `bookclerk_plugin_database_sqlite::open_memory` /
`open_store_memory` (dev-dependency on the plugin crate), then
`LibraryStore::from_connection` when needed.

CRUD runs on typed **SeaORM entities** (see
[`crate::entities`](../crates/bookclerk-library/src/entities)): one
`DeriveEntityModel` per table (`accounts`, `books`, `works`, `title_requests`,
`title_request_sources`, `encrypted_secrets`, …). Reads use `Entity::find()` + `QueryFilter` / `QueryOrder`;
writes use `ActiveModel` insert/update. Upserts that previously relied on
`ON CONFLICT … COALESCE(…)` are load-then-merge in Rust so the same behavior
holds on every backend. All entity integer columns are `i64`, reals `f64`, blobs
`Vec<u8>`, and text (including RFC 3339 timestamps) `String`.

The SQLite proxy in the database plugin returns **typed** SQL `NULL`s so SeaORM
`Option<T>` decoding works: it reads each column's declared type (rusqlite
`decl_type`) and emits the matching `Value::*(None)`. D1 (JSON, no type
metadata) falls back to a column-name heuristic.

### Single greenfield schema

Bookclerk is mostly greenfield: [`migrations.rs`](../crates/bookclerk-library/src/migrations.rs)
exposes `latest_schema_sqlite()` / `latest_schema_postgres()` as the base DDL,
plus a short ordered list (`migration_sql` / `migration_sql_postgres`) so
additive columns (e.g. Discover preference fields on `user_preferences`) can
bump `PRAGMA user_version` / `schema_migrations` without wiping data. Base
statements use `CREATE TABLE/INDEX IF NOT EXISTS`. Tables:
`accounts`, `books`, `ignored_titles`, `saved_filters`, `portal_identities`,
`claim_tickets`, `portal_sessions`, `account_links`, `works`, `work_editions`,
`listening_progress`, `title_requests`, `title_request_sources`, `embeddings`,
`user_preferences`, `encrypted_secrets`.

## Encrypted secrets

The `encrypted_secrets` table (part of the greenfield schema) enables DB-backed
storage for auth credentials, replacing file-based `Accounts/*.auth` files:

```sql
CREATE TABLE encrypted_secrets (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,        -- 'source_auth' | 's3' | 'widevine' | …
  provider TEXT,             -- 'audible' | 'libro' | 'chirp' | 'graphicaudio' | null
  account_type TEXT NOT NULL DEFAULT 'integration',  -- 'integration' | 'operator'
  account_id TEXT,           -- per-provider account stem or null
  name TEXT NOT NULL,        -- file-stem equivalent
  format TEXT NOT NULL,      -- 'sealed-v1' | 'json-encrypted' (legacy read) | 'audible-rs-auth' (legacy read)
  ciphertext BLOB NOT NULL,  -- sealed ciphertext or legacy encrypted payload
  kdf_algorithm TEXT,        -- null for sealed-v1; 'argon2id' for legacy json-encrypted
  kdf_salt BLOB,
  kdf_m_cost INTEGER,
  kdf_t_cost INTEGER,
  kdf_p_cost INTEGER,
  cipher_algorithm TEXT,     -- 'xchacha20poly1305' (used by both sealed-v1 and json-encrypted)
  cipher_nonce BLOB,         -- 24-byte XChaCha20 nonce
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(kind, provider, account_type, account_id, name)
);
```

### account_type: operator vs integration

`account_type` isolates **operator-owned** destination secrets from
**integration** (store-account) secrets:

- `integration` — store or portal account credentials (Audible, Libro.fm,
  Chirp, GraphicAudio, Widevine CDMs). These are purged by
  `delete_secrets_for_account` when a store account is revoked.
- `operator` — destination / control-plane secrets (S3 keys). These outlive
  any store account and are **never** touched by `delete_secrets_for_account`.

S3 credentials are stored with `account_type = "operator"`, `account_id =
"default"`, `name = "default"`. Integration credentials use `account_type =
"integration"` and an `account_id` matching the store account stem.

### Encryption design

All new writes use **`sealed-v1`**: XChaCha20-Poly1305 with a process-wide
**Data Encryption Key (DEK)** plus a random 24-byte nonce. The DEK is stored in
`$BOOKCLERK_FILES_DIR/master.key` and loaded at startup by
`bookclerk_library::configure_master_key`. There is **no per-row key derivation**
(Argon2) for new rows.

**`master.key` file formats:**
- `BCK1` header — raw 32-byte DEK (unprotected, for dev/testing only).
- `BCK2` header — DEK wrapped with `BOOKCLERK_AUTH_PASSWORD` via Argon2id +
  XChaCha20-Poly1305. Strongly recommended for production.

When `BOOKCLERK_AUTH_PASSWORD` or `[auth].password` is set and `master.key`
contains a raw `BCK1` key, Bookclerk re-wraps it as `BCK2` (at startup, on
`bookclerk config master-key wrap` / `config set auth.password`, or daemon
reload via SIGHUP / `POST /api/config/reload`).

**Legacy read support** (no new writes in these formats):
- `json-encrypted` — Argon2id-derived key from `BOOKCLERK_AUTH_PASSWORD`; still
  readable for migration purposes.
- `audible-rs-auth` — raw audible-rs envelope bytes; the outer seal is applied
  on write-back.
- `json` plaintext — rejected on read; migrate by re-saving with a master key
  configured.

**Fail-closed**: if a record exists but cannot be unsealed (wrong master key, corrupt
nonce, or missing DEK), Bookclerk returns an error rather than falling through to
an unencrypted fallback.

### SecretStore API

```rust
use bookclerk_library::{
    SecretStore, EncryptedSecretRecord, secret_kind, secret_account_type,
    build_sealed_record, unseal_secret, upsert_secret,
    configure_master_key, require_master_key, seal_with_dek, unseal_with_dek,
};

// At startup: configure_master_key(&paths.files_dir)?;

// Seal and upsert a new credential:
let record = build_sealed_record(
    &plaintext_bytes,
    secret_kind::SOURCE_AUTH,
    "chirp",
    secret_account_type::INTEGRATION,
    account_id,
    "alice.chirp.auth",
)?;
upsert_secret(db, &record).await?;

// Load and unseal:
let store = SecretStore::new(db);
let record = store.get(secret_kind::SOURCE_AUTH, Some("chirp"), secret_account_type::INTEGRATION, Some("alice"), "alice.chirp.auth").await?;
let plain = unseal_secret(&record)?;
```

### Bootstrap secrets stay outside the DB

These are required to open the DB or derive the master key and cannot be stored here:
- `BOOKCLERK_AUTH_PASSWORD` or `[auth].password` — wraps `master.key` at rest
  (strongly recommended in production; env preferred)
- `BOOKCLERK_DATABASE_POSTGRES_URL` / `BOOKCLERK_D1_API_TOKEN` — DB connection bootstrap
- `BOOKCLERK_OPERATOR_TOKEN` — optional override for the durable operator API token
  (otherwise sealed in `encrypted_secrets`; see `bookclerk daemon token`)
- `config.toml` (remains on disk)

> **No `Accounts/` directory for secrets.** All runtime credentials (Audible, Libro.fm,
> Chirp, GraphicAudio, Widevine CDM, and S3 destination keys) are stored in
> `encrypted_secrets`. S3 also accepts env override via `BOOKCLERK_AWS_ACCESS_KEY_ID` +
> `BOOKCLERK_AWS_SECRET_ACCESS_KEY` (+ optional `BOOKCLERK_AWS_SESSION_TOKEN`) and
> falls back to the AWS SDK default provider chain when no DB row is present.
> Bookclerk no longer creates or reads an `Accounts/` directory.
