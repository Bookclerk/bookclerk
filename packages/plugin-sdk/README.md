# @bookclerk/plugin-sdk

TypeScript guest SDK for Bookclerk plugins (`api_version = 3`).
Workerd isolates host the author classes. Native guests use Rust `serve` /
`PluginWorker`.

| Import | Runtime |
| --- | --- |
| `@bookclerk/plugin-sdk/workerd` | `BookclerkEntrypoint` + named `*Entrypoint` bases (extend `WorkerEntrypoint`) |
| `@bookclerk/plugin-sdk` | ABI types, codecs, and the same workerd author classes |

## Author model

A plugin is a Workers-shaped module. The **default export** extends
`BookclerkEntrypoint` and implements the triggers `plugin.toml` declares;
every **named entrypoint** in `entrypoints = [...]` is an exported class with
the matching name (`storefront` → `Storefront`, `cli` → `Cli`, …) extending
its base:

| `plugin.toml` | Export | Base class |
| --- | --- | --- |
| `[[events.consumers]]` | `default` | `BookclerkEntrypoint.event(batch)` |
| `[triggers] jobs = [...]` | `default` | `BookclerkEntrypoint.job(job)` |
| `[[databases]]` | `default` | `BookclerkEntrypoint.databaseMigrations(binding)` |
| `entrypoints = ["storefront"]` | `Storefront` | `StorefrontEntrypoint` |
| `entrypoints = ["storage"]` | `Storage` | `StorageEntrypoint` |
| `entrypoints = ["remoteLibrary"]` | `RemoteLibrary` | `RemoteLibraryEntrypoint` |
| `entrypoints = ["databaseAdapter"]` | `DatabaseAdapter` | `DatabaseAdapterEntrypoint` |
| `entrypoints = ["cli"]` | `Cli` | `CliEntrypoint` |
| `entrypoints = ["oidc"]` | `Oidc` | `OidcEntrypoint` |

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
optional `describe()` on the default export only refines `displayName` and
similar fields.

### Triggers

- `event(batch)` mirrors Workers `queue(batch)`. Each `EventMessage` records
  exactly one outcome — `ack()`, `retry({ retryAt | delaySeconds, reason })`,
  `reject(reason)`, `deadLetter(reason)`, or
  `suspend({ checkpoint, wakeAt, wakeOnEventType, wakeOnFilter })` — and the
  first call wins. Untouched messages ack when the handler returns and retry
  when it throws.
- `job(job)` mirrors Workers `scheduled(controller)`. Return (optionally
  `{ message, bytesCopied }`) to complete, throw to reject (`unavailable` /
  `deadline_exceeded` errors are retryable, `cancelled` is cancelled), or call
  `job.suspend({ checkpoint, wakeAt })` / `job.retryLater({ retryAt, reason })`.
  `job.input`, `job.output`, `job.progress()`, and `job.signal` expose the
  granted streams and host cancellation.

### `env` bindings

The host grants bindings per invocation on `this.env`: `CONFIG` (`[vars]` plus
operator settings), `SECRETS` (`[secrets]`), `EVENTS` (`[[events.producers]]`),
`WORK_FS` (`[work_fs]`), and one `DatabaseBinding` per `[[databases]]` entry
under its `binding` name. Generate the `Env` type from `plugin.toml`:

```bash
npx bookclerk-plugin types .            # writes bookclerk-configuration.d.ts
npx bookclerk-plugin types . --out src/env.d.ts
```

Depend on the package (npm / `file:` / workspace) — do **not** import a relative
embed path. `bookclerk-workerd` injects `@bookclerk/plugin-sdk/workerd` into the
isolate under that exact module name.

`package.json`:

```json
{
  "dependencies": {
    "@bookclerk/plugin-sdk": "file:../../packages/plugin-sdk"
  }
}
```

## Author tools

```bash
npx bookclerk-plugin check .
npx bookclerk-plugin types [dir] [--out <file>]
npx bookclerk-plugin fmt [--check] plugin.toml
npx bookclerk-plugin package --out dist/plugins .
npx bookclerk-plugin smoke .   # workerd plugins: download pin, describe + health
```

`check` enforces the v3 author model: the main module imports the SDK, does not
reference the removed `BookclerkPlugin` base, and exports one class per named
entrypoint.

`smoke` does **not** need a built Rust `bookclerk-workerd` binary. It downloads
the pinned Cloudflare `workerd` into `~/.cache/bookclerk/workerd` (override with
`BOOKCLERK_WORKERD_CACHE` / `BOOKCLERK_WORKERD_BIN`).

## Tests

```bash
npm run build
npm run lint
npm run test:tools            # check / fmt fixtures
npm run test:db-value         # DbValue goldens
npm run test:plugin-migrations
npm run test:entrypoints      # event/job translation, env bindings, named dispatch
```
