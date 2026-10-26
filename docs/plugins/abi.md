# Entrypoints, events and the ABI

The Workers RPC surface a plugin implements and the bindings it receives: entrypoint classes, events, KV namespaces, queues and the methods of each capability.

## ABI (api_version = 3, object-capability Workers RPC)

Public types are **classes and streams**, not transport verbs. Chunking,
`handleId`, `readChunk`, `writeChunk`, `finalize`, and `abort` are not ABI
methods (abort is stream cancel / `RpcTarget` disposal).

```ts
// Default export: triggers declared in plugin.toml.
class BookclerkEntrypoint<Env> extends WorkerEntrypoint<Env> {
  describe?(): Promise<PluginDescribe>;                 // optional refinement
  event?(batch: EventBatch): Promise<void>;             // [[events.consumers]]
  queue?(batch: QueueBatch): Promise<void>;             // [[queues.consumers]]
  job?(job: JobController): Promise<JobCompletion | void>; // [triggers] jobs
  databaseMigrations?(binding: string): Promise<PluginMigration[]>; // [[databases]]
  shutdown?(): Promise<void>;
}

// Named entrypoints: one exported class per `entrypoints = [...]` item.
export class Storefront extends StorefrontEntrypoint<Env> { /* login, scan, fetchTitle, … */ }
export class Storage extends StorageEntrypoint<Env> { /* head, list, get, put, copy */ }
export class RemoteLibrary extends RemoteLibraryEntrypoint<Env> { /* health, diagnose, start, stop */ }
export class DatabaseAdapter extends DatabaseAdapterEntrypoint<Env> { /* openSession */ }
export class Cli extends CliEntrypoint<Env> { /* describe, invoke */ }
export class Oidc extends OidcEntrypoint<Env> { /* clients, authenticateUser */ }

export default class MyPlugin extends BookclerkEntrypoint<Env> { /* … */ }
```

On the wire this is `PluginWorker { describe; open(invocation, bindings) ->
Entrypoints; shutdown; databaseMigrations }`; the trusted adapter isolate
resolves each `Entrypoints` capability to the matching exported class through
its `PLUGIN_<ENTRYPOINT>` service binding and merges `Bindings` (`config`,
`secrets`, `events`, `databases`, `storage`) onto the author's `env` before
every call. Authors never subclass bare `WorkerEntrypoint`; adapter-private
`GRANTED` / `BRIDGE_TOKEN` / `PLUGIN_DESCRIBE` live only on the wrapper
(`AdapterEnv`).

Between the launcher and the adapter isolate every ABI method call is one
`POST /invoke` whose body is the `<Interface>.<method>$Params` struct as Cap'n
Proto bytes and whose reply is the `$Results` struct — the launcher reuses the
host's typed Cap'n clients over an HTTP hook, and the isolate decodes with the
generated `generated-wire.ts` codecs. Only the control handshake
(`describe` / `open` / `shutdown`) is JSON, and object bodies stream over
dedicated routes; `JobController` capabilities travel as grant-token
descriptors the isolate redeems on `GRANTED`. Maintainer reference:
[`workerd-bridge.md`](../internals/workerd-bridge.md).

`event(batch)` mirrors Workers `queue(batch)`: each `EventMessage` records one
outcome — `ack()`, `retry({ retryAt | delaySeconds, reason })`, `reject(reason)`,
`deadLetter(reason)`, or `suspend({ checkpoint, wakeAt, wakeOnEventType,
wakeOnFilter })` — and the first call wins. Untouched messages ack when the
handler returns and retry when it throws.

`job(job)` mirrors Workers `scheduled(controller)`: returning completes the job
(optionally with `{ message, bytesCopied }`), throwing rejects it
(`unavailable` / `deadline_exceeded` → retryable, `cancelled` → cancelled), and
`job.suspend({ checkpoint, wakeAt })` / `job.retryLater({ retryAt, reason })`
record the other terminal outcomes. `job.signal` is a **locally created**
`AbortSignal` projected from the transport cancellation capability —
AbortSignal does not serialize as a Workers RPC value.

`JobInvocation` is a versioned durable **command envelope** (envelope schema
version and payload schema version are separate). Idempotency keys are scoped
to `(account, plugin, commandType)` until a terminal fenced outcome is
committed; they are not reusable across accounts. `deadlineUnixMs` is a guest
hint; the host fence/lease is authoritative and must not be outlived.
Suspension is durable only after Bookclerk atomically commits the fenced
outcome.

`JobController` grants `input` / `output` / `progress()` for one durable
invocation. Media flows through those streams. Progress, checkpoints,
completion, retry class, and cancellation stay job state — not chunk messages.

