# Working within database limits

A plugin's `[[databases]]` binding talks to its database through the host,
and every request and reply crosses the plugin boundary as a bounded
message. This page explains which limits apply, how the SDKs keep ordinary
code under them, and what to do when a result or a batch is larger than one
message.

The SQL contract defines the limits themselves; see
[Advertised numeric limits](sql-contract/v1.md#advertised-numeric-limits) and
[Streamed results](sql-contract/v1.md#streamed-results). This page is about
using them from a plugin.

## The limits a plugin meets

Every database session advertises its caps as `DbCapabilities`, and a
binding's `limits()` returns them. The ones that shape plugin code:

| Limit | What it bounds |
| --- | --- |
| `maxRequestBytes` | One encoded request: the statements, their binds and the retry identity |
| `maxStatements` | Statements in one request (`batch()` or `exec()`) |
| `maxBinds` | Bind parameters in one statement |
| `maxCellBytes` | One TEXT or BYTES cell in a reply |
| `maxResultRows` / `maxResultBytes` | A reply read inline: rows and encoded bytes per statement |
| `maxAtomicResultBytes` | A reply read inline: encoded bytes for the whole reply |
| `maxStreamedResultBytes` / `maxStreamedResultRows` | A reply read as a stream: bytes for the whole reply and rows per statement. `0` on both means the session does not stream |

First-party sqlite and Postgres stream up to 64 MiB and 250 000 rows; D1 up
to 8 MiB and 50 000 rows. The inline caps stay at 256 KiB.

Any cap that is hit fails the whole call with one typed error: a
`PluginError` whose `code` is `limit_exceeded` and whose `limit` carries
`limit` (the capability name), `cap` and `used`. Results are never
truncated, and a batch that fails a cap is not committed.

```ts
try {
  await env.DB.prepare("SELECT * FROM books").all();
} catch (err) {
  if (err instanceof PluginError && err.limit?.limit === "maxStreamedResultBytes") {
    // The result is too large even for the streamed budget: narrow the query.
  }
  throw err;
}
```

In Python the same error is `PluginError` with `err.code == "limit_exceeded"`
and `err.limit.limit`; in Rust it is `PluginError` with
`err.code == PluginErrorCode::LimitExceeded` and `err.limit()`.

## Reads: `all()` streams when the session can

`prepare(sql).all()`, `raw()`, `first()` and `run()` work as they do on D1.
Behind them the SDK fetches the session's limits once per invocation and,
when the session advertises a streamed budget, reads every row-returning
statement through `GuestDatabase.executeStreamed`. The result arrives as a
sequence of frames that the SDK reassembles into the same `D1Result`, so a
page of enriched rows that would not fit one inline reply is one call, one
statement and one snapshot. The code does not change:

```ts
const { results } = await env.DB.prepare("SELECT id, title FROM books").all();
```

On a session that does not stream, or on a host that answers
`executeStreamed` with `unsupported`, the SDK reads inline and the inline
caps apply.

> [!NOTE]
> Streaming changes how a result crosses the boundary, not how much the
> adapter may return. A result past `maxStreamedResultBytes` or
> `maxStreamedResultRows` still fails closed with `limit_exceeded`, and the
> SDK stops decoding as soon as the budget is passed. Statement `maxRows`
> (`as_first()` / `asFirst()`) stays available as a hard row cap.

### Row at a time: `iterate()`

When a plugin walks a large result without needing it all at once, use
`iterate()`. It issues the same single-snapshot streamed read as `all()` but
yields each row as its frame is decoded, so memory stays at one frame
rather than the whole result. There is no key to page by and no `OFFSET`
that skips or repeats rows under concurrent writes.

```ts
for await (const row of env.DB.prepare("SELECT id, path FROM edition_files").iterate()) {
  await check(row.path);
}
```

```python
async for row in self.env.DB.prepare("SELECT id, path FROM edition_files").iterate():
    await check(row["path"]["value"])
```

The loop fails with the same `limit_exceeded` as `all()` when the result
passes the streamed budget. On a session that does not stream the rows come
from one inline `all()`. `iterate()` is available in the TypeScript and
Python SDKs; a Rust guest reads with `all()` or `raw()`.

## Writes: `batch()` is one transaction, `chunk()` splits a list

`batch([...])` and `exec("...")` run their statements as one transaction,
all or nothing, as D1 documents them. One request is bounded by
`maxStatements` and `maxRequestBytes`, so a long list of statements has to
be split into requests that fit. `chunk()` does only that:

```ts
const inserts = rows.map((r) => env.DB.prepare("INSERT INTO seen (id) VALUES (?)").bind(r.id));
for await (const batch of env.DB.chunk(inserts)) {
  await env.DB.batch(batch);
}
```

```python
inserts = [self.env.DB.prepare("INSERT INTO seen (id) VALUES (?)").bind(r["id"]) for r in rows]
async for batch in self.env.DB.chunk(inserts):
    await self.env.DB.batch(batch)
```

```rust
for batch in db.chunk(inserts, None).await? {
    db.batch(batch, None).await?;
}
```

Each batch is atomic; the loop is not. If a later batch fails, the earlier
ones stay committed, and the plugin decides whether to retry or to record
how far it got (a job checkpoint is the usual place). `chunk()` uses the
session's `maxStatements` and `maxRequestBytes` (the binding's own request
cap, when it has one, tightens the latter); pass explicit caps to split
smaller, or when the host predates `limits()`, and explicit zeros to get one
batch. A single statement that is larger than the request cap is yielded on
its own, so `batch()` reports the typed error for that statement instead of
the split hiding it. Batches are sized with the UUID-shaped retry id the
binding mints, so a caller-chosen retry id that is longer leaves that much
less room.

> [!NOTE]
> There is no guest-side transaction API. A plugin that needs several
> statements to commit together puts them in one `batch()`; a plugin that
> needs more statements than one request holds restructures the work, for
> example by inserting from a staging table in one statement.

## Sizing before sending

`limits()` returns the session's `DbCapabilities`, cached for the
invocation, so a plugin can size its own work: how many binds a statement
may carry, how many statements a request holds, how large a cell may be.
Once the session's limits are known (or the binding was created with a
request cap of its own), the SDK checks the encoded request against
`maxRequestBytes` before sending it, so an oversized request fails locally
with the same typed error the host would raise. When both caps are known the
tighter one applies, the same cap `chunk()` splits to.

