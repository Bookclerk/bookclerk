# bookclerk-workerd bridge transport

Maintainer reference for the private wire between the `bookclerk-workerd`
launcher and the isolate it hosts. Authors never see any of this: they write
`BookclerkEntrypoint` / `*Entrypoint` classes against
`@bookclerk/plugin-sdk/workerd` (or the Python mirror) and the trusted
adapter isolate maps the wire onto them. See [`plugins.md`](plugins.md) for
the author model and the [Workers RPC ADR](adr/plugin-workers-rpc-workerd.md)
for the decision record.

```
host ──stdio Cap'n Proto (plugin.capnp)──▶ bookclerk-workerd
                                              │ loopback HTTP, Bearer BRIDGE_TOKEN
                                              ▼
                                   bridge.js ──Workers RPC──▶ adapter isolate ──Workers RPC──▶ author classes
                                              ▲
                     granted channel (isolate → launcher) ◀── GRANTED service binding
```

The launcher is a `PluginWorker` toward the host. Every host call is turned
into exactly one HTTP request to the isolate; the isolate keeps no cross-request
state except database-adapter sessions (below). Three route families exist:

| Family | Routes | Encoding |
| --- | --- | --- |
| Control plane | `GET /health`, `POST /describe`, `POST /open`, `POST /shutdown` | JSON handshake (bridge JSON: camelCase projection of the typed structs, `Data` as base64) |
| Data plane | `POST /invoke` | Cap'n Proto bytes — one route for **every** entrypoint method and for `PluginWorker.databaseMigrations` |
| Streams | `GET /destination/get`, `PUT /destination/put`, `GET /source/open` | HTTP bodies (the object bytes never enter a Cap'n message) |

The control plane is a policy handshake, not a plugin method: `open` mints
grant tokens and asks the isolate which entrypoint families the signed
manifest lets it export; the isolate does not retain the `Bindings`. That is
why `PluginWorker.open` / `describe` / `shutdown` stay JSON while everything
that *is* an ABI method call travels as Cap'n bytes on `/invoke`.

## `POST /invoke`

One ABI method call. Same framing as `/db/execute` on the granted channel:
an **unpacked, single-segment** Cap'n Proto message (`capnp::serialize::write_message`
in Rust; `encodeMessage` from the generated `generated-wire.ts` codecs in the
isolate; `_wire.py` in Python).

Request headers:

| Header | Value |
| --- | --- |
| `Authorization` | `Bearer <BRIDGE_TOKEN>` |
| `Content-Type` | `application/x-capnp` |
| `X-Bookclerk-Interface` | Cap'n interface name: `ContentSource`, `Destination`, `RemoteLibrary`, `EventConsumer`, `JobRunner`, `PluginCli`, `Oidc`, `Database`, `AdapterDatabaseSession`, `PluginWorker` |
| `X-Bookclerk-Method` | Method name as declared in `plugin.capnp` (`login`, `event`, `job`, `openSession`, `databaseMigrations`, …) |
| `X-Bookclerk-Context` | Bridge JSON `BridgeContext` (`invocation`, `config`, `secrets`, `eventsToken`, `databases` = `{ <BINDING>: <grant token> }`; `jobId` on `JobRunner.job`) — present for every entrypoint call, absent for `PluginWorker.*`. The adapter isolate exchanges the tokens for the author's `EVENTS` / `env.<BINDING>` bindings; authors never see them |
| `X-Bookclerk-Caps` | JSON array of capability descriptors for the request message's capability table (only when the `$Params` struct carries interface-typed fields) |
| `X-Bookclerk-Target` | Isolate-side object id for calls on a capability the isolate returned earlier (`AdapterDatabaseSession.*`) |

Request body: the `<Interface>.<method>$Params` struct as the message root
(the `plugin.layout.json` struct named `"<Interface>.<method>$Params"`; the
generated codecs are `<Interface><Method>ParamsCodec`).

Response:

| Status | Meaning |
| --- | --- |
| `200` | Body is the `<Interface>.<method>$Results` struct (`Content-Type: application/x-capnp`). ABI failures ride **inside** the reply union (`result.err`), exactly as over Cap'n RPC. `X-Bookclerk-Caps` lists descriptors for capabilities the reply carries. |
| `400` | Malformed message, unknown interface/method, missing context, bad capability table. JSON `{ "error": { "code", "message" } }` |
| `401` | Bearer mismatch |
| `404` | Entrypoint not exported by this isolate (`{ "error": { "code": "unsupported", … } }`) |
| `500` | Adapter binding missing / isolate failure (JSON error) |

Non-200 replies are transport failures, never a plugin's own `err`. The
launcher's hook (`crates/bookclerk-workerd/src/invoke.rs`) raises them as Cap'n
RPC errors whose text is `<code>: <message>` (`404` / `unsupported` as
`Unimplemented`, everything else as `Failed`), so the typed clients surface
them exactly like a broken Cap'n connection: `PluginError::unavailable` with
the bridge's wire code and message in the text. Request and reply bodies are
capped at `MAX_INVOKE_BODY_BYTES` (2 × `maxScalarBytes`, the isolate codec's
traversal budget); the launcher always emits a single segment (it re-copies
the params when the heap builder spilled) and rejects multi-segment replies.