Workerd is the **control-plane** front door (invocation / policy / binding /
lifecycle / outcome) for **every** plugin. The host always spawns
`bookclerk-workerd` through `bookclerk-jail`. For `runtime = "native"` it
also spawns the backend as a sibling jail and hands both children the
inherited RPC and proxy links; the launcher never sees `BOOKCLERK_NATIVE_BACKEND`
and never starts the guest itself. Isolate vs native-jail vs future container
are backends the host selects from `runtime`. For a native guest the launcher
speaks typed Cap'n Proto over the inherited RPC link and forwards every
`Entrypoints` family (event consumer, job runner, storefront, storage,
database adapter, remote library, CLI, OIDC) without entering JavaScript; the
adapter isolate only decides `describe` (merged against `PLUGIN_DESCRIBE`) and
`open` policy (`POST /open` → `openInvocation`) and receives `shutdown`.
Direct host↔native Cap'n Proto is not a product path: it exists only as
`SpawnTransport::DirectNativeDiagnostic` for tests and diagnostics, and a
missing `bookclerk-workerd` / `workerd` is a hard spawn error in every
isolation mode. The OS jail is still required. Windows `Isolation::Off`
still requires `bookclerk-jail` beside the host.

Native plugins must be rebuilt against this SDK. Production transport is the
mux `fd:` / `handle:` link and `PluginSocket::into_stream`. `into_split` and
`into_unix_stream` are Unix pathname sockets only; they return an
unsupported-transport error on a mux link and do not open a TCP socket.
The host gives each guest a fresh challenge (`BOOKCLERK_SESSION_CHALLENGE`)
and reads those bytes before it serves the proxy. An unrelated child, or the
other session's guest, cannot complete that challenge. The numeric descriptor
is not the session. On macOS, socket and pipe creation holds a process-wide
lock from allocation through `FD_CLOEXEC` and through in-process
`Command::spawn`, because macOS has no `SOCK_CLOEXEC`. The SDK sets
`FD_CLOEXEC` again when it adopts an inherited descriptor.

List pagination is **opaque and bounded**. Missing/stale cursors return
`invalid_cursor` (never silently restart at page one). Concurrent mutation is
weakly consistent. Local backends do not promise a lexicographic walk over an
unsorted directory without an index.

Scalar RPC values are capped at **256 KiB** (`payload_too_large` if exceeded).
List pages are clamped. Integrity metadata (etag / sha256) rides on
`PutResult` / `ReadResult`. Optional facilities *within* the ABI are feature
flags (`rpc.streams`, `rpc.scalarLimits`, `storage.copy`), not a substitute
for `apiVersion`. Spawn **negotiates** `apiVersion == 3`, matching verified
provenance-qualified identity / declared `entrypoints` + triggers, required `rpc.streams` +
`rpc.scalarLimits`, and rejects zero/unsafe limits. There is no `kind` or
`supportedRoles` on the wire.

**Transports** (same observable contract):

| Runtime | Wire |
| --- | --- |
| **workerd** | Isolate keeps `RpcTarget` stubs; `bookclerk-workerd` serves Bookclerk Cap'n Proto on stdio and talks HTTP/JSRPC to the isolate with streamed bodies (`capnpConnectHost = "plugin"` on the rpc socket). Author methods travel as one `POST /invoke` Cap'n `$Params` body. |
| **native** (behind workerd) | Guest SDK `serve` serves `schema/plugin.capnp` (`capnp-rpc`) with windowed byte streams on stdin/stdout (the inherited RPC link); `bookclerk-workerd` owns the other end, forwards every entrypoint family typed, and serves the host on its own stdio. CONNECT uses the inherited mux link. `ExecutorIdentity.runtime_backend = "native-behind-workerd"` |
| **native** (direct, diagnostic) | Same guest wire, host connected to the guest's stdio with no launcher in between. `SpawnTransport::DirectNativeDiagnostic` only (`runtime_backend = "native-direct"`) |

FD passing / `localPath` remain native-only optimizations behind the stream
adapter, never author-facing. Describe rejects unsupported versions.

First-party destinations (`local`, `s3`) and remaining product guests speak
the object-capability ABI. Echo examples are `api_version = 3` guests that
export `cli` (and optional event/job triggers) — not a `kind = "integration"`.

### Coming from Cloudflare Workers

| Workers concept | Bookclerk v3 |
| --- | --- |
| `export default class extends WorkerEntrypoint` | `export default class extends BookclerkEntrypoint` (`event` / `queue` / `job` triggers) |
| Named `export class` entrypoints | `entrypoints = ["storefront", …]` + matching `*Entrypoint` subclass |
| `env.DB` | `[[databases]]` → granted on `env` |
| `env.KV` | `[[kv_namespaces]]` → `get` / `getWithMetadata` / `put` / `delete` / `list` with `expiration`, `expirationTtl`, and `metadata` |
| `env.SECRETS` | `[secrets]` → granted on `env` |
| `[[queues.producers]]` / `[[queues.consumers]]` | `env.<BINDING>.send` / `sendBatch` and `queue(batch)` with per-message `ack` / `retry` |
| `[[events.consumers]]` | `event(batch)` with `ack` / `retry` / `reject` / `deadLetter` / `suspend` |
| `[[events.producers]]` | `env.EVENTS.publish` (host outbox; forced `source`) |
| Cron / scheduled | Not implemented yet. |
| `[triggers] jobs` | `job(controller)` (durable command envelope) |
| `wrangler types` | `bookclerk-plugin types` → generated `Env` |
| Workers for Platforms / Dynamic Workers | **Not used** — local embedded workerd + OS jail only |

