# ADR: Control plane — bootstrap, host identity, and database configuration

- **Status:** Accepted for the Phase 0 / bounded Phase 1 spike (Refs #192)
- **Date:** 2026-09-29
- **Related:** [SQL database contract](sql-database-contract.md),
  [schema versioning](schema-versioning.md),
  [plugin capabilities v3](plugin-capabilities-v3.md),
  [configuration](../configuration.md), [database](../database.md)

This spike does not make Bookclerk a multi-host production deployment. It
records the control-plane boundary and proves one typed configuration domain,
stable host identity, and secret-root joining on the existing SQL stack.
`SCHEMA_VERSION` stays `0`. The production schema is still unreleased.

## Context

Bookclerk already has one authoritative library database (SQLite, PostgreSQL,
or D1), canonical SQL, atomic batches, `bookclerk_receipts`, and a durable
outbox. Configuration still lives in `config.toml` plus environment variables,
and each process mints `master.key` when the file is absent. That is correct
for a single host and wrong for a second process that opens the same database:
it can seal secrets with a different data-encryption key, and a hostname-derived
id would change when the pod name changes.

Plugin event delivery is per loaded plugin on a host. It is not a broadcast
to every Bookclerk process. Hosts therefore cannot treat `domain_events` as
their configuration bus.

## Decision

### Bootstrap versus application configuration

Bootstrap is the material required before the process can open the library
database and unlock its secret root:

| Material | Where it lives | Why it cannot come from the database |
| --- | --- | --- |
| Database target | `[database]` in `config.toml`, or `BOOKCLERK_DATABASE_*` / D1 / Postgres env | The process needs it to open the connection that would store later configuration |
| Auth password | `BOOKCLERK_AUTH_PASSWORD` or `[auth].password` | Unwraps `master.key`; not itself a library secret |
| Secret root | `$BOOKCLERK_FILES_DIR/master.key` (`BCK1` or `BCK2`) | Decrypts `encrypted_secrets` and is the cluster DEK |
| Host enrollment | `$BOOKCLERK_FILES_DIR/host-identity.json` | Stable `HostId` and the cluster id this files directory has joined |

`[database].plugin`, the SQLite path, the D1 account/database ids, the D1 API
token, and the Postgres URL stay bootstrap. Moving them into the database
would be circular: the database plugin cannot read its own connection settings
from a database it has not opened. Phase 4 may store additional targets after
this one connection exists. This spike does not.

Everything else in `config.toml` is transitional application configuration.
The first domain moved into the database is `[events]` (`core.events`). After
import, that domain has one authority: the database row. Startup, reload, the
CLI, and `BOOKCLERK_EVENTS_*` must not overwrite it. Unmigrated sections
(library, jobs, sources, output, daemon listen, media, plugins, diagnostics,
discovery, auth.oidc) still load from TOML and environment variables.

`daemon.listen` stays in TOML for this spike because the process binds before
it serves HTTP. It is transitional, not a second copy of `core.events`.

### Startup order

1. Load bootstrap configuration (files dir, database target, listen, the
   transitional TOML seed).
2. Open the database plugin and apply the host schema. This does not require
   the DEK.
3. Align the secret root (below). Only then cache the DEK.
4. Load or create `host-identity.json`, bind it to the database cluster id,
   and heartbeat.
5. Import `core.events` when the row is absent, using the TOML/environment
   seed. When the row exists, keep it and overlay it onto the in-memory
   `Config`.
6. Start runtimes that read `Config.events` (dispatcher, pruner, claim cap).

A reload repeats steps 3–5 against the library it is about to publish and
aborts the reload when alignment fails. It does not publish a config that
still prefers the TOML events table.

### Cluster, host, and incarnation

- **Cluster** — one logical Bookclerk service, identified by `cluster_id` in
  the singleton `cluster_identity` row. The cluster is the database that holds
  that row, not a hostname and not a particular SQLite file path.
- **HostId** — a UUID stored in `host-identity.json`. It is not derived from
  the hostname. Restart reads the same file. A new file is a new host.
  Copying the file copies the host. Two processes with different files are
  different hosts even when they share the database.
- **Incarnation** — a UUID kept in process memory and written on heartbeat.
  Restart mints a new incarnation and keeps the same HostId. The `hosts` row
  stores the latest incarnation, heartbeat time, software version, and schema
  display as **observations**. `created_at` and `cluster_id` are identity and
  are not refreshed.

Connecting a files directory whose `cluster_id` does not match
`cluster_identity.cluster_id` fails. The identity file and the database row
are left unchanged. An empty database may adopt the cluster id already
written in the identity file; a database that already has a different cluster
id is not overwritten.

Heartbeat updates only the caller's row. Another host's observation is not
deleted.

SQLite remains one host's database. Two hosts must not share a SQLite file
over a network filesystem. PostgreSQL (and, later, D1) is the shared database.
This spike does not schedule jobs across hosts.

### Configuration documents

Physical table `configuration_documents`, one row per
`(scope_type, scope_id, namespace)`:

| Column | Role |
| --- | --- |
| `scope_type` | `cluster` or `host` |
| `scope_id` | `singleton`, or a HostId |
| `namespace` | Domain name. Not a plugin capability and not a `PluginKey` |
| `schema_version` | Domain schema, starting at 1. Independent of library `SCHEMA_VERSION` |
| `revision` | Monotonic compare-and-swap counter, starting at 1 |
| `document_json` | JSON body of one typed document, at most 16 KiB |
| `updated_at` / `updated_by` | Audit time and actor |
| `write_operation_id` | Idempotency token of the commit that produced this revision |

The public API is typed: `EventsSettingsV1` and `HostRuntimeSettingsV1`.
Callers do not get a general settings key/value service.

Namespaces in this spike:

- `core.events` — cluster scope. The migrated `[events]` domain.
- `host.runtime` — host scope, one `label` field. Effective runtime for a
  process is that host's row only. Another host's label is readable by
  address and is not applied locally.

`PluginKey`, `PluginInstance`, and `PluginDeployment` stay distinct and are
not implemented here. Capabilities describe what a plugin can do. They do not
choose the configuration namespace. A future `plugin_instance` scope will key
on instance id, not on the capability list.

Writes:

- Validate the typed body before any SQL.
- Operator may replace. Administrator, member, and bootstrap may not.
  Bootstrap may insert a missing document once.
- Replace is one canonical SQL batch: slot scratch, `UPDATE … WHERE revision = ?`,
  audit insert, change-notice insert, `bookclerk_receipts` insert, slot delete.
  The batch does not take a global `bookclerk_slots` lock. PostgreSQL row locks
  on the document serialize only that row.
- The scratch slot is keyed by operation id, so two different documents do not
  share a lock.
- A mismatched revision inserts neither an audit row nor a
  `configuration_changes` row. The API returns 409 with `current_revision`.
- Replaying the same operation id and request hash does not bump the revision.
  The receipt, including operation kind and request hash, is resolved after
  authorization and payload validation and before the revision predicate, so a
  later edit does not turn that retry into a conflict. A receipt for that
  operation id still replays after `expires_at`; cleanup deletes other expired
  receipts and keeps the current id.
- A different body or operation kind under the same operation id is an
  idempotency conflict and does not change the document.
- An unsupported `schema_version` fails the read and the write. The stored
  JSON is left as it was.

Import is one canonical batch: conditional `INSERT OR IGNORE` of the document
plus the revision-1 audit row and change notice. A failed batch leaves none of
those rows. A second import, including one with different TOML or environment
values, returns the existing row and does not validate that obsolete seed.
Concurrent initializers leave one document and one notice.

### Why `[events]`

`[events]` is three integers (`retention_days`,
`dead_letter_retention_days`, `concurrency`) with a real runtime consumer:
the dispatcher and pruner read retention on each tick, and delivery claim
reads concurrency as the in-flight cap. It is not a secret, not a plugin
capability namespace, and not the database connection. A bad value is bounded
(retention 1..=3650 days, concurrency 1..=32) and does not change plugin
grants, media confinement, or listen addresses.

Local delivery **task** count is taken from the authoritative document when
the event runtime starts and is not resized until the process restarts.
Retention and the in-flight cap follow the in-memory document after reconcile.
That split is deliberate for this spike.

### Propagation

`configuration_changes` is inserted in the same transaction as a successful
replace. It is a notice that a revision committed. Plugin outbox delivery is
not used to wake hosts: a host with no matching plugin subscription would
never see the event, and a missed delivery would look like a lost
configuration change.

Every process:

- reads the document revision at startup (an offline process catches up by
  reading the current row, not by replaying notices);
- re-reads at `CONFIG_RECONCILE_INTERVAL` (5 seconds) and overlays a newer
  revision of the **same cluster id** onto `Config.events`. A stale read
  cannot replace a newer body. Revisions from a different cluster are not
  compared. Switching databases is an explicit swap and installs that
  cluster's document even when its revision number is lower;
- applies its own successful write immediately, under the same monotonic rule.

A database connection observes a committed revision as soon as the batch
commits. The 5 second interval is only the in-memory apply bound for a
process that was already running. Failed validation and revision conflicts
do not commit, do not write a change notice, and are not logged as applied.

### Desired state versus observations

`configuration_documents` is desired state. `hosts.incarnation`,
`heartbeat_at`, `software_version`, `schema_state`, and `compatible` are
observations from the last heartbeat. Physical plugin install files and the
host install ledger stay on the host. This spike does not reconcile desired
plugin deployments.

### Secret root

`cluster_identity.secret_fingerprint` is the SHA-256 of the unwrapped DEK.
Wrapping `master.key` with a password (`BCK1` to `BCK2`) does not change the
fingerprint.

| Database | Local `master.key` | Result |
| --- | --- | --- |
| No fingerprint, no `sealed-v1` secrets | Absent | Mint a DEK, insert the fingerprint. Single-host startup stays one command |
| No fingerprint, no `sealed-v1` secrets | Present | Record that DEK. Concurrent initializers: one fingerprint wins; a loser that just minted a different file deletes that file and fails |
| No fingerprint, sealed secrets present | Present and able to unseal one row | Record that DEK. Existing single-host databases keep their key |
| No fingerprint, sealed secrets present | Missing or unable to unseal | Fail. Do not mint. Do not update the database |
| Fingerprint present | Missing | Fail. Do not mint |
| Fingerprint present | Fingerprint differs | Fail. Do not replace the database fingerprint. Delete the local file only when this call created it |

There is no KMS or Vault provider. Joining a host means copying `master.key`
(and the auth password when the file is `BCK2`) plus the database bootstrap
settings, and **not** copying `host-identity.json` when the new process should
be a distinct host.

### Schema compatibility

Library schema behavior is unchanged: unreleased checksum, fail closed on
mismatch, `SCHEMA_VERSION = 0`. A host heartbeats only when
`current_schema_state` is the unreleased pack this binary applies. Document
schema versions are per namespace and start at 1. A document this binary does
not understand is not applied and is not rewritten.

### Authorization

Cluster and host configuration writes in this spike require an operator
principal (the local CLI or the daemon operator token on
`PUT /api/config/domains/core.events` and `PATCH /api/settings` for
`events.*`). Administrators and members are rejected before SQL, including
before a receipt replay. `PATCH /api/settings` rejects a body that mixes
`events.*` with file-backed keys before either authority is written. The
response includes `revision` on success and `current_revision` on conflict.

## Consequences

- New unreleased tables: `cluster_identity`, `hosts`,
  `configuration_documents`, `configuration_audit`,
  `configuration_changes`. Checksum of the unreleased pack changes. Existing
  development databases fail closed until recreated (`cargo reset --yes` or a
  new database). That matches the unreleased-schema rule.
- `bookclerk config get/set` for `events.*` reads and writes the database.
  Other keys still edit `config.toml`.
- `bookclerk config show` prints `events.authority = database` when the
  library can be read, and `transitional` when it cannot.
- Single-host SQLite still starts by opening the local database, minting
  `master.key` once, and writing `host-identity.json` once. The CLI does that
  alignment inside the library open, not before command dispatch. `config
  master-key status` and `wrap` do not open the database and do not mint a
  missing key.

## Remaining work

Not in this spike, and not claimed as production HA:

- PluginInstance records, CONFIG/SECRETS from instance state, and
  PluginDeployment reconciliation (Phase 2).
- Moving the rest of `config.toml` (library, jobs, sources, output, media,
  plugins, diagnostics, discovery, OIDC). `daemon.listen` moving off bootstrap
  is part of that, not this slice.
- Job placement, cluster semaphores, and singleton maintenance leases
  (Phase 3). Heartbeats do not claim jobs. Do not run two daemons against one
  database and expect safe active/active acquire.
- Multiple database targets and host-local SQLite affinity (Phase 4).
- Dashboard, drain, and rolling upgrades (Phase 5).
- Resizing the local event delivery task count without a process restart.
- A general key-management provider. Operators copy `master.key`.
- D1 multi-host execution. The executing D1 tests in this repository are an
  HTTP mock over one SQLite connection. This spike's shared-database proof is
  two PostgreSQL pools plus two SQLite connections. A live D1 database was
  not provisioned.
