# Entrypoints, events and the ABI

This page is the reference for the Workers RPC surface a plugin
implements and the bindings it receives: entrypoint classes, events, KV
namespaces, queues, and the methods of each capability.

## ABI (api_version = 3, object-capability Workers RPC)

Public types are **classes and streams**, not transport verbs. Chunking,
`handleId`, `readChunk`, `writeChunk`, `finalize` and `abort` aren't ABI
methods. Abort is a stream cancel, or `RpcTarget` disposal.

```ts
// Default export: triggers declared in plugin.toml.
class BookclerkEntrypoint<Env> extends WorkerEntrypoint<Env> {
  describe?(): Promise<PluginDescribe>;                 // optional refinement
  event?(batch: EventBatch): Promise<void>;             // [[events.consumers]]
  queue?(batch: QueueBatch): Promise<void>;             // [[queues.consumers]]
  job?(job: JobController): Promise<JobCompletion | void>; // [triggers] jobs
  scheduled?(controller: ScheduledController): Promise<void>; // [triggers] crons
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
export class Metadata extends MetadataEntrypoint<Env> { /* health, lookup, search, chapters */ }
export class Covers extends CoverEntrypoint<Env> { /* health, fetchCovers */ }

export default class MyPlugin extends BookclerkEntrypoint<Env> { /* … */ }
```

### On the wire

On the wire this is
`PluginWorker { describe; open(invocation, bindings) -> Entrypoints; shutdown; databaseMigrations }`.

- The trusted adapter isolate resolves each `Entrypoints` capability to
  the matching exported class, through its `PLUGIN_<ENTRYPOINT>` service
  binding.
- Before every call it merges `Bindings` (`config`, `secrets`, `events`,
  `databases`, `storage`) onto the author's `env`.
- Authors never subclass bare `WorkerEntrypoint`. The adapter-private
  `GRANTED`, `BRIDGE_TOKEN` and `PLUGIN_DESCRIBE` live only on the
  wrapper (`AdapterEnv`).

Between the launcher and the adapter isolate, every ABI method call is one
`POST /invoke`. Its body is the `<Interface>.<method>$Params` struct as
Cap'n Proto bytes, and its reply is the `$Results` struct. The launcher
reuses the host's typed Cap'n clients over an HTTP hook, and the isolate
decodes with the generated `generated-wire.ts` codecs.

- Only the control handshake (`describe`, `open`, `shutdown`) is JSON.
- Object bodies stream over dedicated routes.
- `JobController` capabilities travel as grant-token descriptors the
  isolate redeems on `GRANTED`.

Maintainer reference: [workerd bridge](../internals/workerd-bridge.md).

### Events and jobs

`event(batch)` mirrors Workers `queue(batch)`. Each `EventMessage` records
one outcome, and the first call wins:

- `ack()`;
- `retry({ retryAt | delaySeconds, reason })`;
- `reject(reason)`;
- `deadLetter(reason)`;
- `suspend({ checkpoint, wakeAt, wakeOnEventType, wakeOnFilter })`.

Untouched messages ack when the handler returns, and retry when it
throws.

`job(job)` mirrors Workers `scheduled(controller)`:

- Returning completes the job, optionally with `{ message, bytesCopied }`.
- Throwing rejects it. `unavailable` and `deadline_exceeded` are
  retryable, and `cancelled` cancels it.
- `job.suspend({ checkpoint, wakeAt })` and
  `job.retryLater({ retryAt, reason })` record the other terminal
  outcomes.
- `job.signal` is a **locally created** `AbortSignal`, projected from the
  transport cancellation capability, because an AbortSignal doesn't
  serialize as a Workers RPC value.

`JobInvocation` is a versioned durable **command envelope**. The envelope
schema version and the payload schema version are separate.

- Idempotency keys are scoped to `(account, plugin, commandType)` until a
  terminal fenced outcome is committed. They aren't reusable across
  accounts.
- `deadlineUnixMs` is a hint to the guest. The host fence and lease are
  authoritative, and must not be outlived.
- A suspension is durable only after Bookclerk atomically commits the
  fenced outcome.

`JobController` grants `input`, `output` and `progress()` for one durable
invocation. Media flows through those streams. Progress, checkpoints,
completion, retry class and cancellation stay job state, not chunk
messages.

### Workerd is the front door

Workerd is the **control-plane** front door (invocation, policy, binding,
lifecycle, outcome) for **every** plugin.

- The host always spawns `bookclerk-workerd` through `bookclerk-jail`.
- For `runtime = "native"` it also spawns the backend as a sibling jail,
  and hands both children the inherited RPC and proxy links. The launcher
  never sees `BOOKCLERK_NATIVE_BACKEND` and never starts the guest itself.
- Isolate, native jail and a future container are backends the host
  selects from `runtime`.

For a native guest, the launcher speaks typed Cap'n Proto over the
inherited RPC link. It forwards every `Entrypoints` family (event
consumer, job runner, storefront, storage, database adapter, remote
library, CLI, OIDC) without entering JavaScript. The adapter isolate only
decides `describe` (merged against `PLUGIN_DESCRIBE`) and the `open`
policy (`POST /open` → `openInvocation`), and receives `shutdown`.

> [!IMPORTANT]
> Direct host-to-native Cap'n Proto isn't a product path. It exists only
> as `SpawnTransport::DirectNativeDiagnostic`, for tests and diagnostics.
> A missing `bookclerk-workerd` or `workerd` is a hard spawn error in
> every isolation mode. The OS jail is still required, and Windows
> `Isolation::Off` still needs `bookclerk-jail` beside the host.

### Native sockets

Native plugins must be rebuilt against this SDK.

- Production transport is the mux `fd:` or `handle:` link, and
  `PluginSocket::into_stream`.
- `into_split` and `into_unix_stream` are for Unix pathname sockets only.
  They return an unsupported-transport error on a mux link, and don't
  open a TCP socket.
- The host gives each guest a fresh challenge
  (`BOOKCLERK_SESSION_CHALLENGE`) and reads those bytes before it serves
  the proxy. An unrelated child, or another session's guest, can't
  complete that challenge. The numeric descriptor isn't the session.
- On macOS, socket and pipe creation holds a process-wide lock from
  allocation through `FD_CLOEXEC` and through in-process
  `Command::spawn`, because macOS has no `SOCK_CLOEXEC`. The SDK sets
  `FD_CLOEXEC` again when it adopts an inherited descriptor.

### Limits and negotiation

- List pagination is **opaque and bounded**. A missing or stale cursor
  returns `invalid_cursor`, never a silent restart at page one.
  Concurrent mutation is weakly consistent. Local backends don't promise a
  lexicographic walk over an unsorted folder without an index.
- Scalar RPC values are capped at **256 KiB** (`payload_too_large` if
  exceeded). List pages are clamped.
