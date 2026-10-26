# ADR: Control plane — bootstrap, host identity, and database configuration

- **Status:** Accepted for Phase 0, Phase 1, and Phase 2; Phase 3 decided and
  implemented (Refs #192)
- **Date:** 2026-09-29 (Phase 2 and the Phase 3 decision: 2026-10-13; the
  cluster-install decision, the Phase 3 implementation, and the node and
  install/deploy terminology: 2026-10-14)
- **Related:** [SQL database contract](sql-database-contract.md),
  [schema versioning](schema-versioning.md),
  [plugin capabilities v3](plugin-capabilities-v3.md),
  [configuration](../server/configuration.md), [database](../server/database.md)

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
| Host enrollment | `$BOOKCLERK_FILES_DIR/node-identity.json` | Stable `HostId` and the cluster id this files directory has joined |

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
4. Load or create `node-identity.json`, bind it to the database cluster id,
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
- **HostId** — a UUID stored in `node-identity.json`. It is not derived from
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

The public API is typed: `EventsSettingsV1`, `OutputSettingsV1`, and
`NodeRuntimeSettingsV1`. Callers do not get a general settings key/value
service.

Namespaces in this spike:

- `core.events` — cluster scope. The migrated `[events]` domain.
- `core.output` — cluster scope. The migrated shared `[output]` table (see
  *`core.output`* below).
- `node.runtime` — host scope, one `label` field. Effective runtime for a
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

### `core.output`

The second cluster domain (2026-10-15) is the shared `[output]` table:
format, Widevine and CDM provider, naming profile and templates, sidecars,
cover and chapter options, LAME knobs, timestamps, path sanitization and
replacement rules, filename length, and the multi-destination policy, as
`OutputSettingsV1` (schema 1, unknown fields rejected). It uses the same
import-once, compare-and-swap, audit, change-notice, and wakeup path as
`core.events`; a host overlays a newer revision of its cluster onto
`Config.output`, and each acquire or convert reads the overlaid value when it
starts. Writes are operator-only and validated before SQL (bounded sizes,
template length and control characters, replacement rules that cannot inject
a path separator).

`in_progress` is not in the document. It is the acquire scratch directory: a
path on one host, needed before the database opens (the heartbeat sizes it
and plugin jails mount it), so it stays host-local bootstrap in
`config.toml`. A relative `widevine_cdm` resolves under each host's files
directory. Per-destination roots, prefixes, and naming overrides stay
`local` / `s3` instance settings.

### Propagation

`configuration_changes` is inserted in the same transaction as a successful
replace. It is a notice that a revision committed. Plugin subscriptions are
not used to wake hosts: a host with no matching plugin subscription would
never see the event. Since Phase 3 step 3, every committed document also
publishes a `configuration.changed` message to each registered host's own
queue (`node.<node_id>`, consumed only by that node's daemon); see
*Implementation notes* below.

Every process:

- reads the document revision at startup (an offline process catches up by
  reading the current row, not by replaying notices);
- re-reads when its host queue wakes it, and on a 60 second backstop pass,
  and overlays a newer revision of the **same cluster id** onto
  `Config.events` (and, for `core.output`, `Config.output`). A stale read cannot replace a newer body. Revisions from
  a different cluster are not compared. Switching databases is an explicit
  swap and installs that cluster's document even when its revision number
  is lower;
- applies its own successful write immediately, under the same monotonic rule.

A database connection observes a committed revision as soon as the batch
commits. The wakeup only bounds when a running process applies it in
memory; a lost wakeup costs at most the backstop interval. Failed
validation and revision conflicts do not commit, do not write a change
notice, and are not logged as applied.

### Desired state versus observations

`configuration_documents` is desired state. `hosts.incarnation`,
`heartbeat_at`, `software_version`, `schema_state`, `compatible`, and the
capacity columns (`logical_cpus`, `cpu_max_quota_us`, `cpu_max_period_us`,
`memory_max_bytes`, `memory_current_bytes`, `memory_anon_bytes`,
`files_dir_free_bytes`, `scratch_bytes`) are observations from the last
heartbeat. They are not a `node.runtime` document and they are not a second
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
settings, and **not** copying `node-identity.json` when the new process should
be a distinct host.

### Schema compatibility

Library schema behavior is unchanged: unreleased checksum, fail closed on
mismatch, `SCHEMA_VERSION = 0`. A database newer than the binary (a frozen
version past its plan, or an unreleased pack with another checksum) is
refused, never migrated. A start that has DDL to apply does it under the
`schema.migrate` cluster lease (see *Singleton maintenance and the
migration lease* below), so concurrent starters produce one migrator. A host heartbeats only when
`current_schema_state` is the unreleased pack this binary applies. Document
schema versions are per namespace and start at 1. A document this binary does
not understand is not applied and is not rewritten.

### Authorization

Cluster and host configuration writes in this spike require an operator
principal (the local CLI or the daemon operator token on
`PUT /api/config/domains/core.events`, `PUT /api/config/domains/core.output`,
and `PATCH /api/settings` for `events.*` / `output.*`). Administrators and
members are rejected before SQL, including before a receipt replay.
`PATCH /api/settings` rejects a body that mixes `events.*`, `output.*`, and
file-backed keys before any authority is written. The response includes
`revision` on success and `current_revision` on conflict.

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

Every plugin declares a semver `version` in `plugin.toml`; a manifest
without one is not enrolled and `bookclerk plugin check` rejects it.
Enrollment pins a copy of the schema in `plugin_schemas(plugin_key,
schema_sha256, package_version, settings_json, recorded_at,
recorded_by_host)` and the version orders the pin:

- a newer local package re-pins the row;
- the same version with a different schema re-pins in place with a warning
  (an unreleased edit). Equality is a fingerprint of the normalized schema,
  not of the manifest bytes, so line endings or comments never move the pin;
- an older local package is **stale**: the row is untouched, enrollment
  skips the plugin, and the reconciler retires a running guest and records
  an `error` observation naming the pinned version and the host that pinned
  it. The operator upgrades that host's package or rolls the other host
  back.

Every write to the row is a compare-and-swap on the row that was read, so
two hosts pinning at once cannot leave an older version on top of a newer
one. Every enrollment pass, not only the one that moved the pin, reconciles
the plugin's instance documents against the pinned schema: settings the
schema no longer declares, declares as secrets, or whose value no longer
fits the entry's kind (a type or enum change), and secret refs for keys that
are gone or now plain, are dropped, so a document that lost a race or was
written under an earlier manifest is repaired on the next restart instead of
staying invalid. `CONFIG` resolution applies the same rules at spawn and
enforces `required` on secrets as well as settings. A plugin that declares
no non-secret settings needs no `[vars]` binding and spawns with an empty
`CONFIG`.

`PluginKey` stays versionless, so instances, deployments, and accounts
survive an upgrade. Writes through the API or CLI validate against the pinned
row, so an administrator on one host can configure a plugin that is only
installed on another. A spawn still validates the document against the
host's local manifest: unknown keys are dropped with a warning, a missing
required key is an `error` observation.

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
enable|disable [--node]`, `PUT /api/config/plugin-instances/{id}/deployments/
{node_id}`, and the Settings page switch write that row; nothing reads an
`enabled` key from TOML. The host's reconciler spawns present deployments,
retires a running guest when desired flips to absent, and records
observations (`materialized`, `running`, `healthy`, `error`, `retired`). A
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
`node-identity.json`, `daemon.listen`. Database-adapter plugins are not
instances in this phase. The shared `[output]` table (format, Widevine,
naming profile, sidecars, multi-destination policy) has since moved to the
`core.output` cluster document; only its host-local `in_progress` scratch
directory stays in TOML (see *`core.output`* under *Decision*).

KV and Queues are unchanged. Named SQL bindings do not replace them.

## Phase 3 decision: Workers-parity platform services

Decided 2026-10-13 with the owner; implemented in a follow-up stacked on the
Phase 2 change. The goal is a host that can place any plugin instance on any
host, or subset of hosts, as many times as needed, with plugins reacting to
each other only through events the core defines, and plugin development fully
decentralised.

### What is already in place

The manifest is Wrangler-shaped: tables name bindings on `env`, and the guest
exports handlers the host invokes (`job(controller)` for `[triggers]`,
`event(batch)` for `[[events.consumers]]`). `domain_events` is a durable
outbox with fenced delivery, leases, retries, dead-lettering, and per-node
subscriber catalogs (`event_deliveries`, `event_subscribers`). The host
publishes `book_acquired` through the same outbox a plugin's `EVENTS` binding
uses, and Audiobookshelf consumes it in `event(batch)`; nothing in the host
names an integration. That is the model Phase 3 extends. It does not add a
second way to declare interest.

### Decisions

- **No `[[emits]]` / `[[subscribes]]`.** Plugins declare bindings and
  implement handlers, as Workers do. `[[events.producers]]` and
  `[[events.consumers]]` remain the bindings of the domain-event bus; the
  event catalog (types, payload schemas, versions) is core-owned. A plugin
  that does not export a handler for a binding it declares fails to load.
- **Queues parity.** `[queues]` with `[[queues.producers]]` and
  `[[queues.consumers]]` (declared today, "not implemented yet") is
  implemented on the outbox tables, keyed by queue name:
  `env.<BINDING>.send()` / `sendBatch()` and a `queue(batch)` handler with
  per-message `ack` / `retry` mirroring `event(batch)`. The domain-event bus
  becomes one well-known queue the host publishes to; `event(batch)` stays
  as its typed alias. Delivery semantics are those `event_deliveries`
  already implement.
- **KV parity.** `[[kv_namespaces]]` (declared today, "not implemented yet")
  is implemented on the library database, scoped per plugin key and
  namespace, with the Workers surface (`get`, `put`, `delete`, `list`,
  expiration, metadata). Plugin cursors, tokens, and caches move there from
  the per-host `plugin-state` directories, which remain scratch only. This is
  what makes an instance host-independent.
- **The host runs on the same services.** Deployment changes, configuration
  change wakeups (the Phase 3 item in *Remaining work*), and retire signals
  are queue messages host subsystems consume; the reconciler on each host is
  a consumer. There is no second coordination mechanism beside the database.
- **Scheduler.** `plugin_deployments` stays the scheduler's output. It gains
  a replica count and placement constraints (host labels; a `local`
  destination is bound to a disk, a workerd-only plugin to a host with the
  pinned runtime). Enrollment stops writing "this host, present" once a
  scheduler exists; until then it keeps doing so.
- **Cluster installs, not host installs.** "Installed" is a cluster
  record, the way a Workers version is a record and every colo materializes
  it on demand. A `plugin_versions` row per `(plugin_key, version)` carries
  the manifest, the settings schema, one artifact per target with its
  digest, the publisher verification result, and the consent grant; a
  `plugin_installs` row names the active version per plugin key. The node's
  `plugins/` tree is a content-addressed, immutable cache of version
  artifacts. There is no in-place replace, no
  host install ledger to repair, and no pre-commit health gate: health is
  the deployment observation it already is, and rollback is a pointer flip.
  Decided 2026-10-14 after reviewing the stage-and-commit installer in #255
  (closed): its crash window had moved to the commit step and its machinery
  (pending markers, backups, ledger repair) is not needed once the record of
  truth is the database. See *Installs and materialization* below.
- **Out of scope.** Durable Objects, R2 (destinations are the storage
  abstraction), and cron triggers (jobs cover them). Database adapters as
  instances stay Phase 4.

### Installs and materialization

A version is installed once and materialized by every node that is placed
on it. `bookclerk plugins install <coordinate|--archive>` resolves and
verifies the package exactly as the host-local installer did (publisher
trust, archive digests, manifest validation, `[[settings]]` schema), records
the consent grant the operator approved, and writes the `plugin_versions`
row and, on a first install or `--activate`, the active install. Nothing on
the installing node's disk changes. The artifact bytes stay where the
version row points: the registry URL, an operator bucket, the
`plugin-packages/` directory, or the KV store once it exists. Blobs in the
database are a fallback for small packages only (D1 rows are capped well
below a Rust guest binary).

A node materializes a version when its reconciler sees a present deployment
whose active version it has not cached: fetch the artifact for its target
(no artifact for this target is an `error` observation naming the target, so
placement constraints can exclude the host), extract under
`plugins/.staging/<digest>.<pid>`, verify the payload-root digest against
the version row, rename to `plugins/<digest>`, and spawn from there. A
partial extract never reaches the final path, so a crash leaves only
`.staging` garbage to sweep; a cached digest is immutable and reused by every
instance of that version on the node. Digests no deployment references are
collected after a grace period.

What changes in the trust model: a host trusts the database, which is
already the authority for configuration and secrets, instead of its own
`install-ledger.json`. Publisher verification and
`allow_unverified_publisher` happen once at install time and are recorded on
the version. `evaluate_install_in` becomes "payload digest matches the
version row"; `require_key_derived_install_path` and the receipt file go
with the ledger. Consent moves from the per-host `plugin-grants.json` into
the version record (approved once; a new version whose requested
capabilities widen needs re-approval, as `grant_covers` enforces today).

What stays host-local: platform-bundled guests beside the binary (`sqlite`,
`local`, `s3`, the helpers) and the database adapter that must exist before
the database is open. The bootstrap exception already records both. The
local mutation lock still serializes materialization and sweeping on one
host; `require_mutable_child` keeps guarding every path under `plugins/`.

Schema pinning simplifies: the pinned schema is the active version's schema.
A node no longer has "an installed version" independent of the cluster, so
the newer-re-pins / older-is-stale comparison from Phase 2 reduces to
"materialize the active version". The comparison code stays until the
cluster install path lands, then goes.

`bookclerk plugins update` installs a new version and activates it;
`bookclerk plugins rollback <plugin> <version>` activates an earlier one,
which is instant on nodes that still cache the digest. `bookclerk plugins uninstall`
clears the pointer and deployments; the cache sweep removes the files.

### Implementation plan (stacked PR)

Ordered so each step is testable on its own and the branch builds at every
commit; the Phase 2 PR carries versioned schema pinning because the rest
depends on hosts agreeing on a schema.

1. **Queues on the outbox.** Type `[queues]` in the manifest; add
   `queue_name` to `domain_events` and `event_deliveries` (the domain bus is
   `core.events`); route `queue(batch)` through the workerd bridge beside
   `event(batch)`; `env.<Q>.send()` in the SDK; consumer catalog rows per
   queue; conformance tests in `bookclerk-plugin-e2e`; `bookclerk queues
   list|inspect` and `GET /api/queues` for operators.
2. **KV on the library.** `plugin_kv(plugin_key, namespace, key, value,
   metadata, expires_at)`; the `KV` binding in the workerd bridge and native
   RPC; SDK surface; size and count limits in `[[kv_namespaces]]`; a
   two-connection test proving an instance moved to another host reads its
   own cursor.
3. **Node subsystems as consumers.** `node.<node_id>` queues; the reconciler
   and the configuration-change wakeup consume them instead of the 5 s tick;
   `deployment.changed` published on every deployment write; the CLI stays a
   producer only.
4. **Installs and materialization.** `plugin_versions` and the active
   install; install from `bookclerk plugins install|update|rollback` and
   `POST /api/config/plugins`; the reconciler materializes by digest
   under `plugins/<digest>` and spawns from the cache; `install-ledger.json`,
   `receipt.json`, and per-host `plugin-grants.json` retire; a
   two-connection test proving a version installed from one node is
   materialized and spawned on another.
5. **Placement.** `replicas` and `constraints` on `plugin_deployments`;
   `hosts.labels`; `bookclerk plugins deploy <plugin> --replicas N --on
   <label>`; the Settings page shows placement per instance. The scheduler
   is a singleton maintenance lease (already listed under Phase 3) that
   writes deployments; each host's reconciler converges as today.
6. **Docs and ADR.** `plugins.md` loses "not implemented yet" for KV and
   Queues and describes cluster installs instead of host installs; this section records
   any deviation.

Steps 1, 2, and 4 are independent of each other and of 3; 5 needs 3 and 4.

### Implementation notes and deviations

All six steps landed. Where the code differs from the plan above, this
section is the record.

- **Queues (step 1).** As planned. A queue message carries no
  `event_type`; the outbox dedup key includes `queue_name`. Operators read
  queues through the daemon (`bookclerk queues list|inspect`,
  `GET /api/queues`).
- **Host-emitted events.** Event types the host emits itself
  (`book_acquired`) are reserved: manifest validation, both SDK validators,
  and the outbox refuse a plugin producer for them. When a storefront's
  acquire handler returns, the host publishes `book_acquired` in the same
  transaction that records the acquire status; the plugin never emits it.
- **KV (step 2).** As planned. Rows are written through the SeaORM entity
  (canonical `?` SQL only runs through the adapter proxy, not on a direct
  PostgreSQL pool). Expired keys read as missing and the daemon prunes them
  hourly.
- **Host queues (step 3).** The deployment and configuration reconcilers
  are woken by their host queue and keep a 60 second backstop pass (the
  5 second tick is gone). Wakeups are best effort after commit: a publish
  failure is logged, not returned, because the write already committed.
- **Version cache path (step 4).** Materialized trees live under
  `plugins/.versions/<payload-digest>` and partial extracts under
  `plugins/.staging/version-<digest>.<pid>`, not `plugins/<digest>`. The
  leading dot keeps discovery and alias scanning, which walk `plugins/`,
  from treating a cache entry as an installed plugin.
- **Host-local trees remain (step 4).** The ledger, receipts, and
  `plugin-grants.json` retire for cluster-installed plugins only. Platform
  guests, development trees staged by `cargo stage-plugins`, and host-local
  installs made before cluster installs still run from `plugins/<fs-id>`
  with a host grant. No
  operator command writes a host-local install any more. The reconciler
  persists a verified platform guest's grant before its first spawn
  instead of refusing for want of one.
- **Operator packages (step 4).** A package under `plugin-packages/` is
  installed as a version the first time a present deployment needs it,
  offline, carrying the key's existing host grant onto the version row.
  `POST /api/config/plugins` installs one by directory name.
- **Artifact location (step 4).** Archives up to 1 MiB are stored in
  `plugin_artifacts` (`blob:sha256:<hex>`). A larger local archive is
  referenced in place as `file://`, which only hosts that see the path can
  materialize. A shared artifact store is not part of this phase.
- **Local archive keys (step 4).** A local archive's path-derived key
  names its first install only; a later archive of an alias that is
  already installed reuses that alias's key, so `install --archive` of a
  new version is an update rather than a second plugin. `plugins update`
  skips a version with no recorded coordinate.
- **Install flags (step 4).** `install --replace` is kept as an alias of
  `--activate`. `versions`, `deploy`, `undeploy`, `uninstall`, `instances`,
  `config`, and `db` are reserved `plugins` verbs so a plugin CLI cannot
  shadow them.
- **Terminology.** The first cut said "release" and "publish" for a
  cluster install and "host" for a cluster member. Both read as
  infrastructure jargon, so the code, schema, API, and CLI say *install* (a
  `plugin_versions` row; `plugin_installs` names the active one;
  `plugin_artifacts` holds small archives), *deploy* (a deployment row on a
  node), and *node* (`nodes`, `node_status`, `node.runtime`,
  `node-identity.json`, `node.<id>` queues). `plugins enable|disable` folded
  into `plugins deploy|undeploy [--node <id>]`, `remove` became
  `uninstall`, and `releases` became `versions`. The operator API is
  `/api/config/plugins` (list and install), `/api/config/plugins/{key}`
  (read and uninstall), `/api/config/plugins/{key}/active` (activate), and
  `/api/config/plugins/{key}/versions/{version}/approve`.
- **Placement input (step 5).** Replicas and constraints live on a new
  `plugin_placements` row per instance (the scheduler's input), not on
  `plugin_deployments` (its per-host output). A count stored on every host
  row could disagree between rows.
- **Liveness (step 5).** The scheduler first treated a host as live when
  its `node:<node_id>` queue consumer refreshed its event-catalog row
  within 60 s. `nodes.heartbeat_at` is written only at bootstrap, so it
  cannot tell a running daemon from a host where the CLI once ran. The
  periodic host heartbeat below replaced both.
- **Labels and targets (step 5).** Labels are `name` or `name=value`; every
  host also carries `node=<id>` and `target=<target>`; a constraint may
  exclude with `!`. Beyond the constraints, the scheduler skips a host
  whose target has no artifact in the active version (an `any` artifact
  matches every host). Labels are set with `bookclerk nodes label` and
  `PUT /api/config/nodes/{node_id}/labels`.
- **Scheduler lease (step 5).** The singleton is the `scheduler` row in a
  new `cluster_leases` table (holder, fencing token, expiry). Renewal keeps
  the token; takeover after expiry bumps it. Deployment writes stay
  compare-and-swap, so a holder whose lease lapsed mid-pass can at worst
  add one replica that the next pass removes.
- **Singleton maintenance and the migration lease (#192).** The lease
  code moved out of the scheduler into `control_plane::leases`: acquire
  and renew (compare-and-swap on holder and token), a fencing renew that
  also requires the lease to be unexpired, release, a list for status, a
  `LeaseFence` that destructive work checks before each step, and
  `SingletonDuty` ("one live node runs this every interval"). Cluster
  cleanup that every daemon used to run on its own now runs under one lease
  per duty: `maintenance.jobs` (terminal jobs; previously pruned on every
  job-claim loop), `maintenance.events` (delivery and event retention,
  stale catalog rows), `maintenance.kv` (expired KV rows), and
  `maintenance.sessions` (expired operator sessions, OIDC RP states,
  WebAuthn challenges). Every daemon polls every 60 s; the holder renews on
  each poll and runs a duty when it takes the lease and hourly after. The
  lease TTL is 180 s, so a dead holder is replaced within three minutes.
  Fencing is check-then-act, not transactional (D1 has no interactive
  transactions); the residual window is one idempotent delete. Expiry uses
  each node's wall clock. Login, OIDC, and receipt paths still prune
  opportunistically inline; that is bounded and idempotent.
  Schema migration runs under `schema.migrate`. A start whose database
  already matches only reads the state and takes no lease. One that needs
  DDL (uninitialized, or a frozen step pending) first creates
  `cluster_leases` (the same `IF NOT EXISTS` statement as in the pack),
  then takes the lease with a per-attempt holder (`pid<pid>-<nonce>`; the
  node id does not exist yet at that point), re-reads the state, renews the
  lease every 40 s, and checks it before every DDL batch. Other starters
  poll until the schema matches, take over when the migrator's 120 s lease
  expires, and fail closed after 300 s. The released row keeps the last
  migrator's holder and token. `GET /api/cluster/status` reports every
  lease (`cluster.leases`) and the database schema state with whether this
  binary matches it (`cluster.schema`). There is no drain-then-migrate
  flow: a starter does not stop other nodes before it migrates, which is
  safe today only because unreleased schema changes require a new
  database.
- **Backups have no periodic duty.** Recovery points are taken by `bookclerk
  db backup` and automatically before a schema migration, which already
  runs under the migration lease. No daemon schedules backups, so there is
  no backup lease; a scheduled backup would be a `SingletonDuty`.
- **PostgreSQL reconnect.** An ignored test
  (`postgres_connection_killed_mid_commit_replays_once_after_reconnect`)
  stalls a configuration write inside `COMMIT` with a deferred constraint
  trigger, terminates its backend from a second pool, and retries the same
  operation id on the reconnected pool: the killed commit left nothing, the
  retry applies once, and a further retry replays the receipt.
- **Hand-managed versus placed (step 5).** A placed instance refuses
  per-node `deploy`/`undeploy` and the deployment API (409), enrollment
  writes a new node's row `unplaced` for it, and `plugins deploy --manual`
  hands the deployments back unchanged (`placed` → `present`, `unplaced` →
  `absent`). Uninstalling a plugin drops its placements first.
- **Jobs (after step 5).** Durable jobs record the instances they need and
  are claimed only by a node that runs them, under counted cluster
  semaphores built from `cluster_leases` slot rows. See *Job routing and
  cluster semaphores* below.

### Resource-aware scheduling and routing

The first placement pass spread replicas by how many deployments each node
already ran, and nothing routed calls. A cluster of unequal nodes (a small
always-on VPS and desktops that are often off, none able to reach another)
needs the scheduler to know each node's capacity, what is committed there,
and how reliably it is up, and needs calls to land on the best node
running a plugin. Nodes still only share the database.

- **Whose decision a deployment is.** `plugin_deployments.desired` is
  `present | absent` (operator) or `placed | unplaced` (scheduler). Reusing
  `absent` for "the scheduler took it off this node" read as an operator
  decision and lost the distinction on hand-back. The library refuses a
  scheduler write of an operator state and the reverse; bootstrap
  enrollment may write `unplaced` for a placed instance.
- **A periodic node heartbeat.** There was no heartbeat to remove: node
  capacity was sampled at bootstrap and reload only (into `nodes`), and
  liveness came from the event-catalog row. Every daemon now writes
  `node_status` every 15 s: detected capacity (CPUs and memory, capped by
  its cgroup), its budget, the reservations of the guests it runs, measured
  CPU and available memory, free disk, and availability. Live means a
  heartbeat within three intervals. Only the daemon writes it; the CLI
  never makes a node live.
- **Budgets are operator configuration.** `node.runtime` gains
  `cpu_budget_percent` (default 80), `memory_budget_percent` (75), and
  `schedulable` (drain). The heartbeat turns them into absolute budgets
  against what it detected, so a budget follows a resized VM or cgroup
  without an edit. Edited through `bookclerk nodes set`, `PUT
  /api/config/nodes/{id}/runtime`, and Settings → Nodes.
- **Accounting by ceilings.** A node's reservations are the sum of the jail
  ceilings (after `[plugins.jail]`) of its running guests, recorded on each
  deployment observation when the reconciler spawns the guest. Ceilings,
  not samples, because the jail enforces them and a guest's peak is what
  must fit. Measured use is reported beside them: CPU headroom subtracts
  the larger of the two (other work on a desktop counts); memory headroom
  is never more than the machine has free.
- **Availability.** A decaying average of time online (time constant one
  day), updated from the gap between heartbeats. It discounts, rather than
  excludes, nodes that come and go.
- **Placement by headroom.** An instance's demand is the largest ceiling
  any node recorded for it, else its version grant, else the grant
  defaults. A node's score is how many more such guests fit its CPU and
  memory headroom, times `0.5 + 0.5 × availability`. Memory is a hard fit
  (a guest over its ceiling is killed); CPU only ranks. Replicas stay
  while their node qualifies; scale-down drops the least room first; one
  replica per instance per pass moves off a node that is over its memory
  budget. A pass commits each added replica's demand against its node so
  several instances do not land on one node in the same pass.
  `replicas = all` (stored as NULL) runs on every qualifying node where the
  demand fits.
- **Routing deliveries without node-to-node calls.** Every node that runs
  an instance ranks the live nodes whose guest of it is up by the same
  score, so all nodes compute the same order from the same rows. Rank `r`
  claims a delivery only once its `run_after` is `r × 10 s` old
  (`claim_next_event_delivery_due_by`). The best node takes work at once;
  if it is saturated, stalled, or gone, the next takes over after a short
  delay, and the claim's compare-and-swap still admits one runner.
  Rejected: stamping a target node on each delivery at dispatch (the
  dispatcher would have to choose, and a dead target needs reassignment
  logic) and forwarding between nodes (they cannot connect).

### Job routing and cluster semaphores

Built 2026-10-15 (#192 Phase 3). Durable jobs were claimed by whichever
daemon got to the row first, which ran them with its own sources and
destinations, and the per-process `[jobs.concurrency]` limits added up
across nodes.

- **Requirements, recorded at admission.** `JobPayload.requires` lists the
  plugin instance ids the claiming node must run: the pinned instance (when
  some node deploys it) and, for acquire, every deployed instance of a
  node-bound output (the first-party `local` destination). No new state:
  the claim reads `plugin_deployments` (desired `present` or `placed` on
  this node) and `plugin_deployment_observations` (this node reports the
  guest `running` or `healthy`), as delivery routing does. A job with no
  requirements stays claimable anywhere. Rejected: stamping a target node
  at admission (a dead target would need reassignment, as for deliveries).
- **Routed claim.** `claim_routed_job` reads ready candidates, filters them
  host-side, takes the permits, and claims one row with a single CAS on
  `state = 'pending'` and the lease generation (`claim_job_by_id`). That
  CAS needs no interactive transaction or job-queue slot lock, so it runs
  the same on every adapter through SeaORM. The unrouted `claim_next_job`
  and its guest atomic plan skip any job with requirements. A lost-response
  replay can leave a claimed row unexecuted; it expires and is reclaimed,
  which is the fallback the unrouted claim already documented.
- **Counted semaphores on `cluster_leases`.** A semaphore of limit `N` is
  `N` slot rows `sem:<name>#<i>` of the existing lease table, each a fenced
  lease; taking a permit is taking a free or expired slot below the limit.
  No table was added and the singleton scheduler lease is the `N = 1` case.
  Job claims take `jobs:<class>` (`[jobs.concurrency].network`, which is now
  also the cluster-wide count), `plugin-instance:<id>`
  (`[jobs.concurrency].plugin_instance`, default 1), and
  `account:<plugin_key>:<account_id>` (1). Permits are renewed with the job
  heartbeat (a failed renewal is fence loss) and released after
  finalization; a crashed holder's slot expires and is taken over with a
  larger token. Counting running `jobs` rows (as `event_deliveries` does
  for `[events.concurrency]`) was rejected because one job counts against
  several semaphores and the account a job touches is not a column.
- **Requirements sweep** (follow-up to #267). A singleton loop under the
  `jobs:requirements-sweep` lease re-resolves pending jobs' requirements
  with the daemon's discovered node-bound output keys and CAS-writes the
  payload (bumping the lease generation, so it never overwrites a claim).
  A set that only gains instances and still has a common node applies at
  once; anything else waits a 10-minute grace period stamped in the payload
  (`requires_unsettled_since_ms`), then drops retired instances or fails the
  job as `unroutable`. Rejected: counting consecutive unsettled sweeps,
  because sweeps also run on wakeups and the count would shrink with load.
- **Limits stay in `config.toml`.** `[jobs]` has not moved to a database
  document, so each node applies its own limits; nodes should agree.
  Lowering a limit lets held slots above it drain rather than revoking them.

### Node kinds, ownership, and the Host role

Decided 2026-10-14 with the owner. A cluster has two kinds of node:

- **Owner nodes** share the library database and `master.key` and give the
  cluster its HA. Every node described above is one; `nodes.kind` is
  `owner`.
- **Host nodes** are independent Bookclerk installs with their own database
  and their own operator, who enroll in an owner cluster to contribute
  work. They reach an owner node over HTTPS and never hold the cluster's
  database credentials or DEK. Enrollment, the transport policy, the
  node socket, the work frames, the host client, host consent, and
  results and uploads are built (sections below; operator guide in
  [Libraries and branches](../server/cluster-hosts.md)). A redeemed
  enrollment link records the node with `nodes.kind = host`.

Who may administer a node is separate from its kind. `nodes.owner_user_id`
names the user who owns it, assigned by an operator (`bookclerk nodes
owner`, `PUT /api/config/nodes/{id}/owner`), or the user enrolls a host
node of their own. Any active user can own a node (decided 2026-10-15,
below); a disabled user cannot, so that write is refused. That user may change the
node's `node.runtime` document (budgets, drain switch, display name)
without operator authority. The library enforces it: a write carries
`ConfigActor::NodeOwner`, which only `verify_node_owner` builds after it
reads `nodes.owner_user_id`, and which covers exactly that node's
`node.runtime` document. Placement labels, deployments, instances, and
every other document stay operator writes. An unknown node and a node owned
by someone else return the same 403, so a non-owner cannot probe node ids.

A **Host** user role marks a Member who runs nodes; owning a node does not
require it. Hosts, Administrators, Owners, and any other user who owns at
least one node reach `GET /api/config/nodes`,
`PUT /api/config/nodes/{id}/runtime`, and `GET /api/cluster/status` through
`require_operator_or_node_owner_auth`, which scopes them to their own
nodes. The cluster summary in `/api/cluster/status` (node counts, committed
capacity, active jobs, delivery backlog) is visible to every node
administrator. Only an Owner or the operator assigns the Host role.

- Trust follows the role. A Host's node receives only cluster work whose
  secrets belong to that Host (accounts linked to the user); promoting the
  user to Administrator lets the node receive any job. The check runs per
  work item at the owner when the node claims it. Built for jobs (H-4
  step 5, *Per-item trust and the scan planner* below); deliveries and
  instance ownership are not yet.

Decided then, built since in H-4 steps 3 and 8-10 (sections below):

- A node may configure its own `local` output, an instance owned by that
  node and deployed only there. It adds to the cluster's outputs rather
  than replacing them; writes stay mirrored.
- Outputs that need the owner's credentials (S3 keys, the owner's own
  `local` disk) are not run on a Host's node. When a Host's work finishes
  there, the node writes its own local output and uploads the result
  through a secure upload endpoint on an owner node, which writes the
  cluster's outputs with credentials the Host never holds. Both nodes end
  up with a local copy.
- Host nodes and the SPA use WebSockets rather than polling.
- Owner nodes serve the node API over Let's Encrypt TLS, behind a detected
  TLS-terminating proxy, or only within loopback and private address space.
  (Amended 2026-10-15: plain HTTP is allowed on loopback only; a private
  network uses a pinned self-signed certificate.)

### Host-node enrollment and transport

Decided 2026-10-15 with the owner, for the H-4 design:

1. **Transport.** TLS everywhere except loopback. A connection to the node
   API is allowed as `acme` (the built-in rustls listener with a Let's
   Encrypt certificate), `proxy` (plain HTTP from a peer in
   `daemon.trusted_proxies` that sends exactly one `X-Forwarded-Proto:
   https`, with an https `integrations.public_origin`), `self_signed` (the
   built-in listener with a self-signed certificate whose SPKI pin travels
   in the enrollment link), or `loopback` (both ends loopback, no
   forwarding headers). Everything else, a private RFC 1918 peer over plain
   HTTP included, gets 403 `transport_insecure`.
2. **Scan-all.** Only Administrators and Owners trigger it. The owner's job
   planner splits it into one job per account and distributes those under
   the per-item trust rule; anyone else's scan covers only their accounts.
3. **Token write-back.** A host may write back a refreshed store token only
   for a secret ref handed out under a live fence.
4. **Uploads.** Streamed through the owner to each output destination,
   never staged on an owner node's disk.
5. **Enrollment.** The operator or an Owner turns host enrollment on or off
   for the cluster (`core.host_enrollment`, on by default). While it is on,
   any active user may mint a link for themselves and enroll their own
   host; the operator or an Owner may mint for any active user. The node's
   owner is that user. Owning a node no longer requires the Host role;
   trust still follows the role, so a Member's or Host's node only receives
   work whose secrets belong to them.

Built in the first H-4 slice:

- Owner-database tables `node_enrollment_links` (only `hash_token(secret)`
  is stored) and `node_credentials` (Ed25519 sign key, X25519 box key,
  `revoked_at`), and the host-database table `cluster_links` (written by
  the host client, below). All are canonical SQL-v1 with SeaORM entities.
- `core.host_enrollment` (`{ "enabled": bool }`), a cluster document that
  defaults to enabled while absent. Only the operator or a verified Owner
  (`ConfigActor::ClusterOwner`, built by `verify_cluster_owner` from the
  stored role) may write it; that actor writes no other document.
- Mint, list, and revoke links (`bookclerk nodes invite --user <id|login>
  [--ttl 24h]`, `bookclerk nodes invites list|revoke`, and
  `POST|GET /api/config/node-invites`, `DELETE …/{id}` for any signed-in
  user). Revoke an enrolled host with `bookclerk nodes revoke <node_id>` or
  `DELETE /api/config/node-credentials/{node_id}` (the operator or an Owner
  for any host, anyone else for their own). A link is `https://<public_origin>/enroll#bcn1.<link_id>.<secret>`;
  the 32-byte secret sits in the fragment, is printed once, and is never
  logged. Links live 24 h by default, 5 minutes to 7 days.
- Redeem is one canonical batch: the claim `UPDATE` re-checks the hash,
  expiry, single use, revocation, the link user's status, and that
  `core.host_enrollment` is still the revision read as enabled; the same
  batch inserts the `nodes` row (`kind = host`, owned by the link's user),
  `node_credentials`, and a receipt. A retry with the same secret and keys
  answers from the receipt; anything else is one generic 403. Two
  concurrent redeems create one node on SQLite and PostgreSQL.
- `bookclerk nodes revoke <node>` (or the node's own owner) sets
  `node_credentials.revoked_at` and publishes `node.revoked` to every node
  queue; the owner node holding that host's socket closes it with 4403.
- `classify_transport` in `bookclerkd` (`node_api/transport.rs`), a pure
  function of the connection, headers, and the bootstrap `[daemon.node_api]`
  table (`mode`, `acme_contact`, `tls_listen`). `POST /api/node/v1/enroll`
  runs it first and sits outside the cookie and auth layers. The ACME and
  self-signed listeners came in a later step (below); an incomplete
  `[daemon.node_api]` refuses every connection.
- Enroll checks that both public keys are canonical Curve25519 points in
  the prime-order subgroup (`bookclerk-node-proto::validate_*_public_key`),
  so small-order, mixed-order, twist, and non-canonical keys are refused
  before the link is spent.
- The node socket, `GET /api/node/v1/connect` with subprotocol
  `bookclerk-node.v1`, outside the cookie and auth layers (operator
  `auth.enabled = false` does not open it). `classify_transport` runs
  before the upgrade. The `bookclerk-node-proto` crate holds the JSON
  frames (`{v:1, id, type, ...}`, 1 MiB, text only) and the handshake:
  `hello {node_id, incarnation, software_version, resume}` → `challenge
  {nonce, cluster_id}` → `auth {sig}`, an Ed25519 `verify_strict`
  signature over `bookclerk-node-auth-v1` and length-prefixed cluster id,
  node id, incarnation, 32-byte nonce, and the owner's canonical origin
  (its configured `public_origin`, so a signature relayed through another
  server fails). The owner always sends a challenge, so an unknown node and
  a bad signature both close with 4401 and the one reason `auth_failed`
  (the detailed reason is only in the audit row and the log); a valid
  signature from a revoked credential closes with 4403. Each refusal writes
  a `security_audit_events` row (`node_auth_failed` / `node_auth_revoked`).
  A welcomed node stamps `node_credentials.last_connected_at`; the
  connection is registered for `node.revoked` wakes before that stamp, so
  a revoke mid-handshake still closes it. The session
  pings every 20 s, closes after 60 s idle, re-reads `revoked_at` on every
  ping tick, closes on a `node.revoked` wake with 4403, lets a newer
  connection for the same node supersede the old one (4409) once the newer
  one is welcomed (a reconnect refused earlier leaves it up), and closes on
  a binary or malformed frame (4400). The owner serves the work frames
  described in *Work frames, sealed secrets, wakes and resume* below; a
  request frame of a type it does not serve is answered
  `error {code: "unsupported"}`.
- The built-in TLS listener (`node_api/tls.rs`) on `tls_listen`, serving
  only the node API routes over HTTP/1.1 and tagging requests for
  `classify_transport`. `acme` uses `rustls-acme` (TLS-ALPN-01 on the same
  listener, Let's Encrypt production by default, `acme_directory` /
  `acme_directory_ca` for staging or a private CA) with its cache under
  `$FILES/acme/` (0700). `self_signed` generates a P-256 certificate once
  under `$FILES/node-api/self-signed/` (0700, key 0600) and writes its SPKI
  pin beside it; `POST /api/config/node-invites` and `bookclerk nodes
  invite` append the pin to links, and refuse to mint (503
  `node_api_pin_unavailable` / a CLI error) while the pin cannot be read.
  In both modes the node API stays unmounted on every listener (404
  `node_api_unmounted`) until the listener has a certificate, so an ACME
  failure never falls back to plain HTTP. A Pebble issuance test runs when
  `BOOKCLERK_TEST_ACME_DIRECTORY` / `BOOKCLERK_TEST_ACME_CA` are set; CI does
  not run Pebble yet.

### Per-item trust and the scan planner

Built 2026-10-15 (H-4 step 5), on the routed claim above.

- **Trust predicate** (`control_plane/trust.rs`). `node_trust` reads the
  claiming node's `nodes` row, its `node_credentials`, and its owner's
  `users` row on every call: an owner node (stored kind exactly `owner`)
  and a Host owned by an active Owner or Administrator are trusted with
  every job; a Host owned by any other active user is trusted only with
  that user's work; a Host with no owner, a disabled or unknown owner, an
  unparsable role or status, or a missing or revoked credential is trusted
  with nothing. Any kind other than `owner` is judged as a Host. A node id
  with no row is trusted only for a `Claimant::Local` claim (the daemon's
  own worker, which holds the database); a `Claimant::Remote` claim (the
  node API) without a row gets nothing.
- **What "that user's work" means.** `job_trusted` admits only a scan or
  acquire that names a store account, every `accounts` row of which is
  linked to the user through `account_links` (via the user's
  `portal_identities`); an account id another user holds on a second
  plugin is refused, since the job would touch both. A scan-all or
  acquire-all with no account, a job pinned to or requiring a plugin
  instance (instances carry no `owner_user_id` yet, so their settings are
  the operator's), listen-sync, integration scans, plugin copies, and
  invalid jobs are refused. Rejected: recording the requesting user on the
  job and trusting by requester; a Host is trusted with secrets, and the
  secrets a job unseals are its accounts', whoever asked.
- **Where it runs.** `claim_routed_job` takes a `claimant` and filters
  candidates through the predicate before requirements, permits, and the
  CAS, so an untrusted job is skipped rather than claimed and refused. A
  node trusted with nothing reads no further. `handoff_routed_job` re-reads
  the job and the trust inputs just before secrets would be unsealed for
  the node. It hands over only while the claim still owns the row: state
  `running`, the claim's lease owner (which must be the requesting
  worker) and generation, and an unexpired lease; a reclaimed, finished
  or expired claim gets nothing, and a claim under another worker's owner
  is refused without touching the row. If the node is no longer trusted
  (demoted, disabled, unlinked, revoked), or its own lease expired,
  `release_job_claim` returns the row to `pending` under the
  claim's fence without spending an attempt (a resumed claim gets its
  one-shot resume flag back), the permits are released, and nothing is
  handed over. The lease generation stays bumped, so the refused fence
  can never write again. The daemon's own worker claims as
  `Claimant::Local` and needs no handoff; the node API's work frames
  (step 6) claim as `Claimant::Remote` and hand off through it.
- **Scan planner** (`plan_scan`). Every scan admission goes through it,
  from the HTTP API (`require_operator_or_administrator_auth` records a
  `ScanRequester` request extension; a handler without one answers 403),
  the scheduler, and later a Host. Operator authority, an Owner, or an
  Administrator scanning every account gets one scan job per distinct
  scan-enabled account id (within the pinned instance's plugin), so each
  can go to any node trusted with that account; with no scan-enabled
  account the request stays one unsplit job, which only fully trusted
  nodes claim. Any other active user gets only the scan-enabled accounts
  they wholly own, and a named account must be theirs (403 otherwise; an
  unknown account reads the same). `POST /api/library/scan` answers a
  multi-job plan with `job_ids` beside `job_id`: 200 when any was
  accepted, 409 when all were already queued, 429 when the queue was full,
  422 `nothing_to_scan` for an empty plan. Each per-account job dedups on
  its own key, so a repeated scan-all coalesces per account. The HTTP gate
  still admits only operators, Owners, and Administrators; a member scan
  route would rely on the planner's scoping.
- **Tests.** Two SQLite connections to one file, with PostgreSQL twins
  behind `BOOKCLERK_TEST_POSTGRES_URL`: a Member's Host claims its owner's
  account and is refused another user's job and the scan-all until the
  owner is promoted; a demotion between claim and handoff denies and
  returns the job unspent; ownerless, disabled, revoked, and unknown nodes
  claim nothing; the planner splits and scopes as above.

### Work frames, sealed secrets, wakes and resume

Built 2026-10-17 (H-4 step 6), over the node socket and the claim above.

- **Work frames** (`bookclerk-node-proto` frames; `node_api/work.rs`;
  `control_plane/remote_work.rs`). `heartbeat` records `node_status` for
  the Host like any node. `claim {item: job}` runs `claim_routed_job` as
  `Claimant::Remote` with lease owner `host:<node_id>`, then
  `handoff_routed_job`, and only then unseals the job's secrets; it
  answers `work {job, fence, lease_secs, sealed_secrets}` or `none
  {retry_after_secs}`. `renew`, `complete` and `fail` name the job id and
  lease generation; the owner rebuilds the fence from them, so no other
  node can present it. Renewals re-read trust and the credential, extend
  the job lease, and renew the job's semaphore permits, which are found
  again by holder prefix (`permits_held_by`, below the serving node's
  limit and then up to `MAX_SEMAPHORE_LIMIT`, since the claim may have
  been made under a higher limit; an expired slot nobody took over still
  counts). A routed claim's permit holder is
  `<lease owner>/<job id>/g<generation>/<random>`
  (`job_permit_holder_prefix`), with the generation the claim runs at:
  `claim_job_by_id` succeeds only at the generation read with the
  candidate, so the claim's is known before the permits are taken. The
  prefix searched is the presented fence's, so a still-live permit left
  over from an earlier claim of the same job by the same node is never
  renewed or released in place of the current one; it expires with its
  own lease. A lost lease, a taken-over permit, or lost trust
  answers `lost`. The permits are checked before the job lease is
  extended, so a lost renewal never pushes the reclaim out; a permit taken
  over between that check and its renewal cuts the just-extended lease
  back to expire at once. `fail` is always accepted under a live fence;
  `complete` also needs trust and is refused for acquire, which completes
  only when its upload commits (step 9). Deliveries are not served yet
  (`claim {item: delivery}` answers `unsupported`).
- **Which secrets.** For a scan or acquire naming an account: every
  `integration` row of `encrypted_secrets` for each store account the name
  reaches (by id or label, ignoring case, as the executor resolves it), the
  same accounts the trust predicate checked. Other jobs get none; instance settings and
  secrets will travel with `instance.config` (step 7). The refs handed
  are recorded per lease generation in `job_secret_handoffs` (names only,
  no secret material) before the `work` answer goes out, in one batch
  guarded by the claim's fence (a claim that lost its lease while its
  secrets were read records nothing and is not answered), and dropped when
  the job finishes, is given back, or is claimed again. A job whose
  secrets do not unseal or seal (legacy formats, oversized bundle) is
  failed with `secrets_unavailable`, so it retries with backoff and a node
  holding the database can take it, rather than looping on the Host.
- **Sealed box.** No `crypto_box` or HPKE crate is in the tree, so the box
  is composed from primitives that are: X25519 (`curve25519-dalek`),
  HKDF-SHA256 (`hkdf` 0.13, `sha2`), and XChaCha20-Poly1305
  (`chacha20poly1305`). `box = E ‖ AEAD(key, nonce, msg, aad)` with key
  and nonce from `HKDF(salt = E ‖ R, ikm = X25519(e, R))`; an all-zero
  agreement is refused and the recipient key must pass the enrollment
  key checks. A job's bundle is bound by its associated data to the node,
  job, and lease generation, so it cannot be replayed into another claim.
- **Token write-back** (decision 3). The owner sends a fresh box key per
  connection in `welcome`; `secret.update {job_id, fence, ref,
  sealed_value}` is sealed to it and bound to node, job, fence and ref.
  It is stored (re-sealed with the owner's DEK under the same identity)
  only when, immediately before the write, the credential is live, the
  node is still trusted with the job, the fence is the job's live
  generation for this node, the ref was recorded as handed under that
  generation, and it is still one of the job's secrets. A secret created
  for the account after the handoff is refused, so a Host never
  overwrites a credential it was not given. The write is one host batch
  (one transaction on every backend, D1 included): a fenced update of the
  job row, then an update of the existing secret row guarded on the
  fence, the handoff row and the credential, so a reclaim, a claim by
  another node, or a revocation that lands after the checks leaves the
  token untouched. The trust predicate itself (the owner's role and
  status, and the account links) is checked just before the batch, not
  inside it, so a demotion, a disabled owner or an unlinked account that
  lands between the check and the batch can still let one update of a
  secret already handed under that fence through. Moving the whole trust
  predicate into the batch guard is follow-up work (issue #306).
  Rejected: plaintext values inside TLS. tungstenite traces every
  received message at TRACE, and the daemon bridges `log` into tracing
  with the diagnostics ring at TRACE, so a plaintext value would land in
  diagnostics. Sealed in both directions, no secret is ever frame text.
  Secret types redact in `Debug` and zeroize on drop.
- **Wakes.** While a Host's socket is live, the owner node holding it
  registers as the consumer of `node.<host_id>` (catalog row
  `node:<host_id>` on that owner node, heartbeated with the session),
  claims deliveries, and sends `deployment.changed`, `install.changed`
  and `configuration.changed` as `wake` frames; each is acked only after
  the Host's `ack`. Other messages are acked at once; unknown bodies are
  dead-lettered. On disconnect, unacked wakes are released unspent and
  the row is disabled, unless a newer connection for the same Host has
  registered. The two share the row, so registering with enabling it and
  unregistering with disabling it run under one lock, and only the
  connection still registered disables it.
- **Backpressure.** Everything the owner sends goes through one queue of
  64 frames drained by a writer task. A wake is claimed only after a
  queue slot is reserved, and at most 64 wait for acks; a wake unacked
  for its whole lease (60 s) stops counting, since the reclaimer returns
  its delivery for redelivery, and a late ack for it is ignored. With the queue
  full nothing is claimed, so deliveries stay pending or expire and are
  redelivered. An answer that cannot be queued within the idle timeout
  ends the connection; if it was `work`, the claim is given back unspent
  (`release_remote_job`), as it is for a `work` answer produced after
  the connection was told to close (superseded by a newer connection
  for the same Host, or revoked), so such a job does not sit leased to a
  Host that never learns of it until the lease expires.
- **Resume.** A hello lists held fences; after `welcome` the owner
  answers `resumed {renewed, lost, unchecked}`, renewing each through
  the same path as `renew`. Nothing else is released on disconnect and no
  session keeps work state, so a Host may resume on any owner node; a
  fence reclaimed while it was away is lost, and every later frame under
  it is refused. A fence whose renewal failed with an error (a transient
  database fault) is `unchecked`, not lost: the Host keeps the work, and
  its next `renew` decides.
- **Tests.** Library: only the owner's secrets are handed; reclaimed and
  foreign fences are lost; unlinking mid-job loses the fence; write-back
  is refused without a live fence, for a ref not handed, and for a revoked
  node; a secret created after the handoff is refused; a lost renewal
  leaves the lease where it was; a released claim is pending again with
  no attempt spent; an acquire cannot complete. Daemon: over the socket, sealed
  secrets open only with the enrolled key, write-back works under the
  live fence and not otherwise, resume renews a live fence and loses a
  reclaimed one, and with tracing and `log` captured at TRACE no token
  appears in the logs, plain or base64; a full send queue claims no wake
  until the Host reads; a wake is acked only after the Host's `ack`;
  wakes whose lease ran out stop holding the forwarder back; a job
  claimed on a closing connection is given back; a superseded session
  ending late leaves the wake catalog enabled; a database error on
  resume reports the fence unchecked.

### Host client

Built 2026-10-18 (H-4 step 7), the Host side of the socket above, in the
`bookclerk-node-client` crate (so the CLI can reuse it for `clusters
join`) and `bookclerkd/src/cluster_link.rs`.

- **Joining** (`join_cluster`). The whole link is required: its origin is
  the one the Host dials and signs from then on. Fresh Ed25519 and X25519
  keys are generated, the link is redeemed under the transport policy
  below, and only then are both private keys sealed with the Host's DEK
  into one `encrypted_secrets` row (`kind = cluster_link`, `provider` =
  owner cluster id, `account_type = operator`, `account_id` = the minted
  node id) and the `cluster_links` row written (`active`; a re-enrollment
  replaces both). A crash between redeem and write loses the keys and
  needs a new link. The link secret goes once into the request body and
  is never logged.
- **Transport** (`OwnerEndpoint`, decision 1). `https` only, except an
  `http` origin whose host is loopback and resolves only to loopback
  addresses; a pinned link never falls back to plain HTTP. With the
  link's SPKI pin the certificate's SubjectPublicKeyInfo must hash to it
  and must sign the handshake (names and issuers are not consulted, so a
  self-signed private-network owner works); without one it must chain to
  the webpki roots and match the name. The upgrade must select
  `bookclerk-node.v1`.
- **Handshake.** The hello lists every fence still held. The Host signs
  only a challenge naming the cluster it enrolled into, and accepts only a
  welcome naming its own node and cluster with a valid owner box key; the
  `resumed` answer cancels work under lost fences.
- **Session.** One writer task behind a 64-frame queue; requests matched
  to answers by frame id; heartbeats every 15 s from the Host's own
  `NodeSampler`; pings every 20 s; idle close after 60 s; each `wake`
  handed to local subscribers, then acked. A wake no subscriber receives
  is not acked, so the owner keeps the delivery and sends it again after
  its lease or on the next connection. A heartbeat or claim the owner
  does not answer within 30 s is retried with the reconnect backoff
  (a heartbeat never waits longer than its interval); those loops end
  only with their connection. A close with 4403 marks the
  link `revoked`, cancels held work, and stops that link for good;
  anything else reconnects with backoff from 1 s to 60 s plus up to a
  quarter of jitter, reset after each welcome.
- **Database swap.** The daemon runs one client per active link of the
  live library. When a config reload swaps the database, every client is
  stopped (and awaited, so a node never dials twice) and the new store's
  active links are started. A link joined through the daemon (Join a
  library) is started at once, one the CLI recorded within the consent
  poll (5 s), and re-enrolling a running link restarts its client.
- **Jobs.** A claim loop runs while job slots are free. Each job runs on
  its own task, outliving the connection: its secrets are opened with the
  enrolled box key (a bundle that does not open is failed as
  `secrets_unopenable`), a renewer extends the lease every third of it
  (`lost` cancels, `renewed {cancel}` asks the runner to stop), and the
  result is reported with `complete` or `fail`, waiting up to 10 minutes
  for a connection. A lease the owner has not confirmed (by `work`, then
  each `renewed`, counted from when the request was sent) by the time it
  runs out, connected or not, marks the job lost and cancels it, so it
  never runs on a lease the owner may have handed elsewhere. A `work`
  that answers a claim which already timed out is failed at once as
  `claim_abandoned` under its fence (this retries the job like any
  failure; `release` is kept for a job this host will not run) rather
  than left to expire.
  Client shutdown cancels every held job as lost, as revocation does.
  A lost fence's result is discarded. Write-back
  (`HostJobCtx::write_back`) accepts only a ref handed with the job,
  seals the value to the live connection's owner box key with a fresh
  ephemeral key, and needs the fence still held. The daemon runs jobs
  through its own `JobTransport` (`TransportRunner`), with the handoff on
  `JobExecCtx::host`. (Until step 9 an acquire was failed as
  `upload_unavailable` before it ran; see *Host results and uploads*
  below.)
- **Claims are narrowed in the daemon** to the job kinds a Host can run
  (`HOST_RUNNABLE_KINDS`: `scan` and `acquire` since step 9)
  and to the plugins its operator consented to
  (host consent, step 8, below). Every job a Host is trusted with runs an
  owner plugin, and a Host runs one only after its own operator approves
  it. Rejected: claiming and
  failing what cannot run, which would spend owner attempts and starve
  owner nodes.
- **Not decided at step 7.** How a Host's scan returns its results (book
  rows) to the owner; library SQL for owner plugins on a Host is a
  non-goal. Decided in step 9: fenced `scan.results` batches (*Host
  results and uploads*, below).
- **Tests** (`cluster_link/tests.rs`, owner and Host in one process): a
  member's Host enrolls over loopback, heartbeats into `node_status`,
  claims its owner's scan, runs it through the daemon's job transport
  with the opened token, writes a refreshed token back, completes under
  the fence, and acks a wake; with tracing and `log` at TRACE no token
  appears in the logs, plain or base64. A database swap stops the
  previous store's link and connects the new store's. Against a scripted
  owner (`client_tests.rs`), an unanswered heartbeat and claim are
  retried on the same connection, and a wake is acked only once a
  subscriber received it. A fence held across a dropped
  connection is resumed and the job completes under it; one reclaimed
  while the owner was unreachable is lost on reconnect and its run
  cancelled. A wrong SPKI pin fails the TLS handshake before the link is
  spent and the socket never opens; an unpinned self-signed owner is
  refused; the right pin enrolls and connects.

### Host consent

H-4 step 8. A Host runs an owner plugin only after the Host's own
operator approves it, and only with what was approved.

- **Requests.** `deployments.list` (owner, scoped to deployments that
  target the node and should run; empty for a node trusted with nothing)
  carries each instance's plugin key, whether it is the default instance,
  and the plugin's active version: manifest, settings schema, artifacts,
  and the cluster operator's approved grant. The Host records that grant,
  with the URL, archive digest and payload digest of the artifact for its
  own target, as
  the request in `remote_plugin_consents` (keyed by owner cluster and
  plugin key). An unchanged request writes nothing; a changed one keeps
  any stored approval, which is then re-evaluated.
- **Approval.** `bookclerk clusters approve <plugin> --request <DIGEST>
  [--cluster] [--deny-domain]… [--deny-binding]… [--deny-network]` or
  operator-only `POST /api/config/cluster-consents/approve` (body
  `request_digest`, required) stores the request as reviewed plus the
  consent (the request minus the denials; naming something the request
  does not ask for is refused). The digest is the `request_digest` the
  listing shows: SHA-256 over the requested version, the owner's grant
  exactly as recorded, and the artifact URL, archive digest and payload
  digest (shown with it as `requested_artifact`). The payload digest is
  covered because it keys the Host's version cache, whose hit is reused
  without a fetch: an owner that kept the approved URL and archive but
  named another cached tree would otherwise run it unreviewed. An approval whose digest is not the recorded
  request's is refused (`409 approval_refused` from the API; `404` when
  nothing is recorded, `400` for a narrowing the request does not ask for), so a grant
  the owner changed after the operator looked, even under the same
  version, is never approved unseen; the write is also a compare-and-swap
  on the recorded request. `revoke` clears the approval, keeps the
  request, and records `revoked_at`. `bookclerk clusters consents`
  shows the digest and the state (its `--format json` output, like
  `GET /api/config/cluster-consents`, also carries `revoked_at`; both
  answer one shared view of the row). The state is one of `consented`,
  `awaiting_host_consent`, `awaiting_host_reapproval` (a new version asks
  for something outside what was reviewed: a domain, TCP target, CIDR,
  binding, flag, entrypoint, larger budget, or network at all; or the
  artifact URL, archive digest or payload digest is not the approved one,
  even with the
  grant unchanged), or
  `invalid_grant`.
- **Effective grant.** The current request intersected with the consent.
  Removed network destinations are recorded as operator denials, so the
  spawn path's re-merge of manifest domains cannot bring them back.
- **Platform plugins** are approved without asking only when the key is a
  Bookclerk platform key, this Host's own provenance check says its install
  of that key is platform-bundled with the same payload digest as the
  owner's artifact, and the request stays inside the platform envelope.
  Recorded with `approved_by = "platform"`. An operator's revoke holds: a
  row with `revoked_at` set is never approved automatically (the store
  refuses it too), and only the operator approving again clears it.
- **Claims.** The claim frame carries `filter {kinds, plugins}`: the kinds
  this Host runs and the plugin keys it consented to (and has fetched).
  The owner applies it after the trust predicate and before permits and
  the CAS (`JobClaimFilter`; a job's plugins are its pinned instance's key,
  its required instances' keys, and its accounts' keys), so an unconsented
  plugin's job is never claimed. With no consented plugin the Host sends
  no claim. The Host checks again when a job arrives and gives back a
  mismatch with `release`, which returns it to `pending` without spending
  the attempt (unless it has a checkpoint) and frees its permits.
- **Revoke mid-job.** The link supervisor re-reads consent rows every 5 s
  and on approve/revoke through the API, so a CLI revoke is seen without a
  wake. A refresh that could not finish (the owner unreachable, or a
  consented instance whose archive fetch or `instance.config` failed) is
  retried with backoff (5 s doubling to 5 minutes; a new connection or a
  refresh wake retries at once), so a transient failure does not leave a
  consented plugin unclaimable. Losing consent cancels the plugin's held jobs and gives them back
  with `release`, then reports the deployment `error:
  awaiting_host_consent`.
- **Fetch.** For a consented instance the Host materializes the version:
  a `blob:` archive comes from `GET /api/node/v1/artifacts/{sha256}` with
  `Authorization: Bookclerk-Node <node_id> <issued_at> <sig>` (Ed25519 over
  cluster, node, owner origin, digest and time; 5 minutes of skew either
  way; served only for archives of versions deployed to that node; every
  refusal is the same 404), is checked against its digest before it is
  stored in the Host's own `plugin_artifacts`, and extracted by the
  ordinary materializer, which checks the archive and payload digests.
  An `https:` archive is fetched by the materializer outside any plugin
  jail, so it is fetched only when its URL and digest are the approved
  ones, its host is not `localhost` or a loopback, private, link-local or
  unspecified IP literal, and every redirect hop (at most 5, possibly to
  another origin) passes the same check; the archive digest binds the
  bytes. The same address policy is enforced at connect time: the fetch's
  resolver refuses a name (IP literals included) any of whose addresses
  is not public Internet (`is_public_internet`, plus NAT64, 6to4, Teredo,
  IPv4-compatible and reserved forms), and the connector dials only the
  addresses that resolver returned, so a public-looking name that resolves
  or rebinds to a local or private address is never reached, on the first
  hop or any redirect. The fetch ignores `HTTPS_PROXY` / `ALL_PROXY` (a
  proxy would resolve the name itself, past that check); a Host that
  reaches the Internet only through a proxy uses `blob:` archives.
  `file:` is refused.
  Nothing is written to the Host's `plugin_versions`, so its own
  reconciler never runs an owner plugin.
- **Configuration.** `instance.config` answers `CONFIG` and, only for a
  node trusted with everything (instances have no owner yet), `SECRETS`
  sealed to the node's box key and bound to node, instance and revision
  (`bookclerk-node.v1 instance-secrets`); otherwise `secrets_withheld`.
  The Host re-reads it on every connection and on `deployment.changed`,
  `install.changed`, `placement.changed` and `configuration.changed`
  wakes, and reports `materialized` with the applied revision through the
  `observation` frame (a Host may report only `materialized` or `error`,
  only for its own deployments).
- **Bindings.** A handed job's guest bindings (`JobExecCtx::host_bindings`)
  are built from the catalog entry and the job's handed secrets: `CONFIG`,
  `SECRETS` only under a consented `secrets` binding, and the store
  credentials of the job's accounts. They live in memory only, redact
  their `Debug`, and are zeroized on drop.
- **Still nothing ran at step 8.** `HOST_RUNNABLE_KINDS` was empty until
  step 9 (below) gave scans and acquires a way back. The in-process job
  transport refuses a handed job outright, since it would run against the
  Host's own library.
- **Tests** (`cluster_link/tests.rs`): an Administrator's Host records the
  request, reports `awaiting_host_consent`, fetches nothing and never
  claims the queued scan; unsigned, malformed and forged artifact requests
  get 404; after approval the archive comes over the endpoint and the scan
  runs with `CONFIG`, the instance's `SECRETS` and the account credential
  in its bindings; a configuration change wakes the Host and the next
  observation carries the new revision; no secret appears in TRACE logs,
  plain or base64. A Member's Host gets `CONFIG` but not the instance's
  secrets. A CLI revoke mid-job cancels the run and gives the job back
  unspent, and nothing is claimed again. Library tests cover the claim
  filter, release, and read scoping; plugin-host tests cover narrowing,
  re-approval and the platform rule.

### Host results and uploads

H-4 step 9 (decisions 4 and 6). A Host runs scans and acquires
(`HOST_RUNNABLE_KINDS = ["scan", "acquire"]`) through
`HostJobTransport`: the owner's plugin version materialized on the Host,
spawned jailed under the consented grant through `bookclerk-workerd`, with
the handed credentials only. There is no in-process fallback.

- **Whose import settings a scan uses.** The owner writes every row a
  Host scan returns, so a scan's `work` job carries the owner's
  `library.import_episodes` / `import_plus_titles` (`scan_import`) and the
  Host scans with those, never its own; a scan handed without them is
  refused.

- **Which jobs a Host claims.** A Host runs one source guest per job, so
  its claim filter admits a scan or acquire only when the job names one
  source plugin: the pinned instance's, or else the one plugin holding
  the job's account. An unpinned job whose account id exists under several
  plugins is never handed to a Host and stays with nodes that hold the
  database; claiming and failing it would spend owner attempts. The `work`
  frame names that `source_plugin_key`, and the Host selects the guest by
  it, never by the destination plugins an acquire also lists.
- **Cancellation.** A Host job's cancel flag (fence lost, operator cancel,
  consent revoked) drops the running guest call within 50 ms. Dropping the
  guest session shuts the guest down and fences its host-side authority,
  so it stops using the handed credentials; an in-flight title upload is
  dropped too. A cancelled run never reports success: a guest that stops
  cooperatively and returns what it had is answered `cancelled`, and the
  client turns a result that comes back after the cancel into a failure
  (only the owner's own completion, by the closing commit, stands).
- **Scan results** (decision 6). The guest's rows come back unwritten and
  go to the owner in `scan.results` batches (at most 200 rows and 768 KiB;
  a row over 32 KiB is skipped). `accept_scan_results` re-checks the fence
  and `node_may_claim_job` on every batch, requires each row's account to
  exist under the job's plugin, be the job's account when it names one,
  and, for a Host trusted only with its owner's accounts, be that user's;
  one bad row refuses the batch. Rows are written through the store API,
  as a local scan writes them. A Host never writes library SQL.
- **Scan rows.** A batch is accepted only for the job's source plugin,
  never for a destination plugin the job also requires. The fence and the
  node's trust (with its owner's accounts) are confirmed again before each
  row is written, so a lease lost or trust withdrawn mid-batch stops the
  rows not yet written.
- **Acquire plan.** `acquire.plan` answers `acquire.targets {targets,
  destinations}`: the owner's pending titles the job covers and every
  output destination deployed on the answering owner node, each with an
  id and its own naming options. The Host acquires each title into an
  outbox with one sub-directory per destination, so each destination's
  keys come out as a local acquire on the owner would name them (a fix
  after review: the first cut sent only the first destination's options,
  so every destination got that one's keys).
- **Uploads** (decision 4). `upload.grant` carries a manifest: each
  distinct content once (`size`, `sha256`, `content_type`) with one
  `FileCopy {destination, key}` per destination; it is refused unless
  every copy names a known destination, every destination is written, the
  (destination, key) pairs are unique, and the title's primary key is on
  the primary destination. The owner answers `upload.granted` with a
  32-byte bearer token sealed to the Host's box key (only its hash is in
  `node_upload_sessions`; it lives an hour). Each file is `PUT
  /api/node/v1/uploads/{id}/files/{n}` with `Authorization:
  Bookclerk-Upload <token>`, then `POST …/commit`. The owner tees the body
  through bounded channels into `put_stream` of every deployed
  destination under that destination's key, checking size and digest as
  it streams and the fence about once a second; nothing is staged on the
  owner. Any mismatch, lost fence, or broken stream ends every
  destination's stream with an error so each discards its write. A PUT
  while the node's destinations differ from the grant's is `422
  destinations_changed` and aborts the upload. The commit is one
  canonical batch gated on the live fence and credential: it marks the
  title acquired, publishes `book_acquired`, and completes the job with
  the closing upload (an acquire never completes with `complete`).
  Refusals: 401 (token), 421 (another owner node granted it), 409 (lost
  fence), 422. Limits: 512 files, 8 GiB per file, 16 copies, 1 KiB keys.
- **Node-local instances (H-2).** `plugin_instances.owner_node_id` marks
  an instance local to one node: deploying it elsewhere and placing it
  are refused. `bookclerk plugins instances create … --node <id>` and
  `node_id` on `POST /api/config/plugin-instances` create one. The Host
  also keeps a copy of each title in its own deployed destinations (see
  *The Host's own destinations* below), so both sides end up with a local
  copy.
- **Key scope.** A grant names only a title `acquire.plan` would hand
  out now: one whose audio is already acquired (or that the plan leaves
  out otherwise) is refused (`book_not_planned`), so a host cannot
  re-upload over a title the owner already has. The owner binds every key
  of an upload to the granted title. For each destination it computes the title's folder from its own
  row and that destination's naming options, the way a local acquire
  would (with podcast episodes saved to the parent's folder, the
  episode's own folder is also accepted). Each copy must lie below that
  folder; a template with no folder admits only top-level keys. A key
  another title already has (its recorded audio or PDF key, or a sidecar
  of that audio) is refused too (`key_out_of_scope`,
  `key_of_another_book`). The Host's keys never decide where a title goes.
  Every file a title writes is recorded per destination in `title_files`:
  an acquire on a node records each key (audio, chapter or part files,
  sidecars, PDF) just before writing it, and a grant claims every key of
  its manifest for the title. A key recorded for another library title on
  that destination is refused (`key_of_another_book`), so a folder several
  titles share (a template without the title, podcast episodes in the
  parent's folder, a flat layout) never lets one title overwrite another's
  files. The claim is one conditional write per key, so of two uploads of
  different titles racing for one key only one is granted.
- **Host paths.** The Host uses the owner's job and title ids as work
  directory names only when each is one plain segment (ASCII letters,
  digits, `-`, `_`). Any other id fails the job before anything is
  created or removed.
- **The Host's own destinations.** A title is uploaded only after every
  output destination deployed on the Host (H-2) took its copy. A failed
  copy fails the title (copies already written for it are removed), so
  the job fails rather than succeeding with a destination short a title.
  A title whose upload then fails (the grant, a file, the commit, or a
  cancel) has its copies removed from the Host's destinations too.
- **Owner-decided success.** A title's commit marks it acquired and never
  completes the job. Only the closing upload (no title, no files) may ask
  to finish, and the owner completes the job only when no title in the
  job's scope is left with unacquired audio (`titles_remaining`
  otherwise). The Host's `finish` flag is a request, not proof.
  `acquire.plan` hands out exactly those titles: one whose audio is
  acquired but whose companion PDF is missing is left to an acquire on a
  node holding the library, since a Host would fetch and upload the audio
  again and a missing PDF is a soft failure that never keeps a job open.
  The commit's gate also carries the node's trust (an owner node, or an
  active owning user who is privileged or still linked to the title's
  account), so a user disabled or an account unlinked after the preflight
  commits nothing.
- **Abort deletes.** Files go straight to the destinations, so an abort
  rolls back by deleting. An upload aborted after some files arrived (its
  fence lost before a later file or the commit, the destinations changed,
  the title re-sent, or the job finished, was released, or lost its fence
  without it)
  has every received copy deleted by the receiving owner node, and so has
  an upload left open when the closing commit completes the job. A failed
  or released run's uploads are deleted before the job ends and its
  permits are given back. A copy is deleted only while `title_files` still
  names that upload as the key's latest writer, so a cleanup that runs
  late (after a lost fence) leaves alone a file a later upload or acquire
  wrote. A file that finished streaming just as its upload was aborted is
  deleted at once. An upload received by another owner node is left to
  that node and logged.
- **Token lifetime.** The upload token is accepted for an hour past its
  latest extension. The grant, each accepted file, and each renewal of the
  job's lease extend it. Nothing extends it after the fence is lost, and a
  lost fence refuses every file and the commit regardless.
- **Not done.** If a Host vanishes mid-title, so that no further file,
  commit, renewal or finish arrives, its open upload is not swept: the
  files it had received stay until a later attempt of the job re-sends
  that title through the same owner node.
- **Tests.** Library: an acquire completes only through its upload; a
  stale fence cannot write scan rows or open uploads; a commit acquires
  the title and completes the job; manifests that skip or invent a
  destination, repeat a key, or put the primary elsewhere are refused; a
  node-local instance runs only on its node. Daemon
  (`cluster_link/tests.rs`): a Host's scan rows land in the owner's
  library; a Member's Host cannot write rows for an account it does not
  own; a streamed acquire reaches the owner's destination without
  staging; a fence lost mid-upload aborts it; two destinations with
  different templates each get their own key; an upload is refused after
  the destinations change; the manifest sends each content once.

### Live socket and the host consent screen

H-4 step 10 ("Host nodes and the SPA use WebSockets rather than
polling"). `GET /api/live` (subprotocol `bookclerk-live.v1`) tells the SPA
which topics changed; the SPA then re-reads its ordinary routes.

- **Only topic names travel**: `library`, `jobs`, `deliveries`, `nodes`,
  `config`, `consents`, `clusters`, `users`, in `{"v":1,"type":"hello"|"changed",
  "topics":[…]}`. No rows, ids or secrets, so the socket needs no
  authorization model of its own beyond who may hear which topic: the
  operator hears all; an Owner or Administrator hears `library`, `jobs`,
  `deliveries`, `nodes` and `users`; anyone else `library` and `nodes`.
- **Origin first.** The `Origin` header must be exactly the configured
  `public_origin`, or, without one, `http://<Host>` for a loopback `Host`
  with no forwarding headers; anything else is 403 `origin_mismatch`
  before the session is looked at, `auth.enabled = false` included. This
  is what keeps another site's page from opening the socket with the
  user's cookie (cross-site WebSocket hijacking).
- **Session.** Admission runs the same caller resolution as
  `require_operator_or_user_auth` (`resolve_user_caller`); the session is
  re-checked every 30 s and a signed-out caller is closed with 4401 or
  loses topics.
- **Change detection.** Every 2 s the session reads a revision per topic
  (`live_revisions`: row count plus newest `updated_at` of `books`,
  `jobs`, `event_deliveries`, `remote_plugin_consents` and `users`, the
  newest `node_status.heartbeat_at`, the newest
  `configuration_changes` id, and for `cluster_links`, which has no
  change stamp, a digest of its rows), so a change made on any owner node
  or by `bookclerk clusters join|leave` is seen; in-process pokes
  (consent approve/revoke, a link revoked by its owner) are immediate.
  The baseline is read before `hello` is sent, so a write after the
  client hears `hello` is never absorbed into it. Rejected:
  a table of change events or database triggers, which the SQL-v1
  contract would have to carry on every backend.
- **SPA.** One shared client (`ui/src/lib/live.ts`) per page;
  `useLiveRefresh(topics, refresh, {pollMs, openPollMs})` replaces the
  polling timers of Status, Library, Settings → Nodes and User Management.
  While the socket is down, panels poll on their old timers; while it is
  up, Status and Nodes still re-read every 60 s (a node that stops
  heartbeating changes no row) and User Management every 30 s (presence
  ages with time). Reconnects back off from 1 s to 30 s with jitter, and
  a reconnect refreshes every subscribed topic once.
- **Consent screen.** Settings → Library (operator only; first shipped as
  Settings → Clusters) lists the links
  and each owner plugin's request with its state, a deny checkbox per
  requested domain and binding, a deny-network switch, Approve (sending
  the `request_digest` shown, so a changed request is refused rather than
  approved unseen) and Revoke, over the existing
  `/api/config/cluster-consents` routes; a 409 shows the daemon's reason.
  This step shipped no invite or join screen, so that an enrollment link
  never passed through the SPA; [Join a library](#join-a-library-sign-in-and-branch-invites)
  below adds both, at the owner's request.
- **CLI.** `bookclerk clusters join` (link from stdin or a prompt with
  echo off, never argv) and `bookclerk clusters leave [--cluster]` (marks
  the link `left`, deletes its node keys; a running daemon stops the
  link's client within 5 s). Join a library adds `join --url` and tells the owner of a leave.
- **Tests.** Daemon: Origin matching for both modes; a foreign origin or
  no session is refused before the upgrade; the operator hears job and
  consent changes, and a write made right after `hello`; a member hears
  only their topics and is closed with 4401 when signed out; a binary
  frame closes with 1003; the consent routes are operator-only, approve
  narrowed for the reviewed version, refuse another version or an unasked
  domain, deny the network, and revoke; leaving stops a running client.
  Library: joining, a revoke and leaving each change the `clusters`
  revision. CLI (real binary): list, consents, approve with narrowing and
  refusals, revoke, leave, and join refusing a link on argv, an empty
  stdin and a non-link without echoing it, and reading the link from a
  terminal with echo off; a node-local instance names its node. UI
  (vitest): socket URL scheme, backoff and reset, topic dispatch, replay
  after reconnect, the sign-out delay, and polling only while the socket
  is down.

Step 11 is the operator guide, [Libraries and branches](../server/cluster-hosts.md).

### Join a library (sign-in) and branch invites

A later step (decided 2026-10-18) makes joining UI-first. The UI calls an
owner cluster a **library** and a host node a **branch**; code, API and CLI
names are unchanged.

- **Sign-in instead of a link.** The owner's OAuth server (`oidc.rs`) gains
  one built-in public client, `bookclerk-branch`, never stored in the
  clients table (the id cannot be registered): redirect URIs loopback (RFC
  8252) or `https://` only, scope `node:enroll`, S256 PKCE required.
  `/oidc/authorize` validates a branch request and hands it to the SPA
  consent page `/join/approve` ("Let <branch> join <library> as yours?").
  Approving (`POST /api/auth/branch-consent`, a signed-in user, never the
  operator alone) checks the user is active, `core.host_enrollment` is on
  and `classify_transport` allows the connection, then stores a
  single-use code (hashed, 60 s) in `oidc_auth_codes`.
- **One redeem batch.** `POST /api/node/v1/enroll/oauth` consumes the code,
  checks client, redirect URI, scope and verifier, and records an
  enrollment link whose id and secret derive from the code and verifier
  (`authorized_enrollment_link`, `created_by = "oauth:"`, rechecking
  enrollment and the user). The redeem is then the canonical
  `redeem_enrollment_link` batch: one node, `node_credentials`, a
  receipt, replay for a retry of the same exchange, one node under
  concurrency. `GET /api/node/v1/info` tells the joining daemon the
  cluster id, a library name (the public origin's host), the canonical
  origin, the transport mode, whether enrollment is on, and in
  `self_signed` mode the SPKI fingerprint; the joining daemon requires that
  fingerprint to equal the certificate it was shown, and the origin to
  equal the address typed.
- **The verifier stays on the host.** `POST /api/config/join/start`
  (operator) makes the verifier, `state` and node keys and keeps them in a
  `pending_joins` row (SQL-v1 and SeaORM; 10 minutes; the verifier and
  private keys sealed in `encrypted_secrets` as `cluster_join`). The SPA
  relays only `code` and `state` to `POST /api/config/join/callback`; the
  daemon takes the pending row once, redeems, records the link and starts
  its client at once. A running daemon also starts clients for links
  joined on the CLI within 5 s. No restart is needed.
- **Invites in the UI.** "Add a branch" (User management, and each user's
  own menu) mints an enrollment link and shows it once (`Cache-Control:
  no-store`, never in stored SPA state or logs), with outstanding invites
  and Revoke; the join wizard's "I have an invite" tab POSTs a pasted link
  to the daemon, which keeps nothing. This reverses "secrets do not pass
  through the SPA" for links only; the fragment, hash-only storage,
  single use, expiry and revocation all hold.
- **Leave tells the owner.** A new JSON node frame, `node.leaving`
  (`bookclerk-node-proto`; not part of the plugin ABI), makes the owner
  revoke the node's credential, drop its `node_status` row and close with
  4403. Settings → Library → Leave library and `clusters leave` send it
  (over the running client, or a short-lived one, 10 s at most), then mark
  the link `left` and delete the keys either way.
- **First start.** A fresh install (no Owner or Administrator, no link)
  shows its operator a welcome screen: Start a new library (today's
  bootstrap) or Join a library. Settings → Branches (operator or Owner)
  holds "Let new branches join" and the fingerprint.
- **CLI.** `bookclerk clusters join --url <owner>` prints the authorize URL,
  opens a browser when it can, and takes the answer on a loopback
  listener; without `--url` the stdin path is unchanged.
- **Not built here.** The peer ("clerk") connection type that shares the
  library's database, and branches of branches: each is a later section
  (below).
- **Tests.** Daemon: `info` under the transport policy and in
  `self_signed` mode (the fingerprint a branch checks); consent and redeem
  with an idempotent retry and one node under concurrency; refusals for
  enrollment off, an inactive user, a wrong verifier, a reused or unknown
  code, another redirect URI, a malformed request, the operator, and
  plain HTTP from the LAN; the branch handed to the consent page and the
  reserved client id; the invite's `no-store`; join end to end
  (inspect, start, deny, approve, callback, client started at runtime,
  leave revoking the node on the owner) and with an invite. Library: the
  pending join is taken once and expires. CLI: the loopback callback
  takes only its own `state` and reports a denial. UI (vitest): callback
  parsing, the consent redirect check, invite shape, countdown, and
  error copy.

### Join as a clerk (an owner node over a sign-in)

Decided 2026-10-18. The consent page can let a joining install become an
**owner node** (`kind = owner`) that shares the library's database and DEK,
instead of a host node. The UI calls it a **clerk** ("Join as a clerk of
this library" or "Join as a branch"; Settings → **Clerks**); code keeps
owner node and host node.

- **Who may choose.** Only an Owner of the library (the stored role,
  `verify_cluster_owner`, read at consent and again at redeem) is offered
  the choice; everyone else gets branch only. Branch is always the default.
  The joining install must offer it (`clerk_offer=1` on the authorize
  request), which it does only when it is empty: no users, store accounts,
  books or joined libraries of its own, and no `BOOKCLERK_DATABASE_*` /
  `BOOKCLERK_D1_*` override that would outrank the new bootstrap
  (`install_problem`, checked at start and again at the callback).
- **Shareable database only.** Clerk is enabled only when the library's
  `[database]` is PostgreSQL or D1. With SQLite the option is shown
  disabled with the reason. A D1 API token is never sent; the clerk needs
  its own `BOOKCLERK_D1_API_TOKEN`.
- **Fresh step-up.** Approving as a clerk needs a second factor verified in
  that request: a TOTP code, or a user-verified passkey assertion for a
  challenge from `POST /api/auth/branch-consent/step-up` (kind
  `clerk_step_up`). Wrong codes are throttled like sign-in. An Owner with
  neither factor is refused (`step_up_unavailable`) with how to set one up.
  Consent and redeem run under the node API transport policy (TLS, or
  loopback only when both ends are on one machine).
- **The code is bound to the type.** A clerk code is stored with scope
  `node:enroll node:clerk`, a branch code with `node:enroll`, each followed
  by the consented branch name (` name:` and the name in base64url), as for
  a branch. The redeem names the type it expects (`connection`) and must
  match the stored scope's type, so a branch-approved code never redeems as
  a clerk, and the node is labelled with the consented name. The owner
  peeks the code, checks it (and, for a clerk, that its user is still an
  Owner and the database still shareable), records the link, and only then
  consumes it, as for a branch; a refusal consumes it and is audited. The
  node kind is also part of the redeem request hash.
- **Retries.** The joining daemon's clerk callback follows the branch
  lifecycle: it opens the pending join under the callback lock, redeems
  within the same timeout, and keeps the pending join for a retryable
  failure (`join_retry`); it ends it once the redeem's outcome is final.
  A failure to write the bootstrap after the redeem names the node, so
  the library's owner can remove that clerk. `clusters join --as clerk`
  retries a retryable redeem as a branch join does.
- **Redeem.** The same canonical batch records a `nodes` row with `kind =
  owner` and no `node_credentials` row (a clerk never dials the node
  socket). The answer adds `bootstrap`: a `ClerkBootstrap` (`[database]`
  connection and the 32-byte DEK, never the passphrase-wrapped
  `master.key` file) sealed with the H-4 sealed box (X25519, HKDF-SHA256,
  XChaCha20-Poly1305) to the joining node's box key, with associated data
  binding the cluster and node ids. It is answered `no-store`, never logged
  (`Debug` redacts), and never passes through the browser: the joining
  daemon redeems with the verifier only it holds.
- **Applying it.** The joining daemon opens the box with its node key,
  checks the library's database guest is installed here, and writes its
  own bootstrap: `[database]` in `config.toml` (a Postgres URL into a
  `0600` file named by `url_file`, never in the TOML), `node-identity.json`
  with the node id the library recorded, and `master.key` (BCK2 under its
  own `BOOKCLERK_AUTH_PASSWORD` / `[auth].password` when set, BCK1
  otherwise). `config.toml` is written last. It then restarts cleanly: the
  listeners drain as on SIGTERM and the binary re-execs itself with the
  same arguments and environment (same PID on Unix). It comes back as an
  owner node of the library. `clusters join --as clerk` writes the same
  files and asks the operator to restart `bookclerkd`.
- **Removing a clerk.** Settings → Clerks lists the owner nodes
  (`GET /api/config/clerks`: all of them for the operator or an Owner,
  the owned ones for another user, by the same rule as removal);
  `DELETE /api/config/nodes/{node_id}` removes one that is not running
  (operator or Owner; never this node or a host node). Removal does not
  take back the database credentials or DEK it held: rotate them when it
  is no longer trusted.
- **Audit.** `clerk_authorize`, `clerk_authorize_refused`,
  `clerk_enroll_refused`, `clerk_enrolled` (owner), `clerk_joined`
  (joiner), `owner_node_removed`.
- **Tests.** Proto: the bootstrap opens only with the node key and its ids,
  and never prints its secrets. Daemon: clerk refused to non-Owners, with
  SQLite, without a second factor, without the joiner's offer; step-up
  required and a wrong code refused; a branch code refused as a clerk;
  the sealed bootstrap opens only with the node key; a non-empty install
  refused; an empty install joins end to end against a Postgres-configured
  owner (the database itself is not opened: PostgreSQL does not run in
  CI). Library: `install_master_key` raw and wrapped.

### Routed library work (H-5)

Built 2026-10-18. Library work no longer runs where it is asked for.

- **CLI through the queue.** `bookclerk library scan` and `library
  acquire` plan with the same planners as the HTTP API (as operator
  authority, since the CLI holds the database), record each job's routing
  requirements, and insert the rows; any live node or trusted host claims
  them through the routed claim. The CLI waits for every job and prints
  progress (the first Ctrl-C asks them to cancel). Waiting fails closed
  when no `bookclerkd` node of the cluster is live; `--no-wait` only
  queues. The live-store rule holds: `--account <id>` is exactly one scan
  job and `--asin <ASIN>` exactly one acquire job. No guest runs in the CLI
  for either; `--dry-run` still plans locally. Rejected: a CLI fallback
  that runs the guests itself when no daemon is up, which would be an
  in-process path beside the routed one.
- **No license file or per-run overrides** (product decision). The
  never-implemented `library acquire --license <file>` (or `-` for standard
  input) and `-o key=value` flags were removed, with and without
  `--dry-run`. A job carries no license file, setting overrides or node
  pin: an acquire runs with the settings of the node that claims it.
  Rejected: pinning such an acquire to the node that ran the command (it
  added a node pin to the queue for a feature that does not exist), and
  embedding the license file in the job (a secret in a job row).
- **Acquire planner** (`plan_acquire`). Each named title becomes its own
  job. Operator authority, an Owner or an Administrator acquiring
  everything gets one unsplit job (splitting it per account would repeat
  the storage listing for every account; only a fully trusted node claims
  it). Anyone else must own a named account and every library row a named
  title matches; with neither they get one job per account they wholly
  own. `POST /api/library/acquire` takes `titles`, `force` and `pdf_only`
  and answers like a scan plan (422 `nothing_to_acquire`).
- **Per-item trust at claim** (`job_item_accounts`). A scan touches the
  named account's rows; an acquire touches the named account's rows plus
  the account of every library row its filters match (title, account,
  pinned plugin), whatever its acquire status. A node trusted with one
  user's work claims a job only when every item is that user's, and is
  handed exactly those accounts' secrets. An unpinned acquire that names
  no account now runs the plugins of the rows it matches
  (`job_plugin_keys`), so a host's claim filter and `acquire.plan` see
  them. A forced or PDF-only acquire runs only on an owner node
  (`claimant_may_run`): the host acquire path does neither.
- **Scan All from any node** (decision 2). A host's operator runs
  `bookclerk clusters scan`, which its daemon sends over the link as
  `scan.request`; the owner plans it as the node's owner
  (`remote_node_requester`): an Owner or Administrator gets one job per
  scan-enabled account, distributed by the routed claim, and anyone else
  only their own accounts. A node trusted with nothing is refused.
  `job.status` reports only jobs the node would be trusted to claim.
- **Tests.** Library (SQLite with PostgreSQL twins): item sets, per-item
  trust, the queue claiming only a Member's own title, force and PDF-only
  never handed out, the acquire planner's splitting and scoping, and whom
  a host's requests run as; per-item secrets for a title acquire. Daemon:
  a Member's host scan request is scoped, an Administrator's fans out per
  account, and `job.status` hides other users' jobs; the acquire route
  splits titles and scopes a Member. CLI: `--account` queues one job, a
  bare scan one per enabled account, an acquire one per title, waiting
  without a live node queues nothing.

### Branches of branches

Decided 2026-10-18 with the owner, built after Join a library. A branch is
a full library of its own, so its users can join their own branches to
it: a tree, A ← B ← C.

- **One hop at a time.** Each link knows only its direct neighbour. A knows
  B as a node owned by one of A's users and has no node, user or account
  row for C. A's work runs on B and is never re-routed to C. No store
  account id, store token or downstream user crosses a link.
- **`catalog.*` frames (branch → library).** `catalog.begin
  {share_books, include_branches}` reports the link's two settings and
  opens a session under a new per-node **catalog fence**
  (`nodes.catalog_fence`); the answer `catalog.session {fence, cursor,
  ancestry, destinations, wanted}` says where to resume, the library's
  ancestry, its destination ids, and titles whose files it still wants.
  `catalog.sync {fence, batch, cursor, done, titles}` sends at most 100
  titles; each `CatalogTitle` is `{store, product_id, metadata,
  primary_key, pdf_key, files}` (`deny_unknown_fields`, no account or user
  field). The library validates a batch the way it does `scan.results`
  (shape, sizes, keys, no repeats) and answers `catalog.accepted {batch,
  upserted, skipped, wanted}`, `lost` under a stale fence, or `refused`.
  Only a `host` node with a live credential and an active owner may share.
- **`branch_titles` (SQL-v1 + SeaORM).** One row per (provenance, store,
  product id): provenance `node:<B>`, the owner user (B's owner), the
  metadata and manifest (with its SHA-256), the delivered manifest's
  digest, `state` (`shared` or `detached`) and the reason (`stopped_sharing`,
  `left`, `withdrawn`). A title A already holds in its own `books` for that
  user (same store and product id under an account the user owns) is
  skipped, so nothing B got from A comes back as B's.
- **Files reuse the upload endpoint.** `catalog.upload {fence, store,
  product_id, files}` opens a `node_upload_sessions` row bound to the
  catalog fence (job id `catalog`) and answers `upload.granted` with the
  token sealed to B's box key; B then `PUT`s each file and commits as a
  host acquire does, the library tees each body straight to its
  destinations (no staging), and the commit records the delivered digest
  in one batch gated on the live catalog fence. Files keep the branch's
  own keys on every destination (A's naming templates are not applied
  again), so a grant is refused (`key_in_use`) when a key is one of A's
  own books' (its audio, PDF, or a file beside the audio with the same
  stem) or another branch title's: a branch never overwrites what is in
  A's outputs. With no destination on A, the catalog is shared without
  files.
- **Incremental, resumable.** B sends titles in a stable order (last
  change, kind, id) after the cursor the library keeps; a reconnect resumes
  after the last accepted batch and re-offers undelivered files. A change
  of either setting, and a session a day after the last full pass,
  restart from the start; a full pass (`done`) withdraws the titles it no
  longer named, which is how "Include books from my own branches" off, a
  removed book or a sub-branch that left reaches A.
- **Two settings on `cluster_links`** (`share_books`, `include_branches`,
  both on by default), set in Settings → Library on the branch
  (`GET` / `POST /api/config/join/sharing`, the operator or an Owner of
  the branch: a signed-in user's stored role and status are read on every
  call, as for `core.host_enrollment` (`verify_cluster_owner`), and anyone
  else gets 403; an Owner's change is recorded in the security audit) and
  reported to the library at once; the library keeps what was reported on the node row
  (`nodes.share_books`, `nodes.include_branches`) and shows it read-only in
  Settings → Branches (`GET /api/config/branches` → `branches`).
- **Cycles.** `GET /api/node/v1/info` returns `ancestry`: the library's own
  cluster id, then each active link's owner and the ancestry it reported
  (`cluster_links.ancestry_json`, refreshed on every catalog session), at
  most 64 ids. Inspect, start, and the invite and branch sign-in redeems
  refuse a parent whose ancestry contains the joining install's own cluster
  id (`409 join_cycle`), so A ← B ← A and A ← B ← C ← A are refused, and
  one whose ancestry together with the joining install's would pass 64 ids
  (`409 ancestry_too_deep`) rather than cutting the list short. A clerk shares
  its library's database and so reports the same ancestry: the same tree
  node.
- **Stop sharing or leave.** New titles stop; delivered files stay in A's
  outputs; the rows are marked detached (`stopped_sharing` at the next
  `catalog.begin`, `left` when the node leaves or is revoked) and shown as
  "from a branch that left"; the owner removes them in bulk
  (`POST /api/config/branches/titles/remove {node_id?}`, operator or Owner).
- **Tests.** Library: attribution, dedup and resume under a stale fence,
  trust refusal, no echo back, stop/leave/remove, a full pass withdrawing,
  the daily full pass, catalog uploads committing only under the live
  fence and never onto a key in use, ancestry and the cycle check.
  Daemon: a tree A ← B ← C end to end in one process (C's book reaches B, B's own and C's reach A as Riley's
  under `node:<B>`; the frame fields carry no account, user or node of
  C; the echo is skipped; a second pass sends nothing; files went through
  the upload endpoint under the catalog item and nothing was staged; the
  cycle refused; include-sub-branches off withdraws C's title; sharing
  off marks B's titles and the bulk remove clears them; C leaving marks
  its title on B), `info` ancestry, Settings → Branches and the remove, and
  the sharing endpoint reaching the library on a running link, for the
  operator and for an Owner, refused to an Administrator and a Member. UI
  (vitest): the read-only summaries and who sees the sharing settings.

## Consequences

- New unreleased tables: `cluster_identity`, `nodes`,
  `configuration_documents`, `configuration_audit`,
  `configuration_changes`, `plugin_instances`, `plugin_instance_defaults`,
  `plugin_deployments`, `plugin_deployment_observations`, and
  `plugin_schemas`. `plugin_instance_defaults` names the plugin's default
  instance; explicit create adds further instances of the same key.
  `accounts` is unique on `(plugin_key, account_id)`; `books`,
  `ignored_titles`, and `account_links` carry `plugin_key`. Phase 3 adds
  `plugin_kv`, `plugin_versions`, `plugin_installs`,
  `plugin_artifacts`, `plugin_placements` (with nullable `replicas`
  for "all"), `cluster_leases`, and `node_status`;
  `plugin_deployments.desired` allows `placed` and `unplaced`;
  `plugin_deployment_observations` gains the guest's reserved memory and
  CPU;
  `domain_events` gains `queue_name`, `content_type`, and `deliver_after`
  (dedup is unique per queue); `event_deliveries` gains `queue_name`;
  `nodes` gains `target`, `labels_json`, `kind`, and `owner_user_id`.
  H-4 adds `node_enrollment_links`, `node_credentials`, `cluster_links`,
  `job_secret_handoffs`, `remote_plugin_consents`, and (branches of
  branches) `branch_titles`; `cluster_links` gains `share_books`,
  `include_branches` and `ancestry_json`, and `nodes` the reported
  settings and the catalog fence, cursor and pass times.
  Checksum of the
  unreleased pack changes. Existing development databases fail closed until
  recreated (`cargo reset --yes` or a new database). That matches the
  unreleased-schema rule.
- `bookclerk config get/set` for `events.*` reads and writes the database.
  Plugin settings and enablement are `bookclerk plugins config` and
  `bookclerk plugins deploy|undeploy`, also database. Installs are
  `bookclerk plugins install|update|rollback|uninstall|approve|versions` and
  placement is `bookclerk plugins deploy` with `bookclerk nodes label|set`, all
  database. The remaining `config` keys still edit `config.toml`.
- `bookclerk config show` prints `events.authority = database` when the
  library can be read, and `transitional` when it cannot.
- Single-host SQLite still starts by opening the local database, minting
  `master.key` once, and writing `node-identity.json` once. The CLI does that
  alignment inside the library open, not before command dispatch. `config
  master-key status` and `wrap` do not open the database and do not mint a
  missing key.

## Remaining work

Not in this phase, and not claimed as production HA:

- Moving the rest of `config.toml` (library, jobs, media, plugins,
  diagnostics, discovery, OIDC). `daemon.listen` moving off bootstrap is part
  of that. (`core.output` is done; its host-local `in_progress` scratch path
  stays TOML until a node-scoped document carries host paths.)
- Job routing beyond plugin instances. Jobs now carry the instances they
  need and take cluster semaphores (*Job routing and cluster semaphores*
  above), and the requirements sweep re-resolves pending jobs when
  deployments move, but requirements only name instances: jobs are not
  ranked by headroom the way deliveries are, and acquire still writes the
  claiming node's own destinations rather than mirroring node-local outputs
  across nodes. `[jobs]` limits are read from each node's `config.toml`.
- Singleton maintenance and schema migration run under `cluster_leases`
  (see the implementation notes); other work is not leased.
- Synchronous calls other than scans and acquires (portal actions, the
  rest of the HTTP API and CLI) run on the node that receives them. Scans
  and acquires are queued and routed (*Routed library work (H-5)* above);
  `clusters scan` covers only scans from a host, not acquires.
- Per-guest measured usage is not sampled. Reservations are jail ceilings,
  so a guest that idles well below its ceiling still occupies it in the
  scheduler's view. Free disk is reported but not placed on.
- Measured use is whole-machine on every platform (Linux `/proc` and the
  cgroup, macOS Mach host statistics, Windows `GlobalMemoryStatusEx` /
  `GetSystemTimes`). Only Linux reads a container ceiling (cgroup v2); a
  daemon inside a Windows Job Object or another platform's resource limit
  is ranked against the whole machine.
- Rollouts: activating a version updates every placed node on its next
  wakeup at once; there is no staged or health-gated rollout across
  replicas. Placement does not read health: a replica on a host whose guest
  keeps failing stays until the host stops qualifying (routing already
  skips a host whose guest is not up).
- A shared artifact location for versions larger than the 1 MiB blob cap
  (an operator bucket, or KV-backed chunks). Today such an archive must sit
  on storage every placed host can read through its `file://` path.
- Database-adapter plugins as instances and multiple database targets
  (Phase 4).
- Host nodes: Pebble in CI, delivery claims and the KV, queue and EVENTS
  binding frames over the node socket, per-item trust for deliveries and
  instance ownership (`plugin_instances.owner_user_id`; instance
  `SECRETS` reach only a node trusted with every job). Uploaded objects
  carry content type, length and digest but not the title metadata (ASIN,
  title, timestamps) a local acquire records with its files. Branches of
  branches withdraw a title a branch no longer has only on a full pass (a
  settings change, or a day after the last one).
  Everything else in H-4 steps 1-11, Join a library, joining as a clerk
  and branches of branches is built (see the
  sections above and [Libraries and branches](../server/cluster-hosts.md)).
- Rolling upgrades and a cluster dashboard beyond the Status page and
  Settings → Nodes (Phase 5). Drain exists (`nodes set --drain`), but there
  is no drain-then-migrate flow: the migration lease serializes migrators,
  it does not stop older binaries on other nodes from using the database
  while a frozen step applies. That matters from the first release cut.
- Scheduled backups. Backups are operator commands plus the automatic
  pre-migrate recovery point; a scheduled backup would run as a
  `SingletonDuty` under its own lease.
- Schema pinning compares package versions, not schema contents: a newer
  package that only renames a key still drops the operator's value for the
  old name. A declared per-key migration in `[[settings]]` is a possible
  later refinement.
- The CLI writes no observations, so its routing prefers the present default
  instance rather than the healthy one.
- Resizing the local event delivery task count without a process restart.
- A general key-management provider. Operators copy `master.key`.
- The host-local install path (receipts, `install-ledger.json`, in-place
  replace of `plugins/<fs-id>`) remains for platform guests, development
  staging, and host-local installs made before cluster installs. It keeps
  its stage-and-commit crash window, but no operator command writes it any
  more; cluster installs and the digest cache replaced it for everything an
  operator installs. #255 (a
  stage-and-commit installer) was closed for that reason.
- D1 multi-host execution. The executing D1 tests in this repository are an
  HTTP mock over one SQLite connection. The shared-database proof is two
  PostgreSQL pools plus two SQLite connections. A live D1 database was not
  provisioned.
