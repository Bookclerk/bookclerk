# Database plugins

This page explains where Bookclerk keeps its library: how to choose and
configure a database, how to switch, and how secrets are stored in it. The
second half describes how the host and the database plugins split the
work, for contributors.

## How the database works

Bookclerk stores library state (accounts, books, portal, discovery) in a
**database plugin**.

- Exactly one backend is active, unlike destinations, which may fan out.
- Supported backends are **SQL-like only**: SQLite, PostgreSQL and
  Cloudflare D1. Document and key-value stores are out of scope and must
  fail capability negotiation (see
  [ADR: SQL database contract](../adr/sql-database-contract.md)).

| Backend | When to use it |
| --- | --- |
| SQLite (`sqlite`, the default) | One server. A local file, `$BOOKCLERK_FILES_DIR/library.db` |
| PostgreSQL (`postgres`) | A shared database, for example for [clerks](cluster-hosts.md#clerks) |
| Cloudflare D1 (`d1`) | Remote SQLite through the Cloudflare HTTP API |

Which backend to open is **bootstrap** configuration: `[database]` and the
database environment variables. Once the connection is up, cluster
identity, host registration and the `[events]` configuration domain live
in this database. See [ADR: Control plane](../adr/control-plane.md).

> [!NOTE]
> SQLite is the single-host database, not a shared-disk cluster.

## Configure the database

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

The Cloudflare API token comes from the environment only
(`BOOKCLERK_D1_API_TOKEN`, falling back to `CLOUDFLARE_API_TOKEN`). There
is no credentials file.

These environment variables override the file:

| Variable | Role |
| --- | --- |
| `BOOKCLERK_DATABASE_PLUGIN` | `sqlite`, `d1` or `postgres` |
| `BOOKCLERK_DATABASE_SQLITE_PATH` | SQLite path override |
| `BOOKCLERK_D1_ACCOUNT_ID` | Cloudflare account id |
| `BOOKCLERK_D1_DATABASE_ID` | D1 database UUID |
| `BOOKCLERK_D1_API_TOKEN` / `CLOUDFLARE_API_TOKEN` | D1 API token |
| `BOOKCLERK_D1_API_BASE` | API base URL override |
| `BOOKCLERK_DATABASE_POSTGRES_URL` | Postgres connection URL (registered as a secret) |
| `BOOKCLERK_DATABASE_POSTGRES_URL_FILE` | Path to a file containing the Postgres URL |

## The database plugins

The first-party guests are:

- `bookclerk-plugin-database-sqlite` (platform);
- `bookclerk-plugin-database-d1` and `bookclerk-plugin-database-postgres`
  (optional).

Each is a full Workers-RPC guest with its own binary and `plugin.toml`.

To install them:

1. Install platform sqlite under
   `$BOOKCLERK_FILES_DIR/plugins/<plugin-key-fs-id>/`. It is verified
   against `$BOOKCLERK_FILES_DIR/install-ledger.json`.
2. Stage the optional database guests with
   `cargo stage-plugins --optional`.

> [!IMPORTANT]
> Hosts must install or stage the active database guest. A missing guest
> is a hard error: there is no in-process engine fallback.

### Local SQLite

Local SQLite is a **platform-shipped guest**, under
`plugins/<plugin-key-fs-id>/` for the platform sqlite adapter. Occupancy
defaults to `[database].plugin = "sqlite"` until stamped to a PluginKey.

- The host grants `library.db` (and its journal sidecars) in the jail
  allowlist, and passes the path as `BOOKCLERK_SQLITE_PATH` and context
  `sqlitePath`.
- The guest runs with `[sandbox].network = none`.

### D1 and PostgreSQL

External `databaseAdapter` guests also load for **D1** and **Postgres**
when they are discovered and `[database].plugin` matches the plugin id.

- SeaORM proxy calls (`db.query` and `db.execute`) forward through the
  guest. `master.key` never leaves the host.
- First-party ids (`sqlite`, `d1`, `postgres`) receive host-private
  connect parameters, with paths and secrets injected by the host.
- Any other database plugin id receives the public
  `DatabaseAdapterConfig` payload: `pluginDataDir` plus its granted
  `[database.<id>]` settings.

## PostgreSQL

`plugin = "postgres"` connects to a PostgreSQL **16+** server, using the
sqlx-postgres driver inside SeaORM. Connecting fails closed when
`server_version_num < 160000` or when `server_encoding` isn't UTF8.

The connection URL is standard libpq style:
`postgres://user:password@host:5432/dbname`. At least one of `url` or
`url_file` is required:

```toml
[database]
plugin = "postgres"

[database.postgres]
# Option A: inline URL (registered as secret for log redaction)
url = "postgres://bookclerk:secret@db.example.com/bookclerk"

# Option B: file containing the URL (preferred for production)
url_file = "/run/secrets/postgres_url"
```

Or with the environment:

```sh
BOOKCLERK_DATABASE_PLUGIN=postgres
BOOKCLERK_DATABASE_POSTGRES_URL=postgres://user:pass@host/db
# Or:
BOOKCLERK_DATABASE_POSTGRES_URL_FILE=/run/secrets/postgres_url
```

> [!TIP]
> Prefer `url_file` in production, so the password isn't in
> `config.toml`.

### How the Postgres guest reaches the server

The guest is native-behind-workerd.

- The sibling `NetPolicy::Deny` jail blocks ambient TCP. sqlx talks to a
  guest-local Unix socket that the SDK splices through the inherited
  CONNECT mux (Linux and macOS).
- At spawn, the host overlays TCP plus the loopback and private CIDRs
  implied by `[database.postgres].url` onto the guest selected by
  `[database].plugin` (a canonical PluginKey, or an unambiguous kind
  token such as `postgres` or `pg`). The overlay isn't persisted as
  invented structural authority.
- A second install that reuses `id = "postgres"` doesn't receive the
  operator URL when `[database].plugin` names the other PluginKey.
- On Windows the guest uses the same sibling Deny plus the inherited
  `handle:` mux for SDK HTTP. Postgres Unix-socket mediation is Unix-only,
  so when `BOOKCLERK_SOCKET_PROXY` is set on Windows the guest fails closed
  rather than dialing ambient TCP (sqlx has no Unix-socket rewrite there).

## Cloudflare D1

D1 is SQLite-compatible but reached over HTTP, so latency is higher than
a local `library.db`. Each HTTP request is its own connection.

> [!NOTE]
> Cloudflare Time Travel isn't used. It is a **database-wide restore**,
> not a per-request rollback: it can't exclude other writers, and a crash
> before restore leaves partial writes committed. Back up with Bookclerk
> [recovery points](backups.md) instead.

How atomic operations work on D1 is described in
[Atomic operations](#atomic-operations) below.

## Switch backends

Changing `[database].plugin` does **not** copy library data on its own.
After editing the config (or running `bookclerk plugins deploy <id>`):

1. Preview the row counts:

   ```sh
   bookclerk config database migrate --from sqlite --to postgres --dry-run
   ```

2. Copy the library rows and update `config.toml`:

   ```sh
   bookclerk config database migrate --from sqlite --to postgres --apply
   ```

Daemon operators can instead `POST /api/database/migrate` with JSON
`{ "from": "sqlite", "to": "postgres", "dry_run": false, "apply": true }`.
When `apply` is true, the daemon writes `[database].plugin` and **reopens
the library connection without restarting**.

A config reload (`POST /api/config/reload` or SIGHUP) also switches
backends live when `[database].plugin` changes.

## Plugin databases

Plugins that declare `[[databases]]` bindings (`binding = "DB"` and so on)
get Workers-style **named database bindings**: one isolated database per
binding.

- The active adapter provisions each one: a SQLite file, a PostgreSQL
  **database**, or a Cloudflare D1 database by name. It is recorded in the
  host's `plugin_databases` registry.
- These SQL bindings don't replace `[[kv_namespaces]]`.
- Bindings are consented per name (`database:<NAME>` grant entries).
- Jobs never receive the host library as guest SQL.

To list or remove them:

```sh
bookclerk plugins db list
bookclerk plugins db drop <plugin> [binding]
```

`drop` deletes the database itself (the SQLite file, `DROP DATABASE`, or
the D1 delete), then the registry row. Unknown adapters fail closed.

For plugin authors, see
[ABI → Isolated plugin database bindings](../plugins/abi.md#isolated-plugin-database-bindings).

## Encrypted secrets

Store credentials and other runtime secrets are stored in the
`encrypted_secrets` table, not in files:

- store credentials for Audible, Libro.fm, Chirp and GraphicAudio;
- Widevine CDMs;
- S3 destination keys, which are `secret` settings of the `s3` plugin
  instance. With none set, the guest falls back to the AWS SDK default
  provider chain.

> [!NOTE]
> Bookclerk no longer creates or reads an `Accounts/` directory.

### The master key

All new writes use **`sealed-v1`**: XChaCha20-Poly1305 with a process-wide
**Data Encryption Key (DEK)** and a random 24-byte nonce. There is **no
per-row key derivation** (Argon2) for new rows.

The DEK is stored in `$BOOKCLERK_FILES_DIR/master.key` and loaded at
startup by `bookclerk_library::configure_master_key`. The file has one of
two formats:

| Header | Contents |
| --- | --- |
| `BCK1` | The raw 32-byte DEK, unprotected. For development and testing only |
| `BCK2` | The DEK wrapped with `BOOKCLERK_AUTH_PASSWORD`, through Argon2id and XChaCha20-Poly1305 |

> [!IMPORTANT]
> Set `BOOKCLERK_AUTH_PASSWORD` (or `[auth].password`) in production, so
> `master.key` is wrapped at rest.

When a password is set and `master.key` holds a raw `BCK1` key, Bookclerk
re-wraps it as `BCK2`. That happens at startup, on
`bookclerk config master-key wrap` or `config set auth.password`, or on a
daemon reload (SIGHUP or `POST /api/config/reload`).

**Fail-closed:** if a record exists but can't be unsealed (a wrong master
key, a corrupt nonce, or a missing DEK), Bookclerk returns an error rather
than falling through to an unencrypted fallback.

### Bootstrap secrets stay outside the database

These are needed to open the database or unwrap the master key, so they
can't be stored in it:

- `BOOKCLERK_AUTH_PASSWORD` or `[auth].password`, which wraps `master.key`
  at rest (the environment is preferred);
- `BOOKCLERK_DATABASE_POSTGRES_URL` and `BOOKCLERK_D1_API_TOKEN` (or
  `CLOUDFLARE_API_TOKEN`), the database connection;
- `BOOKCLERK_OPERATOR_TOKEN`, an optional override for the durable
  operator API token (otherwise it is sealed in `encrypted_secrets`; see
  `bookclerk daemon token`);
- `config.toml` itself, which stays on disk.

## How it works (for contributors)

The rest of this page is for people changing Bookclerk's database code.

### Who owns what

| Crate | Owns |
| --- | --- |
| [`bookclerk-library`](../../crates/bookclerk-library) | Greenfield DDL ([`migrations`](../../crates/bookclerk-library/src/migrations.rs)), SeaORM entities, domain invariants, host-owned atomic SQL plans, schema application after connect, [`LibraryStore`](../../crates/bookclerk-library) CRUD (`from_connection` only) |
| `bookclerk-plugin-database-{sqlite,d1,postgres}` | Connection and transport, capability advertisement, bind encoding, generic query, execute and atomic batch, error and timing normalization. **Not** Bookclerk table names, schema version selection, or named domain operations |
| Host (`bookclerk-plugin-host`) | Spawns the guest, mediates secrets into tagged `DbConnectParams`, negotiates capabilities, applies remaining DDL, compiles plans, validates result envelopes, interprets statement results, forwards SeaORM through the RPC proxy |

Core stays database-agnostic. It sees a migrated `DatabaseConnection`, and
the host always attaches
[`AtomicTxnBackend`](../../crates/bookclerk-library) (a generic atomic
plan on the guest, and host `interpret_plan` on the result) for atomic
security, job and event operations. Domain names stay in host code.

The guest returns a bootstrap `{ "engine" }` for diagnostics only. The
host always builds the SeaORM RPC proxy as SQLite-shaped canonical SQL
(`?` placeholders), and never admits or rejects a guest based on its
physical engine.

### Why SeaORM

Bookclerk used to use hand-written `rusqlite` SQL in `bookclerk-library`.
To keep schema and queries maintainable the way a TypeScript app uses
Prisma, these Rust options were compared:

| Option | What it is | Prisma-like DX | SQLite | Cloudflare D1 | Fit with Bookclerk |
| --- | --- | --- | --- | --- | --- |
| **SeaORM** | Async ActiveRecord-style ORM (entities, `ActiveModel`, relations, migrations) | Closest maintained match (TypeORM/Prisma ergonomics) | `sqlx-sqlite` | First-class `ProxyDatabaseTrait` (HTTP / Workers) | **Chosen** |
| **Diesel** | Sync compile-time query DSL | Strong types; steeper DSL; less “entity CRUD” feel | Excellent | `diesel-d1` is experimental, WASM Workers-only; HTTP API not ready | Strong for SQLite-only; weak for D1 from `bookclerk`/`bookclerkd` |
| **SQLx** | Compile-checked SQL, not an ORM | Far from Prisma | Yes | No first-class driver | Good toolkit, not the maintainability target |
| **prisma-client-rust** | Generated Prisma client for Rust | Most Prisma-like | Yes | Via Prisma ecosystem | **Unmaintained**, rejected |
| **Stay on rusqlite** | Thin SQLite wrapper | None | Native | Would be bespoke HTTP | Status quo; no ORM win |

SeaORM 2 was chosen because:

- its entity and `ActiveModel` workflow is closest to Prisma Client usage;
- the same `DatabaseConnection` serves local SQLite and D1 (through the
  proxy);
- the schema stays SQL-first (one greenfield version-1 pack) while queries
  use typed entities.

**Why not Diesel?** Diesel's compile-time guarantees are excellent, and
the old `LibraryStore` was sync-friendly. D1 is the deal-breaker for an
in-process daemon and CLI:

- The official path to remote D1 from native binaries is the Cloudflare
  **HTTP** query API.
- SeaORM already supports custom backends through
  `Database::connect_proxy`.
- Diesel custom backends need
  `i-implement-a-third-party-backend-and-opt-into-breaking-changes` and a
  full `Backend`/`Connection` stack, and `diesel-d1` today is Workers WASM
  plus binding only.

### One SQLite link

SeaORM's `sqlx-sqlite` driver needs `libsqlite3-sys` `<0.38`. Upstream
`audible-rs` pins `rusqlite` 0.40 (`libsqlite3-sys` `^0.38`). Cargo can't
link two `links = "sqlite3"` crates in one binary, so Bookclerk:

- Pins workspace `rusqlite` to **0.39**, so it shares `libsqlite3-sys`
  0.37 with a single SQLite link. Host schema apply no longer uses
  `rusqlite_migration`.
- Vendors `audible-rs` under
  [`third_party/audible-rs`](../../third_party/audible-rs) (see
  `BOOKCLERK_PATCH.md` there). The plugin uses audible-rs as a library
  (`default-features = false`). Its optional `cli` feature still pins
  `rusqlite` 0.39, so a CLI rebuild would share the same SQLite link.
- Uses SeaORM's **`proxy`** backend for both local SQLite (a rusqlite
  wrapper) and D1 (HTTP).

`sqlx-postgres` and `runtime-tokio-rustls` are enabled on the `sea-orm`
workspace dependency. `sqlx-sqlite` is deliberately left out, because
SeaORM 2.0's driver currently fails to compile against `sea-query` 1.0.x
(the `Value` payload boxing change).

### LibraryStore

The operator-facing [`LibraryStore`](../../crates/bookclerk-library) is
**SeaORM-backed and async**: it holds a `DatabaseConnection`, and every
method is an `async fn` returning `Result<…>`.

- Production opens go through `bookclerk_plugin_host::open_library_store`
  (an external guest is required).
- Tests may use `bookclerk_plugin_database_sqlite::open_memory` or
  `open_store_memory` (a dev-dependency on the plugin crate), then
  `LibraryStore::from_connection` when needed.

CRUD runs on typed **SeaORM entities** (see
[`crate::entities`](../../crates/bookclerk-library/src/entities)): one
`DeriveEntityModel` per table (`accounts`, `books`, `works`,
`title_requests`, `title_request_sources`, `encrypted_secrets`, …).

- Reads use `Entity::find()` with `QueryFilter` and `QueryOrder`.
- Writes use `ActiveModel` insert and update.
- Upserts that used to rely on `ON CONFLICT … COALESCE(…)` are
  load-then-merge in Rust, so the same behavior holds on every backend.
- Entity integer columns are `i64`, reals `f64`, blobs `Vec<u8>`, and
  text (including RFC 3339 timestamps) `String`.

The SQLite proxy in the database plugin returns **typed** SQL `NULL`s, so
SeaORM `Option<T>` decoding works. It reads each column's declared type
(rusqlite `decl_type`) and emits the matching `Value::*(None)`. D1 (JSON,
with no type metadata) falls back to a guess from the column name.

### Insert ids

The typed execute ABI has no last-insert-id, because D1 and Postgres can't
report a portable one. The proxy's SQLite backend also doesn't use SeaORM
`RETURNING`: `ActiveModel::update` would then emit an unbounded
`UPDATE … RETURNING`, which D1 refuses.

Instead, `db.execute` sends a single-tuple `INSERT … VALUES (…)` into a
table with an `INTEGER PRIMARY KEY AUTOINCREMENT` column as
`… RETURNING <identity>`, with host-proven `maxRows = 1`. It reports the
returned key as `last_insert_id`. So `ActiveModel::insert` and
`Entity::insert(..).exec` see the real key on every engine. Other writes,
including `ON CONFLICT` upserts, report `last_insert_id = 0`.

### Atomic operations

D1's HTTP API can't keep a classic interactive `BEGIN` open across RPCs.
So the D1 guest leaves the host-private interactive-transaction interface
unsupported: `begin` fails, and there is no `commit` or `rollback`.
Ordinary autocommit statements use the documented REST body
`{ "sql", "params" }`.

Atomic library operations use the typed atomic `execute` on every bundled
backend. They include claim redeem, last-owner demote, disable and delete,
password rotation, TOTP enroll and disable, consume-once OIDC RP state and
WebAuthn challenges, job admit and claim, and event publish, dispatch and
claim.

1. The **host** compiles a bounded generic SQL plan: statements, binds,
   outcome selectors and receipt wrapping.
2. The D1 plugin runs that plan as **one** `{ "batch": [...] }` REST
   request (a real SQL transaction). SQLite and PostgreSQL guests run the
   same plan in a native local transaction.
3. Every guest returns the same generic batch result: per-statement
   `rows` and `rowsAffected`.
4. The host interprets receipts and application status (`ok`, `empty`,
   `lastOwner`, …). Guests don't parse Bookclerk operation names or table
   names.

Each call carries an `operationId` and a `requestHash`. Host-authored SQL
writes a durable receipt in the same transaction, so a committed result
whose HTTP or RPC response is lost can be retried without a second
mutation.

Structured statuses include `ok`, `empty`, `lastOwner`, `claimInvalid`,
`passwordRequired`, `notFound` and `idempotencyConflict`. Consume-once
operations use `DELETE … RETURNING`, so a missing or expired row can't be
observed twice.

### Capabilities and limits

After `openSession` the host calls the typed
`AdapterDatabaseSession.capabilities`, and stores the negotiated
`DbCapabilities`.

- Host schema markers use `schemaMigrations` (required). `timing` isn't
  host policy.
- D1 reports `maxBinds: 100` and doesn't advertise backup flags.
- The host rejects plans that exceed `maxStatements`, per-statement
  `maxBinds` or `maxPayloadBytes`, or that have out-of-range selectors.
- A statement that yields more than `maxResultRows` **fails the plan**.
  Nothing is silently truncated.
- Guests also advertise `maxResultBytes`, `maxCellBytes`,
  `maxRequestBytes` and `maxAtomicResultBytes`. `0` means unspecified and
  fails closed. Batch request and result caps are at or below the scalar
  limit.
- Guests that omit `returning`, or advertise `0` row, payload or atomic
  caps, aren't loaded.

D1 refuses `RETURNING` unless host-IR `maxRows` is `1`, and rejects
multi-tuple `VALUES` and semicolon-joined SQL, before HTTP. An overflow or
an oversized body after the HTTP commit is `unavailable`.

### Errors, cancellation and deadlines

Guest failures are classified from SQLSTATE and `SQLITE_*` codes, not by
matching English words such as `"unique"`:

| Cause | Error |
| --- | --- |
| Constraint | `conflict` |
| Busy or serialization | `unavailable` (the same `operationId` is retried) |
| Timeout | `deadline_exceeded` |
| Syntax | `invalid_params` |

Cancel and deadlines stay at the RPC and session level.
`deadlineUnixMs` on the atomic request is transport metadata and isn't
hashed.

- A cancel or deadline seen before `BEGIN` or HTTP, or between
  statements, is `cancelled` or `deadline_exceeded`.
- One seen around `COMMIT` or the HTTP return is `unavailable`, because
  the outcome is ambiguous.

### Schema versions

Schema state is **host-owned**. Guests connect and ping only. After
`db.connect` and capability negotiation the host reads the schema state
and sends the remaining DDL as generic execute. On D1, the unreleased
apply and each future frozen step is one host-compiled
`{ "batch": [...] }`.

There is no production freeze yet:

- [`migrations.rs`](../../crates/bookclerk-library/src/migrations.rs)
  exposes `current_canonical_schema()` as frozen ups plus
  `unreleased_ops`. Today the frozen plan is empty
  (`host_migration_plan()` is empty), so that helper equals
  `unreleased_sql()`. Don't treat that equality as permanent.
- Postgres receives the same pack, lowered mechanically
  ([`schema_postgres.rs`](../../crates/bookclerk-db-exec/src/schema_postgres.rs))
  to `BIGINT` identity and `BYTEA` at execute, never `BIGSERIAL`.
- There is no live incremental chain. Land new DDL as already-separated
  `MigrationOp`s in `unreleased_ops` until a **release cut** copies them
  into a `HostMigrationStep`. See
  [ADR: Schema versioning](../adr/schema-versioning.md).
- SQL-v1 (`SQL_CONTRACT_VERSION = 1`) is the SQL grammar and ABI, not a
  library schema freeze.

Opening the library compares the explicit
[`SchemaState`](../../crates/bookclerk-library/src/schema_state.rs).
`SCHEMA_VERSION = 0` means there are **no frozen schema revisions**, not
that a database is "at schema zero".

| State found | What happens |
| --- | --- |
| Uninitialized | Apply the current canonical schema (frozen ups plus unreleased) and persist `Unreleased { base_version: SCHEMA_VERSION, checksum }` |
| Matching `Unreleased { base_version, checksum }` | Nothing |
| Mismatched checksum, frozen base or malformed markers | Fail closed (`cargo reset --yes`) |
| `Frozen` | Verify checksums, apply remaining frozen steps, then the current unreleased bucket if any |
| Frozen newer than this binary | Fail closed, never auto-downgrade. Restore a backup |

Unreleased state records its frozen base (`unreleased@base0+…` today).

### One migrator at a time

Only one starter migrates a shared database.

- A start that finds the schema already matching just checks it.
- One with DDL to apply creates `cluster_leases` and takes the
  `schema.migrate` lease. The lease lasts 120 s, is renewed while it
  works, and is checked before every DDL batch.
- Other starters wait for the schema to match, take the lease over if the
  migrator dies, and fail closed after 300 s.

The lease row keeps the last migrator's holder and token.
`GET /api/cluster/status` shows it with the schema state
(`cluster.schema`).

`bookclerk db version|backup|restore|migrate` inspects the state and walks
frozen revisions without applying on connect. A `migrate` walk that writes
(a backup or DDL) takes the same `schema.migrate` lease, with the same
wait, renewal and per-batch check.

- The version shows as `uninitialized`, `unreleased@base<n>+<checksum>`
  or `frozen@<version>+<checksum>`.
- With an empty frozen plan, `migrate --to` does nothing, and restore is
  time travel.

### Recovery point format

A Bookclerk **recovery point** is one complete logical database state.
Portable backups are canonical Bookclerk recovery points, not D1 REST
export, VACUUM or `pg_dump`. For the commands, see
[Backups](backups.md).

The backup **repository** may reuse immutable canonical objects from
earlier recovery points: logically full, physically incremental. Restore
never replays older manifests. The layout under
`$BOOKCLERK_FILES_DIR/backups/` is:

```text
manifests/<recovery-point-id>.json
manifests/<recovery-point-id>.sha256
objects/ab/cdef…   # SHA-256 of uncompressed canonical JSON
```

- Objects are gzip-compressed on disk (`flate2`). The content address is
  the SHA-256 of the uncompressed canonical JSON, so identical logical
  content keeps the same identity whatever the compression settings.
- Table data is chunked at a 256 KiB uncompressed-JSON target (a 128–512
  KiB band). That is large enough to avoid heavy per-object overhead, and
  small enough for a small VPS's working set and for `maxResultRows`
  paging.

**Capture** streams `ORDER BY` pages (`LIMIT`/`OFFSET`) inside one
consistent read view (a SQLite transaction, or PostgreSQL
`REPEATABLE READ`). Row order is the primary key, else a unique key, else
a full-row sort of declared columns. It is never physical, heap or
`rowid` order.

**Restore** verifies every referenced object (schema admission, typed
`DbValue` cells, completeness) **before** the first `DROP`. It executes
only admitted canonical DDL, then adapter lowering.

A recovery point captured through one supported SQL adapter restores
through another compatible adapter. Native snapshots, PITR, Time Travel
and D1 export don't define the portable format. First-party D1 does
**not** advertise `consistentBackupRead` or `atomicUnitRestore`:
sequential HTTP isn't a consistent image, and isn't a complete per-unit
replacement.

### Inside a plugin database

Inside a binding, host bookkeeping is always `bookclerk_`-prefixed (such
as `bookclerk_receipts`, for retry-token replay), and that prefix is the
only reserved name space. A plugin may create its own `schema_migrations`.
Plugins get full DML on ordinary execute, but durable `CREATE` and `DROP`
only through the host-controlled `databaseMigrations` sequence, registered
at startup.

### Plugin databases in backups

Plugin databases are enumerated from the `plugin_databases` registry when
`--include-plugin-databases` is set. Every registered binding must be
captured, or the backup fails.

- Bindings are opened through the **active adapter session**, not a
  sqlite, postgres or d1 switch.
- Logical identity is `(plugin_id, binding)`. A source `unit_ref` is
  never the portable restore target.
- A library-only restore **preserves** the target registry and leaves
  physical plugin databases untouched.
- An included restore **rebuilds and rebinds** registry rows to the
  target adapter's placement.
- Plugin schema migration stays plugin-owned. Restore writes the captured
  rows (including a plugin's own version marker) and doesn't run plugin
  migrations.

Each logical database unit has complete replacement semantics.

> [!WARNING]
> A multi-database bundle isn't transactionally atomic across independent
> databases. A later unit's failure can leave earlier units restored.

Adapters without `consistentBackupRead` or `atomicUnitRestore` fail
closed. Manual backups are never pruned automatically. Automatic
pre-migrate recovery points keep the last five, then reachability garbage
collection deletes unreferenced objects.

### Tables

Base statements use `CREATE TABLE/INDEX IF NOT EXISTS`. The tables are
`accounts`, `books`, `title_files`, `ignored_titles`, `saved_filters`,
`users`, `portal_identities`, `claim_tickets`, `portal_sessions`,
`operator_sessions`, `account_links`, `works`, `work_editions`,
`listening_progress`, `title_requests`, `title_request_sources`,
`embeddings`, `user_preferences`, `encrypted_secrets`, `jobs`,
`job_temp_paths`, `domain_events`, `event_deliveries`,
`event_subscriber_nodes`, `event_outbox_stats`, `bookclerk_slots`,
`plugin_databases` and `bookclerk_schema_migrations`.

- The `jobs` table is the durable daemon queue (see
  [Jobs](../internals/jobs.md)).
- Domain events use a durable outbox with fenced deliveries, per-node
  catalogs, and portable `bookclerk_slots` (no PostgreSQL advisory
  locks).
- Isolated plugin binding databases are registered in `plugin_databases`.
  Plugins own their own DDL inside those units.

Event wake-ups work like this:

- Wake registration (`wake_event_type`, `wake_filter_json`,
  `wake_grants_json`) is cleared when a matching event is accepted, so a
  retry isn't woken again.
- The wake page size is derived from the guest's negotiated `maxBinds`
  (100 on D1), minus the fenced UPDATE's fixed binds.
- Claim uses this node's catalog (type, schema and filter) in **host**
  code. The atomic mutation is a compare-and-set on a concrete delivery
  id. Cluster dispatch still unions live nodes.

### The encrypted_secrets table

The `encrypted_secrets` table is part of the greenfield schema, and
replaced the old file-based `Accounts/*.auth` files:

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

`account_type` keeps **operator-owned** destination secrets apart from
**integration** (store account) secrets:

- `integration`: store or portal account credentials (Audible, Libro.fm,
  Chirp, GraphicAudio, Widevine CDMs). `delete_secrets_for_account`
  purges these when a store account is revoked. They use an `account_id`
  matching the store account stem.
- `operator`: destination and control-plane secrets (S3 keys). These
  outlive any store account, and `delete_secrets_for_account` **never**
  touches them. S3 credentials are stored with `account_id = "default"`
  and `name = "default"`.

Older formats are still read, but never written:

- `json-encrypted`: a key derived with Argon2id from
  `BOOKCLERK_AUTH_PASSWORD`. Still readable, for migration.
- `audible-rs-auth`: raw audible-rs envelope bytes. The outer seal is
  applied on write-back.
- `json` plaintext is rejected on read. Migrate it by saving again with a
  master key configured.

### SecretStore API

```rust
use bookclerk_library::{
    SecretStore, EncryptedSecretRecord, secret_kind, secret_account_type,
    build_sealed_record, unseal_secret, upsert_secret,
    configure_master_key, require_master_key,
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

## Next steps

- [Backups, updates, import and export](backups.md): recovery points and
  moving between backends.
- [Configuration](configuration.md#bootstrap-files): the other bootstrap
  files.
- [ADR: SQL database contract](../adr/sql-database-contract.md) and the
  [SQL-v1 contract](../sql-contract/v1.md): the SQL every adapter accepts.
- [Libraries and branches](cluster-hosts.md): sharing one database between
  clerks.