## Publishing events (`env.EVENTS`)

Plugins that declare `[[events.producers]]` publish domain events through the
`EVENTS` binding — a `Bindings.events` capability the host passes to
`PluginWorker.open(invocation, bindings)`. There is no other guest → host
notification path: the former `HOST.notify` reverse channel and its
`host_stub.js` / `NOTIFY` bindings are gone.

```ts
const ok = await env.EVENTS.publish({
  eventType: "wishlist_synced",         // must be a granted [[events.producers]] type
  schemaVersion: 1,                     // defaults to 1
  deduplicationKey: `wishlist:${asin}`, // optional; see below
  payload: { asin, title },             // JSON, ≤ 64 KiB UTF-8 (MAX_EVENT_PAYLOAD_BYTES)
});
// ok: { eventId: string, duplicate: boolean }
```

The host `EventPublisher` writes straight into the library outbox
(`domain_events`) with these rules:

- **Forced provenance.** `source` is always the plugin's PluginKey and
  `account_id` is the opening invocation's account; a guest cannot spoof
  another producer. `correlationId` / `causationId` default to the
  invocation's values when the guest leaves them empty.
- **Host-emitted types.** Some domain events belong to the host.
  `book_acquired` is one: a source plugin's acquire handler returns the
  book, then the host's acquire pipeline records it and publishes
  `book_acquired` in the same transaction as the library row. A plugin
  consumes it under `[[events.consumers]]` but cannot declare it under
  `[[events.producers]]` (`check` and load fail), and the host publisher
  refuses it even from an older grant.
- **Producer grant.** The binding exists only when the manifest declares
  `[[events.producers]]` *and* the operator grant covers those types (the
  consent request lists them). Publishing a type outside that intersection
  fails with `forbidden`; an empty intersection means no `EVENTS` on `env`.
- **Payload cap.** `payload` is UTF-8 JSON of at most 64 KiB
  (`payload_too_large` otherwise). Non-JSON bytes are `invalid_params`; an
  empty payload is stored as `{}`.
- **Deduplication.** `(account_id, source, event_type, deduplication_key)` is
  a permanent unique index on the outbox. A repeat publish returns the existing
  `eventId` with `duplicate = true` instead of inserting a second row. An empty
  `deduplicationKey` gets a fresh UUID, so it always publishes.
- **Availability.** Outbox write failures surface as `unavailable`
  (retryable); the event is never half-written.

**Transports.** Native guests receive `Bindings.events` as a Cap'n Proto
capability. Under workerd the trusted adapter isolate mints a per-`open`,
events-only grant token on the launcher's `GRANTED` channel and hands the author
a `GrantedEvents` `RpcTarget`; `publish()` POSTs the event to
`/events/publish`, where the launcher re-checks the grant (unknown, expired, or
database-only grants are `forbidden`) and enforces the payload cap before
forwarding to the host publisher. Author isolates never see `GRANTED` or the
grant token.

## KV namespaces

`[[kv_namespaces]]` gives a plugin Workers `KVNamespace`-shaped storage
that the host serves from the library database (`plugin_kv`). Every row is
scoped to the plugin's PluginKey and to the namespace the manifest row
names, so the values follow the instance: a sync cursor written on one host
is what the same instance reads after it moves to another.

```toml
[[kv_namespaces]]                # env.KV, namespace "kv"

[[kv_namespaces]]
binding = "CURSORS"              # env.CURSORS
namespace = "sync.cursors"       # [a-z0-9][a-z0-9._-]*; default: binding lowercased
max_value_bytes = 4096           # default 65536, at most 1 MiB
max_keys = 500                   # default 10000, at most 1000000
```

```ts
await env.CURSORS.put(`audible:${accountId}`, nextPageToken, {
  expirationTtl: 86_400,          // or `expiration` (seconds since epoch)
  metadata: { scannedAt: Date.now() },
});
const cursor = await env.CURSORS.get(`audible:${accountId}`); // string | null
const { value, metadata } = await env.CURSORS.getWithMetadata(key, "json");
const listing = await env.KV.list({ prefix: "audible:", limit: 100 }); // { keys, list_complete, cursor? }
await env.KV.delete(key);
```

Rules:

- **Scope.** The guest never chooses the plugin key or namespace; both come
  from the manifest row and the spawned identity. Two rows are separate
  stores unless they name the same `namespace`.
- **Consent.** Declaring any row adds the `plugin_kv` binding token to the
  consent request; without it no `env.<BINDING>` appears.
