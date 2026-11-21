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
| PostgreSQL (`postgres`) | A shared database, for example for [shared branches](branches.md#shared-branches) |
| Cloudflare D1 (`d1`) | Remote SQLite through the Cloudflare HTTP API |

Which backend to open is **bootstrap** configuration: `[database]` and the
database environment variables. Once the connection is up, library
identity, host registration and the `[events]` configuration domain live
in this database. See [ADR: Control plane](../adr/control-plane.md).

> [!NOTE]
> SQLite is the single-host database, not a shared-disk library.

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

The copy moves the whole library, not only the catalog. The destination
opens as the same library, at the same configuration revisions:

- the library identity, every configuration document (`core.output`,
  `core.events`, `core.cache`, the patron scope default, sign-in policy
  and plugin settings) with its revision, audit and change rows;
- plugin versions, stored plugin archives, instances, default instances,
  deployments and placements;
- nodes and their credentials, enrollment links, links to other libraries
  and the books they share;
- each person's playback progress, listening sessions and bookmarks;
- queued jobs and events, and the receipts that make retried operations
  safe.

Nothing is seeded again from `config.toml`. If a host already opened the
new backend (a reload after editing `[database].plugin` does), the
identity, node row and configuration documents it wrote there are
replaced by the source's. Any other row in the destination makes the copy
refuse unless you pass `--force`.

The copy leaves behind only
what Bookclerk rebuilds or rewrites on its own: Discover shelves and
caches, the library page index, in-progress storage scans, maintenance
leases, node heartbeats, live subscriber registrations and observed
deployment state.

The copy writes every row in one transaction, so the destination must
accept one. The first-party SQLite and PostgreSQL adapters do; the D1
adapter does not, so D1 cannot be a migration destination yet. A
destination that refuses is checked before anything is written and is
left untouched.

> [!NOTE]
> Plugin database bindings are not copied. Each one is a separate
> database that belongs to the old adapter; the new adapter provisions an
> empty one when the plugin next runs. Before you switch, list the old
> ones with `bookclerk plugins db list` and drop any you no longer need
> with `bookclerk plugins db drop <plugin> [binding]` (leave out `binding`
> to drop every binding of that plugin).

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
- R2 destination keys and its queue API token, which are `secret`
  settings of the `r2` plugin instance.

> [!NOTE]
> Bookclerk no longer creates or reads an `Accounts/` directory.

### The master key

All new writes use **`sealed-v1`**: XChaCha20-Poly1305 with a process-wide
**Data Encryption Key (DEK)** and a random 24-byte nonce. There is **no
per-row key derivation** (Argon2) for new rows.

The DEK is loaded at startup by `bookclerk_library::configure_master_key`
from `$BOOKCLERK_FILES_DIR/master.key`, or from the OS keychain item that
file names. `master.key` has one of three formats:

| Header | Contents |
| --- | --- |
| `BCK1` | The raw 32-byte DEK, unprotected. For development and testing only |
| `BCK2` | The DEK wrapped with `BOOKCLERK_AUTH_PASSWORD`, through Argon2id and XChaCha20-Poly1305 |
| `BCK3` | No key: a pointer to the OS keychain item that holds the `BCK1` or `BCK2` envelope, and the DEK's SHA-256 fingerprint |

> [!IMPORTANT]
> Set `BOOKCLERK_AUTH_PASSWORD` (or `[auth].password`) in production, so
> `master.key` is wrapped at rest.

When a password is set and the key is a raw `BCK1` (in `master.key` or in
its keychain item), Bookclerk re-wraps it as `BCK2` in the same place.
That happens at startup, on `bookclerk config master-key wrap` or `config set auth.password`, or on a
daemon reload (SIGHUP or `POST /api/config/reload`).

**Fail-closed:** if a record exists but can't be unsealed (a wrong master
key, a corrupt nonce, or a missing DEK), Bookclerk returns an error rather
than falling through to an unencrypted fallback.

### The key in the OS keychain

`[auth].key_store` chooses where a new key is kept:

