# Echo Integration (workerd TypeScript)

Greenfield Workers RPC guest authored against the `api_version = 3` model
from `@bookclerk/plugin-sdk/workerd` (not bare `WorkerEntrypoint`). Id:
`echo_workerd_ts`. Isolation: `bookclerk-jail` + `bookclerk-workerd` + pinned
Cloudflare `workerd`.

Authoring: `src/index.ts` and `modules/index.js` both follow

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

```bash
cd packages/plugin-sdk && npm ci && npm run build
cd ../../examples/plugins-echo-workerd-ts && npm ci
npx bookclerk-plugin types .    # regenerate bookclerk-configuration.d.ts after editing plugin.toml
npm run typecheck
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