- **Limits.** Keys are 1–512 UTF-8 bytes (`MAX_KV_KEY_BYTES`), metadata is
  JSON of at most 1 KiB (`MAX_KV_METADATA_BYTES`), and a value is at most the
  row's `max_value_bytes` (`payload_too_large` otherwise). A `put` of a new
  key into a namespace already holding `max_keys` live keys fails with
  `forbidden`; rewriting an existing key never counts against the cap.
- **Expiry.** Expiry is lazy: an expired key reads as absent, does not
  count against `max_keys`, and is removed by the next write that needs the
  slot or by the host's periodic prune.
- **Consistency.** Reads see the latest committed write (it is a library
  row, not an edge cache), so there is no Workers-style 60-second
  propagation window.
- **Transports.** Native guests receive `Bindings.kv` (`NamedKv`) as Cap'n
  Proto `KvNamespace` capabilities. Under workerd the adapter isolate mints
  one kv-only grant token per binding and the author's `env.<BINDING>`
  calls `POST /kv/get|put|delete|list` on the launcher's `GRANTED` channel.
  Python guests get the same surface as `KvNamespace` (`get`,
  `get_with_metadata`, `put`, `delete`, `list`).

## Queues

`[queues]` gives a plugin Workers-shaped queues on the host outbox. A
`[[queues.producers]]` row is an `env` binding that sends to one queue; a
`[[queues.consumers]]` row delivers that queue to the default export's
`queue(batch)` trigger. Queue names are `[a-z0-9][a-z0-9._-]*`; the `core.`
and `host.` prefixes are the host's (`core.events` is the domain-event bus
that `[[events.*]]` rows bind to). Two plugins that name the same queue share
it: one produces, the other consumes.

```toml
[[queues.producers]]
queue = "thumbnails"
binding = "THUMBNAILS"          # env.THUMBNAILS

[[queues.consumers]]
queue = "thumbnails"
max_batch_size = 10             # messages per queue(batch) call (≤ 100)
max_retries = 3                 # redeliveries before dead-letter
dead_letter_queue = "thumbnails-dlq"
retry_delay = 30                # seconds when retry() names no delay
```

```ts
// Producer: Workers `Queue`-shaped.
const { messageIds } = await env.THUMBNAILS.send({ bookId }, { delaySeconds: 5 });
await env.THUMBNAILS.sendBatch(
  [{ body: "plain", contentType: "text" }, { body: bytes, contentType: "bytes" }],
  { delaySeconds: 0 },
);

// Consumer: one ordered batch from one queue, one outcome per message.
export default class MyPlugin extends BookclerkEntrypoint<Env> {
  async queue(batch: QueueBatch) {
    for (const msg of batch.messages) {          // msg.id, msg.timestamp, msg.attempts, msg.source
      try {
        await handle(msg.json());                // or msg.text() / msg.body
        msg.ack();
      } catch {
        msg.retry({ delaySeconds: 60, reason: "downstream busy" });
      }
    }
  }
}
```

Rules, mirroring `event(batch)`:

- **Outcomes.** `ack()` marks the message delivered; `retry()` redelivers
  after `delaySeconds`, or after the consumer's `retry_delay` when none is
  given. Untouched messages ack when the handler returns and retry when it
  throws. Delivery is at-least-once: consume idempotently on `msg.id`.
- **Dead letters.** A message that is still retrying after `max_retries`
  redeliveries is parked as `dead_letter`; when the consumer names a
  `dead_letter_queue` the host first copies the message there, so a second
  consumer (or an operator) can pick it up.
- **Bodies.** `contentType` is `json` (default; the body is JSON text),
  `text` (UTF-8), or `bytes` (raw). Bodies are capped at 64 KiB
  (`MAX_EVENT_PAYLOAD_BYTES`); a batch sends at most `MAX_LIST_PAGE`
  messages. Sends never coalesce: every `send` is a new message.
- **Forced provenance.** `source` is the sending plugin's PluginKey and the
  account is the opening invocation's; the queue is fixed by the manifest
  row, never chosen per call.
- **Consent.** Each producer binding and each consumer queue is a grant
  token (`queue:<BINDING>`, `queue_consumer:<queue>`) in the consent request.
  A binding the operator has not granted is absent from `env`; a queue the
  grant does not cover is never delivered. `describe()` cannot widen either
  list beyond `plugin.toml`.
- **Transports.** Native guests receive producers on `Bindings.queues` as
  Cap'n Proto capabilities and the batch on `QueueConsumer.queue`. Under
  workerd the adapter isolate mints one queue-only grant token per binding
  and hands the author a `GrantedQueue` `RpcTarget`; `send` / `sendBatch`
  POST to `/queues/send` on the launcher's `GRANTED` channel, where the grant
  and the caps are re-checked before the host producer writes the outbox.

Operators see queues with `bookclerk queues list` (counters and live
consumers per queue) and `bookclerk queues inspect <name>` (recent messages
and deliveries), backed by `GET /api/queues` and `GET /api/queues/{name}`.
Delivery rows are the same `event_deliveries` the `bookclerk events` verbs
act on (`retry`, `ack`, `cancel`).

## Entrypoint methods

