# TypeScript plugin SDK

This page shows how to write a Bookclerk plugin in TypeScript with
`@bookclerk/plugin-sdk` (`api_version = 3`). A TypeScript plugin runs in
a workerd isolate; `bookclerk-workerd` injects the SDK into the isolate
under its package name, so your module imports it like any dependency.

| Import | What it holds |
| --- | --- |
| `@bookclerk/plugin-sdk/workerd` | `BookclerkEntrypoint` and the named `*Entrypoint` bases (they extend `WorkerEntrypoint`) |
| `@bookclerk/plugin-sdk` | The ABI types, codecs and the same author classes |
| `@bookclerk/plugin-sdk/sparse-workerd` | Run a workerd plugin without a built host (`smoke`) |

## Install

Depend on the package from npm, a `file:` path or a workspace. Never
import a relative path to the embed; the module name is what the host
injects.

```json
{
  "dependencies": {
    "@bookclerk/plugin-sdk": "file:../../packages/plugin-sdk"
  }
}
```

## Author model

A plugin is a Workers-shaped module. The **default export** extends
`BookclerkEntrypoint` and implements the triggers `plugin.toml` declares;
every **named entrypoint** in `entrypoints = [...]` is an exported class
with the matching name (`storefront` → `Storefront`, `cli` → `Cli`, …)
extending its base:

| `plugin.toml` | Export | Base class |
| --- | --- | --- |
| `[[events.consumers]]` | `default` | `BookclerkEntrypoint.event(batch)` |
| `[triggers] jobs = [...]` | `default` | `BookclerkEntrypoint.job(job)` |
| `[triggers] crons = [...]` | `default` | `BookclerkEntrypoint.scheduled(controller)` |
| `[[databases]]` | `default` | `BookclerkEntrypoint.databaseMigrations(binding)` |
| `entrypoints = ["storefront"]` | `Storefront` | `StorefrontEntrypoint` |
| `entrypoints = ["storage"]` | `Storage` | `StorageEntrypoint` |
| `entrypoints = ["remoteLibrary"]` | `RemoteLibrary` | `RemoteLibraryEntrypoint` |
| `entrypoints = ["databaseAdapter"]` | `DatabaseAdapter` | `DatabaseAdapterEntrypoint` |
| `entrypoints = ["cli"]` | `Cli` | `CliEntrypoint` |
| `entrypoints = ["oidc"]` | `Oidc` | `OidcEntrypoint` |
| `entrypoints = ["metadata"]` | `Metadata` | `MetadataEntrypoint` |
| `entrypoints = ["covers"]` | `Covers` | `CoverEntrypoint` |

```ts
import {
  BookclerkEntrypoint,
  CliEntrypoint,
  cliArgs,
  jsonPayload,
  type CliInvokeParams,
  type EventBatch,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "./bookclerk-configuration.js";

export class Cli extends CliEntrypoint<Env> {
  async describe() {
    return { commands: [{ name: "ping", about: "Probe", args: [] }] };
  }
  async invoke(params: CliInvokeParams) {
    const { message = "hi" } = cliArgs(params);
    return { exitCode: 0, stdout: `pong: ${message}\n`, stderr: "", payload: jsonPayload({ pong: message }) };
  }
}

export default class MyPlugin extends BookclerkEntrypoint<Env> {
  async event(batch: EventBatch) {
    for (const msg of batch.messages) {
      if (msg.type !== "book_acquired") {
        msg.reject(`unexpected ${msg.type}`);
        continue;
      }
      await this.env.EVENTS.publish({ eventType: "echo_seen", payload: msg.body });
      msg.ack();
    }
  }
}
```

Identity (`apiVersion`, `id`, capabilities) comes from `plugin.toml`; an
optional `describe()` on the default export only refines `displayName`
and similar fields.