### Capability descriptors (`X-Bookclerk-Caps`)

Unpacked Cap'n messages have no capability table of their own, so interface
pointers are written as capability indexes and the table travels beside the
message as JSON. Entry `i` describes capability index `i`:

```jsonc
[
  { "kind": "source",       "token": "<grant>" },   // Source          → granted GET  /open
  { "kind": "destination",  "token": "<grant>" },   // Destination     → granted PUT  /put
  { "kind": "progress",     "token": "<grant>" },   // ProgressSink    → granted POST /progress
  { "kind": "cancellation", "token": "<grant>" },   // Cancellation    → granted GET  /cancel (long poll)
  { "kind": "eventPublisher", "token": "<grant>" }, // EventPublisher  → granted POST /events/publish
  { "kind": "guestDatabase", "token": "<grant>", "binding": "DB" }, // GuestDatabase → granted POST /db/execute
  { "kind": "adapterSession", "id": "<isolate object id>" }         // reply cap: AdapterDatabaseSession
]
```

Host-served kinds carry a **grant token** the isolate presents on the granted
channel; the launcher mints them per call and revokes them when the call
returns (`RevokeGrant`). The only isolate-served kind is `adapterSession`:
`Database.openSession` replies with `AdapterSessionReply.ok` pointing at an
isolate object; later `AdapterDatabaseSession.*` calls name it in
`X-Bookclerk-Target`. `close` (or the owning `open` being dropped) releases it.

Streams are never capabilities on this transport: `Destination.get`,
`Destination.put`, and `Source.open` use the dedicated body routes, and
`JobController.input` / `output` reach the launcher through the granted
`GET /open` / `PUT /put` routes.

### Method table

| Interface | Methods on `/invoke` | Author class |
| --- | --- | --- |
| `PluginWorker` | `databaseMigrations` | default export `databaseMigrations(binding)` |
| `ContentSource` | all 13 | `Storefront extends StorefrontEntrypoint` |
| `Destination` | `head`, `list`, `copy`, `delete`, `commit`, `abortStage` (`get` / `put` are stream routes) | `Storage extends StorageEntrypoint` |
| `RemoteLibrary` | all 7 | `RemoteLibrary extends RemoteLibraryEntrypoint` |
| `EventConsumer` | `event` — **one call per batch** | default export `event(batch)` |
| `JobRunner` | `job` — `JobController` caps are grant tokens | default export `job(job)` |
| `PluginCli` | `describe`, `invoke` | `Cli extends CliEntrypoint` |
| `Oidc` | `clients`, `authenticateUser` | `Oidc extends OidcEntrypoint` |
| `Database` | `openSession` | `DatabaseAdapter extends DatabaseAdapterEntrypoint` |
| `AdapterDatabaseSession` | all 10 (`X-Bookclerk-Target`) | the `AdapterDatabaseSession` object `openSession` returned |

Native-behind-workerd guests never touch `/invoke`: the launcher forwards
their entrypoint families as typed Cap'n Proto straight from its own vat
(`Backend::Native`), and only the control plane reaches the isolate.

## Granted channel (isolate → launcher)

`GRANTED` is a service binding on the adapter isolate; every request carries
`Authorization: Bearer <grant token>` and the launcher fails closed on
unknown, expired, or revoked tokens.

| Route | Grant flag | Purpose |
| --- | --- | --- |
| `GET /open?key=` | `allow_open` | `JobController.input.open` (streamed body) |
| `PUT /put?key=` | `allow_put` | `JobController.output.put` (streamed body) |
| `POST /progress` | `allow_progress` | `JobController.progress.report` |
| `GET /cancel` | `cancel` present | Long poll: `200 {"cancelled":true}` once the host cancellation fires, `204` after the poll window so the isolate re-arms; the adapter turns it into `job.signal` |
| `POST /db/execute` | `allow_database` | Typed SQL on the granted `GuestDatabase` (Cap'n bytes) |
| `POST /events/publish` | `events` present | `env.EVENTS.publish` |

## Isolate embed

The adapter and author isolates import `@bookclerk/plugin-sdk/workerd`; the
launcher injects `packages/plugin-sdk/embed/bookclerk_plugin.js` under that
name. The embed is **built**, not hand-written: `npm run build` in
`packages/plugin-sdk` bundles `dist/workerd.js` (author runtime, generated
codecs, typed SQL helpers) into one ES module with `scripts/build-embed.mjs`
(esbuild; only `cloudflare:workers` stays external). CI rebuilds it and fails
on drift; `scripts/gen-plugin-abi.py --check` and `scripts/sync-workerd-pin.py --check`
keep the Rust (`crates/bookclerk-plugin-sdk/embed/`) and Python
(`bookclerk_plugin_sdk/bridge/`) mirrors byte-identical.