The methods each capability entrypoint answers, by capability.

### Common

| Method | Purpose |
| --- | --- |
| `describe` | Identity, version, capabilities, brand (`metadataJson`) |
| `shutdown` | Graceful teardown |
| `health` | Connectivity / config check |
| `diagnose` | Human-readable CLI probe lines |
| `cliDescribe` | Declared CLI command schema (`CliSchema`) |
| `cliInvoke` | Run a declared command (`CliInvokeParams` → `CliInvokeResult`) |
| `oidcClients` | Bookclerk-as-IdP relying-party templates (`OidcClientTemplate[]`; empty when unused) |

Role contexts carry the instance's effective settings (schema defaults under
the document) as JSON — the same object the guest sees as `env.CONFIG`.

The optional `metadataJson` field `cli` may embed the same schema as
`cliDescribe`. Prefer advertising capability `cli` and implementing both
methods. You may also mirror the schema in `plugin.toml` under `[cli]` so
`bookclerk plugins <id> --help` works without spawning the plugin; at invoke
time `describe` / `cliDescribe` remain authoritative.

### Plugin CLI

Host commands that apply to every plugin stay on Bookclerk verbs
(`plugins list|info|diagnose|approve|enable|disable`, `integrations …`,
`auth …`). Plugin-specific commands are declared and invoked as:

```bash
bookclerk plugins <plugin-id> <command> [args…]
```

Example schema (JSON / `metadataJson` `cli` / `cliDescribe`):

```json
{
  "commands": [
    {
      "name": "ping",
      "about": "Probe echo plugin",
      "args": [
        {
          "name": "message",
          "long": "message",
          "kind": "string",
          "required": false,
          "default": "hi"
        }
      ]
    }
  ]
}
```

`cliInvoke` params: `{ "command": "ping", "args": { "message": "hi" } }`.
Result: `{ "exitCode": 0, "stdout": "…", "stderr": "…", "json": … }`.

### Integration / remote-library capabilities

Handler family is derived from declared entrypoints and triggers — there is
no `kind = "integration"`. Remote-library guests export `remoteLibrary` (and
often `oidc` / `cli`); event-driven guests declare `[[events.consumers]]` and
implement `event(batch)` on the default `BookclerkEntrypoint`.

| Surface | Notes |
| --- | --- |
| `event(batch)` | Typed [`DomainEvent`](../../packages/plugin-sdk/src/plugin.ts) batch (`eventId`, `eventType`, `schemaVersion`, correlation/causation, `source`, `deduplicationKey`, `deliveryAttempt`, bounded payload). Each `EventMessage` records `ack` / `retry` / `reject` / `deadLetter` / `suspend`. A `suspend` parks a checkpoint; the next delivery copies `checkpointJson`, `checkpointSchemaVersion`, `invocationSequence`, and `resumePending`. `wakeOnEventType` / `wakeOnFilterJson` ask the host to wake on a matching later event (empty = timestamp-only). Host delivery is at-least-once; guests must be idempotent on `deduplicationKey`. |
| `remoteLibrary.health` / `diagnose` | Liveness and human-readable probe lines |
| `remoteLibrary.start` / `stop` | Background watchers |
| `remoteLibrary.scanLibrary` | `{ force: bool }` |
| `remoteLibrary.syncListening` | Listening progress snapshots; host upserts tagged with plugin id |
| `remoteLibrary.pollEvents` | Observed external users — host polls after `start` and kicks off **core** workflows (e.g. claim tickets). The plugin stays oblivious to portal/tickets |
| `oidc.authenticateUser` | `{ username, password }` → external user (when `oidc` is exported) |
| `cli` | Declared under `[cli]` / `entrypoints = ["cli"]` |

Declare durable consumers in `plugin.toml` (omit them to receive **no**
outbox deliveries — fail closed):

```toml
[[events.consumers]]
type = "book_acquired"
schema_versions = [1]
supports_suspend = false
# resource_class = "network"   # default; the only accepted value today
# filter = { accountId = "acct" }
# max_retries = 5
```

Optional `resource_class` (default `"network"`, currently the only accepted
value) is copied onto `event_deliveries`. A typo or `cpu` is rejected at
manifest validate and skipped fail-closed at dispatch. Optional `filter` is a
host-owned JSON object; the dispatcher matches **payload object key equality**
only (no plugin-provided code / CEL). Echo and Audiobookshelf may omit both
(defaults).

