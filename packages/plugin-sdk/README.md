# @bookclerk/plugin-sdk

TypeScript / Node guest SDK for Bookclerk plugins (`api_version = 1`).
**Dual-stack:** workerd isolates and native stdio guests.

| Import | Runtime |
| --- | --- |
| `@bookclerk/plugin-sdk/workerd` | `BookclerkPlugin` extends `WorkerEntrypoint` |
| `@bookclerk/plugin-sdk/native` | `BookclerkPlugin` + `BookclerkPluginGuest.serve()` |
| `@bookclerk/plugin-sdk` | ABI types + `BookclerkPlugin` (workerd, backwards compatible) |

`BookclerkPlugin` is the guest contract on both stacks. `BookclerkPluginGuest`
is the native stdin/stdout Workers RPC runner (workerd hosts the class via
`WorkerEntrypoint` instead).

Depend on the package (npm / `file:` / workspace) — do **not** import a relative
embed path. `bookclerk-workerd` injects `@bookclerk/plugin-sdk/workerd` into the
isolate under that exact module name.

## Workerd

```ts
import {
  BookclerkPlugin,
  type HandshakeParams,
  type HandshakeResult,
} from "@bookclerk/plugin-sdk/workerd";

export default class MyPlugin extends BookclerkPlugin {
  async handshake(_params: HandshakeParams): Promise<HandshakeResult> {
    return {
      apiVersion: 1,
      id: "my-plugin",
      kind: "integration",
      capabilities: ["health"],
    };
  }
}
```

`package.json`:

```json
{
  "dependencies": {
    "@bookclerk/plugin-sdk": "file:../../packages/plugin-sdk"
  }
}
```

## Native Node

```js
import { BookclerkPlugin, BookclerkPluginGuest } from "@bookclerk/plugin-sdk/native";

class MyPlugin extends BookclerkPlugin {
  handshake() {
    return {
      apiVersion: 1,
      id: "my-plugin",
      kind: "integration",
      capabilities: ["health"],
    };
  }
}

await BookclerkPluginGuest.serve(new MyPlugin());
```

## Author tools

```bash
npx bookclerk-plugin check .
npx bookclerk-plugin fmt [--check] plugin.toml
npx bookclerk-plugin package --out dist/plugins .
npx bookclerk-plugin smoke .   # workerd plugins: download pin, handshake + health
```

`smoke` does **not** need a built Rust `bookclerk-workerd` binary. It downloads
the pinned Cloudflare `workerd` into `~/.cache/bookclerk/workerd` (override with
`BOOKCLERK_WORKERD_CACHE` / `BOOKCLERK_WORKERD_BIN`), materializes Cap’n Proto +
bridge under the plugin dir, then POSTs `handshake` / `health` to `/rpc`.

See Echo examples under [`examples/`](../../examples/).
