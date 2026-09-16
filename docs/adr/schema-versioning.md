# ADR: Schema state, unreleased development packs, and backups

- **Status:** Accepted
- **Date:** 2026-09-03
- **Related:** [SQL database contract](sql-database-contract.md),
  [database.md](../database.md), [migration.md](../migration.md)

## Context

Bookclerk’s library schema used to grow as an incremental chain (named
fragments V2–V29). No production databases exist, so that chain is not an
upgrade path. Developers reset local DBs. The host still auto-applies the
current development pack on library open.

Integer `0` cannot distinguish a fresh SQLite database from an applied
development schema (`PRAGMA user_version` defaults to 0). A future release
cut must not treat an existing unreleased database as an empty predecessor
to frozen v1.

`SQL_CONTRACT_VERSION = 1` / SQL-v1 is the **SQL grammar/ABI**, not a
library schema freeze. There is **no** frozen production schema v1.

## Decision

### Explicit schema state

Internal and CLI state is not an integer identity:

```text
Uninitialized
Unreleased { base_version, checksum }
Frozen { version, checksum }
```

`SCHEMA_VERSION` is the highest **frozen** revision this binary knows. Today
it is `0` because `host_migration_plan()` is empty — that means “there are
no frozen schema revisions,” **not** “this database is at schema zero.”

Unreleased always records its frozen base explicitly. Today that is
`Unreleased { base_version: 0, checksum }` (pre-v1 development). After a
future v1 freeze:

- a release database may be `Frozen { version: 1, checksum }`
- a development database with additional unreleased DDL becomes
  `Unreleased { base_version: 1, checksum }`

Malformed / missing checksum / contradictory markers are a failure class,
not a state to continue from. Do not infer uninitialized or unreleased from
integer `0` alone.

On disk, `bookclerk_schema_migrations` stores `namespace`, `state` (`unreleased` |
`frozen`), `version`, `checksum`, `app_version`, and `applied_at`. Primary
key is `(namespace, state, version)`. Host library and Bookclerk-owned
binding bootstrap use namespace `bookclerk`. Plugin-owned evolution uses a
separate `bookclerk_plugin_migrations` journal in the binding database, not this
table and not the `bookclerk` namespace. For unreleased host rows, `version`
**is** the frozen base (`0` before any freeze). Host logic keys off `state`
plus that base. `PRAGMA user_version` is a frozen-version cache only.

`bookclerk db version` prints `uninitialized`,
`unreleased@base<n>+<checksum>`, or `frozen@<version>+<checksum>`.

### Unreleased pack until a real release cut

[`host_migration_plan()`](../../crates/bookclerk-library/src/migrations.rs)
is **empty**. Live schema lives in `unreleased_ops` (already-separated
`MigrationOp`s). Fresh databases apply those ops (frozen ups, then the
current unreleased list) and persist
`Unreleased { base_version: SCHEMA_VERSION, checksum }`. Joined scripts
(`current_canonical_schema()`, `unreleased_sql()`) are derived for
diagnostics, export, and `execute_batch` fixtures; they are not re-parsed
to recover statement boundaries.

`current_canonical_schema()` is **not** permanently equal to
`unreleased_sql()`. Today the frozen plan is empty, so they coincide. After a
freeze it is concatenated frozen ups plus whatever is again unreleased.

There is **no** production schema v1 freeze in this tree.

Connect transitions:

| Database | Library open |
| --- | --- |
| `Uninitialized` | Apply frozen canonical base known to this binary, then the current unreleased bucket; persist current state |
| Matching `Unreleased { base_version, checksum }` | No-op (`base_version` must equal this binary’s `SCHEMA_VERSION`) |
| Mismatched unreleased checksum or frozen base | **Fail closed** (`cargo reset --yes`) unless a later explicit exact-checksum promote exists |
| `Frozen { version, checksum }` | Verify frozen checksums; apply remaining frozen steps; then apply the current unreleased bucket if one exists. Resulting state is `Unreleased { base_version: latest_frozen, checksum }` when the bucket is non-empty |
| `Frozen` newer / unknown / checksum mismatch | **Fail closed** |
| Pre-state-machine DB (`user_version` ≫ 0, no state row) | Unsupported / malformed; reset |

A future freeze that copies `unreleased_ops` into
`HostMigrationStep { version: 1 }` must **not** apply v1 ups on top of an
existing `Unreleased { base_version: 0, … }` database just because both
historically used integer zero. Default: documented development reset.
Optional later promote: exact-checksum marker rewrite only.

Never auto-downgrade on daemon or CLI library open.

### Backups (recovery points)

A **recovery point** is one complete logical database state at a specific
time. The physical repository may reuse immutable canonical objects from
earlier recovery points. Restore never replays older manifests (no chained
incrementals).