Any `[[events.consumers]]` row requires an `event(batch)` handler on the default export.
Each host heartbeats discovered config-enabled integration manifests (even when
spawn failed) and currently loaded integrations into `event_subscriber_nodes`
keyed by `(node_id, PluginKey)` (`plugin_id` stores the canonical PluginKey, not
the display alias). Nodes do not delete catalog rows they lack.
A plugin is live when any heartbeating node (60s TTL) has it enabled; matching
subscriptions are the union of those enabled rows. The dispatcher then
`INSERT OR IGNORE`s deliveries for pending events (one D1 atomic op per
`(event_id, plugin_id)` with receipt `dispatch-{event_id}-{plugin_id}`) **and**
late-joins already-`dispatched` events via a missing-pair anti-join (receipt
`reconcile-{event_id}-{plugin_id}`). An unchanged live catalog with no missing
pairs does a bounded empty `SELECT` and zero dispatch writes. Heartbeat of this
node’s catalog runs **before** that reconcile so a catch-up page cannot starve
the 60s TTL. Each tick dispatches at most 32 pending outbox rows, then always
runs a claimed wake slice (UUID fence token, not the node id) even when
undispatched remain; accepting a wake clears `wake_event_type` /
`wake_filter_json` / `wake_grants_json` so retry is not re-woken. A dispatch error clears the empty-reconcile skip cache; a bounded
reconcile still runs at least every 60s as a backstop. The catalog key is the node id from `node-identity.json`. A `suspended` result is accepted only when a subscription matches
that exact `(type, schema_version)` and sets `supports_suspend = true`;
otherwise it is stored as a permanent reject. Non-empty `wakeOnEventType` must
also match a declared subscription type. The host derives wake grants from
those subscriptions (`schemaVersions` plus the intersection of `sub.filter` and
`wakeOnFilterJson`); an empty requested filter keeps the subscription filter
and cannot broaden it. Waiting on an event does not grant a new binding.
Publish commits `wake_pending` and returns; the dispatcher claims bounded wake
slices so producer latency does not track sleeper count. Acquire success writes
`book_acquired` into `domain_events` in the same transaction as the library
acquire-status change (book uuid, storage key, product ids — never media bytes)
and sets envelope `source` to the book’s storefront plugin id.
The producer `ordering_key` is stored on the envelope and copied verbatim onto
each delivery. Each VPS claims only PluginKeys loaded on that process **and**
only events its node-local catalog matches (type, schema version, filter). The
host evaluates catalog JSON filters, then compare-and-sets a concrete delivery
id inside a generic atomic plan. Wake page size follows negotiated `maxBinds`
(D1 is 100) so `IN (…)` and the fenced sleeper UPDATE stay under the bind cap;
the UPDATE includes `EXISTS (wake_pending ∧ lease owner)`. `[events.concurrency]`
(default 1) is the number of local delivery workers **and** the cluster-wide
max `running` deliveries per `(plugin_id, resource_class)` (`network` today),
enforced at claim time with a portable `bookclerk_slots` row so two
VPSes cannot over-admit under `READ COMMITTED`. FIFO per ordering key stays; unrelated keys are only
blocked by that cap. The delivery worker
heartbeats the lease during `event(batch)` (`lease/3`); fence loss or operator
`cancel_requested` cancels the in-flight RPC (including workerd/native).
Expired-lease reclaim restores `resume_pending` when `checkpoint_json` is set.
See [jobs.md](../internals/jobs.md).

### Storefront capabilities

Export `storefront` (there is no `kind = "source"`). Typed Cap'n methods on
[`ContentSource`](../../packages/plugin-sdk/src/plugin.ts) / `StorefrontEntrypoint`:

| Method | Notes |
| --- | --- |
| `login` | Password storefronts. Params include `pluginDataDir`, marketplace/label/email/password. Result: `{ account, credentials? }` — host seals credentials (`provider = plugin id`) and upserts the account row |
| `loginStart` / `loginComplete` | OAuth storefronts (Audible). Start returns `{ sessionId, url }`; complete returns login result |
| `scan` | Params include `pluginDataDir`, filters, and host-injected `credentials` map (`accountId` → opaque JSON; **no** library DB path). Result includes `books[]` DTOs; host upserts with `source` forced to plugin id |
| `fetchTitle` | Host injects `credentials` from `encrypted_secrets`; plugin writes media under the work directory and returns **plain** paths (DRM guests decrypt before return) |

Plugins must not open `library.db` or read `master.key`. Do not put Encrypted
content keys on the wire — decrypt in the guest when needed.

### Storage entrypoints

Export `storage` (there is no `kind = "output"`). Guests implement
[`Destination`](../../packages/plugin-sdk/src/plugin.ts) /
`StorageEntrypoint`: `head` / `list` (paginated) / streamed `get` / streamed
`put` / optional `copy`. The host never reassembles a large object into
`Bytes` and never writes the full object to guest scratch then `put_file`. S3
guests feed the existing multipart sink as bytes arrive.

Oversized scalar `put`/`get` fail closed. There is no public `handleId` /
`readChunk` / `writeChunk` protocol: destinations transfer media through
`ByteSource` streams. Range, multipart, and checkpoint product work in
issue #120 builds on this contract without another public ABI redesign.

First-party S3 ships as `bookclerk-plugin-destination-s3` (`api_version = 3`).
Its bucket, region, endpoint, prefix, naming overrides, and credentials are
`[[settings]]`; the host runs it wherever its instance has a present
deployment and has no in-process S3 backend. A second install that reuses
`id = "s3"` is a distinct PluginKey with its own instances.

