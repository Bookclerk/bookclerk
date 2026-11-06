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
Generate the `Env` type from `plugin.toml`:

```bash
npx bookclerk-plugin types .            # writes bookclerk-configuration.d.ts
npx bookclerk-plugin types . --out src/env.d.ts
```

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