`DbCapabilities.consistentBackupRead` means the adapter can expose one
stable logical state while Bookclerk reads schema, rows, and identity.
`DbCapabilities.atomicUnitRestore` means destructive restore of one logical
unit does not leave that unit partially replaced after an ordinary failure.
Orchestration keys on these flags, never on sqlite/postgres/d1 plugin ids. The host owns IDs,
timestamps, manifests, retention, and lookup. The durable artifact is
canonical Bookclerk content (admitted schema + portable `DbValue` chunks),
not VACUUM / `pg_dump` / D1 REST / native pages.

| Adapter | Consistent capture | Complete unit restore |
| --- | --- | --- |
| SQLite | Read transaction over admitted tables; paged `ORDER BY` rows as `DbValue` | Restore transaction + `PRAGMA defer_foreign_keys` (never toggle `foreign_keys` on the pool); `PRAGMA foreign_key_check` before commit; `sqlite_sequence` high-water |
| Postgres | `REPEATABLE READ` transaction | Transactional replace; `bookclerk_identity` high-water (not native sequences) |
| D1 | **Not advertised.** Sequential HTTP is not a consistent image. | **Not advertised.** Sequential REST DROP/INSERT is not complete per-unit replacement. |

A recovery point never silently omits an expected table or fabricates SQL
`NULL` for an unrepresentable value. Restore eligibility does not require the
same adapter that captured the backup.

Missing backup capability fails backup/restore closed. Restore never
merges. Restore does **not** auto-migrate the host library and does **not**
run plugin-owned migrations. Integrity (manifest parse, supported format,
every object digest, admitted schema, typed cells) is verified **before**
any destructive action. The manifest is published only after every
referenced object exists.

Crash-safe object write: unique same-directory temp → fsync the file →
no-clobber hard-link install → fsync the parent directory. Concurrent
writers that lose the install verify the winner's object. Incomplete
staging remains invisible. List/restore ignore unpublished work. Object
publication and GC/prune take an exclusive repository lock (`backups/.lock`)
so in-progress objects are not collected before the manifest commit.
Archive extraction refuses `..`, absolute paths, symlinks, hardlinks, and
other non-file/directory tar types, and enforces entry-count, per-entry,
total-expanded, and gzip-decoded stream budgets. Stored objects declare
uncompressed length in the envelope and refuse gzip expansion past that
cap. Temporary unpack directories are removed on success and failure.

Retention prunes automatic `pre-migrate` recovery points only. **Never** prune
`manual` backups. Reachability GC deletes objects no retained manifest
references. `bookclerk db backup create` is manual;
`bookclerk db backup list` lists by time; `backup verify` / restore accept
path, id, or timestamp. Duplicate / ambiguous lookup fails rather than picking
arbitrarily.

Skip capture only for `Uninitialized`. Unreleased databases with data are
backup-capable. Library schema at capture is the schema that matches the
recorded `SchemaState` (frozen vN under a newer binary exports vN, not the
latest pack).

`--include-plugin-databases` enumerates `plugin_databases` registry rows
(not the `plugin-databases/` directory). Each logical unit is
`(plugin_id, binding)`, opened through the active adapter session. A failed
unit fails the requested bundle. Units are replaced individually; a bundle is
not one transaction across independent databases. Plugin schema/version
markers restore as ordinary rows; the plugin may issue host-mediated
BookclerkSQL in its binding after startup. Restore does not run plugin
migrations.
Library-only restore preserves the target registry. Included restore rebinds
registry rows to the target adapter’s physical placement (never source
`unit_ref`).

### Future portable PITR (not implemented)

A later design may journal canonical Bookclerk transactions at commit
boundaries and restore `base recovery point + committed change segments`.
Required future invariants would include: journal entry atomic with DB
commit; transaction boundaries; all mutations including DDL and plugin DDL
without stealing plugin migration ownership; replay idempotency; retention
tied to the oldest retained base recovery point. This repository’s object
store is intended to remain the base layer for that work. Do not emit a
change journal until that design lands.

### Host-owned schema vs plugin-owned binding migrations

Bookclerk defines migration identity, order, versioning, compatibility, and
recovery for **host** schema (`host_migration_plan`, numeric
`HostMigrationStep.version`, checksum chain, explicit downgrade when every
step has `down`). A database claiming frozen host version N must verify all
retained/applied host migration checksums required by that version, not
merely the final marker. Production schema remains unreleased / version 0.

Plugins declare an ordered migration history at startup using opaque
plugin-owned migration IDs and BookclerkSQL operations. Bookclerk assigns
no semantic meaning to those IDs (not integer versions, not contiguous
numbers, not semver, not timestamps, not UUIDs, no `version - 1`
predecessor). Bookclerk owns validation, immutable-history verification,
serialization, atomic execution, bookkeeping, backup/restore integration,
and stale-session fencing.