| Value | Where | Default for |
| --- | --- | --- |
| `keychain` | macOS Keychain, Windows Credential Manager, or the Secret Service on a Linux desktop | The desktop window |
| `systemd-creds` | Sealed with `systemd-creds` (TPM2, or the host key) beside `master.key`, or a unit credential the administrator sealed | Nothing (set it on a Linux server) |
| `file` | `master.key` itself | `bookclerkd` and the CLI |

On a keychain or `systemd-creds` install `master.key` only names the
item (`BCK3` for the keychain, `BCK4` for `systemd-creds`), so a copy of
the files directory no longer carries a usable key. The item holds the
same `BCK1`/`BCK2` envelope, so a password still wraps it when set.

- **Loading follows `master.key`.** A `BCK3` or `BCK4` file reads the item
  and checks the key against the fingerprint. A missing item, a locked
  keychain or a different key is an error; Bookclerk never mints a new key
  in its place.
- **The desktop window moves its key.** On start it moves a key still in
  `master.key` into the keychain, unless `[auth].key_store` is set. If the
  keychain can't take it, the key stays in the file and the window starts.
- **Servers move only when asked:**
  `bookclerk config master-key move keychain` (or `move file` to go back).
  The key itself doesn't change, so sealed secrets stay readable. The
  command also sets `[auth].key_store`. Stop `bookclerkd` first.