```ts
const caps = await env.DB.limits();
const perStatement = Math.floor(caps.maxBinds / COLUMNS);
```

## Interactive transactions

External database adapters refuse `begin()`: the adapter session answers
`unsupported: interactive adapter transactions`, and the hosted demo runs
on such an adapter. Host code writes the way a plugin does, as one atomic
batch (`LibraryStore::execute_host_batch`) or a named atomic operation.
`crates/bookclerk-library/clippy.toml` disallows
`sea_orm::TransactionTrait::begin` in the library crate, so a new
interactive transaction fails Clippy; the native-connection tools (backend
migration, backup restore) and the in-process fallbacks a store without an
atomic backend takes carry a narrow allow with the reason beside it.

## Testing against the limits

A plain in-memory SQLite connection accepts `begin()` and never hits a cap,
so a test that passes on one can still fail on an external adapter. The
`bookclerk-db-testkit` crate opens an in-memory library that behaves like
the adapter instead:

```rust
use bookclerk_db_testkit::{fixtures, Profile, TestDatabase};

let db = TestDatabase::open(Profile::Minimum).await?;
let store = db.store(); // a LibraryStore: begin() is refused, batches are checked
// ... run the code under test ...
assert_eq!(db.refused_begins(), 0);
```

- **Profiles.** `Profile::FirstParty` is the shipped sqlite and Postgres
  advertisement, `Profile::D1` the D1 one, and `Profile::Minimum` the
  smallest advertisement the host connects to
  (`DbCapabilities::host_minimum()`: 4,096-byte results, 1,024-byte cells,
  one row per statement, D1's statement payload, 64 KiB requests, 32
  function arguments, no streamed reads). Run host and plugin database
  code under all three (`Profile::ALL`): what works on the shipped adapters
  has to work when a plugin advertises only the minimums.
- **Refused `begin()`.** The connection's `begin()` is refused the way the
  adapter refuses it, and `refused_begins()` counts the attempts. A host
  write that works on an external adapter leaves the count at zero.
- **Named atomic operations.** Job admission and claims, outbox publish
  and dispatch, user security changes and listing links go the way the
  plugin host sends them to an adapter: compiled to one typed batch,
  admitted against the profile, run through the same executor and read
  back from the typed reply. They never take the in-process `begin()`
  fallback a store without a database plugin uses, so a named operation
  the adapter would refuse fails in the test too.