Registration (`BookclerkPlugin.databaseMigrations(binding)`):

1. Occurs at binding initialization, before ordinary execute.
2. The plugin presents the complete ordered sequence (opaque `id` + already
   separated `schema`/`data` BookclerkSQL operations). Resource limits are
   independent: migration count (`maxListPage`), operations per migration
   (`maxPluginMigrationOps`), total operations
   (`maxPluginMigrationTotalOps`), per-SQL bytes (`maxScalarBytes`), and
   aggregate id+SQL UTF-8 (`maxPluginMigrationRegistrationBytes`).
3. Empty list means no plugin-owned migrations.
4. The host proves the **entire** sequence with one evolving `SqlTypeEnv`
   (migration B typechecks against the schema produced by A).
5. A malformed registration fails before the binding is available for jobs.
6. The plugin cannot inspect or rewrite the durable host journal.

Durable history is an **exact prefix** of the current registration:

```text
durable plugin migration history == prefix(current registered history)
```

For every durable journal row, `migration_id` and checksum must match the
registration at that position, in order. Fail closed if an already-applied
migration was edited, removed, renamed, or reordered, or if durable history
is longer than the installed plugin (older/incompatible plugin). Only a new
suffix is pending.

The journal is `bookclerk_plugin_migrations(ordinal, migration_id, checksum, applied_at)`
inside the binding database. `ordinal` is host-private storage order only and
is not a plugin migration version. Every host table inside a binding
(journal, serialization slots, atomic receipts, catalog tables, the
`bookclerk_src` wrap alias) carries the `bookclerk_` prefix, and that prefix
is the only name space reserved from plugin SQL (besides engine catalogs and
schema-qualified names). There is no native SQL escape hatch.

Each pending suffix migration is one atomic unit (serialization-slot
mutation + ops + journal append) in a short transaction. Concurrent
Bookclerk instances registering the same sequence converge without a
process-spanning or walk-spanning lock: each host re-reads durable history
before applying the next suffix item; uniqueness/conflict or an ambiguous
commit reply causes another journal read; matching `(id, checksum)` at the
expected ordinal is success; contradictory history fails closed; transient
failures retry under the existing bounded policy. The serialization-slot
SQL runs only inside that same atomic unit — it does not hold a lock
across later independent migration transactions.

There is no host-managed plugin downgrade. A plugin that wants to undo an
earlier change registers a new forward migration. Operational rollback uses a
compatible backup/recovery point. Destructive restore writes captured
schema/data/journal exactly and does **not** walk the installed plugin's
migrations. On the next ordinary startup, registration occurs; restored
history must be an exact prefix of the installed sequence, then the pending
suffix applies. Restored history newer than or incompatible with the
installed plugin fails closed.

Stale-session fencing captures the complete history digest
(`history@{n}+{digest}` of ordered `(migration_id, checksum)` pairs) when
reconciliation completes. Ordinary execute re-reads the journal and fails
the session if the digest changed (peer advanced or rewritten history).
Fencing does not use a plugin-chosen latest ID.

Host-owned binding bootstrap remains [`binding_bootstrap_ops`](../../crates/bookclerk-library/src/migrations.rs)
(`Schema` ops: receipts, SQL catalog, `bookclerk_plugin_migrations`, serialization
slots). `BINDING_SCHEMA_VERSION = 0` until a binding freeze. Connect:

| Binding | Open |
| --- | --- |
| `Uninitialized` | Apply bootstrap ops + unreleased marker (one atomic unit; retry uniqueness / unavailable after re-read) |
| Matching `Unreleased { base_version, checksum }` | No-op |
| Mismatched checksum / unexpected frozen | **Fail closed** (restore or drop the binding) |

Then register/prove plugin migrations and apply the pending suffix.

Ordinary binding `execute` uses `GuestSqlPolicy::binding_owned` (query/DML).
Only the host-controlled migration path uses `binding_migration` (bounded
`CREATE`/`DROP IF [NOT] EXISTS`). `ALTER` and `CREATE TABLE AS` stay refused.

There is **no** backend-native escape hatch (`pg_dump`, `VACUUM INTO`, D1 REST
migrate, sqlite `.dump`). Cross-adapter backup/restore uses canonical
Bookclerk data/schema representations; backups include the plugin migration
journal as ordinary captured rows plus the history digest in unit metadata.

### Migration ops (still unreleased)

```text
HostMigrationStep { version, introduced_in, steps, down }
MigrationOp = Schema(canonical stmt) | Data(canonical stmt)
```