A database adapter runs each host batch through `executeAdapterBatch` or
`executeAdapterBatchStreamed`, which stores the guest's replay receipt
before `COMMIT` and, for a streamed reply, rolls back when the frames do
not fit one scalar or the adapter's advertised streamed budget
(`executeStreamLimits(caps)`) (see
[Isolate adapter batches](../../sql-contract/v1.md#isolate-adapter-batches-and-replay-receipts)).

## Triggers

- `event(batch)` mirrors Workers `queue(batch)`. Each `EventMessage`
  records exactly one outcome, `ack()`, `retry({ retryAt | delaySeconds,
  reason })`, `reject(reason)`, `deadLetter(reason)` or `suspend({
  checkpoint, wakeAt, wakeOnEventType, wakeOnFilter })`, and the first
  call wins. Untouched messages ack when the handler returns and retry
  when it throws.
- `job(job)` mirrors Workers `scheduled(controller)`. Return (optionally
  `{ message, bytesCopied }`) to complete, throw to reject
  (`unavailable` and `deadline_exceeded` errors are retryable,
  `cancelled` is cancelled), or call `job.suspend({ checkpoint, wakeAt
  })` or `job.retryLater({ retryAt, reason })`. `job.input`,
  `job.output`, `job.progress()` and `job.signal` expose the granted
  streams and host cancellation.

## The `env` bindings

The host grants bindings per invocation on `this.env`: `CONFIG` (`[vars]`
plus operator settings), `SECRETS` (`[secrets]`), `EVENTS`
(`[[events.producers]]`), `WORK_FS` (`[work_fs]`), and one
`DatabaseBinding` per `[[databases]]` entry under its `binding` name.
A binding's `limits()` returns the `DbCapabilities` its session negotiated
(`maxRequestBytes`, `maxStatements`, `maxResultBytes`, `maxResultRows`,
…); a request or result over one of them rejects with a `PluginError`
whose `code` is `limit_exceeded` and whose `limit` names the cap. On a
host that predates `limits()` the call rejects with `unsupported`.
`all()` reads through the session's streamed budget when it advertises
one, `prepare(sql).iterate()` yields rows one at a time from the same
read, `db.chunk(statements)` splits a long list into batches that each
fit one request, and `db.writeChunks(...)` commits those batches
resumably; see
[Working within database limits](../../database-limits.md).
A storefront's `scan` can hand its paging to `scanPage(params, fetch)`,
which slices the store's own pages into replies the host accepts; see
[Paged scans](../../abi.md#paged-scans).
Generate the `Env` type from `plugin.toml`:

```bash
npx bookclerk-plugin types .            # writes bookclerk-configuration.d.ts
npx bookclerk-plugin types . --out src/env.d.ts
```

## Testing database code

`createTestDatabase()` from `@bookclerk/plugin-sdk/testkit` gives a
Node test suite the same binding a host grants, served by the
`bookclerk-db-testkit` executable under one capability profile with the
plugin's `databaseMigrations` applied. `bookclerk-plugin test .` runs the
suite once per profile (`first-party`, `d1` and `minimum`), so code that
passes on the shipped adapters is also proven against D1's caps and the
smallest advertisement a host accepts:

```ts
import { createTestDatabase } from "@bookclerk/plugin-sdk/testkit";

const db = await createTestDatabase({ migrations: MIGRATIONS }); // profile from `bookclerk-plugin test`
try {
  await addNotes(db.binding, ["hello"]);
} finally {
  await db.close();
}
```

The `echo_workerd_ts` example (`examples/plugins-echo-workerd-ts/`) is a
worked plugin: a `notes` table it writes with `chunk()`, lists with
`iterate()` or key-paged reads, sizes with `limits()`, and tests under
every profile. See
[Testing against the limits](../../database-limits.md#testing-against-the-limits).

## Author tools

```bash
npx bookclerk-plugin check .
npx bookclerk-plugin types [dir] [--out <file>]
npx bookclerk-plugin fmt [--check] plugin.toml
npx bookclerk-plugin package --out dist/plugins .
npx bookclerk-plugin smoke .   # download the pinned workerd, then describe + health
```

`check` enforces the v3 author model: the main module imports the SDK,
does not reference the removed `BookclerkPlugin` base, and exports one
class per named entrypoint.

`smoke` does not need a built `bookclerk-workerd`. It downloads the
pinned Cloudflare `workerd` into `~/.cache/bookclerk/workerd` (override
with `BOOKCLERK_WORKERD_CACHE` or `BOOKCLERK_WORKERD_BIN`).

## Next steps

- [plugin.toml](../../manifest.md): declare the entrypoints and
  triggers the classes above implement.
- [Entrypoints, events and the ABI](../../abi.md): every method and
  event in detail.
- [Try the Echo example](../../README.md#try-the-echo-example): a
  complete TypeScript plugin under `examples/`.

## Reference

The sections below are generated from the SDK's source: one page per
module, and what the package root exports.