### Database adapter entrypoints

Export `databaseAdapter` (there is no `kind = "database"`). Guests are
**BookclerkSQL adapters**. They implement the SeaORM proxy boundary over
Workers RPC plus a generic atomic-plan executor.
Guests speak only canonical BookclerkSQL; adapters lower at execute
(`bookclerk-db-exec`). Engine connect/migrate/proxy code lives in the guest
(`bookclerk-plugin-database-sqlite` and optional d1/postgres guests); the host
does not link SQL engines. The host owns schema, domain SQL, proofs, and
Bookclerk invariants
([ADR: SQL database contract](../adr/sql-database-contract.md)). Guests must not
import Bookclerk entities or embed application table names. The host opens the
library through the external database loader (guest required — no in-process
fallback). SQLite opens `library.db` at the jail-granted path
(`BOOKCLERK_SQLITE_PATH` / `sqlitePath`) when the session opens.

A backend that cannot advertise `atomicBatch`, parameterized statements, and
bind/statement limits at or above the host minimum is **not loaded** (fail
closed). Non-SQL engines are unsupported.

| Method | Notes |
| --- | --- |
| `Database.openSession` | Opens the adapter session. The guest connects its engine from `DatabaseContext.config` (verified first-party guests receive host-injected connect params; SQLite: path grant; D1/Postgres: host-injected credentials). |
| `Database.dropUnit` | Physically deletes one provisioned binding unit (`unitRef` is adapter-owned: sqlite path, postgres database, D1 name). Missing units are success. Adapters that cannot prove deletion fail closed. |
| `AdapterDatabaseSession.capabilities` | Typed control-plane call after `openSession`. Advertises SQL contract version, execution semantics, `schemaMigrations`, backup flags, and all limits. Host policy requires `schemaMigrations`. Diagnostic engine identity is not a capability. The host must not invent these from the plugin id. |
| `AdapterDatabaseSession.bootstrap` | Diagnostic engine name only (`engine`). Not part of `DbCapabilities`, never used to admit a guest or generate SQL. Any string is valid. |
| `AdapterDatabaseSession.execute` | The one typed atomic operation (`AdapterExecuteRequest` → `ExecuteReply`). The request is already-desugared canonical Bookclerk SQL (`?` placeholders) plus 1:1 hash-bound proofs. Adapters lower at execute. Guests do not interpret Bookclerk operation names. `job(job)` does **not** receive the host library on `env`. Plugins that need durable SQL declare `[[databases]]` bindings and receive physically separate units on `env.<BINDING>`. |
| `AdapterDatabaseSession.close` | Release the session and its engine connection. |

Host-private (never visible to plugin authors; first-party guests built with
the abi `host` feature only): `HostAdapterDatabaseSession.begin` opens a native
interactive SeaORM transaction and returns an `AdapterTransaction`
(`execute` / `commit` / `rollback`); the host records a sticky per-task fault
when `begin` fails so later statements cannot fall back to autocommit, and a
failed `commit` is surfaced to `LibraryStore` (SeaORM's proxy hook is
infallible). `AdapterDatabaseSession.execute` and
`HostAdapterDatabaseSession.execute` carry the same
`AdapterExecuteRequest` (canonical SQL, structured proofs, optional
receipt-persist hint). D1 keeps `begin` unsupported and routes
host `execute` through its native batch proxy.

Built-in ids: `sqlite`, `d1`, `postgres` (match `[database].plugin`).

### Isolated plugin database bindings

Plugins may declare Workers-style **named database bindings** in the manifest:

```toml
[[databases]]
binding = "DB"

[[databases]]
binding = "CACHE"   # [A-Z][A-Z0-9_]*, unique, max 8
```

SQL bindings do not replace `[[kv_namespaces]]`. Named bindings are
**plugin-private state**, not a place to put host tables.
The durable job queue, library catalog, and secrets stay on the host library
database; `job(job)` never gets an `env` binding pointed at
`library.db`. Bindings are provisioned by the active adapter — physically
separate from the Bookclerk library and from every other plugin
(near-equivalent to a Cloudflare Workers D1 binding):

- **SQLite** — one file per binding under
  `$BOOKCLERK_FILES_DIR/plugin-databases/<plugin-key-fs-id>/<BINDING>.db`
  (the sqlite adapter jail grants that directory). The leaf is
  `PluginKey::fs_id()` (`pk-` plus 32 hex / 128 bits), not the display alias and not
  the canonical PluginKey (which is not a valid Windows path component).
  The `plugin_databases` registry still stores the canonical PluginKey.
- **PostgreSQL** — one database per binding (`pb_` + 32 hex of the
  `(plugin, binding)` digest; 35 ≤ 63), created on first use (`CREATEDB`
  required). This is a separate database, not a schema on the library DB,
  so plugin SQL cannot see host tables.
- **Cloudflare D1** — one D1 database per binding
  (`bookclerk-pb-` + the same 32 hex), resolved or created by name through
  the REST API. Provisioning fails closed with an operator-facing error when
  the API token cannot create databases.