The **op list is the source of truth**. Human-readable SQL
(`unreleased_sql()`, `current_canonical_schema()`, `HostMigrationStep::up_sql`)
is derived by joining already-separated statements with `;\n` for
diagnostics/export/tests. Apply, checksum, and backup walk the ordered ops.
Each `MigrationOp` is exactly one BookclerkSQL statement; `Schema` must be
admitted DDL and `Data` admitted DML, both proven through the SQL-v1
typechecker before adapter execution. A packer failure on static or imported
migration SQL fails closed (never checksumed as one opaque statement).

`host_migration_plan()` stays **empty**. Live schema is [`unreleased_ops`](../../crates/bookclerk-library/src/migrations.rs)
(mostly `Schema`, plus seed `Data` inserts). Checksums hash the
**length-prefixed ordered statement list**, not a joined script. `db migrate
--to <older>` applies `down` only when every step has it; otherwise restore a
backup.

Invariants (locked with synthetic plans, not a v1 freeze):

- Integer version order for **host** schema; fail closed on contradictory
  `bookclerk_schema_migrations` rows. Plugin history uses opaque IDs and exact-prefix
  `bookclerk_plugin_migrations` verification, not host numeric versions.
- Retry uniqueness / duplicate-object / unavailable after re-read; matching
  host version+checksum or plugin `(id, checksum)` at the expected ordinal is
  success; FK / CHECK / NOT NULL are not races.
- Crash: marker/journal row not visible ⇒ retry the same unit.
- Forward-only on library/binding open; rolling binaries fail closed on
  unknown newer frozen host state or plugin history that is not a prefix.
- Multi-node: portable `bookclerk_slots` (`schema:{namespace}` for
  host; `bookclerk_plugin_migrations` for plugin suffix). Stale sessions re-check the
  plugin history digest at execute boundaries.
- Backup capture includes the plugin journal and history digest; restore
  into a newer app stays fail-closed until registration/suffix apply is an
  ordinary open (not inside the restore transaction).
- Oldest frozen host revision is derived from `host_migration_plan()`
  (`None` while the plan is empty). There is no independent
  `MIN_SUPPORTED_SCHEMA_VERSION` constant.

### Last-reversible CLI

`bookclerk db version|backup|restore|migrate` uses
`connect_without_migrate`.

With an empty frozen plan, `migrate --to` is a no-op. Time-based
**restore** is how operators move independently of schema revision.

### Out of scope

- Declaring production schema v1
- D1 Time Travel / portable PITR / canonical change journaling
- Transactional atomicity across library DB + independent plugin DBs
- Using semver as `bookclerk_schema_migrations.version`

### Plugin binding migration registration

Plugins register a complete ordered history at startup
(`databaseMigrations`). Host library frozen steps remain a separate numeric
`MigrationPlan` engine. Plugin IDs are opaque; the host journal ordinal is
storage order only.

| Concern | Rule |
| --- | --- |
| Per-plugin / per-binding history | Private `bookclerk_plugin_migrations` journal in the binding DB (not `bookclerk_schema_migrations`, not namespace `bookclerk`). |
| Ordered progression | Registration order. Host proves BookclerkSQL with one evolving `SqlTypeEnv`. |
| Durable vs registered | `durable history == prefix(current registered history)`; anything else fails closed. |
| DDL outside registration | Fail closed. Ordinary binding execute is DML/query only. |
| Multi-node concurrency | Each apply unit includes serialization-slot mutation + ops + journal append; re-read durable history on conflict or ambiguous result. No lock spans later independent transactions. |
| Retries / interrupted recovery | Uniqueness / unavailable after re-read; matching `(id, checksum)` at expected ordinal ⇒ success; absent row ⇒ retry same migration; contradictory history ⇒ fail closed. |
| Upgrade | Only a new suffix is pending. Stored history longer than registration ⇒ older plugin, fail closed. |
| Downgrade | Never. New forward migration or restore a recovery point. |
| Restore | Restore captured journal exactly. Do not migrate inside the restore transaction. Next ordinary open registers and may apply the suffix. |
| Stale session | Binding execute re-checks the complete history digest. |

Acceptance includes opaque IDs, evolving type environment, crash/retry, two
nodes racing the same suffix, old-session fencing, edited/renamed/reordered
history rejection, older-plugin rejection, backup → restore → suffix apply
(SQLite / Postgres; D1 does not advertise backup flags).

## Consequences

- Fresh databases apply the unreleased pack once and record
  `unreleased@base0+<checksum>` until a freeze exists.
- Editing `unreleased_ops` against an existing development DB is a hard
  fail + reset, not a silent reshape.
- There is no incremental V2–V29 chain and no `migrations_legacy` module.
  Current-schema FK/UNIQUE coverage lives on the greenfield pack.
- A later freeze can concatenate frozen ups + a new unreleased bucket
  through `current_canonical_schema()` without unwinding callers.
- Schema-apply retries uniqueness / duplicate-object races and transient
  unavailability after re-reading durable state. Foreign key, CHECK, and
  NOT NULL failures are not schema-apply races.
