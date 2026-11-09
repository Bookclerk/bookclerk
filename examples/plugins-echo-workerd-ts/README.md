# Echo Integration (workerd TypeScript)

Greenfield Workers RPC guest authored against the `api_version = 3` model
from `@bookclerk/plugin-sdk/workerd` (not bare `WorkerEntrypoint`). Id:
`echo_workerd_ts`. Isolation: `bookclerk-jail` + `bookclerk-workerd` + pinned
Cloudflare `workerd`.

Authoring: `src/index.ts` follows

```ts
import { BookclerkEntrypoint, CliEntrypoint, cliArgs, jsonPayload } from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";

// `entrypoints = ["cli"]` in plugin.toml → exported `Cli` class.
export class Cli extends CliEntrypoint<Env> {
  async describe() { /* CLI schema */ }
  async invoke(params) {
    const { message = "hi" } = cliArgs(params);
    return { exitCode: 0, stdout: `pong: ${message}\n`, stderr: "", payload: jsonPayload({ pong: message }) };
  }
}

// `[[events.consumers]]` in plugin.toml → default export `event(batch)` trigger.
export default class EchoPlugin extends BookclerkEntrypoint<Env> {
  async describe() {
    return { displayName: "Echo Integration (workerd TypeScript)" };
  }
  async event(batch) {
    for (const msg of batch.messages) {
      if (msg.type === "book_acquired") msg.ack();
      else msg.reject(`unexpected ${msg.type}`);
    }
  }
}
```

`apiVersion`, `id`, and capabilities come from `plugin.toml`; the host merges
that projection over `describe()`. `bookclerk-configuration.d.ts` is generated
from the manifest with `npx bookclerk-plugin types .` and typed as
`Env` (`CONFIG`, plus any granted `SECRETS` / `EVENTS` / `WORK_FS` / database
bindings).

(`package.json` depends on `@bookclerk/plugin-sdk`; the isolate import is
injected by `bookclerk-workerd`). The `echo_native_node` example is also a
workerd guest (same JS class ABI).

See [docs/adr/plugin-workers-rpc-workerd.md](../../docs/adr/plugin-workers-rpc-workerd.md).

## The `notes` database

The `DB` binding (`[[databases]]` in `plugin.toml`) holds a `notes` table,
a worked example of the SDK database helpers
([docs/plugins/database-limits.md](../../docs/plugins/database-limits.md)).
`src/notes.ts` keeps the database code and `src/index.ts` wires it up:

- `databaseMigrations("DB")` registers the table; the host applies it
  before it grants the binding.
- `notes-add --body <text> --count <n>` checks each note against
  `limits().maxCellBytes`, then writes with `chunk()` + `batch()`.
- `notes-list` reads with one `iterate()` when the session streams, and
  otherwise pages by `id`, sized from `maxResultRows` and halved when a
  page passes `maxResultBytes`.
- A typed `limit_exceeded` error is reported by the cap it names.

```bash
bookclerk plugins echo_workerd_ts notes-add --body "remember" --count 3
bookclerk plugins echo_workerd_ts notes-list
```

## Build and test

`src/` is the source; `npm run build` emits the shipped `modules/`, and
CI fails when the committed `modules/` differs from a fresh build.

```bash
cd packages/plugin-sdk && npm ci && npm run build
cd ../../examples/plugins-echo-workerd-ts && npm ci
npx bookclerk-plugin types .    # regenerate bookclerk-configuration.d.ts after editing plugin.toml
npm run typecheck
npm run build                   # rewrite modules/ from src/
```

The tests (`test/*.test.ts`) open the binding with `createTestDatabase()`
from `@bookclerk/plugin-sdk/testkit`, which spawns `bookclerk-db-testkit`.
Run them from the repository root under every database profile:

```bash
cargo build -p bookclerk-db-testkit
cargo plugin -- test examples/plugins-echo-workerd-ts
```

Install layout: `plugin.toml` + `modules/` under
`$BOOKCLERK_FILES_DIR/plugins/echo_workerd_ts/` (or staged artifacts).

`cargo build-app --platform` / `cargo dev` / `cargo ensure-workerd` fetch the
pinned Cloudflare `workerd` beside `target/<profile>/bookclerk-workerd`. There
is no JS-less shim — workerd guests require that binary. Override path with
`BOOKCLERK_WORKERD_BIN` if needed.

Out-of-tree author smoke (no Rust `bookclerk-workerd` binary required):

```bash
npx bookclerk-plugin smoke .   # from packages/plugin-sdk after npm ci && npm run build
```

Sibling examples: `plugins-echo-workerd-python`, `plugins-echo-workerd-rust`,
`plugins-echo-native-*`.