- **Third-party adapters** — advertise `DbCapabilities.pluginDatabases` and
  the backup flags they can actually provide (`consistentBackupRead`,
  `atomicUnitRestore`). The host opens plugin bindings through the active
  adapter session (`DatabaseAdapterConfig` + `provision`); it does not
  switch on sqlite/postgres/d1 crates. Adapters
  that do not advertise support fail the job rather than sharing a database.

Consent: each binding appears as a `database:<NAME>` grant entry and requires
operator approval before enable, like other capabilities. Provisioned units
are recorded in the host `plugin_databases` registry (an existing row wins so
re-opens never re-target a binding); inspect and remove them with
`bookclerk plugins db list` / `bookclerk plugins db drop <plugin> [binding]`
(the drop command calls the adapter's `Database.dropUnit`, then removes the registry row; it fails closed if physical delete cannot be proven).

Inside a binding the plugin **owns its schema** by registering a complete
ordered history at startup (`databaseMigrations(binding)`): opaque
plugin-chosen IDs plus already-separated BookclerkSQL `schema`/`data`
operations. Bookclerk assigns no version meaning to those IDs; registration
order is the forward sequence. Registration is a bounded startup scalar: at
most `maxListPage` migrations, `maxPluginMigrationOps` operations per
migration, `maxPluginMigrationTotalOps` operations across the registration,
each SQL at most `maxScalarBytes`, and aggregate id+SQL UTF-8 at most
`maxPluginMigrationRegistrationBytes`. The host proves the sequence with one evolving
type environment, verifies durable `bookclerk_plugin_migrations` history is an exact
prefix of the registration, and applies only the pending suffix. Ordinary
binding `execute` is query/DML only; durable `CREATE`/`DROP` is admitted only
while the host applies that registration. `ALTER` and `CREATE TABLE AS` stay
refused.
`REFERENCES` targets use the same reserved-name rules as `CREATE`/`DROP`:
inside a binding the host reserves exactly one identifier prefix,
`bookclerk_` (its bookkeeping — `bookclerk_schema_migrations`,
`bookclerk_plugin_migrations`, `bookclerk_receipts`, `bookclerk_slots`, the
`bookclerk_sql_*` catalog, the `bookclerk_src` wrap alias — all carry it),
plus engine catalogs (`sqlite_*`, `pg_*`, `information_schema`) and
schema-qualified names. Any other name, including a framework's own
`schema_migrations`, is the plugin's. The guest grammar still applies —
single statement, no `ATTACH`/`PRAGMA`/session verbs, no schema-qualified
names — and functions are Bookclerk SQL v1 portable helpers (not a wider
SQLite dialect; `hex` is denied; column types are `INTEGER` / `REAL` /
`TEXT` / `BLOB` / `BOOLEAN`; `INSERT OR IGNORE` is unique/PK-conflict only and
is lowered to `ON CONFLICT DO NOTHING` on every backend, including SQLite and
D1 (`SELECT`/`WITH` sources are wrapped as `SELECT * FROM (<source>) AS
bookclerk_src WHERE true`); helper arity and wire types are enforced so callers need
not `CAST` for `round` / `sum` / `avg` / `count`. Canonical `LIKE` is
case-sensitive (SQLite/D1 `GLOB` lowering; Postgres `COLLATE "C"`). Schema
metadata is durable in adapter-private `bookclerk_sql_catalog` /
`bookclerk_sql_schema` / `bookclerk_sql_ddl` (Postgres identity in
`bookclerk_identity`); all are guest-denied. Opening a binding
reloads types from that catalog. A matching no-op `CREATE TABLE IF NOT
EXISTS` does not rewrite catalog or identity: the first admitted CREATE
must persist complete canonical DDL, and a binding missing that catalog
fails closed (reset/recreate).
See [`docs/sql-contract/v1.md`](../sql-contract/v1.md).
Plugin schema apply is one atomic receipt per registered migration (slot
lock, ops, journal append). Mixed guest `CREATE` + `INSERT` is not a product
path: register/apply, then DML. Same-token DML replay must not double-insert.
The binding's own `bookclerk_receipts` and `bookclerk_plugin_migrations` tables stay
host-owned so retry tokens replay inside the binding and plugins cannot
edit the journal.

Delivery: `PluginWorker.open` receives the bindings as the append-only
`Bindings.databases :List(NamedDatabase)` field. Rust guests call
`DatabaseBinding::take_named_from_bindings(&mut bindings, "DB")`. For workerd
guests the launcher mints one database-only grant token per binding at `open`
and carries it in the bridge context (`databases: { DB: <token> }`); the
trusted adapter isolate turns each token into a granted-channel transport over
`POST /db/execute` and installs the resulting `env.DB` — a full
`prepare`/`bind`/`run`/`all`/`first`/`raw`/`batch` `DatabaseBinding` — on the
author's `env` for every call of that `open` (events, storefront, jobs, …).
Authors never see the token.