- **`systemd-creds` seals on start.** With `key_store = "systemd-creds"`,
  `bookclerkd` moves a key still in `master.key` into a `systemd-creds`
  credential when it starts, and seals a new key there. It always falls
  back: if sealing fails for any reason, the key stays in the file with a
  warning. See
  [Running Bookclerk → Sealing the key with systemd-creds](operations.md#sealing-the-key-with-systemd-creds),
  which also covers sealing it yourself on an older systemd.
- **A set `key_store = "keychain"` is required.** With `keychain`
  configured, a keychain that can't be reached stops a first start
  instead of writing the file.
  A config reload applies a changed `key_store` to the next key placed (a
  shared-branch join); it does not move an existing key.

> [!NOTE]
> A headless Linux server usually has no Secret Service (no session
> D-Bus, nothing to unlock a collection at boot). Use `systemd-creds`
> there, or keep the file wrapped with a password; see
> [Running Bookclerk → systemd](operations.md#systemd).

Back up a keychain or `systemd-creds` key with a recovery copy:

```sh
bookclerk config master-key export --out ./bookclerk-recovery.key
bookclerk config master-key import --from ./bookclerk-recovery.key
```

`export` wraps the copy with a password read from stdin (never argv).
`import` restores it, or a copied `master.key`, on an install that has no
key yet, into that install's store. `bookclerk config master-key status`
shows the store and whether the key is wrapped.

The operator can also save a recovery copy from the web app:

- **The desktop window asks once.** Starting a library in the window
  asks for a recovery password (at least 12 characters) and downloads
  `bookclerk-recovery-key.bck`, wrapped with it. The step can be skipped
  after a warning.
- **Settings reminds.** While the key is in the keychain or sealed with
  `systemd-creds` and no copy is recorded as saved, Settings → Server
  Settings shows a reminder with the same form, for the desktop window and
  for a server with `key_store = "keychain"` or `"systemd-creds"`. An Owner sees the reminder under Account,
  with a pointer to **Elevate to Operator**.
- **The answer is per key.** It is kept in `master-key-recovery.json`
  beside `master.key`, with the key's fingerprint, so a restored or
  replaced key asks again. The file holds no secret.

The routes behind it: `GET /api/config/master-key` (the operator or an
Owner) reads the status; `POST /api/config/master-key/recovery-copy` (the
password goes in the body) and `PUT
/api/config/master-key/recovery-copy/state` are the operator's. See the
[Web API](api/README.md) reference.

### Bootstrap secrets stay outside the database

These are needed to open the database or unwrap the master key, so they
can't be stored in it:

- `BOOKCLERK_AUTH_PASSWORD` or `[auth].password`, which wraps the key at
  rest (the environment is preferred), and `[auth].key_store`, which says
  where new keys go;
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

`sqlx-postgres` and `runtime-tokio` are enabled on the `sea-orm`
workspace dependency. Postgres TLS comes from sqlx's `tls-rustls-aws-lc-rs`
feature, which `bookclerk-db-guest` enables (see
[One TLS backend](../internals/tls.md)). `sqlx-sqlite` is deliberately left out, because
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
- Guests that stream results advertise `maxStreamedResultBytes` and
  `maxStreamedResultRows` (each at least its inline counterpart; `0` on
  both means no streaming). The host reads every standalone library
  batch, such as a catalog page or a Discover feed, through
  `executeStreamed` when the guest streams, so a page of enriched rows
  arrives whole instead of being re-read in smaller pieces. First-party
  sqlite and Postgres stream up to 64 MiB and 250 000 rows; D1 up to
  8 MiB and 50 000 rows. A result past the streamed budget still fails
  closed with `limit_exceeded`. Against a guest that doesn't stream, those
  reads fall back to pieces, halving a piece whose inline result would
  exceed `maxResultBytes` or whose rows would exceed the guest's
  `maxResultRows`.
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
  the outcome is ambiguous. Ambiguity wins over a cap: a reply that may
  have committed stays `unavailable` even when it reports a
  `limit exceeded: …` failure, and the canonical limit text stays in the
  message for the host to read back.
- On D1, a batch that only reads (on a binding, a guest batch whose own
  statements are all reads) has committed nothing it asked for, so a
  result past a cap there is `limit_exceeded`. D1 finalizes the host's
  receipt as a read-only one first; if it cannot, the answer stays
  `unavailable`.

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
- One with DDL to apply creates `library_leases` and takes the
  `schema.migrate` lease. The lease lasts 120 s, is renewed while it
  works, and is checked before every DDL batch.
- Other starters wait for the schema to match, take the lease over if the
  migrator dies, and fail closed after 300 s.

The lease row keeps the last migrator's holder and token.
`GET /api/library-status` shows it with the schema state
(`library.schema`).

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
`accounts`, `books`, `ignored_titles`, `saved_filters`,
`users`, `portal_identities`, `claim_tickets`, `portal_sessions`,
`operator_sessions`, `account_links`, `patron_controls`, `admin_controls`, `patron_scopes`,
`patron_scope_keys`, `account_shares`, `account_credential_revisions`, `listening_progress`, `title_requests`, `title_request_sources`,
`embeddings`, `discover_candidates`, `discover_store_calls`,
`discover_store_call_hits`, `discover_feeds`, `discover_tastes`, `discover_fetches`,
`discover_request_queue`,
`playback_progress`, `playback_sessions`, `bookmarks`,
`player_passwords`, `player_cards`, `player_card_tokens`,
`user_preferences`, `encrypted_secrets`, `jobs`,
`job_temp_paths`, `domain_events`, `event_deliveries`,
`event_subscriber_nodes`, `event_outbox_stats`, `bookclerk_slots`,
`plugin_databases` and `bookclerk_schema_migrations`, plus the library
model tables below.

`patron_controls` holds the member controls an Administrator set for one
Member, as JSON naming only the controls that differ from the library's
default (the `core.member_controls` configuration document); a Member
with no row follows the default for every control (see
[Sharing](../guide/sharing.md#limit-what-a-patron-may-do)).
`admin_controls` holds the Administrator switches an Owner or the
operator set for one Administrator, the same way: JSON naming only the
switches that differ from the library's default (the
`core.admin_controls` configuration document), removed with the user (see
[Sharing](../guide/sharing.md#limit-what-an-administrator-may-do)). `patron_scopes` holds a
Member's own patron scope as JSON; a Member with no row follows the
library's default scope (see [Sharing](../guide/sharing.md#limit-what-a-patron-sees)). `patron_scope_keys`
holds a scope's include and exclude values as rows, keyed by a hash of
the whole value set, so the Library filter joins them with a fixed
number of parameters however many rules a scope has; the rows are
written the first time a scope is used and never change. `account_shares` records each
account a person's join sent to a branch (`sent`, peer `node:<id>`) or
received from a library (`received`, peer `link:<library id>`); it is
removed with the account. `account_credential_revisions` holds, per account
and peer, the newest sign-in revision of a single-keeper account: on a
library the last one applied from its keeper branch, on that branch the
last one sent to each library, with a fingerprint of the sign-in sent
(`observed`). It is removed with the account. When an
account is taken back with no link left, its `node:` rows in both tables
go too, so the library picks its next keeper afresh. Writes that link a
signed-in store account, take back a refused sign-in, or revoke a link
are serialized per account by the `bookclerk_slots` row
`store-account:<plugin key length>:<plugin key>:<account id>`, which is
kept once created.

The library model ([ADR](../adr/library-data-model.md)) stores what the
library holds as works, expressions and editions:

- `works`, `expressions` and `editions`, with `work_identifiers` and
  `edition_identifiers` (a strong identifier is unique through
  `strong_key`); an edition records which metadata provider last filled
  its gaps (`enrich_source`, `enrich_confidence`, `enrich_updated_at`), so
  gap-filling skips it next time
  ([Metadata gap-fill](../internals/discovery.md#metadata-gap-fill-open-library-compliance));
- `store_listings` (one per plugin, marketplace and product, with the
  store's own text for it: synopsis, cover, credits as printed, series and
  the store's own series or show product id, ratings, and `is_explicit`
  when the store marks the edition as explicit or clean; a scan fills it from
  the title it lists, writing only a field that changed; a cover in a
  store's text also becomes the cover of the edition the listing sells
  when that edition has none, so the first store to report one sets it),
  `listing_offers` (its price per audience, `non_member` or `member`) and
  `holdings` (who holds each listing: an account or a branch library, in
  typed columns whose foreign keys drop the holding with its account or
  node; a holding a scan no longer lists, of a revoked account, or of a
  branch that no longer shares the title, is removed with a
  `holding_removed` event; a branch's holding also keeps the title as it
  shared it and whether its files arrived). A
  listing with no holding is a catalog title a store returned to Discover;
  Discover's candidates and its storefront call cache refer to listings
  instead of keeping their own copies
  ([Shared listing pool](../internals/discovery.md#shared-listing-pool));
- `contributors` and `credits`, `series` and `series_entries`;
- `resolution_overrides` (an Administrator's pins and splits; a pin is
  made with `PUT /api/library/listings/{id}/pin`, which moves the listing
  to the edition in the same batch, and a split with
  `POST /api/library/works/{id}/split`, which moves the expressions to a
  new work and isolates it in the same batch, after locking the original
  work's row so two splits of one work cannot both pass their checks on
  Postgres; each write of the row gets a new `revision`) and `resolution_conflicts` (identifier clashes the resolver
  recorded instead of merging, listed by `GET /api/library/conflicts` and
  dropped when the listing is pinned);
- `metadata_claims`, `chapter_lists`, `chapter_list_entries` and
  `edition_chapters`: an edition's chapters as claims, one per source
  (an Administrator's edit, Bookclerk's aligned result, a metadata
  provider, the store's markers or part names), each naming a chapter
  list by SHA-256 digest. A list's chapters are written in batches the
  database plugin accepts and its `chapter_lists` head row last, so a list
  with a head row is complete and a claim on an unfinished list never
  wins. `edition_chapters` holds the list the resolver chose; it is
  written only when no claim changed since the resolver read them (the
  edition's highest claim `revision`), with an `edition_updated` event
  in the same batch (see [Chapters](playback.md#chapters));
- `edition_files`: every key an acquire or a host upload wrote, per
  destination, with the edition it belongs to (a branch's delivered
  title's included), its role (`audio`, `part`,
  `chapter`, `pdf`, `cover`, `sidecar`, `transcript`, `epub`) and, for an
  upload, the file's SHA-256. An audio file's row also holds what a
  probe of the file found: its media type (`content_type`, from the
  container, not the file name), its average bitrate (`bitrate_kbps`),
  whether the encoding is lossless (`lossless`), the store the copy came
  from (`source_store`) and when it was probed (`probed_at`). An acquire's
  probe also stores the encoding (`codec`), the AAC profile
  (`aac_profile`), `channels`, `sample_rate_hz` and the running time a
  player sees (`duration_ms`, empty when unknown, never 0). The acquire
  that writes a file probes it; an upload carries the sender's values and
  they are stored when it commits. `probed_at` is empty for a file that
  was never probed (it is not audio, the media worker was unavailable, or
  it was stored before probes were recorded), and the other four columns
  are then empty too. See [What a stored file's row
  says](media.md#what-a-stored-files-row-says). An audio file an acquire
  writes also names the library copy it belongs to (`copy_format`, such
  as `m4b` or `mp3`, and `copy_quality`, such as `original` or
  `64k-mono`; see [Keeping several
  copies](output.md#keeping-several-copies)); both are empty for
  sidecars, the PDF and uploads.
- `edition_copies`: each edition's copy on each destination: `stored`
  with the edition's primary audio key there, `pending` with the last
  error and when the next background attempt is due, `failed` after 8
  failed attempts in a row, or `missing` when its file disappeared from
  the destination and `output.missing_files` is `mark_missing`. An acquire records each destination's outcome
  when it returns, and a branch's upload records the destinations it
  wrote when it commits. Acquire runs read it to visit an acquired
  edition a destination still lacks (see [Several
  destinations](output.md#several-destinations)).
- `destination_objects`: the last state Bookclerk saw of each file at
  each destination (size, change signature, modification time), with the
  destination's change sequence when it last changed. A removed file
  stays as a tombstone for 30 days so readers can page what changed. It
  is not copied by a database migration; the next listing rebuilds it
  (see [Noticing changes made outside
  Bookclerk](output.md#noticing-changes-made-outside-bookclerk)).
- `destination_feeds`: each destination's change sequence, change feed
  state (`off`, `healthy`, `gap`, `failing`) and cursor, its listing in
  progress and schedule, and the number of files it held. Not copied by
  a database migration either.
- `edition_acquisitions`: each edition's acquire state (status, primary
  storage key, last error) and the account holding (`holding_id`) whose
  title's write set it, written with the title's own status. Only that
  holding changes an acquired edition while it exists, so a sibling
  title's failed attempt never hides its files. A write applies only
  while its holding still holds the edition when it runs, so a status read
  before a revoke or rescan cannot name a holding that is gone. Once the
  acquiring holding is gone, the edition has no owner and any title that
  holds it (reconcile included) may change it. The companion PDF's state
  (`pdf_status`, `pdf_storage_key`) and the holding that set it
  (`pdf_holding_id`) follow the same rule.
- `library_work_rows`: the page index, one row per top-level work that
  has ever been held (title and author sort keys, first series, cover
  edition, media kinds, furthest acquire state, edition and holding
  counts, newest holding time, and the series, runtime and purchase-time
  sort keys); pages skip rows with no holding left. It
  is derived data, refreshed by edition, holding and acquire events (not
  by a credit or series write alone) and rebuilt
  when `library_index_builds` has no row for the current layout; see
  [Library index](../internals/jobs.md#library-index).
- `library_work_facets`: the page index's filter values, one row per
  author, narrator, series, genre, publisher, source or library tag a
  held top-level work has, keyed by the lower-cased value. Written with the work's
  `library_work_rows` row and fenced by its refresh stamp; an unheld work
  has none. A refresh's facets can span several batches, so the row's
  `facets_ready` is 0 until the last of them is written, and a patron
  scope admits only rows at 1.
- `user_title_state`: one user's finished flag, ratings and tags for a
  work, keyed `(user_id, work_id)`. Fields imported for an account reach
  every user linked to that account, and each user edits their own with
  `PUT /api/library/works/{id}/state`. Discover reads its taste from here,
  not from `books`.
- `user_work_tags`: one user's tags on a work, one row per tag, written
  in the same batch as `user_title_state.tags` so the library page can
  filter by a tag.
- `work_tags`: library tags on a top-level work, which everyone sees, one
  row per tag and source (`admin` for tags set in Bookclerk, `sidecar`
  for tags read from a metadata file), keyed by the lower-cased tag.
  Replacing the `admin` tags (`PUT /api/library/works/{id}/tags`) leaves
  the sidecar ones, and refreshes the work's page index row so the
  Library filter (facet `library_tag`) and patron scopes see the change
  at once. A split copies them to the new work.

The library's collections are not a table: they live in the
`core.collections` configuration document (names and storage key
prefixes). A work with a store listing marked explicit
(`store_listings.is_explicit`) carries facet `explicit`, which a patron
scope that hides explicit books excludes. The page index stores each work's collections as facet
`collection`, read from its editions' files (`edition_files`), so a
change to the document drops the finished index build and the next
rebuild recomputes every work.

Per-person rows point at works instead of copying a title:
`listening_progress` (one row per listener and work, with the edition when
the item's identifier named one) and `title_requests` (every wish names its
work and, when known, its edition) keep no title, authors, ASIN or ISBN of
their own; readers take those from the work and the wish's store sources.
A wish for a book the library does not hold creates a catalog work and
edition with no listing or holding.

The built-in player keeps its own rows, each pointing at an edition and
removed with its user (see [Playback reference](playback.md)):
`playback_progress` (one row per user and edition: position, reported
runtime and finished flag), `playback_sessions` (one row per stretch of
listening, kept as history after it closes) and `bookmarks` (places a
user marked). `listening_progress` stays what other servers reported.

Player apps ([Player apps](player-apps.md)) sign in with player cards. A
card is a `portal_sessions` row of kind `player`, so it is listed and
signed out with the patron's other sessions. `player_cards` names the app
and the player password that made the card, `player_card_tokens` holds the
SHA-256 digests of its access and refresh tokens, and `player_passwords`
holds the digests of the passwords patrons made for their apps. Removing a
password removes its cards.

A book is acquired once per edition: the account rows of one edition share
its files, and an acquire of one is marked acquired at the key the
edition's acquiring title recorded (`edition_acquisitions`) when every
destination already holds it. The companion PDF is shared the same way
once it is on the primary destination. Storage matching and reconcile
walk the titles accounts hold and record what they find on the edition,
once per edition; only the acquiring title clears or converts it. A host upload never replaces a file of
its edition that another account's acquire or upload wrote, and an acquire
never writes a key another edition holds: the write is refused before it
reaches storage. A book with
no edition (a listing the resolver refused or has not resolved yet) is not
acquired until a scan resolves it. A host upload names the title by its
account holding, and one that resolves to no edition is refused
(`unknown_title`).

Every stored title is linked into its work, expression, edition,
identifiers, listing and holding in one guarded batch (`linkListing`); a
batch that lost a race answers `retry` and the title is resolved again, up
to three times. Its contributors, credits, series and series entries follow
in separate batches, so a scan that stops between them can leave a linked
title without them; the next scan writes what is missing. A title that still cannot be resolved
is not stored, is logged as a warning, and is resolved again on
the next scan, so one title never stops the rest of a scan. A work takes
its titles' description, subjects, categories and cover where it has none
yet, and an episode stored before its show is linked to the show once the
show has been scanned. A work first stored from a listing that named no
author gains its matching key once a listing names one, so other stores'
listings of the same book can find it. Likewise, an expression first
stored without a language, an abridgement or a narrator gains them from a
later scan that names them, so another store's listing with the same ISBN
can join its edition; values already known are never overwritten.

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
  id. Library dispatch still unions live nodes.

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
  [SQL-v1 contract](../plugins/sql-contract/v1.md): the SQL every adapter accepts.
- [Libraries and branches](branches.md): sharing one database between
  a library and its branches.
