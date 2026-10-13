# ADR: Control plane — bootstrap, host identity, and database configuration

- **Status:** Accepted for Phase 0, Phase 1, and Phase 2 (Refs #192)
- **Date:** 2026-09-29 (Phase 2: 2026-10-13)
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
  stores the latest incarnation, heartbeat time, software version, schema
  display, and capacity sample as **observations**. The capacity sample is
  `logical_cpus`, `cpu_max_quota_us`, `cpu_max_period_us`, `memory_max_bytes`,
  `memory_current_bytes`, `memory_anon_bytes`, `files_dir_free_bytes`, and
  `scratch_bytes`. Missing cgroup files stay null. `created_at` and
  `cluster_id` are identity and are not refreshed.

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
`heartbeat_at`, `software_version`, `schema_state`, `compatible`, and the
capacity columns (`logical_cpus`, `cpu_max_quota_us`, `cpu_max_period_us`,
`memory_max_bytes`, `memory_current_bytes`, `memory_anon_bytes`,
`files_dir_free_bytes`, `scratch_bytes`) are observations from the last
heartbeat. They are not a `host.runtime` document and they are not a second
copy of `media.workers`. Physical plugin install files and the host install
ledger stay on the host. This spike does not reconcile desired plugin
deployments.

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

## Phase 2: plugin instances, declared schemas, PluginKey-keyed accounts

Phase 2 makes the database the only home for plugin configuration and
enablement. The host hard-codes nothing per plugin: what a plugin can be
configured with comes from its manifest, where it runs comes from a
deployment row, and which accounts it serves comes from the plugin key.

### Instances

A plugin instance is a durable id, not a function of the plugin key, alias,
capability, or host. `plugin_instances.plugin_instance_id` is a hyphenated
UUID with an operator-facing `display_name`. Two rows may name the same
canonical plugin key, and both may run on one host.
`plugin_instance_defaults` names the plugin's **default instance**: the one a
bare plugin key or alias routes to when a job does not pin an instance.
Enrollment creates it; `bookclerk plugins instances create` adds more.

### Declared schema

A plugin declares its operator settings in `plugin.toml` as `[[settings]]`
entries (`key`, `label`, `type` ∈ `string | bool | integer | enum | secret`,
`values`, `default`, `required`, `description`). The manifest is the authority,
as it already is for identity and capabilities. `describe().configOptions` is
presentation only: an empty list means "derive from the manifest"; a non-empty
list must equal the manifest-derived list or the spawn fails
(`validate_described_config_options`). A guest cannot widen its schema at
runtime.

Enrollment pins a copy in `plugin_schemas(plugin_key, manifest_sha256,
package_version, settings_json, recorded_at, recorded_by_host)` whenever the
local manifest hash differs. Writes through the API or CLI validate against
that row, so an administrator on one host can configure a plugin that is only
installed on another. A spawn validates the document against the host's local
manifest: unknown keys are dropped with a warning, a missing required key is
an `error` observation. PluginKey is versionless, so two hosts with different
package versions pin last-writer-wins; the local check on spawn is what keeps
a guest from seeing a shape it does not understand.

### Instance document

The instance's settings and secret refs live in `configuration_documents` at
scope `plugin_instance`, namespace `config`, schema version 1. The effective
`CONFIG` binding for a spawn is the schema defaults with the document's
settings on top. A `secret` setting is never stored in the document: the
value is sealed into `encrypted_secrets` (`kind = plugin_instance`) and the
document carries only a ref name; the guest receives it in `SECRETS`.
Unknown keys, type mismatches, enum values outside `values`, and plain values
for a secret are rejected at write time. Compare-and-swap uses the same
revision and receipt path as `core.events`. `[vars]` stays the declaration
that the guest receives `CONFIG`; it carries no defaults.

### Deployments are the enable switch

`plugin_deployments` holds `desired ∈ {present, absent}` per (instance, host).
"Enabled on this host" means a present deployment. `bookclerk plugins
enable|disable [--host]`, `PUT /api/config/plugin-instances/{id}/deployments/
{host_id}`, and the Settings page switch write that row; nothing reads an
`enabled` key from TOML. The host's reconciler spawns present deployments,
retires a running guest when desired flips to absent, and records
observations (`installed`, `running`, `healthy`, `error`, `retired`). A
desired write never updates observations.

Enrollment is generic and idempotent, and runs in both the CLI and the daemon
bootstrap. For every plugin discovered on the host except those whose only
entrypoint is `databaseAdapter`: pin the schema; ensure the default instance
and, when its document is absent, seed it with the schema defaults; ensure
this host's deployment with `desired = present` when a covering consent grant
exists and `absent` otherwise. Platform guests auto-grant, so `local` and
`sqlite` come up enabled on a fresh files directory; third-party plugins stay
absent until an operator approves and enables them.

### One bootstrap

`bookclerk_plugin_host::open_host_control_plane` is the single path both
binaries use: open the database, align the cluster secret root, load or mint
the host identity, heartbeat, import `core.events`, enrol local plugins, then
load sources, destinations, and integrations from this host's present
deployments only. A plugin with no present deployment here is not loaded;
there is no TOML fallback and no transitional guest. The CLI spawns guests
for the lifetime of a command and does not write observations, so "route to
the healthy default" degrades to "route to the present default" there.

### Accounts belong to the plugin, not the instance

`accounts` is keyed `(plugin_key, account_id)`; `books`, `ignored_titles`,
and `account_links` carry `plugin_key` with composite foreign keys; `source`
remains the display alias. `SourceScope` carries the plugin key (isolation)
and the source id (display). Any instance of that plugin on any host may scan
or acquire for any of its accounts, which is what lets a cluster assign work
freely. An `account_id` is unique per plugin, not globally: the same email is
a legitimate account on two stores that both use email as the id. Paths that
must pick one row (`find_account`, `library set-scan`, `library
revoke-account`, portal revoke) take the plugin key, or `--source`, and fail
with a clear conflict when a bare id matches two plugins.
`title_request_sources.source` and `domain_events.source` are provenance,
not ownership, and are unchanged.

### Routing

`SourceRegistry::route(needle, pin)` resolves an instance id exactly, and a
plugin key or alias to the default instance when it is present on this host
(else the oldest). Jobs carry `plugin_instance_id: Option<String>`; `library
scan|acquire --instance` and the API bodies may set it. `scan_all` iterates
plugin keys, one routed instance each. The earlier "ambiguous key" fail-closed
paths are gone.

### What left `config.toml`

`[sources.<id>]`, `[output.local]`, `[output.s3]`, `[integrations.<id>]`,
their `enabled` flags, their `plugin` occupancy keys, and every environment
override for them (`BOOKCLERK_SOURCE_<ID>_ENABLED`, `BOOKCLERK_OUTPUT_*`,
`BOOKCLERK_S3_*`, `BOOKCLERK_AWS_*`, `BOOKCLERK_GA_*`,
`BOOKCLERK_AUDIBLE_BITRATE`, `BOOKCLERK_LIBRO_CONTAINER`,
`BOOKCLERK_ABS_API_KEY`, `BOOKCLERK_*_PASSWORD`). Per-destination `root`,
`prefix`, naming, S3 bucket/region/endpoint, and S3 credentials are
`[[settings]]` of the `local` / `s3` plugins; `bookclerk config
s3-credentials` and the `kind = s3` operator secret row are gone. The host's
in-process `LocalFsBackend` / `S3Backend` fallbacks are gone with them. A
network host the document names (an S3 `endpoint`, the Audiobookshelf
`base_url`) is granted to the guest as a deployed endpoint derived from the
document, not from TOML. Store passwords reach a guest only as the sealed
secret stored when the account is connected.

Bootstrap stays in TOML and env: `[database]`, `master.key`,
`host-identity.json`, `daemon.listen`. Database-adapter plugins are not
instances in this phase. The shared `[output]` table (format, Widevine,
naming profile, sidecars, multi-destination policy) also stays in TOML for
now; moving it to a `core.output` cluster document is deferred.

KV and Queues are unchanged. Named SQL bindings do not replace them.

## Consequences

- New unreleased tables: `cluster_identity`, `hosts`,
  `configuration_documents`, `configuration_audit`,
  `configuration_changes`, `plugin_instances`, `plugin_instance_defaults`,
  `plugin_deployments`, `plugin_deployment_observations`, and
  `plugin_schemas`. `plugin_instance_defaults` names the plugin's default
  instance; explicit create adds further instances of the same key.
  `accounts` is unique on `(plugin_key, account_id)`; `books`,
  `ignored_titles`, and `account_links` carry `plugin_key`. Checksum of the
  unreleased pack changes. Existing development databases fail closed until
  recreated (`cargo reset --yes` or a new database). That matches the
  unreleased-schema rule.
- `bookclerk config get/set` for `events.*` reads and writes the database.
  Plugin settings and enablement are `bookclerk plugins config` and
  `bookclerk plugins enable|disable`, also database. The remaining `config`
  keys still edit `config.toml`.
- `bookclerk config show` prints `events.authority = database` when the
  library can be read, and `transitional` when it cannot.
- Single-host SQLite still starts by opening the local database, minting
  `master.key` once, and writing `host-identity.json` once. The CLI does that
  alignment inside the library open, not before command dispatch. `config
  master-key status` and `wrap` do not open the database and do not mint a
  missing key.

## Remaining work

Not in this phase, and not claimed as production HA:

- `core.output`: the shared `[output]` table (format, Widevine, naming
  profile, sidecars, multi-destination policy) is still TOML. Moving it to a
  cluster document like `core.events` is the next configuration domain.
- Moving the rest of `config.toml` (library, jobs, media, plugins,
  diagnostics, discovery, OIDC). `daemon.listen` moving off bootstrap is part
  of that.
- Job placement, cluster semaphores, and singleton maintenance leases
  (Phase 3). Heartbeats do not claim jobs. Do not run two daemons against one
  database and expect safe active/active acquire. A notification-driven
  wakeup for configuration changes is also Phase 3; today every process
  re-reads documents on a 5 s tick.
- Multi-host reconciliation, placement, rollouts, and uninstall: a host only
  reconciles its own deployments. Database-adapter plugins as instances and
  multiple database targets (Phase 4).
- Dashboard, drain, and rolling upgrades (Phase 5).
- Schema pinning is versionless (PluginKey has no version). Two hosts with
  different package versions pin last-writer-wins; the spawn-time check
  against the local manifest is the guard.
- The CLI writes no observations, so its routing prefers the present default
  instance rather than the healthy one.
- Resizing the local event delivery task count without a process restart.
- A general key-management provider. Operators copy `master.key`.
- Install crash window between stage and commit. There is no durable
  pending-install marker. A restart can observe a staged tree that never
  passed pre-commit health. Health failure in a live process still rolls
  that tree back before commit.
- The reconciler holds the process-wide plugin mutation lock across extract
  and the health check. Extract runs on a blocking thread. The lock is still
  held across the health await so a second installer cannot commit the same
  tree.
- D1 multi-host execution. The executing D1 tests in this repository are an
  HTTP mock over one SQLite connection. The shared-database proof is two
  PostgreSQL pools plus two SQLite connections. A live D1 database was not
  provisioned.