- **Every cap applied.** Each host batch is admitted against the profile
  (`maxStatements`, `maxBinds`, `maxPayloadBytes`, `maxRequestBytes`), run
  as one transaction under its result caps and its reply checked, so a
  test sees the same typed `limit_exceeded` error the adapter answers.
  `batch_sizes()` lists the statement count of every batch sent.
- **Faults.** `lose_next_replies(n)` applies the next `n` batches but loses
  their replies, so the caller sees `unavailable` and a retry has to be
  idempotent; `lock_next_requests(n)` refuses the next `n` batches with
  SQLite lock contention before anything is applied.
- **Fixtures.** `fixtures::description_near_cell_cap`,
  `fixtures::multibyte_text` and `fixtures::rows_over_result_cap` build
  rows sized from the advertisement (multibyte text so byte and character
  counts differ, wide JSON so a listing exceeds `maxResultBytes`), and
  `paging::assert_pages_match_full` checks that a paged listing returns
  what the unpaged one does (`assert_pages_match_full_within` bounds the
  walk for a listing that skips rows it cannot read).
- **Plugin batches.** `execute_batch(request)` runs one host-authored
  `ExecuteRequest` through the profile, for tests that drive the adapter
  contract directly.

Inside `bookclerk-library` the same kit is `test_support::ProfileStore`,
which the store tests open their fixtures from, and
`test_support::paging` holds the paged-listing checks the kit re-exports.
Every paged listing of the store (book pages, job claim candidates,
dispatched and missing-delivery events, storage-scan identities, Discover
candidates) has a test that walks it under every profile and compares the
walk with the unpaged read. Ordinary SeaORM reads through the connection
are held to the engine proxy's first-party caps, as on the shipped sqlite
adapter.

### A plugin's own binding

A plugin's `env.DB` is a `[[databases]]` binding: an isolated unit the host
serves through the deployed adapter, scoped to the plugin's own tables.
`bookclerk_db_testkit::BindingDatabase` opens one in memory under a
profile, applies the plugin's `databaseMigrations` registration the way
enrollment does, and serves the `GuestDatabase` methods the host serves,
so a plugin's SQL meets the caps, the typed `limit_exceeded` error and the
binding-owned scope before it meets a real adapter.

The TypeScript and Python SDKs reach the same binding from a test suite.
`createTestDatabase({ profile })` spawns the `bookclerk-db-testkit` binary
(`cargo build -p bookclerk-db-testkit` in the workspace; `BOOKCLERK_DB_TESTKIT`
names it, else it is looked up on `PATH`), which serves the binding over
the granted channel a workerd isolate already speaks and exits when the
test closes it:

```ts
import { createTestDatabase, TEST_DATABASE_PROFILES } from "@bookclerk/plugin-sdk/testkit";

for (const profile of TEST_DATABASE_PROFILES) {
  const db = await createTestDatabase({ profile, migrations: MIGRATIONS });
  try {
    await db.binding.prepare("INSERT INTO notes (id, body) VALUES (?, ?)").bind(id, body).run();
    const { results } = await db.binding.prepare("SELECT id, body FROM notes ORDER BY id").all();
  } finally {
    await db.close();
  }
}
```

```python
from bookclerk_plugin_sdk.testkit import TEST_DATABASE_PROFILES, create_test_database

for profile in TEST_DATABASE_PROFILES:
    async with await create_test_database(profile=profile, migrations=MIGRATIONS) as db:
        await db.binding.prepare("INSERT INTO notes (id, body) VALUES (?, ?)").bind(id, body).run()
        rows = (await db.binding.prepare("SELECT id, body FROM notes ORDER BY id").all())["results"]
```

`db.binding` is the D1-shaped binding the plugin gets on `env`; `db.limits()`
is the profile's advertisement. The default profile is `minimum`, the
strictest; run the suite under all three. A registration that fails the
BookclerkSQL proof, or a profile the kit does not know, is refused before
the binding is handed out, with the kit's message.

### The suite under every profile

`bookclerk-plugin test <dir>` (`cargo plugin -- test <dir>` in a Bookclerk
checkout) runs the plugin's own test runner once per profile. It picks
`npm test` when `package.json` has a `test` script, else `python -m pytest`
when `pyproject.toml` exists, else `cargo test`, and hands each run
`BOOKCLERK_DB_TESTKIT` (the kit binary: `BOOKCLERK_DB_TESTKIT` itself, a
`bookclerk-db-testkit` beside the CLI, or one on `PATH`) and
`BOOKCLERK_DB_TESTKIT_PROFILE`, which `createTestDatabase()` and
`create_test_database()` take as their default profile, and which a Rust
suite reads with `bookclerk_db_testkit::selected_profile()` before it
opens its `TestDatabase` or `BindingDatabase`. So a suite written with the
default profile runs under `first-party`, `d1` and `minimum`; one that
opens a fixed profile passes all three runs while testing only that one:

