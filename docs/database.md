# Database plugins

Bookclerk stores library state (accounts, books, portal, discovery) in a
**database plugin**. Exactly one backend is active — unlike destinations, which
may fan out. Supported backends are **SQL-like only**: SQLite, PostgreSQL, and
Cloudflare D1. Document / key-value stores are out of scope and must fail
capability negotiation ([ADR: SQL database contract](adr/sql-database-contract.md)).

Default: local SQLite file (`$BOOKCLERK_FILES_DIR/library.db`). Optional:
Cloudflare D1 (remote SQLite via the Cloudflare HTTP API) or PostgreSQL.

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
- Schema stays SQL-first (one greenfield version-1 pack) while queries
  use typed entities.

### `libsqlite3` alignment note

SeaORM’s `sqlx-sqlite` driver needs `libsqlite3-sys` `<0.38`. Upstream
`audible-rs` pins `rusqlite` 0.40 (`libsqlite3-sys` `^0.38`). Cargo cannot
link two `links = "sqlite3"` crates in one binary.

Bookclerk therefore:

- Pins workspace `rusqlite` to **0.37** so it shares `libsqlite3-sys` 0.35
  with a single SQLite link.
- Vendors `audible-rs` under [`third_party/audible-rs`](../third_party/audible-rs)
  (see `BOOKCLERK_PATCH.md` there). The plugin uses audible-rs as a library
  (`default-features = false`); its optional `cli` feature still pins
  `rusqlite` 0.37 so a CLI rebuild would share the same SQLite link.
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
(`plugins/sqlite/`, default `[database].plugin = "sqlite"`). The host grants
`library.db` (and journal sidecars) in the jail allowlist and passes the path as
`BOOKCLERK_SQLITE_PATH` / context `sqlitePath`; the guest runs with
`[sandbox].network = none`. The matching guest is **required** — there is no
in-process engine fallback.

External `kind = "database"` guests also load for **D1** and **Postgres** when
discovered and `[database].plugin` matches the plugin id. SeaORM proxy calls
(`db.query` / `db.execute`) forward through the guest; `master.key` never leaves
the host. First-party ids receive host-private connect params with
host-injected paths/secrets (`sqlite` / `d1` / `postgres`), while any other
database plugin id receives the public `DatabaseAdapterConfig` payload
(`pluginDataDir` plus its granted `[database.<id>]` settings). The guest
returns bootstrap `{ "sqlFamily", "dialect" }` (and related capability fields)
so the host builds the RPC proxy without hardcoding backends.

First-party guests: `bookclerk-plugin-database-sqlite` (platform),
`bookclerk-plugin-database-d1` and `bookclerk-plugin-database-postgres` (optional).
Each is a full Workers-RPC guest with its own binary and `plugin.toml`. Install
platform sqlite under `$BOOKCLERK_FILES_DIR/plugins/sqlite/`; stage optional DB
guests with `cargo stage-plugins --optional`.

### Isolated plugin database bindings

