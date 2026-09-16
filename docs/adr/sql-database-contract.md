# ADR: SQL database plugins as thin adapters

- **Status:** Accepted
- **Date:** 2026-08-21
- **Related:** [#178](https://github.com/fritz-fritz/bookclerk/issues/178),
  [#177](https://github.com/fritz-fritz/bookclerk/pull/177),
  [Workers RPC + workerd](plugin-workers-rpc-workerd.md)

## Context

Bookclerk ships three first-party database guests (SQLite, PostgreSQL,
Cloudflare D1). Ordinary CRUD already crosses a generic SeaORM proxy.
Atomic work did not: the wire type was a named
domain enum (`deleteUser`, `publishDomainEvent`, `claimNextJob`, …). D1
recompiled each variant into schema-aware SQL inside the guest; SQLite and
PostgreSQL ran a separate SeaORM implementation of the same invariants.
Event claiming branched on `DatabaseBackend::Postgres` for
`pg_advisory_xact_lock`. Schema changes required coordinated edits in the
host and the D1 planner.

That made D1 a second Bookclerk repository, not a SQL adapter.

## Decision

### SQL-like databases only

Supported backends are SQLite, PostgreSQL, and Cloudflare D1 (SQLite
dialect over HTTP). Document, key-value, and other non-SQL stores are out
of scope. A guest that cannot meet the required semantics must **fail
capability negotiation** rather than silently weaken correctness.

### Ownership

| Layer | Owns |
| --- | --- |
| **Host / `bookclerk-library`** | Schema and migrations; domain operations; SQL and query plans; deduplication; leases and fences; queue/event state machines; result interpretation; idempotency policy |
| **Database plugin** | Connection and transport; negotiated SQL family and limits; bind encoding; generic query / execute / atomic-batch mechanics; error normalization; backend timing; unavoidable engine quirks |
| **ABI** | Generic, bounded execution primitives and capability negotiation. It must not require an adapter to understand users, jobs, books, events, or Bookclerk table names |

Domain names (`publishDomainEvent`, `claimNextJob`, `deleteUser`) stay in
host code. The ABI is an escape hatch for backend mechanics, not a second
repository interface.

### Capability negotiation

After `openSession` the host calls typed `AdapterDatabaseSession.capabilities`.
`DbCapabilities` advertises the SQL contract version,
execution semantics (`atomicBatch`, `returning`, `affectedRows`,
`cancellation`, `timing`), schema versioning (`pragmaUserVersion` /
`schemaMigrations` / `atomicSchemaBatch`), and all numeric limits
(`maxBinds`, `maxStatements`, `maxResultRows`, `maxPayloadBytes`,
`maxResultBytes`, `maxCellBytes`, `maxRequestBytes`,
`maxAtomicResultBytes`, `maxFunctionArgs`, `maxSchemaColumns`,
`maxPatternBytes`, `maxLoweredStatementBytes`). Schema kind is chosen from the schema flags
(exactly one of `pragmaUserVersion` or `schemaMigrations`;
`atomicSchemaBatch` requires `schemaMigrations`). Bootstrap metadata
(`sqlFamily`, SeaORM `dialect`) is **not** on typed `DbCapabilities`;
it travels on the separate typed `DbBootstrap` / the host connect path after
semantic negotiation succeeds. `DbCapabilities` `@17` is `pluginDatabases`
and `@21` is `maxLoweredStatementBytes`.

The host must not invent capabilities from the plugin id. Missing required
fields, `atomicBatch: false`, `returning: false`, unspecified (`0`) limits,
limits below the host's compiled minimums, `maxPayloadBytes` /
`maxRequestBytes` / `maxAtomicResultBytes` above `MAX_SCALAR_BYTES`, or a
bootstrap `dialect` that does not match `sqlFamily` are a hard error. Wake
page size and `IN (…)` chunking are derived from `maxBinds`.
`maxPayloadBytes` bounds request SQL plus binds per statement and must not
exceed the scalar ceiling. `maxLoweredStatementBytes` (`0` = unspecified) is
the adapter's physical statement ceiling after standardized Bookclerk
sqlite-family lowering (overflow wraps, LIKE→GLOB, NULLIF, `INSERT OR IGNORE`,
query LIMIT wrap, bytes-placeholder expansion). After the host has a real
`ResolvedStatement`, and before adapter dispatch, it compares that proven
upper bound to the advertised ceiling. The host must not branch on engine
identity. D1 advertises `100000` and still validates the same bound as
defense in depth. D1 `maxPayloadBytes` remains the largest canonical payload
whose mechanical-only lowering is proven to stay within that 100 KiB
ceiling. `maxRequestBytes` / `maxAtomicResultBytes` bound
the whole encoded `ExecuteRequest` / `ExecuteReply`. Guests track encoded
result bytes incrementally as statement results are built and keep one
exact pre-commit check.

First-party values: D1 `maxBinds = 100`; SQLite and PostgreSQL report the
engine bind cap (host still chunks conservatively).

### Canonical SQL

The host compiler emits **canonical Bookclerk SQL** (`?` placeholders,
SQLite-shaped helpers such as `INSERT OR IGNORE`, `json_extract`,
`json_valid`). The normative grammar, types, helpers, result semantics, and
version policy live in [`docs/sql-contract/v1.md`](../sql-contract/v1.md);
machine-readable vectors are under
`crates/bookclerk-db-exec/testdata/sql_v1/`. Adapter admission is “passes
Bookclerk SQL v1 conformance,” not affinity with SQLite/PostgreSQL identity.

Adapter SDKs lower placeholders and functions at execute time
(`bookclerk-db-exec::lower_canonical_sql`). Optional plan choices may branch
only on semantic capabilities, not on plugin id or `sqlFamily`.
`sqlContractVersion` versions are monotonic supersets; hosts require
`>= SQL_CONTRACT_VERSION`.

### SQL frontend (issue #178)

Canonical SQL text stays the public representation. Parser-library AST never
appears on Cap’n Proto, guest SDKs, or `ExecuteRequest`. A host-only spike
compared Syntaqlite 0.9 (Lemon C + Analyzer) and sqlparser-rs 0.59
(`SQLiteDialect` + Visitor) against the SQL-v1 corpus. Neither deletes a
Bookclerk-owned category of machinery: Syntaqlite `physical_tables_accessed`
omits DML destinations and is SQLite-typed; sqlparser-rs has no semantic
analysis, so `TScan` / TypeCx would remain. Admission stays the fail-closed
recursive-descent grammar. The comparison crate was removed after go/no-go;
independent wins that *did* land: full CREATE schema on `ResolvedStatement`,
SeaQuery for host catalog DML, and proof-directed authorization when a type
env exists.

### Bootstrap metadata isolation

`sqlFamily` and SeaORM `dialect` are bootstrap-only (typed `DbBootstrap` on
the plugin-host connect path). Typed `DbCapabilities` does not carry them
(`@17` is `pluginDatabases`; `@18`–`@21` are numeric caps including
`maxLoweredStatementBytes`). An architecture lint (`scripts/check-db-plugin-isolation.py`)
forbids `bookclerk-library` production sources from reading bootstrap fields
(or defining planner-side `SqlFamily`). SeaORM proxy open maps bootstrap in
`bookclerk-plugin-host` after typed capability negotiation succeeds.

First-party connect wiring (`DbConnectParams::{Sqlite,D1,Postgres}`) injects
host-resolved paths and secrets for `sqlite` / `d1` / `postgres`. That is a
convenience, not the contract: any other `kind = "database"` plugin id receives
`DbConnectParams::Guest { pluginDataDir }` and must read connection settings
from plugin-owned config / secrets bindings, then return bootstrap
`sqlFamily` / `dialect` on connect. Missing or mismatched bootstrap fields
fail closed.

### Generic atomic execute

The data plane is typed `AdapterDatabaseSession.execute(ExecuteRequest) ->
ExecuteReply` (granted job SQL uses `GuestDatabase.execute`). Every request is an ordered non-empty statement list
(batch-of-one for ordinary reads/mutations). Parameters and rows use
Cap'n `DbValue` (`null(expectedType)`, `bool`, `int64`, `float64`, `text`,
`bytes`). Unknown union members fail closed as `unsupported`. Cursor is
result transport, not a second mutation primitive. Host-private interactive
`begin` lives on `HostAdapterDatabaseSession` for the first-party SeaORM
proxy (`plugin_host.capnp`); nested work uses `AdapterTransaction.execute`
on the open txn so it stays on the typed data plane without a second `BEGIN`.

The guest runs the statements as **one SQL transaction** (D1 HTTP
`{ "batch": [...] }`; SQLite/PostgreSQL `BEGIN`) and returns per-statement
rows / `rowsAffected` / timing. The host validates the envelope before
`interpret_plan`. A statement that yields more than `maxResultRows` fails
the plan (rollback / D1 ambiguous) rather than truncating. D1 HTTP cannot
roll back after the JSON body returns, so the guest refuses `RETURNING`
unless the host-IR `maxRows` is `1`, the SQL string is a single statement
(no top-level `;`), and any `VALUES` list is exactly one tuple. Overflow
or an oversized HTTP body after a committed batch is `unavailable` (replay
the same `operationId`); only a definitive non-retryable 4xx is permanent.
Receipt rows live in host-authored SQL against `db_atomic_receipts`.
Guests must not parse Bookclerk operation names or interpret receipts.
`rowsAffected` is uniform by kind: `select` is `0`; `returning` is the
number of returned rows; `execute` is the engine change count.

Stable error categories come from SQLSTATE / rusqlite codes (not English
`"unique"` matching): constraint → `conflict`; serialization/busy/locked →
`unavailable`; timeout → `deadline_exceeded`; COMMIT-time I/O or lost HTTP
→ `unavailable` (retry the same `operationId`); unsupported SQL →
`unsupported`; syntax → `invalid_params`.

Cancellation and deadlines stay RPC/session-level (not a field on the SQL
plan). The host races in-flight typed `execute` against a cancel flag
(drop aborts the RPC). Guests may see an optional `deadlineUnixMs` on
`ExecuteRequest` (transport metadata; not hashed). Observed cancel/deadline
before `BEGIN`/HTTP or between statements is `cancelled` /
`deadline_exceeded`; around `COMMIT` / HTTP return is `unavailable`
(ambiguous).

### Portable concurrency

Domain/store code must not branch on a concrete database type for
correctness. Serialization uses schema-based slot rows
(`db_serialization_slots`): `INSERT` the key, then `UPDATE bump = bump + 1`
to take a write lock. PostgreSQL advisory locks and SQLite-only
`job_queue_control` dual-paths are not used for new atomic work.

JSON filter / catalog matching for event claim is evaluated in the host.
The atomic mutation is a compare-and-set on a concrete delivery id.

### Fail closed

A backend that cannot provide parameterized statements, reliable
affected-row counts, atomic batch or interactive transactions, durable
commit with replay-safe lost-response handling, or the negotiated bind
limits is not loaded.

## Consequences

- First-party database plugins shrink to connect, ping, proxy CRUD, and a
  generic batch executor. The host selects and applies schema versions after
  capability negotiation (generic execute / one atomic batch; D1 schema
  apply is still one host-compiled HTTP batch).
- An architecture lint forbids plugin and `bookclerk-db-guest` production
  sources from importing Bookclerk migrations, embedding application table
  names, or interpreting named operations (`DbAtomicParams`, `atomic_status`,
  `interpret_plan`). The same lint forbids `bookclerk-library` planners and
  domain code from reading bootstrap-only `sqlFamily` / `diagnosticEngine`
  (or reintroducing planner-side `SqlFamily`).
- Equal performance across engines is not guaranteed.
- Integration plugins never receive database credentials or raw
  connections.
- Non-SQL backends remain unsupported by product policy, not by a
  missing adapter.