```sh
cargo build -p bookclerk-db-testkit
cargo plugin -- test examples/plugins-echo-workerd-ts
cargo plugin -- test . --profile minimum -- --grep notes   # one profile, runner arguments after --
```

Every profile runs even after one fails, and the report names the ones
that did. `cargo test-staged` covers the migrations of every staged plugin
the same way: the registration each `[[databases]]` binding answers is
applied on a plugin-owned unit under every profile, so a migration the
minimum advertisement refuses fails there rather than at enrollment on an
operator's adapter.

### A database adapter's own suite

The profiles above serve code that *uses* a database. An adapter plugin
(an `entrypoints = ["databaseAdapter"]` guest) is held to the same contract
from the other side by `bookclerk_db_testkit::adapter`. Point it at the
adapter open on an empty unit and it drives that unit through the public
`Database` / `AdapterDatabaseSession` surface the way the host drives a
plugin's binding: it applies the binding bootstrap and a migration, then
runs every guest batch through the host's authorization, receipt wrap and
reply checks.

```rust
use bookclerk_db_testkit::assert_adapter_conforms;

// `database` is any `bookclerk_plugin_abi::Database`: the adapter value in
// process, or the RPC client of the spawned guest.
assert_adapter_conforms(&database).await;
```

`run_adapter_suite` returns the report instead of panicking. Each case
passes, fails with the reason, or is skipped when an earlier case it needs
failed or the advertisement makes it unreachable:

| Case | The adapter must |
| --- | --- |
| `capabilities` | Meet the host minimums, declare `schemaMigrations`, and answer the same advertisement on every call and every session |
| `bootstrap` | Apply the binding bootstrap and a plugin migration as typed atomic batches |
| `values` | Return every value as the variant it was written: `Int64` extremes, `Float64`, multibyte and `b64:` text, bytes, `Boolean` and typed NULL |
| `atomic_batch` | Leave nothing of a batch whose last statement fails |
| `max_statements` / `max_binds` | Accept a batch of exactly `maxStatements` statements and a statement with exactly `maxBinds` binds |
| `result_rows` | Return `maxResultRows` rows, and refuse one more with `limit_exceeded` (`maxResultRows`) instead of truncating |
| `cell_bytes` / `result_bytes` | Round-trip a cell near `maxCellBytes`, and refuse a wider cell or a result past the inline byte caps with `limit_exceeded` |
| `streamed` | Answer `executeStreamed` with the same rows as `execute`, and with a result past `maxResultRows`, when it advertises a streamed budget; answer `unsupported` when it does not |
| `receipt_replay` / `receipt_conflict` | Replay a retried batch's first reply without applying it again (the retry after a lost reply), and refuse the same operation id with another request as `conflict` |
| `close` | Close the session cleanly |

To run it against the shipped artifact rather than the adapter value,
`adapter::NativeAdapter::spawn` starts a native guest binary, `open`s its
`databaseAdapter` with the connect params you pass (for a binding, the
same `DbConnectParams` the host sends) and hands back the RPC client. It
runs the guest unconfined and without the `bookclerk-workerd` front door,
so it is for tests only.

The first-party adapters run the suite in CI: sqlite in process and as its
guest binary, Postgres as its guest binary against every supported major
(the `postgres` check's `adapter_conformance` step), and D1 against its
mocked HTTP API. The D1 run pins one known gap: on a binding, D1 answers an
over-cap read as retryable `unavailable` because the receipt-wrapped batch
has committed before the size is known
([#847](https://github.com/Bookclerk/bookclerk/issues/847)).

## Checklist

- Read with `all()`; the SDK streams when it can, and the result is still
  one snapshot.
- Walk a large result with `iterate()` instead of paging with `OFFSET`.
- Split long writes with `chunk()` and keep each `batch()` to what must
  commit together.
- Handle `limit_exceeded` by its `limit`, not by matching the message.
- Prefer narrower queries over larger budgets: the caps are the adapter's,
  and D1's are smaller than sqlite's.
- Test under every `bookclerk-db-testkit` profile, and keep
  `refused_begins()` at zero.
- Writing an adapter: pass `bookclerk_db_testkit::assert_adapter_conforms`
  on an empty unit.