Plugins that declare `capabilities.bindings.databases = ["DB", ...]` get
Workers-style **named database bindings**: one isolated database per binding,
provisioned by the active adapter (SQLite file / PostgreSQL **database** /
Cloudflare D1 database by name) and recorded in the host
`plugin_databases` registry. Bindings are consented per name
(`database:<NAME>` grant entries), carry their own `db_atomic_receipts` for
retry-token replay, and allow plugin-owned schema (full DML plus idempotent
`CREATE`/`DROP` `TABLE`/`INDEX` with `IF [NOT] EXISTS`). Jobs never receive
the host library as guest SQL. Operator lifecycle:
`bookclerk plugins db list` / `bookclerk plugins db drop <plugin> [binding]`
(physical delete of the SQLite file / `DROP DATABASE` / D1 delete, then the
registry row; unknown adapters fail closed).
See [plugins.md — Isolated plugin database bindings](plugins.md#isolated-plugin-database-bindings).

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
[`latest_schema_postgres`](../crates/bookclerk-db-exec/src/schema_postgres.rs)
(the single greenfield DDL, with `BIGSERIAL` / `BIGINT` / `BYTEA` so integer
columns match the shared `i64` entities) and record version `1` in
`schema_migrations`. Every statement is `IF NOT EXISTS`, so re-application is a
no-op. Because the schema is greenfield rather than an incremental chain,
changing it means editing the Postgres pack in `bookclerk-db-exec` and
recreating (or manually altering) an existing Postgres DB.

**Compiled features**: `sqlx-postgres` + `runtime-tokio-rustls` are enabled on
the `sea-orm` workspace dependency. `sqlx-sqlite` is intentionally excluded to
avoid the `libsqlite3-sys` link conflict with `rusqlite 0.37`.

## D1 caveats

- D1 is SQLite-compatible but accessed over HTTP; latency is higher than
  local `library.db`.
- Each HTTP request is its own connection. D1's HTTP API cannot keep a classic
  interactive `BEGIN` open across RPCs, and Cloudflare Time Travel is a
  **database-wide restore**, not a per-request rollback (it cannot exclude
  other writers, and a crash before restore leaves partial writes committed).
- The D1 guest therefore keeps the host-private interactive-transaction
  interface unsupported (`begin` fails; there is no `commit` / `rollback`).
  Ordinary autocommit statements use the documented REST body
  `{ "sql", "params" }`.   Atomic library operations (claim redeem, last-owner
  demote/disable/delete, password rotation, TOTP enroll/disable, consume-once OIDC RP state and
  WebAuthn challenges, job admit/claim, event publish/dispatch/claim) use the
  typed atomic `execute` on every bundled backend. The **host** compiles a
  bounded generic SQL plan (statements + binds + outcome selectors + receipt
  wrapping). The
  D1 plugin runs that plan as **one** `{ "batch": [...] }` REST request (a real
  SQL transaction) and returns per-statement `rows` / `rowsAffected`. SQLite
  and PostgreSQL guests run the same plan in a native local transaction and
  return the same generic batch result. The host interprets receipts and
  application status (`ok`, `empty`, `lastOwner`, …). Guests do not parse
  Bookclerk operation names or table names. Each call carries an
  `operationId` and `requestHash`; host-authored SQL writes a durable receipt
  in the same transaction so a committed result whose HTTP/RPC response is
  lost can be retried without a second mutation.
  Structured statuses include `ok`, `empty`, `lastOwner`, `claimInvalid`,
  `passwordRequired`, `notFound`, and `idempotencyConflict`. Consume-once ops
  use `DELETE … RETURNING` so a missing or expired row cannot be observed
  twice. After `openSession` the host calls typed
  `AdapterDatabaseSession.capabilities`. Schema selection uses `pragmaUserVersion` /
  `schemaMigrations` / `atomicSchemaBatch`, not `sqlFamily`. D1 reports
  `atomicSchemaBatch: true` and `maxBinds: 100`. The host stores the
  negotiated `DbCapabilities` and rejects plans that exceed `maxStatements`,
  per-statement `maxBinds`, `maxPayloadBytes`, or out-of-range selectors.
  A statement that yields more than `maxResultRows` **fails the plan**
  (no silent truncate). Guests also advertise `maxResultBytes`, `maxCellBytes`,
  `maxRequestBytes`, and `maxAtomicResultBytes` (`0` is unspecified and
  fails closed). Batch request/result caps are at or below the scalar
  limit. D1 refuses
  `RETURNING` unless host-IR `maxRows` is `1` (and rejects multi-tuple
  `VALUES` / semicolon-joined SQL) before HTTP; overflow or an oversized body
  after HTTP commit is `unavailable`. Guests that omit `returning`, advertise
  `0` row/payload/atomic caps, or mismatch bootstrap `dialect`/`sqlFamily`
  are not loaded. Time Travel is not used.
- Guest failures are classified from SQLSTATE / `SQLITE_*` codes (not English
  `"unique"` matching). Constraint → `conflict`; busy/serialization →
  `unavailable` (the same `operationId` is retried); timeout →
  `deadline_exceeded`; syntax → `invalid_params`. Cancel and deadlines stay
  RPC/session-level (`deadlineUnixMs` on the atomic request is transport
  metadata and is not hashed). Observed cancel/deadline before `BEGIN`/HTTP
  or between statements is `cancelled` / `deadline_exceeded`; around `COMMIT`
  or HTTP return is `unavailable` (ambiguous).
- Schema versions are **host-owned**. After `db.connect` and capability
  negotiation the host reads the current version and sends remaining DDL as
  generic execute (one host-compiled `{ "batch": [...] }` on D1).
  Guests connect and ping only.

### Boundary: core vs database plugins

| Crate | Owns |
| --- | --- |
| [`bookclerk-library`](../crates/bookclerk-library) | Greenfield DDL ([`migrations`](../crates/bookclerk-library/src/migrations.rs)), SeaORM entities, domain invariants, host-owned atomic SQL plans, schema application after connect, [`LibraryStore`](../crates/bookclerk-library) CRUD (`from_connection` only) |
| `bookclerk-plugin-database-{sqlite,d1,postgres}` | Connection/transport, capability advertisement, bind encoding, generic query/execute/atomic-batch, error/timing normalization. **Not** Bookclerk table names, schema version selection, or named domain operations. |
| Host (`bookclerk-plugin-host`) | Spawn guest, mediate secrets into tagged `DbConnectParams`, negotiate capabilities, apply remaining DDL, compile plans, validate result envelopes, interpret statement results, forward SeaORM via RPC proxy |

Core stays database-agnostic: it sees a migrated `DatabaseConnection`, and
the host always attaches [`AtomicTxnBackend`](../crates/bookclerk-library)
(generic atomic plan on the guest; host `interpret_plan` on the result)
for atomic security, job, and event operations. Domain names stay in host
code.
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

Owner / Administrator / Member is part of this greenfield schema. There is no
Admin→Owner upgrade; testing and development hosts should recreate
`library.db` after that role change (`cargo reset --yes`).

### Single greenfield schema

Bookclerk is greenfield: [`migrations.rs`](../crates/bookclerk-library/src/migrations.rs)
exposes `latest_schema_sqlite()` as the current library DDL, and
[`latest_schema_postgres()`](../crates/bookclerk-library/src/migrations.rs)
is that pack lowered at the adapter edge. Fresh databases apply one version-1
plan (`host_migration_plan`). Statements use `CREATE TABLE/INDEX IF NOT EXISTS`.
Changing the schema means editing that pack and recreating (or manually
altering) an existing database (`cargo reset --yes` in development).

Tables include: `accounts`, `books`, `ignored_titles`, `saved_filters`,
`users`, `portal_identities`, `claim_tickets`, `portal_sessions`,
`operator_sessions`, `account_links`, `works`, `work_editions`,
`listening_progress`, `title_requests`, `title_request_sources`, `embeddings`,
`user_preferences`, `encrypted_secrets`, `jobs`, `job_temp_paths`,
`job_queue_control`, `domain_events`, `event_deliveries`,
`event_subscriber_nodes`, `event_outbox_stats`, `db_atomic_receipts`,
`db_serialization_slots`, `plugin_databases`.

The `jobs` table is the durable daemon queue (see [jobs.md](jobs.md)).
`job_queue_control` is a singleton row used to serialize admission and
scratch-quota updates under PostgreSQL `READ COMMITTED`. `users.last_seen_at`
is a durable last-authenticated timestamp. `users.avatar_source` is
`NULL`/`auto`, `monogram`, `gravatar`, `upload`, or
`sso:{portal_identities.id}`; `portal_identities.picture_url` stores
IdP-supplied avatars. `webauthn_credentials.name` is the passkey label;
`users.totp_enabled` is confirmed authenticator-app TOTP (secrets stay in
`encrypted_secrets`). `user_preferences.theme` is `system`, `light`, or
`dark`. `oidc_clients` include `enabled` and `plugin_id`.

Domain events are **not** job kinds; see [jobs.md](jobs.md). Duplicate
publishes coalesce on `(account_id, source, event_type, dedup_key)`.
Deliveries are idempotent on `(event_id, plugin_id)`. `domain_events` stores
producer `source`, `ordering_key`, `wake_pending`, claimed wake slices, and
`dispatch_snapshot_json`. `event_deliveries` stores `wake_event_type` /
`wake_filter_json` / `wake_grants_json`, `cancel_requested`, and
`resource_class`. Live dispatch unions `event_subscriber_nodes` whose
heartbeat is within 60 seconds. `db_serialization_slots` is portable
COUNT+mutate serialization (no PostgreSQL advisory locks). Wake page size is
derived from the guest’s negotiated `maxBinds` (D1 is 100) minus the fenced
UPDATE’s fixed binds. Claim uses this node’s catalog in **host** code; the
atomic mutation is a compare-and-set on a concrete delivery id.

## Encrypted secrets

The `encrypted_secrets` table (part of the greenfield schema) enables DB-backed
storage for auth credentials, replacing file-based `Accounts/*.auth` files:

```sql
CREATE TABLE encrypted_secrets (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,        -- 'source_auth' | 's3' | 'widevine' | 'totp' | …
  provider TEXT,             -- 'audible' | 'libro' | 'chirp' | 'graphicaudio' | 'local' | null
  account_type TEXT NOT NULL DEFAULT 'integration',  -- 'integration' | 'operator' | 'user'
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
