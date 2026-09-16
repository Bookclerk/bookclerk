# ADR: Capability-driven plugin ABI v3

- **Status:** Accepted
- **Date:** 2026-09-10
- **Supersedes:** role / factory / `HOST.notify` sections of
  [`plugin-workers-rpc-workerd.md`](plugin-workers-rpc-workerd.md)
  (`api_version = 2`). Isolation, jail, network consent, workerd pin, and
  distribution-tier decisions in that ADR remain in force.

## Context

`api_version = 2` froze an object-capability Workers RPC with **role factories**
(`BookclerkPlugin`, `Destination`, `Source`, `JobHandler`, …), JSON method
payloads for several storefront / integration surfaces, and a reverse
`HOST.notify` channel. That model fought the direction of the product:

- Authors already think in Workers terms (default export + named entrypoints,
  `env` bindings, queue/scheduled triggers).
- The host needs one control-plane front door (`bookclerk-workerd`) for every
  guest, including native storefronts and database adapters.
- Domain events belong in a durable outbox with typed consume/publish, not a
  guest→host notify stub.
- Database guests must stay engine-agnostic and isolated from host bookkeeping
  (`bookclerk_` prefix — see [`sql-database-contract.md`](sql-database-contract.md)).

## Decision

1. **Product `api_version = 3`.** Manifests are wrangler-shaped:
   `main` / `[workerd]`, `entrypoints = [...]`, `[[events.consumers]]` /
   `[[events.producers]]`, `[[databases]]`, `[vars]` / `[secrets]`, optional
   `[triggers] jobs`, `[cli]`, `[[oidc.clients]]`. There is **no** `kind`,
   `methods.list`, or `supportedRoles`. Handler family is derived from
   declared entrypoints and triggers. Consent is by entrypoint + binding +
   producer + **event consumer** + **job trigger** (+ network), not by role
   string. Event-consumer identity includes `type`, `schema_versions`,
   `supports_suspend`, and `filter` (what the plugin receives). Operational
   knobs `resource_class` and `max_retries` are not authority. Operators may
   narrow structural capabilities; they may not invent ones the package did
   not declare. Adding a consumer or job on a same-PluginKey upgrade leaves
   the existing approval usable for the previously granted subset; the new
   capability stays pending until the operator consents. Empty structural
   sets mean “approve none” for every grant schema version, including
   omitted/`0` legacy JSON; they do not inherit the manifest.
   `authority_revision` includes consumers, jobs,
   and operator-controlled resource budgets.

2. **`PluginWorker.open(invocation, bindings) -> Entrypoints`.** One open per
   invocation returns typed capabilities (`eventConsumer`, `jobRunner`,
   `storefront`, `storage`, `databaseAdapter`, `remoteLibrary`, `cli`,
   `oidc`); absent families are null. Role factories are deleted. `Bindings`
   carries `EVENTS` (`EventPublisher`), named `[[databases]]`, config,
   secrets, and optional `WORK_FS`. Per-job `input` / `output` / `progress` /
   `cancel` travel on `JobController`, not on open.

3. **Typed Cap'n Proto everywhere.** Storefront, CLI, OIDC, remote-library,
   event, and job method payloads are Cap'n structs (generated TS interfaces /
   Python TypedDicts + `@internal` codecs). JSON remains only for
   plugin-specific extensible config and event payloads (bounded). In-band
   errors use `result` unions (`ok` / `err`) with `PluginErrorCode` wire
   strings.

4. **SDK idioms mirror Workers.** Authors extend `BookclerkEntrypoint`
   (default export) with optional `event(batch)` / `job(controller)` triggers
   and export named `*Entrypoint` classes. `EventMessage` records
   `ack` / `retry` / `reject` / `deadLetter` / `suspend`. `bookclerk-plugin
   types` generates `Env` from `plugin.toml`. Native Rust guests implement
   `PluginWorker` + `serve` on stdio Cap'n RPC.

5. **`env.EVENTS.publish` replaces `HOST.notify`.** The host
   `EventPublisher` forces `source` = plugin id, intersects producers with the
   operator grant, requires / synthesizes dedup keys, caps payload size, and
   writes the library outbox. Consume is typed `EventConsumer.event` with
   at-least-once delivery; guests must be idempotent on `deduplicationKey`.

6. **Workerd is the mandatory front door.** The host spawns every plugin
   through `bookclerk-workerd`. Isolate vs native-jail is a backend the
   launcher selects from `runtime`. One `POST /invoke` carries Cap'n
   `$Params` bytes (headers name interface / method / bridge context / cap
   table); the trusted adapter dispatches to author classes or the native
   broker. Broker fast paths (`AdapterDatabaseSession`, byte `Source`) never
   enter JavaScript. Direct host↔native Cap'n Proto is
   `SpawnTransport::DirectNativeDiagnostic` only (tests / diagnostics).

7. **Three-path conformance.** Shared vectors run against (a) workerd
   author fixtures, (b) workerd → broker → nested-jailed native backend,
   and (c) diagnostic direct Cap'n Proto — **for every surface those
   fixtures support** (storefront/CLI and jobs on all three paths; events
   and named-database binding vectors on workerd author fixtures; product
   sqlite `databaseAdapter` on native-behind-workerd). Path (b)'s OS
   confinement is the nested `NetPolicy::Deny` around the native child
   inside `bookclerk-workerd` (the harness does not separately spawn
   `bookclerk-jail`). Shared helpers in
   `crates/bookclerk-workerd/tests/conformance.rs` plus
   `bookclerk-workerd-native-fixture` keep the transports honest.

8. **One network policy engine.** `fetch()`, workerd `connect()`, and the
   native-behind-workerd socket proxy consume the same `EgressPolicy`:
   fetch hosts, TCP `host+ports`, redirect consent, and CIDR address-space
   grants. Fetch does not imply TCP. Default address space is public
   Internet only. Native guests are nested under `NetPolicy::Deny` and must
   use the SDK socket capability; the launcher jail stays `OutboundListen`
   for the RPC bridge. Grant mutation is live: `plugin-grants.json` changes
   fence running vats immediately (vat shutdown + kill child + drop the
   socket proxy and granted channels), including CLI writes observed by
   `bookclerkd`. Idle mediated TCP is interrupted on the fence; enforcement
   does not wait for the next host RPC. `grant_revision` (persisted operator
   consent) is distinct from `authority_revision` (effective runtime grant
   after host overlays and clamped budgets) and from
   `configuration_revision` (manifest / host-config hash). The grant
   watcher compares grant revisions only and retries malformed files
   without mass-fencing.

## Consequences

- Daemon / CLI / UI group plugins by handler family, not `PluginKind`.
- Echo examples and product guests are `api_version = 3` only.
- CI fails on Cap'n schema drift (`scripts/gen-plugin-abi.py --check`),
  author-surface leakage (`tools/author-surface-check`), and workerd pin /
  bridge mirror drift (`scripts/sync-workerd-pin.py --check`).
- Docs: this ADR is the v3 decision record; [`plugins.md`](../plugins.md)
  is the author/operator handbook.

## Non-goals

- Dual-stack support for `api_version = 2` role factories or `HOST.notify`.
- Cloudflare-hosted Dynamic Workers for operator plugin execution.
- Marketing workerd alone as the security boundary (OS jail remains
  mandatory — unchanged from the v2 ADR).