- A SQL request or result over a negotiated `DbCapabilities` cap fails
  with `limit_exceeded`. `PluginError.details` (a JSON object, empty when
  an error carries none) holds `{ "limit", "cap", "used" }`, and the
  message reads `limit exceeded: <cap> is <n>; used <m>`; the SDKs expose
  both as `err.limit`. `GuestDatabase.limits` returns the caps of the
  session so a guest can stay under them; on a host that predates the
  method it fails with `unsupported`.
- A result larger than one inline reply is read as a stream.
  `GuestDatabase.executeStreamed` (and the adapter-side
  `AdapterDatabaseSession.executeStreamed`) answer the same atomic batch
  as `ExecuteStreamFrame`s under `maxStreamedResultBytes` /
  `maxStreamedResultRows`, which are at least the inline caps; an adapter
  that advertises `0` for both does not stream and answers `unsupported`.
  The SDKs read `all()` and `iterate()` through it when the session
  streams. See [streamed results](sql-contract/v1.md#streamed-results)
  and [Working within database limits](database-limits.md).
- Integrity metadata (etag, sha256) rides on `PutResult` and
  `ReadResult`.
- Optional facilities *within* the ABI are feature flags (`rpc.streams`,
  `rpc.scalarLimits`, `storage.copy`), not a substitute for `apiVersion`.

Spawn **negotiates**:

- `apiVersion == 3`;
- a matching verified, provenance-qualified identity, declared
  `entrypoints` and triggers;
- the required `rpc.streams` and `rpc.scalarLimits`.

It rejects zero or unsafe limits. There is no `kind` or `supportedRoles`
on the wire, and describe rejects unsupported versions.

### Transports

Every transport has the same observable contract.

- **workerd.** The isolate keeps `RpcTarget` stubs. `bookclerk-workerd`
  serves Bookclerk Cap'n Proto on stdio, and talks HTTP/JSRPC to the
  isolate with streamed bodies (`capnpConnectHost = "plugin"` on the rpc
  socket). Author methods travel as one `POST /invoke` Cap'n `$Params`
  body.
- **native, behind workerd.** The guest SDK's `serve` serves
  `schema/plugin.capnp` (`capnp-rpc`), with windowed byte streams on
  stdin/stdout (the inherited RPC link). `bookclerk-workerd` owns the
  other end, forwards every entrypoint family typed, and serves the host
  on its own stdio. CONNECT uses the inherited mux link.
  `ExecutorIdentity.runtime_backend = "native-behind-workerd"`.
- **native, direct (diagnostic).** The same guest wire, with the host
  connected to the guest's stdio and no launcher in between. This is
  `SpawnTransport::DirectNativeDiagnostic` only
  (`runtime_backend = "native-direct"`).

FD passing and `localPath` remain native-only optimizations behind the
stream adapter, never visible to authors.

First-party destinations (`local`, `s3`) and the remaining product guests
speak the object-capability ABI. The Echo examples are `api_version = 3`
guests that export `cli` (and optional event and job triggers), not a
`kind = "integration"`.

### Coming from Cloudflare Workers

| Workers concept | Bookclerk v3 |
| --- | --- |
| `export default class extends WorkerEntrypoint` | `export default class extends BookclerkEntrypoint` (`event` / `queue` / `job` / `scheduled` triggers) |
| Named `export class` entrypoints | `entrypoints = ["storefront", …]` + matching `*Entrypoint` subclass |
| `env.DB` | `[[databases]]` → granted on `env` |
| `env.KV` | `[[kv_namespaces]]` → `get` / `getWithMetadata` / `put` / `delete` / `list` with `expiration`, `expirationTtl`, and `metadata` |
| `env.SECRETS` | `[secrets]` → granted on `env` |
| `[[queues.producers]]` / `[[queues.consumers]]` | `env.<BINDING>.send` / `sendBatch` and `queue(batch)` with per-message `ack` / `retry` |
| `[[events.consumers]]` | `event(batch)` with `ack` / `retry` / `reject` / `deadLetter` / `suspend` |
| `[[events.producers]]` | `env.EVENTS.publish` (host outbox; forced `source`) |
| `[triggers] crons` / `scheduled(controller)` | `[triggers] crons` and `scheduled(controller)`, five-field UTC schedules (see [Scheduled triggers](#scheduled-triggers)) |
| `[triggers] jobs` | `job(controller)` (durable command envelope) |
| `wrangler types` | `bookclerk-plugin types` → generated `Env` |
| Workers for Platforms / Dynamic Workers | **Not used**: local embedded workerd + OS jail only |

## Publishing events (`env.EVENTS`)

Plugins that declare `[[events.producers]]` publish domain events through
the `EVENTS` binding. It is a `Bindings.events` capability the host passes
to `PluginWorker.open(invocation, bindings)`.

There is no other guest-to-host notification path. The former
`HOST.notify` reverse channel and its `host_stub.js` and `NOTIFY` bindings
are gone.

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
(`domain_events`), with these rules:

- **Forced provenance.** `source` is always the plugin's PluginKey, and
  `account_id` is the opening invocation's account, so a guest can't
  spoof another producer. `correlationId` and `causationId` default to the
  invocation's values when the guest leaves them empty.
- **Host-emitted types.** Some domain events belong to the host.
  `book_acquired` is one: a source plugin's acquire handler returns the
  book, then the host's acquire pipeline records it and publishes
  `book_acquired` in the same transaction as the library row.
  `edition_linked`, `edition_updated` and `holding_added` are written by
  the library model resolver in the same batch as the store listing they
  describe, and `wishlist_changed` and `listening_synced` by the library
  after a wishlist or listening write. A plugin can consume them under
  `[[events.consumers]]`, but can't declare them under
  `[[events.producers]]` (`check` and load fail), and the host publisher
  refuses them even from an older grant.
- **Producer grant.** The binding exists only when the manifest declares
  `[[events.producers]]` *and* the operator grant covers those types (the
  consent request lists them). Publishing a type outside that
  intersection fails with `forbidden`. An empty intersection means no
  `EVENTS` on `env`.
- **Payload cap.** `payload` is UTF-8 JSON of at most 64 KiB
  (`payload_too_large` otherwise). Non-JSON bytes are `invalid_params`,
  and an empty payload is stored as `{}`.
- **Deduplication.** `(account_id, source, event_type, deduplication_key)`
  is a permanent unique index on the outbox. A repeat publish returns the
  existing `eventId` with `duplicate = true`, instead of inserting a second
  row. An empty `deduplicationKey` gets a fresh UUID, so it always
  publishes.
- **Availability.** Outbox write failures surface as `unavailable`
  (retryable). The event is never half-written.

**Transports.** Native guests receive `Bindings.events` as a Cap'n Proto
capability. Under workerd:

1. The trusted adapter isolate mints a per-`open`, events-only grant token
   on the launcher's `GRANTED` channel.
2. It hands the author a `GrantedEvents` `RpcTarget`.
3. `publish()` POSTs the event to `/events/publish`. The launcher checks
   the grant again (unknown, expired or database-only grants are
   `forbidden`) and enforces the payload cap, before forwarding to the host
   publisher.

Author isolates never see `GRANTED` or the grant token.

## KV namespaces

`[[kv_namespaces]]` gives a plugin Workers `KVNamespace`-shaped storage,
which the host serves from the library database (`plugin_kv`). Every row
is scoped to the plugin's PluginKey and to the namespace the manifest row
names. So the values follow the instance: a sync cursor written on one
host is what the same instance reads after it moves to another.

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

The rules:

- **Scope.** The guest never chooses the plugin key or namespace; both
  come from the manifest row and the spawned identity. Two rows are
  separate stores unless they name the same `namespace`.
- **Consent.** Declaring any row adds the `plugin_kv` binding token to the
  consent request. Without it, no `env.<BINDING>` appears.
- **Limits.** Keys are 1–512 UTF-8 bytes (`MAX_KV_KEY_BYTES`), metadata is
  JSON of at most 1 KiB (`MAX_KV_METADATA_BYTES`), and a value is at most
  the row's `max_value_bytes` (`payload_too_large` otherwise). A `put` of a
  new key into a namespace already holding `max_keys` live keys fails with
  `forbidden`. Rewriting an existing key never counts against the cap.
- **Expiry.** Expiry is lazy. An expired key reads as absent, doesn't
  count against `max_keys`, and is removed by the next write that needs
  the slot, or by the host's periodic prune.
- **Consistency.** Reads see the latest committed write (it is a library
  row, not an edge cache), so there is no Workers-style 60-second
  propagation window.
- **Transports.** Native guests receive `Bindings.kv` (`NamedKv`) as Cap'n
  Proto `KvNamespace` capabilities. Under workerd, the adapter isolate
  mints one kv-only grant token per binding, and the author's
  `env.<BINDING>` calls `POST /kv/get|put|delete|list` on the launcher's
  `GRANTED` channel. Python guests get the same surface as `KvNamespace`
  (`get`, `get_with_metadata`, `put`, `delete`, `list`).

## Queues

`[queues]` gives a plugin Workers-shaped queues on the host outbox.

- A `[[queues.producers]]` row is an `env` binding that sends to one
  queue.
- A `[[queues.consumers]]` row delivers that queue to the default export's
  `queue(batch)` trigger.
- Queue names are `[a-z0-9][a-z0-9._-]*`. The `core.` and `host.` prefixes
  are the host's (`core.events` is the domain-event bus that `[[events.*]]`
  rows bind to).
- Two plugins that name the same queue share it: one produces, the other
  consumes.

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

The rules mirror `event(batch)`:

- **Outcomes.** `ack()` marks the message delivered. `retry()` redelivers
  after `delaySeconds`, or after the consumer's `retry_delay` when none is
  given. Untouched messages ack when the handler returns, and retry when
  it throws. Delivery is at-least-once, so consume idempotently on
  `msg.id`.
- **Dead letters.** A message still retrying after `max_retries`
  redeliveries is parked as `dead_letter`. When the consumer names a
  `dead_letter_queue`, the host first copies the message there, so a
  second consumer (or an operator) can pick it up.
- **Bodies.** `contentType` is `json` (the default; the body is JSON
  text), `text` (UTF-8) or `bytes` (raw). Bodies are capped at 64 KiB
  (`MAX_EVENT_PAYLOAD_BYTES`), and a batch sends at most `MAX_LIST_PAGE`
  messages. Sends never coalesce: every `send` is a new message.
- **Forced provenance.** `source` is the sending plugin's PluginKey, and
  the account is the opening invocation's. The queue is fixed by the
  manifest row, never chosen per call.
- **Consent.** Each producer binding and each consumer queue is a grant
  token (`queue:<BINDING>`, `queue_consumer:<queue>`) in the consent
  request. A binding the operator hasn't granted is absent from `env`, and
  a queue the grant doesn't cover is never delivered. `describe()` can't
  widen either list beyond `plugin.toml`.
- **Transports.** Native guests receive producers on `Bindings.queues` as
  Cap'n Proto capabilities, and the batch on `QueueConsumer.queue`. Under
  workerd, the adapter isolate mints one queue-only grant token per
  binding and hands the author a `GrantedQueue` `RpcTarget`. `send` and
  `sendBatch` POST to `/queues/send` on the launcher's `GRANTED` channel,
  where the grant and the caps are checked again before the host producer
  writes the outbox.

Operators see queues with `bookclerk queues list` (counters and live
consumers per queue) and `bookclerk queues inspect <name>` (recent
messages and deliveries), backed by `GET /api/queues` and
`GET /api/queues/{name}`. Delivery rows are the same `event_deliveries`
that the `bookclerk events` verbs act on (`retry`, `ack`, `cancel`).

## Scheduled triggers

`[triggers] crons` runs the default export's `scheduled(controller)` on a
schedule, as Workers Cron Triggers do.

```toml
[triggers]
crons = ["*/30 * * * *", "0 3 * * MON-FRI"]
```

```ts
export default class MyPlugin extends BookclerkEntrypoint<Env> {
  async scheduled(controller: ScheduledController) {
    // controller.cron, controller.scheduledTime (UTC Unix ms),
    // controller.fireId, controller.attempt, controller.deadlineUnixMs
    await refresh(controller.scheduledTime);
  }
}
```

Python authors define `async def scheduled(self, controller)` and receive a
frozen `ScheduledController` (`cron`, `scheduled_time`, `fire_id`,
`attempt`, `deadline_unix_ms`). Native guests implement
`ScheduledHandler::scheduled` and return it from `open` as
`Entrypoints.scheduled_handler`.

- **Schedules.** Each entry has five fields (minute, hour, day of month,
  month, day of week) and is read in UTC. Fields take `*`, values, ranges,
  lists and `/step`; months and weekdays also take `JAN`–`DEC` and
  `SUN`–`SAT`, and `7` is Sunday. When both day fields are restricted, a
  day matching either one fires, as in Vixie cron. `L`, `W`, `#`, `@daily`
  style macros and seconds are rejected, and so are schedules that never
  fire or can go nine years without firing (such as `0 0 29 2 */7`,
  February 29 on a Sunday). A manifest declares at most 8 schedules of at most 128 bytes each
  (`MAX_CRON_BYTES`).
- **One fire per instance.** Each fire runs once per deployed plugin
  instance, cluster-wide. Every daemon checks its schedules every 15
  seconds, and a compare-and-swap on the shared `plugin_cron_cursors` row
  lets exactly one node claim a fire. The claim admits a durable
  `plugin_scheduled` job in the same batch, and the job queue routes it to
  a node that runs the instance (see
  [Job kinds](../internals/jobs.md#job-kinds)).
- **Missed fires coalesce.** A cluster that was down through several fire
  times runs one late fire for the newest of them, with that time as
  `scheduledTime`. The same goes for an instance that was undeployed and
  is deployed again. A schedule seen for the first time (a new instance,
  or a schedule a new version adds) waits for its next fire.
- **Overlap.** A fire that comes due while the previous fire of the same
  schedule is still queued or running is skipped, and the daemon logs a
  warning.
- **Retries.** Returning completes the fire. Throwing retries the same
  fire (same `scheduledTime` and `fireId`) with backoff, up to 3
  attempts, so handlers should be idempotent on `fireId`.
- **Consent.** Each schedule is a grant token (`cron:<schedule>`, with the
  schedule's whitespace normalized). A schedule the grant doesn't cover
  never fires, and `describe()` can't add schedules beyond `plugin.toml`.

> [!NOTE]
> Fires run only on library nodes. An owned host node (one that runs work
> for a single member) doesn't claim `plugin_scheduled` jobs.

## Entrypoint methods

The methods each capability entrypoint answers, by capability.

### Common

| Method | Purpose |
| --- | --- |
| `describe` | Identity, version, capabilities, brand (`metadataJson`) |
| `shutdown` | Graceful teardown |
| `health` | Connectivity and config check |
| `diagnose` | Human-readable CLI probe lines |
| `cliDescribe` | Declared CLI command schema (`CliSchema`) |
| `cliInvoke` | Run a declared command (`CliInvokeParams` → `CliInvokeResult`) |
| `oidcClients` | Bookclerk-as-IdP relying-party templates (`OidcClientTemplate[]`; empty when unused) |

Role contexts carry the instance's effective settings (schema defaults
under the document) as JSON: the same object the guest sees as
`env.CONFIG`.

The optional `metadataJson` field `cli` may embed the same schema as
`cliDescribe`. Prefer advertising capability `cli` and implementing both
methods.

You may also mirror the schema in `plugin.toml` under `[cli]`, so
`bookclerk plugins <id> --help` works without spawning the plugin. At
invoke time, `describe` and `cliDescribe` remain authoritative.

### Plugin CLI

Host commands that apply to every plugin stay on Bookclerk verbs
(`plugins list|info|diagnose|approve|enable|disable`, `integrations …`,
`auth …`). Plugin-specific commands are declared and invoked as:

```sh
bookclerk plugins <plugin-id> <command> [args…]
```

An example schema (JSON, `metadataJson` `cli`, or `cliDescribe`):

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

### Integration and remote-library capabilities

The handler family is derived from the declared entrypoints and triggers.
There is no `kind = "integration"`.

- Remote-library guests export `remoteLibrary` (and often `oidc` and
  `cli`).
- Event-driven guests declare `[[events.consumers]]`, and implement
  `event(batch)` on the default `BookclerkEntrypoint`.

| Surface | Notes |
| --- | --- |
| `event(batch)` | A typed `DomainEvent` batch (see below) |
| `remoteLibrary.health` / `diagnose` | Liveness and human-readable probe lines |
| `remoteLibrary.start` / `stop` | Background watchers |
| `remoteLibrary.scanLibrary` | `{ force: bool }` |
| `remoteLibrary.syncListening` | Listening progress snapshots; the host upserts them tagged with the plugin id |
| `remoteLibrary.pollEvents` | Observed external users. The host polls after `start`, and kicks off **core** workflows (such as claim tickets). The plugin stays unaware of the portal and tickets |
| `oidc.authenticateUser` | `{ username, password }` → external user (when `oidc` is exported) |
| `cli` | Declared under `[cli]` / `entrypoints = ["cli"]` |

`event(batch)` receives a typed
[`DomainEvent`](../../packages/plugin-sdk/src/plugin.ts) batch: `eventId`,
`eventType`, `schemaVersion`, correlation and causation, `source`,
`deduplicationKey`, `deliveryAttempt`, and a bounded payload.

- Each `EventMessage` records `ack`, `retry`, `reject`, `deadLetter` or
  `suspend`.
- A `suspend` parks a checkpoint. The next delivery copies
  `checkpointJson`, `checkpointSchemaVersion`, `invocationSequence` and
  `resumePending`.
- `wakeOnEventType` and `wakeOnFilterJson` ask the host to wake on a
  matching later event (empty means timestamp-only).
- Host delivery is at-least-once, so guests must be idempotent on
  `deduplicationKey`.

### Event consumers

Declare durable consumers in `plugin.toml`. Leave them out to receive
**no** outbox deliveries (fail closed):

```toml
[[events.consumers]]
type = "book_acquired"
schema_versions = [1]
supports_suspend = false
# resource_class = "network"   # default; the only accepted value today
# filter = { accountId = "acct" }
# max_retries = 5
```

- Any `[[events.consumers]]` row needs an `event(batch)` handler on the
  default export.
- The optional `resource_class` (default `"network"`, currently the only
  accepted value) is copied onto `event_deliveries`. A typo or `cpu` is
  rejected when the manifest is checked, and skipped (fail closed) at
  dispatch.
- The optional `filter` is a host-owned JSON object. The dispatcher
  matches **payload object key equality** only (no plugin-provided code
  or CEL).
- Echo and Audiobookshelf may leave both out (defaults).

### How events are delivered

**The subscriber catalog.**

- Each host heartbeats into `event_subscriber_nodes` the discovered,
  config-enabled integration manifests (even when spawn failed) and the
  integrations currently loaded. Rows are keyed by `(node_id, PluginKey)`:
  `plugin_id` stores the canonical PluginKey, not the display alias.
- The catalog key is the node id from `node-identity.json`. Nodes don't
  delete catalog rows they lack.
- A plugin is live when any heartbeating node (60 s TTL) has it enabled.
  Matching subscriptions are the union of those enabled rows.

**Dispatch.**

- The dispatcher `INSERT OR IGNORE`s deliveries for pending events, one D1
  atomic operation per `(event_id, plugin_id)`, with receipt
  `dispatch-{event_id}-{plugin_id}`.
- It **also** late-joins already-`dispatched` events through a
  missing-pair anti-join (receipt `reconcile-{event_id}-{plugin_id}`). An
  unchanged live catalog with no missing pairs does a bounded empty
  `SELECT` and zero dispatch writes.
- This node's catalog heartbeat runs **before** that reconcile, so a
  catch-up page can't starve the 60 s TTL.
- Each tick dispatches at most 32 pending outbox rows.
- A dispatch error clears the empty-reconcile skip cache. A bounded
  reconcile still runs at least every 60 s as a backstop.

**Suspend and wake.**

- After its dispatch step, each tick always runs a claimed wake slice (a
  UUID fence token, not the node id), even when undispatched rows remain.
- Accepting a wake clears `wake_event_type`, `wake_filter_json` and
  `wake_grants_json`, so a retry isn't woken again.
- A `suspended` result is accepted only when a subscription matches that
  exact `(type, schema_version)` and sets `supports_suspend = true`.
  Otherwise it is stored as a permanent reject.
- A non-empty `wakeOnEventType` must also match a declared subscription
  type.
- The host derives wake grants from those subscriptions: `schemaVersions`,
  plus the intersection of `sub.filter` and `wakeOnFilterJson`. An empty
  requested filter keeps the subscription filter, and can't broaden it.
  Waiting on an event doesn't grant a new binding.
- Publish commits `wake_pending` and returns. The dispatcher claims
  bounded wake slices, so producer latency doesn't track the number of
  sleepers.
- The wake page size follows the negotiated `maxBinds` (100 on D1), so
  `IN (…)` and the fenced sleeper UPDATE stay under the bind cap. The
  UPDATE includes `EXISTS (wake_pending ∧ lease owner)`.

**`book_acquired`.** A successful acquire writes `book_acquired` into
`domain_events` in the same transaction as the library acquire-status
change (book uuid, storage key, product ids; never media bytes). It sets
the envelope `source` to the book's storefront plugin id.

**Library model events.** Recording a store listing publishes, in the same
batch as its rows:

- `edition_linked` (`editionId`, `listingId`) when a listing is first
  stored or now sells another edition;
- `edition_updated` (`editionId`) when the listing filled a missing
  subtitle, publication date or duration on its edition;
- `holding_added` (`listingId`, `holder`) when an account or branch
  library holds the listing for the first time; `account_id` is the
  account for an account holder.

Their `source` is the storefront plugin that reported the listing.
Each daemon's search index consumes them, with `book_acquired`, to keep its
node-local index current (see
[Search index](../internals/jobs.md#search-index)).

**Taste events.** The library publishes these after the write they
describe, best effort (a failed publish is logged, and the write stands):

- `wishlist_changed` (`requestId`, `identityId`, `status`) when a wishlist
  entry is added or changes status (`open` or `cancelled`), or an open entry
  is removed with its user (`deleted`); `identityId` is the portal identity
  that wished, or null;
- `listening_synced` (`provider`, `identityIds`) when a listening sync
  changed a stored row's owner, matched book or work, authors, start,
  quarter or finish; its
  `source` is the integration that synced, and `identityIds` names the
  portal identities whose rows changed. A sync that changes none of these
  publishes nothing.

Discover's host consumer reads them to refresh shelves (see
[Discovery design](../internals/discovery.md#taste-events)).

**Claiming.**

- The producer's `ordering_key` is stored on the envelope, and copied
  verbatim onto each delivery.
- Each VPS claims only PluginKeys loaded on that process, **and** only
  events its node-local catalog matches (type, schema version, filter).
- The host evaluates catalog JSON filters, then compare-and-sets a
  concrete delivery id inside a generic atomic plan.
- `[events.concurrency]` (default 1) is the number of local delivery
  workers **and** the cluster-wide maximum of `running` deliveries per
  `(plugin_id, resource_class)` (`network` today). It is enforced at
  claim time with a portable `bookclerk_slots` row, so two VPSes can't
  over-admit under `READ COMMITTED`.
- FIFO per ordering key stays. Unrelated keys are only blocked by that
  cap.

**Running.**

- The delivery worker heartbeats the lease during `event(batch)`
  (`lease/3`).
- A fence loss or an operator `cancel_requested` cancels the in-flight RPC
  (including workerd and native).
- An expired-lease reclaim restores `resume_pending` when
  `checkpoint_json` is set.

See [Jobs](../internals/jobs.md).

### Storefront capabilities

Export `storefront` (there is no `kind = "source"`). The typed Cap'n
methods on [`ContentSource`](../../packages/plugin-sdk/src/plugin.ts) and
`StorefrontEntrypoint` are:

| Method | Notes |
| --- | --- |
| `login` | Password storefronts. Params include `pluginDataDir`, marketplace, label, email and password. Result: `{ account, credentials? }`. The host seals the credentials (`provider = plugin id`) and upserts the account row |
| `loginStart` / `loginComplete` | OAuth storefronts (Audible). Start returns `{ sessionId, url }`; complete returns the login result |
| `scan` | Params include `pluginDataDir`, filters, and a host-injected `credentials` list (one `{ accountId, credentials }` entry per account, with the credentials as opaque JSON bytes; **no** library database path). The result includes `books[]` records (see [Book records](#book-records)); the host stores them with `source` forced to the plugin id |
| `searchCatalog` | `{ query, region, limit, page, sort, field }` → `CatalogHit[]` (see [Book records](#book-records)). A `limit` of `0` means the storefront's default page of 20 hits; an empty query returns `[]` |
| `catalogDetail` | `{ productId, isbn? }` → `{ found, hit }`: the full record for one product (description, narrators, runtime, genres). A store that declares `detailByIsbn` also answers a bare ISBN, which the host sends in both fields |
| `fetchTitle` | The host injects `credentials` from `encrypted_secrets`. The plugin writes media under the work folder and returns **plain** paths (DRM guests decrypt before returning) |
| `purchaseHint` | Purchase link and, when `withPrice` is set, the live price of one title. `null` when the store does not sell it |
| `purchaseHints` | The same for several titles in one call (`{ queries }`, one answer per query in order). The default in every SDK asks `purchaseHint` for each query, those with a product id first, and stops starting lookups after 4 s, answering `null` for the rest, so the host's batch gets what the store managed; a store whose catalog prices many products per request (Audible's `products?asins=` takes fifty, Chirp's GraphQL takes aliased ids) overrides it to make one request, so a page of Discover cards costs one call per store |

When the store answers **429**, return a `PluginError` with code
`rate_limited` and, when the store sent `Retry-After`, `details.retryAfterMs`
(`PluginError::rate_limited(message, retry_after)` in Rust; the TS and Python
SDKs take the same shape). The host then leaves that store's price lookups
alone for the time it said, else for a minute that doubles on each repeat
up to fifteen minutes, and shows the cards without a price meanwhile. Any
other failure of a price lookup is soft: the card simply has no price.

> [!IMPORTANT]
> Plugins must not open `library.db` or read `master.key`. Don't put
> encrypted content keys on the wire: decrypt in the guest when needed.

#### Catalog search capabilities

Discover's catalog search merges every storefront's `searchCatalog` hits
into one list. It never names a store; a storefront takes part in ranking
and enrichment by declaring it:

- **Rank hints.** Set `rankHint` on each `searchCatalog` hit to the
  store's own 1-based rank for the requested sort, counted across pages
  (page 2 of 20-hit pages starts at 21), or to a store-wide popularity rank
  when the store has one. The relevance and popularity sorts order the
  merged list by the best rank any store gave a work, then put unranked
  works after ranked ones. Leave it `0` when the store's order means
  nothing. The Rust storefront crate's `CatalogSearchOpts::stamp_rank_hints`
  fills it from the page and limit.
- **ISBN detail lookup.** Set `catalog.detailByIsbn` in `describe()` when
  `catalogDetail` resolves any audiobook ISBN to a full record. After
  ranking a page, the host asks those stores, in `sortKey` order, to fill
  description, narrators, runtime and genres on lean rows from any store.
  A row is looked up by its ISBN, or by the store's own product id when the
  store listed the work itself. The first record found fills the gaps and
  adds that store's edition. The lookups share a short time budget, so a
  slow store can't hold up a search.

Both default to off. The first-party storefronts declare them as follows:
Audible ranks its hits, Libro.fm answers ISBN lookups, and Chirp and
GraphicAudio declare neither.

#### Book records

`scan` returns `ScanBook` records, and the catalog methods (`searchCatalog`,
`catalogDetail`, `expandCandidates`, `listDeals`) return `CatalogHit`
records. Both describe a title in structured fields, following the
[library data model](../adr/library-data-model.md):

| Field | Meaning |
| --- | --- |
| `workKind` | `book` (the default), `podcastShow`, `podcastEpisode` or `drama` |
| `expressionKind` | `narration` (the default), `text`, `translation`, `dramatisation` or `podcastAudio`. A full-cast production is a `dramatisation` |
| `mediaKind` | `audiobook` (the default), `ebook`, `print` or `podcastAudio`. Ebook and print records show in Discover but are never acquired |
| `identifiers` | `{ scheme, value, marketplace? }` entries. `scheme` is `isbn` (ISBN-10 or ISBN-13), `asin` (set `marketplace`), `openLibraryEdition` or `openLibraryWork`. The host trims each value, upper-cases ASINs, and turns a valid ISBN-10 into its ISBN-13 |
| `credits` | `{ name, role, level }` entries in billing order, one person or body per entry. `role` is `author` (the default), `narrator`, `translator`, `editor`, `illustrator`, `publisher`, `host`, `cast` or `adaptor`. Set `level` to `roleDefault` unless the credit sits somewhere unusual: by default authors and adaptors credit the work, narrators, translators, hosts and cast the expression, and editors, illustrators and publishers the edition |
| `seriesEntries` | `{ name, position?, storeSeriesId? }` entries, one per series the work belongs to. `position` is the label as the store prints it (`1`, `2.5`, `1-3`) |
| `parentProductId` | The show's product id, on a podcast episode |
| `language`, `abridgement` | Content language, and `unknown` (the default), `unabridged` or `abridged` |

`ScanBook` also carries `publishedAt` and `purchasedAt` (RFC 3339) and
`categories`.

Fill these from the store's own data. When a store returns a list of
names, send one credit per name; split a display string such as
`"Jane Doe, John Roe"` only when that is all the store gives.

> [!NOTE]
> Until the library stores works, expressions and editions, the host maps
> these records onto its current flat title row: names join by role, the
> first ISBN and ASIN fill the identifier columns, and the first series
> entry fills the series columns. An episode with no `storeSeriesId` is
> filed under the show named by `parentProductId`. Ebook and print scan
> records are not stored yet, since the flat row cannot keep them out of
> acquire.

### Metadata providers

Export `metadata` to answer bibliographic lookups from a public catalog
(OpenLibrary, Audnexus and similar). A metadata provider only describes
what the catalog says; the host decides which library title a record
belongs to, and never lets a record name an existing row.

The typed methods on `MetadataProvider` and `MetadataEntrypoint` are:

| Method | Notes |
| --- | --- |
| `health` | Liveness and readiness |
| `lookup` | `{ identifiers, marketplace?, limit? }` → `MetadataRecord[]`. Look up the editions the identifiers name. Ignore schemes you don't serve, and return `[]` when nothing matches |
| `search` | `{ title, author?, language?, marketplace?, limit? }` → `MetadataRecord[]`, most relevant first |
| `chapters` | `{ identifiers, marketplace? }` → `MetadataChapters`: `found`, chapters in pre-order with `startMs`, `lengthMs?` and `depth` (0 for top level), `brandIntroMs?`, `brandOutroMs?`, `runtimeMs?`, and `accurate` when the offsets are exact |

A `MetadataRecord` carries the same [book record](#book-records) fields as
a `CatalogHit` (kinds, identifiers, credits, series, language,
abridgement), plus `description`, `coverUrl`, `url`, `publisher`,
`lengthMinutes`, `publishedAt`, `categories` and an optional rating. It has
no store product id or price, because nothing is sold.

A provider that serves only some methods leaves the others to the base
class, which answers typed `unsupported`. Declare the catalog hosts you
call under `[capabilities.network]`. The guest has no other network
access, and the host never fetches on its behalf.

```ts
export class Metadata extends MetadataEntrypoint<Env> {
  async lookup(params: MetadataLookupParams): Promise<MetadataRecord[]> {
    const isbn = params.identifiers.find((id) => id.scheme === "isbn");
    // Identifier values come from library data: check them before they
    // reach a URL.
    if (!isbn || !/^[0-9X-]{10,17}$/i.test(isbn.value)) return [];
    const res = await fetch(`https://openlibrary.org/isbn/${encodeURIComponent(isbn.value)}.json`);
    if (!res.ok) return [];
    const edition = (await res.json()) as { title: string };
    return [
      {
        title: edition.title,
        abridgement: "unknown",
        workKind: "book",
        expressionKind: "text",
        mediaKind: "print",
        identifiers: [isbn],
        credits: [],
        seriesEntries: [],
      },
    ];
  }
}
```

For a complete provider, see the first-party `openlibrary` and `audnexus`
plugins under `crates/bookclerk-plugins/optional/` (`metadata-openlibrary`
and `metadata-audnexus`).

The host asks every deployed provider in turn: for Audible ASIN matching
during scans, Discover title details, the acquire chapter overlay, and
`bookclerk discover enrich-metadata`. Lookups and searches collect records
from every provider; `chapters` stops at the first layout with `found`
set. A provider that fails is logged and skipped
([integrations](../server/integrations.md#open-library-and-audnexus)).

### Cover fetchers

Export `covers` to download cover images for the host. The host never
fetches a cover URL itself: it hands the URLs to a cover fetcher, then
checks each body's real type, decodes it with limits and re-encodes it
before it stores or serves anything.

The typed methods on `CoverFetcher` and `CoverEntrypoint` are:

| Method | Notes |
| --- | --- |
| `health` | Liveness and readiness |
| `fetchCovers` | `({ urls, maxBytes, conditions }, sink)` → `CoverImage[]`. Fetch each URL and put its body into `sink` under the key `String(i)`, where `i` is the URL's index. Answer one `CoverImage` per URL, in request order: `{ url, stored, notModified, contentType?, error?, etag?, lastModified?, cacheControl?, expires?, date?, age? }` |

- `urls` holds 1 to `MAX_COVER_URLS` (32) absolute `https` URLs, and
  `maxBytes` is at most `MAX_COVER_BYTES` (10 MiB).
- `conditions` holds the validators the host already has, by index, and
  may be shorter than `urls`. Send `conditions[i].ifNoneMatch` as
  `If-None-Match` and `conditions[i].ifModifiedSince` as
  `If-Modified-Since` when they are set. Each value is at most
  `MAX_COVER_HEADER_BYTES` (1024) bytes.
- When the server answers `304 Not Modified`, put nothing into the sink
  and answer `stored: false, notModified: true`: the host's copy is still
  current.
- Copy the server's `ETag`, `Last-Modified`, `Cache-Control`, `Expires`,
  `Date` and `Age` into the answer verbatim, for a `200` or a `304`. Leave
  out a value longer than `MAX_COVER_HEADER_BYTES`; the host refuses an
  answer that carries one, or one that is both `stored` and
  `notModified`. The values are advisory: the host parses them again with
  its own limits.
- `sink` is a host-served `Destination` that accepts only `put`, only for
  the keys `0` to `urls.length - 1`, each once. A body over `maxBytes`
  fails that `put` with `payload_too_large`; every other method is
  `unsupported`.
- Answer a failed URL with `stored: false` and an `error` such as
  `HTTP 404`. Throw only when the whole request is malformed.
- The host checks the answer: one entry per URL, the URLs as asked, and a
  body for every `stored: true`. A `stored` answer with no body becomes an
  error.

Bodies go to the sink rather than into the reply, because a reply is
capped well below one image (see the
[workerd bridge](../internals/workerd-bridge.md#post-invoke)).

```ts
export class Covers extends CoverEntrypoint<Env> {
  async fetchCovers(params: CoverFetchParams, sink: Destination): Promise<CoverImage[]> {
    const out: CoverImage[] = [];
    for (const [i, url] of params.urls.entries()) {
      const res = await fetch(url);
      if (!res.ok || res.body === null) {
        out.push({ url, stored: false, notModified: false, error: `HTTP ${res.status}` });
        continue;
      }
      await sink.put(String(i), res.body);
      out.push({ url, stored: true, notModified: false });
    }
    return out;
  }
}
```

The first-party `covers` plugin (`crates/bookclerk-plugins/optional/covers`)
is the complete fetcher. It holds the `*`
[any public website](capabilities.md#network-rules) grant, because cover
art lives on every catalog's own CDN.

### Storage entrypoints

Export `storage` (there is no `kind = "output"`). Guests implement
[`Destination`](../../packages/plugin-sdk/src/plugin.ts) and
`StorageEntrypoint`: `head`, `list` (paginated), streamed `get`, streamed
`put`, and optional `copy`.

- The host never reassembles a large object into `Bytes`, and never writes
  the full object to guest scratch and then `put_file`. S3 guests feed the
  existing multipart sink as bytes arrive.
- Oversized scalar `put` and `get` fail closed.
- There is no public `handleId` / `readChunk` / `writeChunk` protocol.
  Destinations transfer media through `ByteSource` streams. Range,
  multipart and checkpoint work in issue #120 builds on this contract,
  without another public ABI redesign.

First-party S3 ships as `bookclerk-plugin-destination-s3`
(`api_version = 3`). Its bucket, region, endpoint, prefix, naming
overrides and credentials are `[[settings]]`. The host runs it wherever
its instance has a present deployment, and has no in-process S3 backend.

A second install that reuses `id = "s3"` is a separate PluginKey with its
own instances.

### Database adapter entrypoints

Export `databaseAdapter` (there is no `kind = "database"`). Guests are
**BookclerkSQL adapters**: they implement the SeaORM proxy boundary over
Workers RPC, plus a generic atomic-plan executor.

- Guests speak only canonical BookclerkSQL. Adapters lower it at execute
  (`bookclerk-db-exec`).
- Engine connect, migrate and proxy code lives in the guest
  (`bookclerk-plugin-database-sqlite`, and the optional d1 and postgres
  guests). The host doesn't link SQL engines.
- The host owns the schema, domain SQL, proofs and Bookclerk invariants
  (see [ADR: SQL database contract](../adr/sql-database-contract.md)).
  Guests must not import Bookclerk entities or embed application table
  names.
- The host opens the library through the external database loader (the
  guest is required; there is no in-process fallback). SQLite opens
  `library.db` at the jail-granted path (`BOOKCLERK_SQLITE_PATH` or
  `sqlitePath`) when the session opens.

> [!IMPORTANT]
> A backend that can't advertise `atomicBatch`, parameterized statements,
> and bind and statement limits at or above the host minimum is **not
> loaded** (fail closed). Non-SQL engines aren't supported.

An adapter proves it keeps this contract with the adapter conformance
suite in `bookclerk-db-testkit` (see
[A database adapter's own suite](database-limits.md#a-database-adapters-own-suite)),
which every first-party adapter passes.

| Method | Notes |
| --- | --- |
| `Database.openSession` | Opens the adapter session. The guest connects its engine from `DatabaseContext.config` (verified first-party guests receive host-injected connect params; SQLite: path grant; D1/Postgres: host-injected credentials) |
| `Database.dropUnit` | Physically deletes one provisioned binding unit (`unitRef` is adapter-owned: a sqlite path, a postgres database, a D1 name). Missing units are success. Adapters that can't prove deletion fail closed |
| `AdapterDatabaseSession.capabilities` | A typed control-plane call after `openSession`. Advertises the SQL contract version, execution semantics, `schemaMigrations`, backup flags and all limits. Host policy requires `schemaMigrations`. Diagnostic engine identity isn't a capability. The host must not invent these from the plugin id |
| `AdapterDatabaseSession.bootstrap` | The diagnostic engine name only (`engine`). Not part of `DbCapabilities`, and never used to admit a guest or generate SQL. Any string is valid |
| `AdapterDatabaseSession.execute` | The one typed atomic operation (`AdapterExecuteRequest` → `ExecuteReply`). The request is already-desugared canonical Bookclerk SQL (`?` placeholders) plus 1:1 hash-bound proofs. Adapters lower at execute. Guests don't interpret Bookclerk operation names. `job(job)` does **not** receive the host library on `env`: plugins that need durable SQL declare `[[databases]]` bindings, and receive physically separate units on `env.<BINDING>` |
| `AdapterDatabaseSession.executeStreamed` | The same atomic batch as `execute`, answered as `ExecuteStreamFrame`s (`ExecuteStreamReply`: `inline` bytes, a `ByteSource`, or `err`) so the result may grow to `maxStreamedResultBytes` / `maxStreamedResultRows`. The batch commits before the first frame. Adapters that do not stream advertise `0` for both caps and answer `unsupported` |
| `AdapterDatabaseSession.close` | Releases the session and its engine connection |

Some methods are host-private: never visible to plugin authors, and only
in first-party guests built with the abi `host` feature.

- `HostAdapterDatabaseSession.begin` opens a native interactive SeaORM
  transaction and returns an `AdapterTransaction` (`execute`, `commit`,
  `rollback`).
- The host records a sticky per-task fault when `begin` fails, so later
  statements can't fall back to autocommit. A failed `commit` is surfaced
  to `LibraryStore` (SeaORM's proxy hook is infallible).
- `AdapterDatabaseSession.execute` and `HostAdapterDatabaseSession.execute`
  carry the same `AdapterExecuteRequest` (canonical SQL, structured
  proofs, an optional receipt-persist hint).
- D1 keeps `begin` unsupported, and routes host `execute` through its
  native batch proxy.

The built-in ids are `sqlite`, `d1` and `postgres` (matching
`[database].plugin`).

### Isolated plugin database bindings

Plugins may declare Workers-style **named database bindings** in the
manifest:

```toml
[[databases]]
binding = "DB"

[[databases]]
binding = "CACHE"   # [A-Z][A-Z0-9_]*, unique, max 8
```

SQL bindings don't replace `[[kv_namespaces]]`. Named bindings are
**plugin-private state**, not a place to put host tables. The durable job
queue, library catalog and secrets stay in the host library database, and
`job(job)` never gets an `env` binding pointed at `library.db`.

The active adapter provisions bindings, physically separate from the
Bookclerk library and from every other plugin (close to a Cloudflare
Workers D1 binding):

- **SQLite:** one file per binding, under
  `$BOOKCLERK_FILES_DIR/plugin-databases/<plugin-key-fs-id>/<BINDING>.db`
  (the sqlite adapter jail grants that folder). The leaf is
  `PluginKey::fs_id()` (`pk-` plus 32 hex, 128 bits), not the display
  alias and not the canonical PluginKey (which isn't a valid Windows path
  component). The `plugin_databases` registry still stores the canonical
  PluginKey.
- **PostgreSQL:** one database per binding (`pb_` plus 32 hex of the
  `(plugin, binding)` digest; 35 ≤ 63 characters), created on first use
  (`CREATEDB` required). This is a separate database, not a schema on the
  library database, so plugin SQL can't see host tables.
- **Cloudflare D1:** one D1 database per binding (`bookclerk-pb-` plus the
  same 32 hex), found or created by name through the REST API. Provisioning
  fails closed with an operator-facing error when the API token can't
  create databases.
- **Third-party adapters:** advertise `DbCapabilities.pluginDatabases` and
  the backup flags they can actually provide (`consistentBackupRead`,
  `atomicUnitRestore`). The host opens plugin bindings through the active
  adapter session (`DatabaseAdapterConfig` + `provision`), and doesn't
  switch on sqlite, postgres or d1 crates. Adapters that don't advertise
  support fail the job, rather than sharing a database.

**Consent.** Each binding appears as a `database:<NAME>` grant entry, and
needs operator approval before enable, like other capabilities.

**Registry.** Provisioned units are recorded in the host
`plugin_databases` registry. An existing row wins, so re-opens never
re-target a binding.

Operators inspect and remove them with
`bookclerk plugins db list` and
`bookclerk plugins db drop <plugin> [binding]`. The drop command calls the
adapter's `Database.dropUnit`, then removes the registry row, and fails
closed if a physical delete can't be proven.

#### Schema migrations inside a binding

Inside a binding the plugin **owns its schema**. It registers a complete,
ordered history at startup (`databaseMigrations(binding)`): opaque IDs the
plugin chooses, plus already-separated BookclerkSQL `schema` and `data`
operations. Bookclerk gives those IDs no version meaning; registration
order is the forward sequence.

Registration is a bounded startup scalar:

- at most `maxListPage` migrations;
- `maxPluginMigrationOps` operations per migration;
- `maxPluginMigrationTotalOps` operations across the registration;
- each SQL at most `maxScalarBytes`;
- aggregate id and SQL UTF-8 at most
  `maxPluginMigrationRegistrationBytes`.

The host then:

1. Proves the sequence with one evolving type environment.
2. Checks that the durable `bookclerk_plugin_migrations` history is an
   exact prefix of the registration.
3. Applies only the pending suffix, one atomic receipt per registered
   migration (slot lock, operations, journal append).

Ordinary binding `execute` is query and DML only. Durable `CREATE` and
`DROP` are admitted only while the host applies that registration.
`ALTER` and `CREATE TABLE AS` stay refused.

Mixed guest `CREATE` plus `INSERT` isn't a product path: register and
apply, then DML. A same-token DML replay must not insert twice.

#### Reserved names

Inside a binding, the host reserves exactly one identifier prefix,
`bookclerk_`. All of its bookkeeping carries it:
`bookclerk_schema_migrations`, `bookclerk_plugin_migrations`,
`bookclerk_receipts`, `bookclerk_slots`, the `bookclerk_sql_*` catalog,
and the `bookclerk_src` wrap alias.

- Engine catalogs (`sqlite_*`, `pg_*`, `information_schema`) and
  schema-qualified names are reserved too.
- `REFERENCES` targets use the same reserved-name rules as `CREATE` and
  `DROP`.
- Any other name, including a framework's own `schema_migrations`, is the
  plugin's.
- The binding's own `bookclerk_receipts` and `bookclerk_plugin_migrations`
  tables stay host-owned, so retry tokens replay inside the binding and
  plugins can't edit the journal.

#### The SQL a plugin may use

The guest grammar still applies: a single statement, no `ATTACH`,
`PRAGMA` or session verbs, and no schema-qualified names. It is Bookclerk
SQL v1, not a wider SQLite dialect:

- Functions are the portable helpers, and `hex` is denied.
- Column types are `INTEGER`, `REAL`, `TEXT`, `BLOB` and `BOOLEAN`.
- `INSERT OR IGNORE` is for unique or primary-key conflicts only, and is
  lowered to `ON CONFLICT DO NOTHING` on every backend, including SQLite
  and D1. `SELECT` and `WITH` sources are wrapped as
  `SELECT * FROM (<source>) AS bookclerk_src WHERE true`.
- Helper arity and wire types are enforced, so callers needn't `CAST` for
  `round`, `sum`, `avg` or `count`.
- Canonical `LIKE` is case-sensitive (`GLOB` lowering on SQLite and D1,
  `COLLATE "C"` on Postgres).

Schema metadata is durable in the adapter-private
`bookclerk_sql_catalog`, `bookclerk_sql_schema` and `bookclerk_sql_ddl`
tables (Postgres identity is in `bookclerk_identity`). All are
guest-denied.

- Opening a binding reloads types from that catalog.
- A matching no-op `CREATE TABLE IF NOT EXISTS` doesn't rewrite the
  catalog or identity. The first admitted CREATE must persist complete
  canonical DDL, and a binding missing that catalog fails closed (reset or
  recreate it).

See the [SQL contract v1](sql-contract/v1.md).

#### How bindings reach the guest

`PluginWorker.open` receives the bindings as the append-only
`Bindings.databases :List(NamedDatabase)` field.

- Rust guests call
  `DatabaseBinding::take_named_from_bindings(&mut bindings, "DB")`.
- For workerd guests, the launcher mints one database-only grant token per
  binding at `open`, and carries it in the bridge context
  (`databases: { DB: <token> }`). The trusted adapter isolate turns each
  token into a granted-channel transport over `POST /db/execute` (with
  `GET /db/limits` for `limits()`; the launcher also serves
  `POST /db/execute-streamed`, the streamed read the SDK helpers adopt
  next, see the [SQL contract](sql-contract/v1.md#streamed-results)), and
  installs the resulting
  `env.DB` on the author's `env` for every call of that `open` (events,
  storefront, jobs, …). It is a full `DatabaseBinding`: `prepare`, `bind`,
  `run`, `all`, `first`, `raw`, `batch` and `limits()`.

Authors never see the token.

## Next steps

- [plugin.toml](manifest.md): declare entrypoints, triggers and bindings.
- [Capabilities and consent](capabilities.md): what each binding needs
  approved.
- [Jobs](../internals/jobs.md): the durable queue behind jobs and event
  deliveries.
